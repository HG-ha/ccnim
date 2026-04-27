use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use app_config::{AppConfig, NimApiKey};
use async_stream::stream;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use nim_client::{KeyPool, KeyPoolEntry, KeySnapshot, NimClient};
use proxy_core::{
    anthropic_to_nim, count_input_tokens, MessagesRequest, ModelMapping, ModelsListResponse,
    NimRequestOptions, SseBuilder, TokenCountRequest, TokenCountResponse,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<AppConfig>,
    pub nim: NimClient,
    pub key_pool: KeyPool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub listen_url: String,
    pub default_model: String,
    pub keys: Vec<KeySnapshot>,
}

pub struct RunningServer {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    addr: SocketAddr,
    key_pool: KeyPool,
}

/// Hard cap on how long we wait for axum's graceful shutdown to drain
/// in-flight requests before we force the listener closed. SSE streams
/// (Claude Code keeps `/v1/messages` connections open for the whole reply)
/// will happily sit on the socket for the full request timeout — without
/// a deadline here, exiting the GUI while a chat is mid-stream would leave
/// the TCP port stuck in `LISTEN` until the upstream actually finishes.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn key_pool(&self) -> &KeyPool {
        &self.key_pool
    }

    /// Trigger axum's graceful shutdown and wait for the serve task to
    /// finish, but cap the total wait at [`GRACEFUL_SHUTDOWN_TIMEOUT`].
    /// On timeout we abort the task: dropping the listener inside it is
    /// what releases the listening socket, which is the whole point — any
    /// half-open client connection is the OS's problem from there.
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, &mut self.task).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(
                    timeout = ?GRACEFUL_SHUTDOWN_TIMEOUT,
                    "proxy server did not finish graceful shutdown in time; aborting to release port"
                );
                self.task.abort();
                let _ = self.task.await;
            }
        }
    }
}

/// Convert the structured per-key configuration carried in
/// [`AppConfig`] into the runtime-only [`KeyPoolEntry`] shape understood by
/// [`KeyPool`]. Centralised here so callers (the GUI, the standalone proxy)
/// don't each repeat the field-by-field copy.
pub fn key_pool_entries(keys: &[NimApiKey]) -> Vec<KeyPoolEntry> {
    keys.iter()
        .map(|k| KeyPoolEntry {
            value: k.value.clone(),
            label: k.label.clone(),
            expires_at: k.expires_at,
        })
        .collect()
}

pub async fn start_server(config: AppConfig) -> anyhow::Result<RunningServer> {
    let addr: SocketAddr = config.listen_addr().parse()?;
    let key_pool = KeyPool::new(
        key_pool_entries(&config.nim_api_keys),
        config.rate_limit_per_key,
        Duration::from_secs(config.rate_window_secs),
    );
    let nim = NimClient::new(key_pool.clone())?;
    let state = ProxyState {
        config: Arc::new(config),
        nim,
        key_pool: key_pool.clone(),
    };
    let app = router(state);
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        info!("Rust proxy listening on http://{addr}");
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = rx.await;
        });
        if let Err(err) = server.await {
            error!("proxy server failed: {err}");
        }
    });
    Ok(RunningServer {
        shutdown: Some(tx),
        task,
        addr,
        key_pool,
    })
}

pub fn router(state: ProxyState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/nim/models", get(nim_models))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn root(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    match require_auth(&state, &headers) {
        Ok(()) => Json(serde_json::json!({
            "status": "ok",
            "provider": "nvidia_nim",
            "model": state.config.model_mapping.default_model
        }))
        .into_response(),
        Err(response) => response,
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "healthy" }))
}

async fn status(State(state): State<ProxyState>) -> impl IntoResponse {
    Json(ProxyStatus {
        running: true,
        listen_url: format!("http://{}:{}", state.config.host, state.config.port),
        default_model: state.config.model_mapping.default_model.clone(),
        keys: state.key_pool.snapshots(),
    })
}

async fn models(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    match require_auth(&state, &headers) {
        Ok(()) => Json(ModelsListResponse::claude_compatible()).into_response(),
        Err(response) => response,
    }
}

async fn nim_models(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    match require_auth(&state, &headers) {
        Ok(()) => match state.nim.list_models().await {
            Ok(models) => Json(models).into_response(),
            Err(err) => api_error(StatusCode::BAD_GATEWAY, err.to_string()),
        },
        Err(response) => response,
    }
}

async fn count_tokens(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(request): Json<TokenCountRequest>,
) -> Response {
    match require_auth(&state, &headers) {
        Ok(()) => Json(TokenCountResponse {
            input_tokens: count_input_tokens(
                &request.messages,
                request.system.as_ref(),
                request.tools.as_deref(),
            ),
        })
        .into_response(),
        Err(response) => response,
    }
}

async fn messages(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(mut request): Json<MessagesRequest>,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return response;
    }
    if request.messages.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "messages cannot be empty");
    }

    let mapping: ModelMapping = state.config.model_mapping.clone().into();
    request.model = mapping.resolve(&request.model);
    let input_tokens = count_input_tokens(
        &request.messages,
        request.system.as_ref(),
        request.tools.as_deref(),
    );
    let body = anthropic_to_nim(
        &request,
        &NimRequestOptions {
            enable_thinking: state.config.enable_thinking,
            ..NimRequestOptions::default()
        },
    );
    let stream = match state.nim.stream_chat(body).await {
        Ok(stream) => stream,
        Err(err) => return api_error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    let sse = stream_anthropic_sse(request.model, input_tokens, stream);
    Sse::new(sse)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn stream_anthropic_sse(
    model: String,
    input_tokens: usize,
    mut upstream: impl Stream<Item = nim_client::NimResult<proxy_core::ChatCompletionChunk>>
        + Send
        + Unpin
        + 'static,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        let mut builder = SseBuilder::new(model, input_tokens);
        yield Ok(Event::default().event("message_start").data(strip_sse(builder.message_start())));
        let mut last_finish_reason: Option<String> = None;
        let mut last_usage = None;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    if let Some(usage) = chunk.usage {
                        last_usage = Some(usage);
                    }
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(reason) = &choice.finish_reason {
                            last_finish_reason = Some(reason.clone());
                        }
                    }
                    for frame in builder.apply_chunk(chunk) {
                        if let Some((event, data)) = parse_anthropic_frame(&frame) {
                            yield Ok(Event::default().event(event).data(data));
                        }
                    }
                }
                Err(err) => {
                    for frame in builder.error(&format!("Upstream NVIDIA NIM error: {err}")) {
                        if let Some((event, data)) = parse_anthropic_frame(&frame) {
                            yield Ok(Event::default().event(event).data(data));
                        }
                    }
                    break;
                }
            }
        }
        for frame in builder.finish(last_finish_reason.as_deref(), last_usage) {
            if let Some((event, data)) = parse_anthropic_frame(&frame) {
                yield Ok(Event::default().event(event).data(data));
            }
        }
    }
}

// `Response` is large, but boxing it would punish the hot success path
// (`Ok(())`) for the benefit of the rare 401 path. Keep it inline.
#[allow(clippy::result_large_err)]
fn require_auth(state: &ProxyState, headers: &HeaderMap) -> Result<(), Response> {
    if state.config.auth_token.trim().is_empty() {
        return Ok(());
    }
    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("anthropic-auth-token"))
        .or_else(|| headers.get("authorization"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let provided = provided.strip_prefix("Bearer ").unwrap_or(provided);
    if provided == state.config.auth_token {
        Ok(())
    } else {
        Err(api_error(StatusCode::UNAUTHORIZED, "invalid API key"))
    }
}

fn api_error(status: StatusCode, detail: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": { "message": detail.into() } })),
    )
        .into_response()
}

fn parse_anthropic_frame(frame: &str) -> Option<(String, String)> {
    let mut event = None;
    let mut data = None;
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        }
    }
    Some((event?, data?))
}

fn strip_sse(frame: String) -> String {
    parse_anthropic_frame(&frame)
        .map(|(_, data)| data)
        .unwrap_or(frame)
}

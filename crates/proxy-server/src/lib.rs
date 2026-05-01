use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use app_config::{AppConfig, NimApiKey};
use async_stream::stream;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use nim_client::{
    AnthropicPassthroughClient, KeyPool, KeyPoolEntry, KeySnapshot, NimClient, NimClientError,
};
use proxy_core::{
    anthropic_to_nim, count_input_tokens, MessagesRequest, ModelsListResponse,
    NimRequestOptions, ProviderKind, SseBuilder, TokenCountRequest, TokenCountResponse,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<AppConfig>,
    pub nim: NimClient,
    pub anthropic: AnthropicPassthroughClient,
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
/// don't each repeat the field-by-field copy. The base URL is resolved
/// to a normalised, trailing-slash-free form so the upstream clients can
/// concatenate paths without bookkeeping.
pub fn key_pool_entries(keys: &[NimApiKey]) -> Vec<KeyPoolEntry> {
    keys.iter()
        .map(|k| KeyPoolEntry {
            value: k.value.clone(),
            label: k.label.clone(),
            expires_at: k.expires_at,
            provider: k.provider,
            base_url: k.effective_base_url(),
            enabled: k.enabled,
        })
        .collect()
}

/// Get the model mapping for a specific key by its index.
/// Returns None if the key doesn't have a custom model mapping configured.
fn get_key_model_mapping(state: &ProxyState, key_id: usize) -> Option<app_config::ModelMappingConfig> {
    state.config.nim_api_keys.get(key_id).and_then(|k| k.model_mapping.clone())
}

pub async fn start_server(config: AppConfig) -> anyhow::Result<RunningServer> {
    let addr: SocketAddr = config.listen_addr().parse()?;
    let key_pool = KeyPool::new(
        key_pool_entries(&config.nim_api_keys),
        config.rate_limit_per_key,
        Duration::from_secs(config.rate_window_secs),
    );
    let nim = NimClient::new(key_pool.clone())?;
    let anthropic = AnthropicPassthroughClient::new()?;
    let state = ProxyState {
        config: Arc::new(config),
        nim,
        anthropic,
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
        // Historical name. New callers should use `/v1/upstream/models?provider=…`,
        // but the GUI (and any older smoke tests) still hit this path expecting
        // a NIM-shaped catalog, so we keep it as an alias that hard-codes the
        // NIM provider.
        .route("/v1/nim/models", get(nim_models))
        .route("/v1/upstream/models", get(upstream_models))
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
        Ok(()) => match state.nim.list_models(ProviderKind::Nim).await {
            Ok(models) => Json(models).into_response(),
            Err(err) => api_error(StatusCode::BAD_GATEWAY, err.to_string()),
        },
        Err(response) => response,
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamModelsQuery {
    /// Provider family whose model catalog to fetch. Defaults to NIM
    /// when omitted so existing GUI calls keep working unchanged.
    #[serde(default)]
    provider: Option<ProviderKind>,
}

/// Fetch the upstream model catalog for the requested provider. Only
/// meaningful for OpenAI-compatible upstreams (`nim` / `openai_compat`)
/// — Anthropic-compat hosts have no `/v1/models` of their own and
/// requests with `provider=anthropic_compat` return a 400.
async fn upstream_models(
    State(state): State<ProxyState>,
    Query(query): Query<UpstreamModelsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_auth(&state, &headers) {
        return response;
    }
    let provider = query.provider.unwrap_or(ProviderKind::Nim);
    if matches!(provider, ProviderKind::AnthropicCompat) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Anthropic-compat upstreams do not expose a /v1/models endpoint",
        );
    }
    match state.nim.list_models(provider).await {
        Ok(models) => Json(models).into_response(),
        Err(err) => api_error(StatusCode::BAD_GATEWAY, err.to_string()),
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

    // Pick a healthy lease across *all* configured providers, then split
    // on the lease's protocol family. This is what makes the dashboard
    // promise of "all keys round-robin together regardless of provider"
    // hold true: the next request is always served by whichever key has
    // the most headroom right now.
    let lease = match state.key_pool.acquire() {
        Some(lease) => lease,
        None => {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no healthy upstream API key is available",
            );
        }
    };

    // Use the key's own model mapping if configured, otherwise fall back to global config
    let mapping: proxy_core::ModelMapping = get_key_model_mapping(&state, lease.key_id())
        .map(|m| m.into())
        .unwrap_or_else(|| state.config.model_mapping.clone().into());
    request.model = mapping.resolve(&request.model);

    match lease.provider() {
        ProviderKind::AnthropicCompat => anthropic_passthrough(state, lease, request).await,
        ProviderKind::Nim | ProviderKind::OpenaiCompat => {
            openai_compat_messages(state, lease, request).await
        }
    }
}

/// OpenAI-compatible upstream: convert anthropic→openai, stream
/// completions, then synthesize anthropic SSE on the way back.
async fn openai_compat_messages(
    state: ProxyState,
    lease: nim_client::KeyLease,
    request: MessagesRequest,
) -> Response {
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
    let stream = match state.nim.stream_chat_with_lease(lease, body).await {
        Ok(stream) => stream,
        Err(err) => return upstream_error_response(err),
    };
    let sse = stream_anthropic_sse(request.model, input_tokens, stream);
    Sse::new(sse)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

/// Anthropic-compat upstream: forward the request body verbatim and
/// splice the upstream SSE bytes back to the client without any
/// re-encoding. We *do* rewrite `model` (already done by the caller)
/// so the upstream's catalog determines what actually runs, but
/// everything else — `tools`, `thinking`, `metadata`, `extra_body` —
/// flows through untouched.
async fn anthropic_passthrough(
    state: ProxyState,
    lease: nim_client::KeyLease,
    request: MessagesRequest,
) -> Response {
    // Force `stream: true` so the upstream sends SSE; the rest of the
    // local proxy assumes streaming responses end-to-end. Most clients
    // already set this, but Claude Code occasionally omits it.
    let body_value = match serde_json::to_value(&request) {
        Ok(serde_json::Value::Object(mut obj)) => {
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
            serde_json::Value::Object(obj)
        }
        Ok(other) => other,
        Err(err) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed re-encoding request: {err}"),
            );
        }
    };

    let upstream = match state
        .anthropic
        .stream_messages_with_lease(lease, body_value)
        .await
    {
        Ok(stream) => stream,
        Err(err) => return upstream_error_response(err),
    };

    // Splice the upstream byte stream straight into the client's
    // response body. We don't use axum's `Sse` here because the
    // upstream already produced valid SSE frames — we just need to
    // forward them with the right Content-Type so intermediaries
    // (proxies, browsers) treat the response as a live event stream
    // rather than a buffered download.
    //
    // The `Result<Bytes, std::io::Error>` shape is what `Body::from_stream`
    // expects. We translate `NimClientError::Stream` into a synthetic
    // SSE `event: error` frame so the Anthropic client can surface a
    // useful message to the user instead of silently truncating.
    let mapped = stream! {
        futures_util::pin_mut!(upstream);
        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => yield Ok::<Bytes, std::io::Error>(bytes),
                Err(err) => {
                    let frame = format!(
                        "event: error\ndata: {{\"type\":\"error\",\"message\":\"{}\"}}\n\n",
                        err.to_string().replace('"', "\\\"")
                    );
                    yield Ok(Bytes::from(frame));
                    break;
                }
            }
        }
    };

    use axum::body::Body;
    use axum::http::header;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(mapped))
        .unwrap_or_else(|err| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed building passthrough response: {err}"),
            )
        })
}

fn upstream_error_response(err: NimClientError) -> Response {
    let status = match err {
        NimClientError::NoHealthyKey => StatusCode::SERVICE_UNAVAILABLE,
        NimClientError::Authentication => StatusCode::UNAUTHORIZED,
        NimClientError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        NimClientError::Request(_) | NimClientError::Stream(_) => StatusCode::BAD_GATEWAY,
    };
    api_error(status, err.to_string())
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

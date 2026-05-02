mod metrics;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    anthropic_to_nim, count_input_tokens, MessagesRequest, ModelsListResponse, NimRequestOptions,
    ProviderKind, SseBuilder, TokenCountRequest, TokenCountResponse,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

pub use metrics::{KeyMetrics, MetricsSnapshot, ModelMetrics};
use metrics::{MetricsRegistry, UsageSniffer};

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<AppConfig>,
    pub nim: NimClient,
    pub anthropic: AnthropicPassthroughClient,
    pub key_pool: KeyPool,
    pub metrics: MetricsRegistry,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub listen_url: String,
    pub default_model: String,
    pub keys: Vec<KeySnapshot>,
    /// Live request statistics. `Some` only when the proxy is running;
    /// `None` when stopped (the in-memory registry was destroyed with
    /// the previous `RunningServer`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsSnapshot>,
}

pub struct RunningServer {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    addr: SocketAddr,
    key_pool: KeyPool,
    metrics: MetricsRegistry,
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

    /// Snapshot live request metrics for the dashboard. Counters are
    /// kept in-memory and reset whenever this server is dropped.
    pub fn metrics_snapshot(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
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
/// Convert the structured per-key configuration carried in
/// [`AppConfig`] into the runtime-only [`KeyPoolEntry`] shape understood
/// by [`KeyPool`].
///
/// `default_nim_limit` is the global "every NIM key gets this many
/// requests per window" knob. It is *only* applied to keys whose
/// provider is [`ProviderKind::Nim`] and which haven't set their own
/// override; OpenAI- / Anthropic-compatible keys default to no local
/// rate limit because their quotas don't share NIM's neat 40 RPM cap.
pub fn key_pool_entries(keys: &[NimApiKey], default_nim_limit: usize) -> Vec<KeyPoolEntry> {
    keys.iter()
        .map(|k| KeyPoolEntry {
            stable_id: k.id.clone(),
            value: k.value.clone(),
            label: k.label.clone(),
            expires_at: k.expires_at,
            provider: k.provider,
            base_url: k.effective_base_url(),
            enabled: k.enabled,
            rate_limit: k.effective_rate_limit(default_nim_limit),
        })
        .collect()
}

/// Resolve the effective model mapping for a leased key. The per-key
/// override merges *field by field* with the global mapping: any field
/// the user left blank on the key card inherits its value from the
/// dashboard-level mapping, so partial overrides ("only sonnet differs
/// for this upstream") work without forcing the user to copy the rest.
fn effective_mapping_for_key(state: &ProxyState, key_id: usize) -> proxy_core::ModelMapping {
    let global = &state.config.model_mapping;
    let pk = state
        .config
        .nim_api_keys
        .get(key_id)
        .and_then(|k| k.model_mapping.as_ref());
    let trimmed = |s: Option<&String>| {
        s.map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    proxy_core::ModelMapping {
        default_model: trimmed(pk.and_then(|p| p.default_model.as_ref()))
            .unwrap_or_else(|| global.default_model.clone()),
        opus_model: trimmed(pk.and_then(|p| p.opus_model.as_ref()))
            .or_else(|| global.opus_model.clone()),
        sonnet_model: trimmed(pk.and_then(|p| p.sonnet_model.as_ref()))
            .or_else(|| global.sonnet_model.clone()),
        haiku_model: trimmed(pk.and_then(|p| p.haiku_model.as_ref()))
            .or_else(|| global.haiku_model.clone()),
    }
}

pub async fn start_server(config: AppConfig) -> anyhow::Result<RunningServer> {
    let addr: SocketAddr = config.listen_addr().parse()?;
    let key_pool = KeyPool::new(
        key_pool_entries(&config.nim_api_keys, config.rate_limit_per_key),
        Duration::from_secs(config.rate_window_secs),
    );
    let nim = NimClient::new(key_pool.clone())?;
    let anthropic = AnthropicPassthroughClient::new()?;
    let metrics = MetricsRegistry::new();
    let state = ProxyState {
        config: Arc::new(config),
        nim,
        anthropic,
        key_pool: key_pool.clone(),
        metrics: metrics.clone(),
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
        metrics,
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
        metrics: Some(state.metrics.snapshot()),
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

    let mapping = effective_mapping_for_key(&state, lease.key_id());
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

    let key_id = lease.stable_id().to_string();
    let model = request.model.clone();
    let started = Instant::now();

    let raw = match state.nim.stream_chat_with_lease(lease, body).await {
        Ok(stream) => stream,
        Err(err) => {
            state
                .metrics
                .record_failure(&key_id, &model, started.elapsed());
            return upstream_error_response(err);
        }
    };

    // Tap each chunk to capture the latest `usage` payload before
    // forwarding it downstream. We record the success exactly once, when
    // the upstream stream terminates, so partial reads still report the
    // most accurate input/output counts the upstream gave us.
    let metrics = state.metrics.clone();
    let key_id_for_stream = key_id.clone();
    let model_for_stream = model.clone();
    let estimated_input = input_tokens as u64;
    let tapped = stream! {
        let mut last_usage: Option<proxy_core::Usage> = None;
        let mut had_error = false;
        futures_util::pin_mut!(raw);
        while let Some(item) = raw.next().await {
            match &item {
                Ok(chunk) => {
                    if let Some(u) = chunk.usage {
                        last_usage = Some(u);
                    }
                }
                Err(_) => had_error = true,
            }
            yield item;
        }
        let elapsed = started.elapsed();
        let (input, output) = match last_usage {
            Some(u) => (
                u.prompt_tokens.map(|n| n as u64).unwrap_or(estimated_input),
                u.completion_tokens.map(|n| n as u64).unwrap_or(0),
            ),
            None => (estimated_input, 0),
        };
        if had_error {
            metrics.record_failure(&key_id_for_stream, &model_for_stream, elapsed);
        } else {
            metrics.record_success(&key_id_for_stream, &model_for_stream, input, output, elapsed);
        }
    };
    let tapped: nim_client::NimChunkStream = Box::pin(tapped);

    let sse = stream_anthropic_sse(request.model, input_tokens, tapped);
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
    let estimated_input = count_input_tokens(
        &request.messages,
        request.system.as_ref(),
        request.tools.as_deref(),
    ) as u64;

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

    let key_id = lease.stable_id().to_string();
    let model = request.model.clone();
    let started = Instant::now();

    let upstream = match state
        .anthropic
        .stream_messages_with_lease(lease, body_value)
        .await
    {
        Ok(stream) => stream,
        Err(err) => {
            state
                .metrics
                .record_failure(&key_id, &model, started.elapsed());
            return upstream_error_response(err);
        }
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
    //
    // Token usage on this path can't be parsed by a structured
    // deserializer without breaking the byte-perfect passthrough
    // contract, so we sniff the SSE bytes for the latest
    // `"input_tokens"` / `"output_tokens"` integers and fall back to
    // the local input estimate when the upstream doesn't emit a usage
    // block at all.
    let metrics = state.metrics.clone();
    let key_id_for_stream = key_id.clone();
    let model_for_stream = model.clone();
    let mapped = stream! {
        let mut sniffer = UsageSniffer::new();
        let mut had_error = false;
        futures_util::pin_mut!(upstream);
        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    sniffer.observe(&bytes);
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    had_error = true;
                    let frame = format!(
                        "event: error\ndata: {{\"type\":\"error\",\"message\":\"{}\"}}\n\n",
                        err.to_string().replace('"', "\\\"")
                    );
                    yield Ok(Bytes::from(frame));
                    break;
                }
            }
        }
        let elapsed = started.elapsed();
        if had_error {
            metrics.record_failure(&key_id_for_stream, &model_for_stream, elapsed);
        } else {
            let input = if sniffer.saw_input { sniffer.input_tokens } else { estimated_input };
            let output = if sniffer.saw_output { sniffer.output_tokens } else { 0 };
            metrics.record_success(&key_id_for_stream, &model_for_stream, input, output, elapsed);
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

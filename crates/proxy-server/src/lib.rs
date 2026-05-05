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
use parking_lot::RwLock;
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

/// Atomically swappable handle to the live proxy configuration. Reads
/// clone the inner `Arc` (cheap, lock held for ~one pointer copy);
/// writes replace the `Arc` wholesale. Wrapped in a separate alias so
/// the storage strategy can change later (e.g. swap to `arc_swap`)
/// without churning every call site.
pub type SharedConfig = Arc<RwLock<Arc<AppConfig>>>;

#[derive(Clone)]
pub struct ProxyState {
    /// Hot-swappable config snapshot. Use [`ProxyState::config`] to get
    /// an `Arc<AppConfig>` that's stable for the duration of the current
    /// request — never deref this field directly, otherwise a concurrent
    /// `RunningServer::update_config` could invalidate the snapshot half
    /// way through a handler.
    config: SharedConfig,
    pub nim: NimClient,
    pub anthropic: AnthropicPassthroughClient,
    pub key_pool: KeyPool,
    pub metrics: MetricsRegistry,
}

impl ProxyState {
    /// Snapshot the current config into an `Arc` the caller can hold
    /// for the duration of a request. Cheap (one `Arc` clone under a
    /// short read lock). Hot-swaps performed via
    /// [`RunningServer::update_config`] are invisible to handlers that
    /// already hold a snapshot — they keep seeing the values that were
    /// live when their request arrived, and only the *next* request
    /// observes the new config.
    pub fn config(&self) -> Arc<AppConfig> {
        Arc::clone(&self.config.read())
    }
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
    /// Shared with the running [`ProxyState`] so the GUI can hot-swap
    /// the config (via [`RunningServer::update_config`]) without tearing
    /// down the listening socket. Holding it on `RunningServer` keeps
    /// the swap API discoverable next to `key_pool()` / `stop()`.
    config: SharedConfig,
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

    /// Hot-swap the config seen by all subsequent requests. Used by the
    /// GUI's `save_config` path so per-key model mappings, the global
    /// `model_mapping`, `auth_token`, and `enable_thinking` toggles
    /// take effect immediately instead of staying frozen at whatever
    /// the file held when [`start_server`] ran.
    ///
    /// `host` / `port` changes still require a manual restart — the
    /// listening socket is owned by the spawned axum task and isn't
    /// rebound by this method. The dashboard surfaces both before and
    /// after values when that mismatch happens.
    pub fn update_config(&self, config: AppConfig) {
        *self.config.write() = Arc::new(config);
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
///
/// `stable_id` (the config-level UUID, *not* the runtime pool index)
/// is what we look the per-key override up by. Using the runtime
/// `key_id` here was the source of a long-standing bug: the live
/// `KeyPool` is rebuilt by `update_keys` on every save, so its indices
/// drift as soon as the user adds / removes / reorders keys, while
/// the in-process `AppConfig` snapshot the proxy reads from is
/// independently swapped via `update_config`. Indexing across those
/// two clocks happily returned the wrong key (or `None`, which
/// silently fell back to the global mapping) — exactly the symptom
/// users reported as "my per-key mapping gets overridden by the
/// global mapping".
fn effective_mapping_for_key(config: &AppConfig, stable_id: &str) -> proxy_core::ModelMapping {
    let global = &config.model_mapping;
    let pk = config
        .nim_api_keys
        .iter()
        .find(|k| k.id == stable_id)
        .and_then(|k| k.model_mapping.as_ref());
    let trimmed = |s: Option<&String>| {
        s.map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // Trim the global side too — any string that survives serde but
    // collapses under `trim()` (a manual `"   "` edit, a stray space
    // pasted in, …) should be treated as "not set" so the runtime
    // falls all the way through to `ModelMapping::resolve`'s
    // passthrough safety net instead of forwarding whitespace
    // upstream.
    let global_trimmed = |s: &String| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    // Per-slot `extra_body` lives only on the per-key side for now
    // (the global mapping page has no UI for it). Hand-edited values
    // that aren't a non-empty JSON object are treated as "no
    // overrides" — that mirrors the runtime guard in
    // [`proxy_core::ModelMapping::resolve`] so a user who put `42`
    // or `{}` in `secrets.json` doesn't get a confusing 400 from the
    // upstream.
    let extra = |slot: Option<&serde_json::Value>| match slot {
        Some(serde_json::Value::Object(map)) if !map.is_empty() => Some(slot.unwrap().clone()),
        _ => None,
    };
    proxy_core::ModelMapping {
        default_model: trimmed(pk.and_then(|p| p.default_model.as_ref()))
            .or_else(|| global_trimmed(&global.default_model))
            .unwrap_or_default(),
        default_extra_body: extra(pk.and_then(|p| p.default_extra_body.as_ref())),
        opus_model: trimmed(pk.and_then(|p| p.opus_model.as_ref()))
            .or_else(|| global.opus_model.as_ref().and_then(global_trimmed)),
        opus_extra_body: extra(pk.and_then(|p| p.opus_extra_body.as_ref())),
        sonnet_model: trimmed(pk.and_then(|p| p.sonnet_model.as_ref()))
            .or_else(|| global.sonnet_model.as_ref().and_then(global_trimmed)),
        sonnet_extra_body: extra(pk.and_then(|p| p.sonnet_extra_body.as_ref())),
        haiku_model: trimmed(pk.and_then(|p| p.haiku_model.as_ref()))
            .or_else(|| global.haiku_model.as_ref().and_then(global_trimmed)),
        haiku_extra_body: extra(pk.and_then(|p| p.haiku_extra_body.as_ref())),
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
    let shared_config: SharedConfig = Arc::new(RwLock::new(Arc::new(config)));
    let state = ProxyState {
        config: Arc::clone(&shared_config),
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
        config: shared_config,
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
    let config = state.config();
    match require_auth(&config, &headers) {
        Ok(()) => Json(serde_json::json!({
            "status": "ok",
            "provider": "nvidia_nim",
            "model": config.model_mapping.default_model
        }))
        .into_response(),
        Err(response) => response,
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "healthy" }))
}

async fn status(State(state): State<ProxyState>) -> impl IntoResponse {
    let config = state.config();
    Json(ProxyStatus {
        running: true,
        listen_url: format!("http://{}:{}", config.host, config.port),
        default_model: config.model_mapping.default_model.clone(),
        keys: state.key_pool.snapshots(),
        metrics: Some(state.metrics.snapshot()),
    })
}

async fn models(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    match require_auth(&state.config(), &headers) {
        Ok(()) => Json(ModelsListResponse::claude_compatible()).into_response(),
        Err(response) => response,
    }
}

async fn nim_models(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    match require_auth(&state.config(), &headers) {
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
    if let Err(response) = require_auth(&state.config(), &headers) {
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
    match require_auth(&state.config(), &headers) {
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
    let config = state.config();
    if let Err(response) = require_auth(&config, &headers) {
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

    // Resolve the per-key mapping by `stable_id` against the same
    // config snapshot we authenticated against, so a hot-swap mid
    // request can never split-brain "which key is this lease".
    let mapping = effective_mapping_for_key(&config, lease.stable_id());
    let resolution = mapping.resolve(&request.model);
    request.model = resolution.model;
    // Clone the slot's extra_body out of the mapping so we can move
    // it across the await boundary. `Resolution::extra_body` is a
    // borrow into `mapping`, but `mapping` is local to this scope —
    // owning a `Value` here keeps the upstream calls free to await
    // without holding the mapping alive.
    let extra_body = resolution.extra_body.cloned();

    match lease.provider() {
        ProviderKind::AnthropicCompat => {
            anthropic_passthrough(state, config, lease, request, extra_body).await
        }
        ProviderKind::Nim | ProviderKind::OpenaiCompat => {
            openai_compat_messages(state, config, lease, request, extra_body).await
        }
    }
}

/// OpenAI-compatible upstream: convert anthropic→openai, stream
/// completions, then synthesize anthropic SSE on the way back.
async fn openai_compat_messages(
    state: ProxyState,
    config: Arc<AppConfig>,
    lease: nim_client::KeyLease,
    request: MessagesRequest,
    extra_body: Option<serde_json::Value>,
) -> Response {
    let input_tokens = count_input_tokens(
        &request.messages,
        request.system.as_ref(),
        request.tools.as_deref(),
    );
    let body = anthropic_to_nim(
        &request,
        &NimRequestOptions {
            enable_thinking: config.enable_thinking,
            ..NimRequestOptions::default()
        },
    );

    let key_id = lease.stable_id().to_string();
    let model = request.model.clone();
    let started = Instant::now();

    let raw = match state
        .nim
        .stream_chat_with_lease(lease, body, extra_body)
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
    _config: Arc<AppConfig>,
    lease: nim_client::KeyLease,
    request: MessagesRequest,
    extra_body: Option<serde_json::Value>,
) -> Response {
    let estimated_input = count_input_tokens(
        &request.messages,
        request.system.as_ref(),
        request.tools.as_deref(),
    ) as u64;

    // Force `stream: true` so the upstream sends SSE; the rest of the
    // local proxy assumes streaming responses end-to-end. Most clients
    // already set this, but Claude Code occasionally omits it.
    let mut body_value = match serde_json::to_value(&request) {
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
    // Apply the per-mapping-slot extra_body to the outgoing JSON,
    // with config values winning. Done here (rather than inside the
    // passthrough client) so the merge contract is identical to the
    // OpenAI-compat path: same `deep_merge_json` semantics, same
    // "config wins" priority.
    if let Some(extra) = extra_body.as_ref() {
        proxy_core::deep_merge_json(&mut body_value, extra);
    }

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
fn require_auth(config: &AppConfig, headers: &HeaderMap) -> Result<(), Response> {
    if config.auth_token.trim().is_empty() {
        return Ok(());
    }
    let provided = headers
        .get("x-api-key")
        .or_else(|| headers.get("anthropic-auth-token"))
        .or_else(|| headers.get("authorization"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let provided = provided.strip_prefix("Bearer ").unwrap_or(provided);
    if provided == config.auth_token {
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

#[cfg(test)]
mod tests {
    use super::*;
    use app_config::{ModelMappingConfig, NimApiKey, PerKeyModelMappingConfig};

    fn cfg_with_keys(keys: Vec<NimApiKey>, global: ModelMappingConfig) -> AppConfig {
        AppConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            auth_token: String::new(),
            nim_api_keys: keys,
            model_mapping: global,
            rate_limit_per_key: 40,
            rate_window_secs: 60,
            enable_thinking: true,
        }
    }

    fn key_with_mapping(
        id: &str,
        value: &str,
        mapping: Option<PerKeyModelMappingConfig>,
    ) -> NimApiKey {
        NimApiKey {
            id: id.to_string(),
            value: value.to_string(),
            label: None,
            expires_at: None,
            provider: ProviderKind::Nim,
            base_url: String::new(),
            enabled: true,
            model_mapping: mapping,
            rate_limit: None,
        }
    }

    /// Per-key fields take precedence over the global mapping when set,
    /// and missing fields inherit from the global. The runtime `key_id`
    /// (pool index) must NOT be used — we look up by the stable
    /// config-level identifier instead so add/remove/reorder edits
    /// don't smear mappings across keys.
    #[test]
    fn per_key_mapping_overrides_global_field_by_field() {
        let global = ModelMappingConfig {
            default_model: "global-default".into(),
            opus_model: Some("global-opus".into()),
            sonnet_model: Some("global-sonnet".into()),
            haiku_model: Some("global-haiku".into()),
        };
        let keys = vec![
            key_with_mapping(
                "key-A",
                "nvapi-A",
                Some(PerKeyModelMappingConfig {
                    default_model: Some("A-default".into()),
                    opus_model: Some("A-opus".into()),
                    sonnet_model: None,
                    haiku_model: Some("   ".into()), // blanks inherit too
                    ..Default::default()
                }),
            ),
            key_with_mapping("key-B", "nvapi-B", None),
        ];
        let config = cfg_with_keys(keys, global);

        let mapping_a = effective_mapping_for_key(&config, "key-A");
        assert_eq!(mapping_a.default_model, "A-default");
        assert_eq!(mapping_a.opus_model.as_deref(), Some("A-opus"));
        assert_eq!(mapping_a.sonnet_model.as_deref(), Some("global-sonnet"));
        assert_eq!(mapping_a.haiku_model.as_deref(), Some("global-haiku"));

        let mapping_b = effective_mapping_for_key(&config, "key-B");
        assert_eq!(mapping_b.default_model, "global-default");
        assert_eq!(mapping_b.opus_model.as_deref(), Some("global-opus"));
        assert_eq!(mapping_b.sonnet_model.as_deref(), Some("global-sonnet"));
        assert_eq!(mapping_b.haiku_model.as_deref(), Some("global-haiku"));
    }

    /// Regression for the historical "global default silently overrides
    /// the per-key mapping" bug: looking up a key whose config slot
    /// shifted (because `update_keys` ran with a different ordering)
    /// must still find the right per-key mapping by stable id, not by
    /// pool index.
    #[test]
    fn lookup_by_stable_id_survives_reorder() {
        let global = ModelMappingConfig {
            default_model: "G".into(),
            opus_model: None,
            sonnet_model: None,
            haiku_model: None,
        };
        let key = key_with_mapping(
            "stable-X",
            "nvapi-X",
            Some(PerKeyModelMappingConfig {
                default_model: Some("X-only".into()),
                ..Default::default()
            }),
        );
        // Insert a couple of unrelated keys before / after so the
        // "stable-X" key sits at index 1 in one config and index 0 in
        // another; the lookup must produce the same mapping in both.
        let cfg_index_1 = cfg_with_keys(
            vec![
                key_with_mapping("stable-Y", "nvapi-Y", None),
                key.clone(),
                key_with_mapping("stable-Z", "nvapi-Z", None),
            ],
            global.clone(),
        );
        let cfg_index_0 = cfg_with_keys(
            vec![key.clone(), key_with_mapping("stable-Y", "nvapi-Y", None)],
            global,
        );

        assert_eq!(
            effective_mapping_for_key(&cfg_index_1, "stable-X").default_model,
            "X-only",
        );
        assert_eq!(
            effective_mapping_for_key(&cfg_index_0, "stable-X").default_model,
            "X-only",
        );
    }

    /// An empty `model_mapping` object on the per-key record (which the
    /// GUI used to round-trip when the user typed and then deleted a
    /// value) must still inherit from the global mapping.
    #[test]
    fn empty_per_key_mapping_falls_back_entirely() {
        let global = ModelMappingConfig {
            default_model: "G".into(),
            opus_model: Some("G-opus".into()),
            sonnet_model: None,
            haiku_model: None,
        };
        let key = key_with_mapping(
            "stable-X",
            "nvapi-X",
            Some(PerKeyModelMappingConfig::default()),
        );
        let config = cfg_with_keys(vec![key], global);
        let mapping = effective_mapping_for_key(&config, "stable-X");
        assert_eq!(mapping.default_model, "G");
        assert_eq!(mapping.opus_model.as_deref(), Some("G-opus"));
    }

    /// A stable_id that no longer exists in the config (lease leaked
    /// across an `update_keys` that removed the key) must degrade to
    /// the global mapping rather than panic — a missing per-key entry
    /// is exactly the "no override" case.
    #[test]
    fn missing_stable_id_uses_global_mapping() {
        let global = ModelMappingConfig {
            default_model: "G".into(),
            opus_model: Some("G-opus".into()),
            sonnet_model: None,
            haiku_model: None,
        };
        let config = cfg_with_keys(Vec::new(), global);
        let mapping = effective_mapping_for_key(&config, "stale");
        assert_eq!(mapping.default_model, "G");
        assert_eq!(mapping.opus_model.as_deref(), Some("G-opus"));
    }

    /// A whitespace-only global `default_model` (a hand-edited
    /// `config.json` that bypassed `validate_for_save`) must not be
    /// forwarded as a literal `"   "` upstream. The merge collapses
    /// it to empty, and `ModelMapping::resolve` then falls back to
    /// the original Claude model name.
    #[test]
    fn whitespace_global_default_collapses_to_empty_for_passthrough() {
        let global = ModelMappingConfig {
            default_model: "   ".into(),
            opus_model: Some("   ".into()),
            sonnet_model: None,
            haiku_model: None,
        };
        let config = cfg_with_keys(vec![key_with_mapping("k", "nvapi-k", None)], global);
        let mapping = effective_mapping_for_key(&config, "k");
        assert_eq!(mapping.default_model, "");
        assert_eq!(mapping.opus_model, None);
        // Round-trip through the resolver: empty default ⇒ passthrough.
        assert_eq!(mapping.resolve("claude-opus-4").model, "claude-opus-4");
        assert_eq!(mapping.resolve("claude-sonnet-4").model, "claude-sonnet-4");
    }

    /// `extra_body` for each slot must propagate through
    /// `effective_mapping_for_key` from the per-key config to the
    /// runtime mapping, then through `ModelMapping::resolve` so the
    /// upstream merge picks up the right object for the requested
    /// model family.
    #[test]
    fn per_key_extra_body_propagates_through_resolution() {
        use serde_json::json;
        let global = ModelMappingConfig {
            default_model: "G".into(),
            opus_model: None,
            sonnet_model: None,
            haiku_model: None,
        };
        let key = key_with_mapping(
            "stable-X",
            "nvapi-X",
            Some(PerKeyModelMappingConfig {
                default_model: Some("X-default".into()),
                default_extra_body: Some(json!({ "temperature": 0.1 })),
                opus_model: Some("X-opus".into()),
                opus_extra_body: Some(json!({ "temperature": 0.9 })),
                // Sonnet has an extra_body but no model — the slot
                // doesn't win resolution, so its extras must NOT
                // leak into a default-routed request.
                sonnet_extra_body: Some(json!({ "ignored": true })),
                ..Default::default()
            }),
        );
        let config = cfg_with_keys(vec![key], global);
        let mapping = effective_mapping_for_key(&config, "stable-X");

        let opus = mapping.resolve("claude-opus-4");
        assert_eq!(opus.model, "X-opus");
        assert_eq!(opus.extra_body, Some(&json!({ "temperature": 0.9 })));

        let sonnet = mapping.resolve("claude-sonnet-4");
        assert_eq!(sonnet.model, "X-default");
        assert_eq!(sonnet.extra_body, Some(&json!({ "temperature": 0.1 })));
    }

    /// Hand-edited `secrets.json` that puts a non-object value into
    /// an `extra_body` slot must NOT propagate to the runtime — the
    /// merge would otherwise emit malformed JSON to the upstream.
    /// `effective_mapping_for_key` collapses it to `None` so
    /// resolution behaves as if the slot had no extras at all.
    #[test]
    fn non_object_extra_body_is_dropped_at_merge_time() {
        use serde_json::json;
        let global = ModelMappingConfig {
            default_model: "G".into(),
            opus_model: None,
            sonnet_model: None,
            haiku_model: None,
        };
        let key = key_with_mapping(
            "stable-X",
            "nvapi-X",
            Some(PerKeyModelMappingConfig {
                default_extra_body: Some(json!(42)),
                opus_extra_body: Some(json!("hello")),
                sonnet_extra_body: Some(json!({})), // empty object also drops
                haiku_extra_body: Some(json!([1, 2])),
                ..Default::default()
            }),
        );
        let config = cfg_with_keys(vec![key], global);
        let mapping = effective_mapping_for_key(&config, "stable-X");
        assert_eq!(mapping.default_extra_body, None);
        assert_eq!(mapping.opus_extra_body, None);
        assert_eq!(mapping.sonnet_extra_body, None);
        assert_eq!(mapping.haiku_extra_body, None);
    }
}

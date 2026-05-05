use std::pin::Pin;
use std::time::Duration;

use async_stream::try_stream;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use proxy_core::{ChatCompletionChunk, ChatCompletionRequest, NimModelList, ProviderKind};
use reqwest::{header, Client, StatusCode};
use thiserror::Error;

use crate::{KeyLease, KeyPool};

/// Canonical NVIDIA NIM endpoint. Kept as an exported constant for
/// callers (the Tauri layer, smoke tests) that still want to reference
/// the default explicitly. Per-key base URLs are stored in
/// [`crate::KeyPoolEntry::base_url`] and propagate through the
/// [`KeyLease`] returned by `acquire`.
pub const NIM_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";

#[derive(Debug, Error)]
pub enum NimClientError {
    #[error("no healthy upstream API key is available")]
    NoHealthyKey,
    #[error("upstream authentication failed")]
    Authentication,
    #[error("upstream rate limited all available keys")]
    RateLimited,
    #[error("upstream request failed: {0}")]
    Request(String),
    #[error("upstream stream parse failed: {0}")]
    Stream(String),
}

pub type NimResult<T> = Result<T, NimClientError>;
pub type NimChunkStream =
    Pin<Box<dyn Stream<Item = NimResult<ChatCompletionChunk>> + Send + 'static>>;

/// HTTP client that talks to OpenAI-compatible upstreams (NIM and any
/// "OpenAI-compat" provider — DeepSeek, Moonshot, Groq, OpenRouter, …).
/// The base URL is resolved per request from the key lease, not stored
/// on the client itself, so a single instance can multiplex over keys
/// pointing at completely different hosts.
///
/// The client deliberately does *not* own a [`KeyPool`]: callers
/// acquire a lease themselves so that the choice of provider /
/// fallback is driven by request context, not by the HTTP layer.
#[derive(Clone)]
pub struct NimClient {
    http: Client,
    key_pool: KeyPool,
}

impl NimClient {
    pub fn new(key_pool: KeyPool) -> NimResult<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| NimClientError::Request(e.to_string()))?;
        Ok(Self { http, key_pool })
    }

    /// List the model catalog of the upstream addressed by some
    /// healthy key with provider `provider`. Used by the Tauri layer
    /// to populate the model dropdown — only meaningful for OpenAI-
    /// compatible providers (NIM and OpenaiCompat). For Anthropic the
    /// proxy doesn't expose a `/v1/models` of its own; callers should
    /// avoid calling this with `AnthropicCompat`.
    pub async fn list_models(&self, provider: ProviderKind) -> NimResult<NimModelList> {
        let lease = self.key_pool.acquire_for(provider);
        let base = match &lease {
            Some(l) => l.base_url().to_string(),
            None => provider.default_base_url().to_string(),
        };
        let mut request = self
            .http
            .get(format!("{}/models", base.trim_end_matches('/')));
        if let Some(lease) = lease.as_ref() {
            request = request.bearer_auth(lease.key());
        }
        let response = request.send().await.map_err(|e| {
            if let Some(lease) = lease.as_ref() {
                lease.pool().mark_network_error(lease.key_id());
            }
            NimClientError::Request(e.to_string())
        })?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            if let Some(lease) = lease.as_ref() {
                lease.pool().mark_auth_failed(lease.key_id());
            }
        } else if status != StatusCode::OK {
            return Err(NimClientError::Request(format!("HTTP {status}")));
        }
        let models = response
            .json::<NimModelList>()
            .await
            .map_err(|e| NimClientError::Request(e.to_string()))?;
        if let Some(lease) = lease {
            lease.pool().mark_success(lease.key_id());
        }
        Ok(models)
    }

    /// Borrow access to the underlying key pool. The proxy server uses
    /// this to acquire leases of its choosing before calling
    /// [`Self::stream_chat_with_lease`].
    pub fn key_pool(&self) -> &KeyPool {
        &self.key_pool
    }

    /// Fetch the model catalog of an upstream addressed by the supplied
    /// `base_url` + `key`, bypassing the key pool entirely. Used by the
    /// GUI to populate the autocomplete dropdown of the *currently
    /// edited* credential — we cannot route through the pool because
    /// the user might be editing a key whose live entry hasn't been
    /// rebuilt yet (or is `Disabled` / `Expired`).
    ///
    /// `provider` is consulted only to validate that the upstream
    /// actually exposes `/models`; Anthropic-compatible hosts return
    /// an explicit error so callers can show a friendlier message
    /// instead of a `404`.
    pub async fn list_models_direct(
        &self,
        provider: ProviderKind,
        base_url: &str,
        key: &str,
    ) -> NimResult<NimModelList> {
        if matches!(provider, ProviderKind::AnthropicCompat) {
            return Err(NimClientError::Request(
                "Anthropic-compatible upstreams do not expose /models".to_string(),
            ));
        }
        let trimmed = base_url.trim().trim_end_matches('/');
        let base = if trimmed.is_empty() {
            provider.default_base_url().trim_end_matches('/')
        } else {
            trimmed
        };
        if base.is_empty() {
            return Err(NimClientError::Request(
                "missing upstream base URL for this provider".to_string(),
            ));
        }
        let mut request = self.http.get(format!("{base}/models"));
        if !key.trim().is_empty() {
            request = request.bearer_auth(key.trim());
        }
        let response = request
            .send()
            .await
            .map_err(|e| NimClientError::Request(e.to_string()))?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(NimClientError::Request(format!("HTTP {status}")));
        }
        response
            .json::<NimModelList>()
            .await
            .map_err(|e| NimClientError::Request(e.to_string()))
    }

    /// Stream a chat completion via an OpenAI-compatible upstream using
    /// an already-acquired key lease. The caller (typically the proxy
    /// server) is in charge of picking the lease — that lets the server
    /// choose a provider based on the request and avoids re-acquiring
    /// the same key twice.
    ///
    /// `extra_body`, when supplied, is deep-merged into the outgoing
    /// JSON request body *after* serialisation, with config values
    /// winning over any matching keys produced by `body`. This is how
    /// per-mapping-slot overrides (`temperature`, `top_p`,
    /// `chat_template_kwargs.thinking`, …) actually reach the upstream:
    /// pre-merging at the typed level would either lose unknown keys
    /// or risk duplicate top-level fields when `ChatCompletionRequest.extra`
    /// (which is `serde(flatten)`) collides with one of the typed
    /// fields. Doing it on the serialized JSON sidesteps both.
    ///
    /// The lease's provider must be NIM or OpenaiCompat; passing an
    /// Anthropic-compat lease here is a programming error caught by a
    /// debug assertion (and, in release builds, a generic `Request`
    /// error from the wrong endpoint shape).
    pub async fn stream_chat_with_lease(
        &self,
        lease: KeyLease,
        body: ChatCompletionRequest,
        extra_body: Option<serde_json::Value>,
    ) -> NimResult<NimChunkStream> {
        debug_assert!(
            !matches!(lease.provider(), ProviderKind::AnthropicCompat),
            "Anthropic-compat upstream must use AnthropicPassthroughClient"
        );
        let mut json_body = serde_json::to_value(&body).map_err(|err| {
            NimClientError::Request(format!("failed serializing chat body: {err}"))
        })?;
        if let Some(extra) = extra_body.as_ref() {
            proxy_core::deep_merge_json(&mut json_body, extra);
        }
        let base = lease.base_url().trim_end_matches('/').to_string();
        let response = self
            .http
            .post(format!("{base}/chat/completions"))
            .bearer_auth(lease.key())
            .json(&json_body)
            .send()
            .await
            .map_err(|e| {
                lease.pool().mark_network_error(lease.key_id());
                NimClientError::Request(e.to_string())
            })?;

        let retry_after = parse_retry_after(response.headers().get(header::RETRY_AFTER));
        handle_status_with_retry(&lease, response.status(), retry_after)?;
        lease.pool().mark_success(lease.key_id());

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let _lease = lease;
            let mut pending = String::new();
            futures_util::pin_mut!(byte_stream);
            while let Some(item) = byte_stream.next().await {
                let bytes: Bytes = item.map_err(|e| NimClientError::Stream(e.to_string()))?;
                pending.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = pending.find("\n\n") {
                    let frame = pending[..pos].to_string();
                    pending = pending[pos + 2..].to_string();
                    for chunk in parse_sse_frame(&frame)? {
                        yield chunk;
                    }
                }
            }
            if !pending.trim().is_empty() {
                for chunk in parse_sse_frame(&pending)? {
                    yield chunk;
                }
            }
        };
        Ok(Box::pin(stream))
    }
}

/// HTTP client that forwards Anthropic-shaped requests verbatim to an
/// Anthropic-compatible upstream. Skips the entire anthropic→openai→
/// anthropic conversion that [`NimClient`] performs, so native features
/// (`thinking` blocks, `tool_use` schema, custom `metadata`) survive the
/// round trip.
///
/// Like [`NimClient`], it does not own the [`KeyPool`] — leases are
/// acquired by the caller and passed in.
#[derive(Clone)]
pub struct AnthropicPassthroughClient {
    http: Client,
}

impl AnthropicPassthroughClient {
    pub fn new() -> NimResult<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| NimClientError::Request(e.to_string()))?;
        Ok(Self { http })
    }

    /// Forward an Anthropic Messages request to the upstream and return
    /// the raw byte stream of its SSE response. The proxy server splices
    /// these bytes straight into the client connection — we do not
    /// attempt to parse, validate, or rewrite events along the way,
    /// which is the whole point of "pass-through".
    ///
    /// `body_json` is the JSON-serialized [`proxy_core::MessagesRequest`]
    /// (with `model` already rewritten by the caller to whatever the
    /// upstream expects).
    pub async fn stream_messages_with_lease(
        &self,
        lease: KeyLease,
        body_json: serde_json::Value,
    ) -> NimResult<AnthropicByteStream> {
        debug_assert!(
            matches!(lease.provider(), ProviderKind::AnthropicCompat),
            "AnthropicPassthroughClient requires an AnthropicCompat lease"
        );
        let base = lease.base_url().trim_end_matches('/').to_string();

        let response = self
            .http
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", lease.key())
            .header("anthropic-version", "2023-06-01")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .json(&body_json)
            .send()
            .await
            .map_err(|e| {
                lease.pool().mark_network_error(lease.key_id());
                NimClientError::Request(e.to_string())
            })?;

        let retry_after = parse_retry_after(response.headers().get(header::RETRY_AFTER));
        handle_status_with_retry(&lease, response.status(), retry_after)?;
        lease.pool().mark_success(lease.key_id());

        let upstream = response.bytes_stream();
        let stream = try_stream! {
            let _lease = lease;
            futures_util::pin_mut!(upstream);
            while let Some(item) = upstream.next().await {
                let bytes: Bytes = item.map_err(|e| NimClientError::Stream(e.to_string()))?;
                yield bytes;
            }
        };
        Ok(Box::pin(stream))
    }
}

/// Raw SSE byte stream returned by [`AnthropicPassthroughClient`].
pub type AnthropicByteStream = Pin<Box<dyn Stream<Item = NimResult<Bytes>> + Send + 'static>>;

fn handle_status_with_retry(
    lease: &KeyLease,
    status: StatusCode,
    retry_after: Option<Duration>,
) -> NimResult<()> {
    match status {
        StatusCode::OK => Ok(()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            lease.pool().mark_auth_failed(lease.key_id());
            Err(NimClientError::Authentication)
        }
        StatusCode::TOO_MANY_REQUESTS => {
            lease.pool().mark_rate_limited(lease.key_id(), retry_after);
            Err(NimClientError::RateLimited)
        }
        // Upstream 5xx is overwhelmingly *not* a key problem (provider
        // outage, bad gateway, edge timeout). Feed it into the same
        // sliding-window network-error counter we use for transport
        // failures so a single hiccup is harmless but a sustained
        // outage backs the key off without permanently marking it bad.
        other if other.is_server_error() => {
            lease.pool().mark_network_error(lease.key_id());
            Err(NimClientError::Request(format!("HTTP {other}")))
        }
        other => Err(NimClientError::Request(format!("HTTP {other}"))),
    }
}

fn parse_retry_after(value: Option<&header::HeaderValue>) -> Option<Duration> {
    let seconds = value?.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

fn parse_sse_frame(frame: &str) -> NimResult<Vec<ChatCompletionChunk>> {
    let mut chunks = Vec::new();
    for line in frame.lines() {
        let line = line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let data = line.trim_start_matches("data:").trim();
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        chunks.push(
            serde_json::from_str::<ChatCompletionChunk>(data)
                .map_err(|e| NimClientError::Stream(format!("{e}: {data}")))?,
        );
    }
    Ok(chunks)
}

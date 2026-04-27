use std::pin::Pin;
use std::time::Duration;

use async_stream::try_stream;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use proxy_core::{ChatCompletionChunk, ChatCompletionRequest, NimModelList};
use reqwest::{header, Client, StatusCode};
use thiserror::Error;

use crate::{KeyLease, KeyPool};

pub const NIM_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";

#[derive(Debug, Error)]
pub enum NimClientError {
    #[error("no healthy NVIDIA NIM API key is available")]
    NoHealthyKey,
    #[error("NVIDIA NIM authentication failed")]
    Authentication,
    #[error("NVIDIA NIM rate limited all available keys")]
    RateLimited,
    #[error("NVIDIA NIM request failed: {0}")]
    Request(String),
    #[error("NVIDIA NIM stream parse failed: {0}")]
    Stream(String),
}

pub type NimResult<T> = Result<T, NimClientError>;
pub type NimChunkStream =
    Pin<Box<dyn Stream<Item = NimResult<ChatCompletionChunk>> + Send + 'static>>;

#[derive(Clone)]
pub struct NimClient {
    http: Client,
    base_url: String,
    key_pool: KeyPool,
}

impl NimClient {
    pub fn new(key_pool: KeyPool) -> NimResult<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| NimClientError::Request(e.to_string()))?;
        Ok(Self {
            http,
            base_url: NIM_BASE_URL.to_string(),
            key_pool,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub async fn list_models(&self) -> NimResult<NimModelList> {
        let lease = self.key_pool.acquire();
        let mut request = self.http.get(format!("{}/models", self.base_url));
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

    pub async fn stream_chat(&self, body: ChatCompletionRequest) -> NimResult<NimChunkStream> {
        let lease = self
            .key_pool
            .acquire()
            .ok_or(NimClientError::NoHealthyKey)?;
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(lease.key())
            .json(&body)
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

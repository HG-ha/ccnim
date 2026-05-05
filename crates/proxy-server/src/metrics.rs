//! Live request statistics for the running proxy.
//!
//! Counters are aggregated in-memory and reset whenever the proxy is
//! stopped (the `RunningServer`, and therefore the registry it owns,
//! is dropped). That trade-off keeps the implementation cheap and
//! stateless on disk — the dashboard is the only consumer and it
//! polls every couple of seconds.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

#[derive(Clone)]
pub struct MetricsRegistry {
    inner: Arc<Mutex<MetricsInner>>,
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct MetricsInner {
    started_at: i64,
    by_key: HashMap<String, KeyAccumulator>,
    by_model: HashMap<String, ModelAccumulator>,
}

#[derive(Default)]
struct KeyAccumulator {
    requests: u64,
    successes: u64,
    failures: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_latency_ms: u128,
    last_latency_ms: u64,
    last_request_at: i64,
}

#[derive(Default)]
struct ModelAccumulator {
    calls: u64,
    successes: u64,
    failures: u64,
    input_tokens: u64,
    output_tokens: u64,
    last_used_at: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KeyMetrics {
    pub stable_id: String,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub avg_latency_ms: u64,
    pub last_latency_ms: u64,
    /// Unix-secs of the most recent request observed for this key, or
    /// `0` when the key has never been used since the proxy started.
    pub last_request_at: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelMetrics {
    pub model: String,
    pub calls: u64,
    pub successes: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricsSnapshot {
    pub started_at: i64,
    pub uptime_secs: u64,
    pub total_requests: u64,
    pub total_successes: u64,
    pub total_failures: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub keys: Vec<KeyMetrics>,
    pub models: Vec<ModelMetrics>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MetricsInner {
                started_at: now_unix_secs(),
                by_key: HashMap::new(),
                by_model: HashMap::new(),
            })),
        }
    }

    pub fn record_success(
        &self,
        key_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        elapsed: Duration,
    ) {
        let elapsed_ms = duration_as_ms(elapsed);
        let now = now_unix_secs();
        let mut inner = self.inner.lock();
        let key = inner.by_key.entry(key_id.to_string()).or_default();
        key.requests += 1;
        key.successes += 1;
        key.input_tokens = key.input_tokens.saturating_add(input_tokens);
        key.output_tokens = key.output_tokens.saturating_add(output_tokens);
        key.total_latency_ms = key.total_latency_ms.saturating_add(elapsed_ms as u128);
        key.last_latency_ms = elapsed_ms;
        key.last_request_at = now;

        let model_acc = inner.by_model.entry(model.to_string()).or_default();
        model_acc.calls += 1;
        model_acc.successes += 1;
        model_acc.input_tokens = model_acc.input_tokens.saturating_add(input_tokens);
        model_acc.output_tokens = model_acc.output_tokens.saturating_add(output_tokens);
        model_acc.last_used_at = now;
    }

    pub fn record_failure(&self, key_id: &str, model: &str, elapsed: Duration) {
        let elapsed_ms = duration_as_ms(elapsed);
        let now = now_unix_secs();
        let mut inner = self.inner.lock();
        let key = inner.by_key.entry(key_id.to_string()).or_default();
        key.requests += 1;
        key.failures += 1;
        key.total_latency_ms = key.total_latency_ms.saturating_add(elapsed_ms as u128);
        key.last_latency_ms = elapsed_ms;
        key.last_request_at = now;

        let model_acc = inner.by_model.entry(model.to_string()).or_default();
        model_acc.calls += 1;
        model_acc.failures += 1;
        model_acc.last_used_at = now;
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let inner = self.inner.lock();
        let now = now_unix_secs();
        let uptime = (now - inner.started_at).max(0) as u64;

        let mut keys: Vec<KeyMetrics> = inner
            .by_key
            .iter()
            .map(|(id, acc)| KeyMetrics {
                stable_id: id.clone(),
                requests: acc.requests,
                successes: acc.successes,
                failures: acc.failures,
                input_tokens: acc.input_tokens,
                output_tokens: acc.output_tokens,
                avg_latency_ms: if acc.requests > 0 {
                    let avg = acc.total_latency_ms / acc.requests as u128;
                    avg.min(u64::MAX as u128) as u64
                } else {
                    0
                },
                last_latency_ms: acc.last_latency_ms,
                last_request_at: acc.last_request_at,
            })
            .collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.requests));

        let mut models: Vec<ModelMetrics> = inner
            .by_model
            .iter()
            .map(|(model, acc)| ModelMetrics {
                model: model.clone(),
                calls: acc.calls,
                successes: acc.successes,
                failures: acc.failures,
                input_tokens: acc.input_tokens,
                output_tokens: acc.output_tokens,
                last_used_at: acc.last_used_at,
            })
            .collect();
        models.sort_by_key(|m| std::cmp::Reverse(m.calls));

        let total_requests = keys.iter().map(|k| k.requests).sum();
        let total_successes = keys.iter().map(|k| k.successes).sum();
        let total_failures = keys.iter().map(|k| k.failures).sum();
        let total_input_tokens = keys.iter().map(|k| k.input_tokens).sum();
        let total_output_tokens = keys.iter().map(|k| k.output_tokens).sum();

        MetricsSnapshot {
            started_at: inner.started_at,
            uptime_secs: uptime,
            total_requests,
            total_successes,
            total_failures,
            total_input_tokens,
            total_output_tokens,
            keys,
            models,
        }
    }
}

fn duration_as_ms(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Incrementally extract the most recent `"input_tokens"` and
/// `"output_tokens"` integers from a stream of upstream SSE bytes.
///
/// We keep a small overlap buffer so that integers split across chunk
/// boundaries are still recoverable on the next observation. Used to
/// observe Anthropic-passthrough responses without breaking the
/// byte-for-byte forwarding contract.
pub struct UsageSniffer {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub saw_input: bool,
    pub saw_output: bool,
    tail: Vec<u8>,
}

impl Default for UsageSniffer {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageSniffer {
    /// Largest tail kept across observations. 4 KiB easily covers any
    /// JSON usage block while preventing unbounded memory growth on
    /// long replies.
    const MAX_TAIL: usize = 4096;

    pub fn new() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            saw_input: false,
            saw_output: false,
            tail: Vec::new(),
        }
    }

    pub fn observe(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut combined = Vec::with_capacity(self.tail.len() + bytes.len());
        combined.extend_from_slice(&self.tail);
        combined.extend_from_slice(bytes);

        if let Some(n) = extract_last_int(&combined, b"\"input_tokens\"") {
            self.input_tokens = n;
            self.saw_input = true;
        }
        if let Some(n) = extract_last_int(&combined, b"\"output_tokens\"") {
            self.output_tokens = n;
            self.saw_output = true;
        }

        self.tail = if combined.len() > Self::MAX_TAIL {
            combined[combined.len() - Self::MAX_TAIL..].to_vec()
        } else {
            combined
        };
    }
}

fn extract_last_int(haystack: &[u8], key: &[u8]) -> Option<u64> {
    let mut last: Option<u64> = None;
    let mut i = 0usize;
    while i + key.len() <= haystack.len() {
        if &haystack[i..i + key.len()] == key {
            let mut j = i + key.len();
            while j < haystack.len() && matches!(haystack[j], b' ' | b'\t' | b':' | b'\r' | b'\n') {
                j += 1;
            }
            let start = j;
            while j < haystack.len() && haystack[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(s) = std::str::from_utf8(&haystack[start..j]) {
                    if let Ok(n) = s.parse::<u64>() {
                        last = Some(n);
                    }
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_last_int_picks_the_last_match() {
        let data = b"{\"output_tokens\": 12} ... {\"output_tokens\":345}";
        assert_eq!(extract_last_int(data, b"\"output_tokens\""), Some(345));
    }

    #[test]
    fn sniffer_reads_input_and_output_tokens_across_observations() {
        let mut s = UsageSniffer::new();
        s.observe(b"event: message_start\ndata: {\"usage\":{\"input_tok");
        s.observe(b"ens\": 17}}\n\n");
        s.observe(b"event: message_delta\ndata: {\"usage\":{\"output_tokens\":42}}\n\n");
        assert!(s.saw_input);
        assert_eq!(s.input_tokens, 17);
        assert!(s.saw_output);
        assert_eq!(s.output_tokens, 42);
    }

    #[test]
    fn snapshot_reports_aggregated_totals() {
        let m = MetricsRegistry::new();
        m.record_success("k1", "m1", 100, 200, Duration::from_millis(150));
        m.record_success("k1", "m1", 50, 25, Duration::from_millis(50));
        m.record_failure("k2", "m2", Duration::from_millis(900));

        let snap = m.snapshot();
        assert_eq!(snap.total_requests, 3);
        assert_eq!(snap.total_successes, 2);
        assert_eq!(snap.total_failures, 1);
        assert_eq!(snap.total_input_tokens, 150);
        assert_eq!(snap.total_output_tokens, 225);
        assert_eq!(snap.keys.len(), 2);
        let k1 = snap.keys.iter().find(|k| k.stable_id == "k1").unwrap();
        assert_eq!(k1.requests, 2);
        assert_eq!(k1.avg_latency_ms, 100);
        let m1 = snap.models.iter().find(|m| m.model == "m1").unwrap();
        assert_eq!(m1.calls, 2);
        assert_eq!(m1.input_tokens, 150);
    }
}

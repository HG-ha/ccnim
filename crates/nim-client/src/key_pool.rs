use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use proxy_core::ProviderKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Healthy,
    CoolingDown,
    Exhausted,
    Disabled,
    /// Key is past its user-configured expiration timestamp.
    Expired,
}

/// Configuration knobs for a single key as seen by the pool. Carries enough
/// metadata to surface label/expiry/provider on the dashboard without
/// re-reading the secrets file on every snapshot, and enough information
/// for [`KeyLease`] consumers to know which upstream protocol/URL to talk
/// to.
#[derive(Debug, Clone)]
pub struct KeyPoolEntry {
    /// Stable identifier for this key — typically the UUID stored on
    /// `NimApiKey.id`. Used to attach long-lived statistics (token
    /// usage, latency, success/failure totals) to the *same* logical
    /// key even if the user rotates its secret value, and used by
    /// [`KeyPool::update_keys`] to merge live counters across edits.
    pub stable_id: String,
    pub value: String,
    pub label: Option<String>,
    /// Unix epoch seconds. `None` means "never expires".
    pub expires_at: Option<i64>,
    /// Upstream protocol family (selects request/response handling).
    pub provider: ProviderKind,
    /// Already-resolved base URL. Callers should pass the result of
    /// [`crate::app_config::NimApiKey::effective_base_url`] (or any
    /// equivalent) so the pool never needs to reach back into config
    /// to resolve defaults.
    pub base_url: String,
    /// Whether this key is enabled.
    pub enabled: bool,
    /// Per-key rate limit in requests per pool-wide window.
    /// `None` disables the local cap entirely — useful for upstreams
    /// (OpenAI / Anthropic compat hosts) whose quotas vary widely
    /// and are best left to the upstream itself.
    pub rate_limit: Option<usize>,
}

impl Default for KeyPoolEntry {
    fn default() -> Self {
        Self {
            stable_id: String::new(),
            value: String::new(),
            label: None,
            expires_at: None,
            provider: ProviderKind::Nim,
            base_url: String::new(),
            enabled: true,
            rate_limit: None,
        }
    }
}

impl KeyPoolEntry {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            stable_id: value.clone(),
            value,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeySnapshot {
    pub id: usize,
    /// Stable, config-level identifier (matches `NimApiKey.id`). The
    /// frontend joins this against per-key metrics so usage history
    /// follows the key across edits / pool rebuilds.
    #[serde(default)]
    pub stable_id: String,
    pub masked: String,
    pub label: Option<String>,
    pub expires_at: Option<i64>,
    /// Provider family for this key. The dashboard renders a small badge
    /// (`NIM` / `OpenAI 兼容` / `Anthropic 兼容`) so users can tell at a
    /// glance which upstream is going to be hit.
    pub provider: ProviderKind,
    /// The base URL the proxy will actually send requests to for this
    /// key. Echoed back so the GUI can show "via …" alongside the
    /// masked credential.
    pub base_url: String,
    pub state: KeyState,
    pub inflight: usize,
    pub recent_requests: usize,
    pub failure_count: usize,
    /// Whether this key is enabled.
    pub enabled: bool,
    /// Effective rate limit (requests per the pool-wide window) for
    /// this key. `None` means the local cap is disabled — the pool
    /// will not refuse to lease this key based on `recent_requests`.
    #[serde(default)]
    pub rate_limit: Option<usize>,
}

#[derive(Debug)]
struct KeyEntry {
    id: usize,
    stable_id: String,
    value: String,
    label: Option<String>,
    expires_at: Option<i64>,
    provider: ProviderKind,
    base_url: String,
    state: KeyState,
    inflight: usize,
    recent_requests: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
    failure_count: usize,
    enabled: bool,
    rate_limit: Option<usize>,
}

impl KeyEntry {
    fn fresh(id: usize, entry: KeyPoolEntry) -> Self {
        Self {
            id,
            stable_id: entry.stable_id,
            value: entry.value,
            label: entry.label,
            expires_at: entry.expires_at,
            provider: entry.provider,
            base_url: entry.base_url,
            state: KeyState::Healthy,
            inflight: 0,
            recent_requests: VecDeque::new(),
            cooldown_until: None,
            failure_count: 0,
            enabled: entry.enabled,
            rate_limit: entry.rate_limit,
        }
    }

    /// Build a new entry that inherits live counters from `prev`. Used by
    /// [`KeyPool::update_keys`] so editing a label / expiry / base URL
    /// does not reset inflight / recent / failure stats for an unchanged
    /// key.
    fn merged(id: usize, entry: KeyPoolEntry, prev: &KeyEntry) -> Self {
        Self {
            id,
            stable_id: entry.stable_id,
            value: entry.value,
            label: entry.label,
            expires_at: entry.expires_at,
            provider: entry.provider,
            base_url: entry.base_url,
            state: prev.state,
            inflight: prev.inflight,
            recent_requests: prev.recent_requests.clone(),
            cooldown_until: prev.cooldown_until,
            failure_count: prev.failure_count,
            enabled: entry.enabled,
            rate_limit: entry.rate_limit,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyPool {
    inner: Arc<Mutex<Vec<KeyEntry>>>,
    /// Sliding window over which each entry's rate-limit counter is
    /// pruned. The actual cap is per-entry (see [`KeyEntry::rate_limit`])
    /// — keeping the window pool-wide just avoids a redundant copy on
    /// every key.
    rate_window: Duration,
}

#[derive(Debug)]
pub struct KeyLease {
    pool: KeyPool,
    key_id: usize,
    stable_id: String,
    key: String,
    provider: ProviderKind,
    base_url: String,
}

impl KeyPool {
    /// Build a new key pool. Each [`KeyPoolEntry`] carries its own
    /// rate-limit cap (`None` means "unlimited") — the only pool-wide
    /// knob left here is the window length used to slide the
    /// rate-limit counters.
    pub fn new(keys: Vec<KeyPoolEntry>, rate_window: Duration) -> Self {
        let entries = build_entries(keys, &[]);
        Self {
            inner: Arc::new(Mutex::new(entries)),
            rate_window,
        }
    }

    /// Replace the active set of keys, preserving live counters for keys
    /// whose `value` matches a previous entry. Allows the GUI to apply label
    /// / expiry edits without restarting the proxy.
    pub fn update_keys(&self, keys: Vec<KeyPoolEntry>) {
        let mut entries = self.inner.lock();
        let prev = std::mem::take(&mut *entries);
        *entries = build_entries(keys, &prev);
    }

    pub fn acquire(&self) -> Option<KeyLease> {
        self.acquire_filtered(|_| true)
    }

    /// Acquire a healthy lease, restricted to keys whose provider matches
    /// `provider`. Returns `None` if no key for that provider is healthy
    /// (regardless of whether other providers have capacity), so the
    /// caller can surface a precise error like "no healthy Anthropic-
    /// compatible key" instead of falsely picking a NIM key.
    pub fn acquire_for(&self, provider: ProviderKind) -> Option<KeyLease> {
        self.acquire_filtered(|entry| entry.provider == provider)
    }

    fn acquire_filtered<F>(&self, predicate: F) -> Option<KeyLease>
    where
        F: Fn(&KeyEntry) -> bool,
    {
        let now = Instant::now();
        let now_unix = current_unix_secs();
        let mut entries = self.inner.lock();
        for entry in entries.iter_mut() {
            refresh_key_state(entry, now, now_unix);
            prune_requests(entry, now, self.rate_window);
        }

        let candidate = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                predicate(entry)
                    && entry.enabled
                    && entry.state == KeyState::Healthy
                    && entry
                        .rate_limit
                        .map(|limit| entry.recent_requests.len() < limit)
                        .unwrap_or(true)
            })
            .min_by_key(|(_, entry)| (entry.inflight, entry.recent_requests.len()))
            .map(|(idx, _)| idx)?;

        let entry = &mut entries[candidate];
        entry.inflight += 1;
        entry.recent_requests.push_back(now);
        Some(KeyLease {
            pool: self.clone(),
            key_id: entry.id,
            stable_id: entry.stable_id.clone(),
            key: entry.value.clone(),
            provider: entry.provider,
            base_url: entry.base_url.clone(),
        })
    }

    pub fn mark_success(&self, key_id: usize) {
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == key_id) {
            entry.failure_count = 0;
            // Keep terminal states sticky; only "transient unhealthy" gets reset.
            if matches!(entry.state, KeyState::CoolingDown) {
                entry.state = KeyState::Healthy;
            }
        }
    }

    pub fn mark_auth_failed(&self, key_id: usize) {
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == key_id) {
            entry.state = KeyState::Disabled;
            entry.failure_count += 1;
        }
    }

    pub fn mark_rate_limited(&self, key_id: usize, retry_after: Option<Duration>) {
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == key_id) {
            entry.state = KeyState::CoolingDown;
            entry.failure_count += 1;
            entry.cooldown_until = Some(
                Instant::now()
                    + retry_after
                        .unwrap_or_else(|| Duration::from_secs(10 * entry.failure_count as u64)),
            );
        }
    }

    pub fn mark_network_error(&self, key_id: usize) {
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == key_id) {
            entry.failure_count += 1;
            if entry.failure_count >= 3 {
                entry.state = KeyState::CoolingDown;
                entry.cooldown_until = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    }

    pub fn release(&self, key_id: usize) {
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == key_id) {
            entry.inflight = entry.inflight.saturating_sub(1);
        }
    }

    pub fn snapshots(&self) -> Vec<KeySnapshot> {
        let now = Instant::now();
        let now_unix = current_unix_secs();
        let mut entries = self.inner.lock();
        entries
            .iter_mut()
            .map(|entry| {
                refresh_key_state(entry, now, now_unix);
                prune_requests(entry, now, self.rate_window);
                KeySnapshot {
                    id: entry.id,
                    stable_id: entry.stable_id.clone(),
                    masked: mask_key(&entry.value),
                    label: entry.label.clone(),
                    expires_at: entry.expires_at,
                    provider: entry.provider,
                    base_url: entry.base_url.clone(),
                    state: entry.state,
                    inflight: entry.inflight,
                    recent_requests: entry.recent_requests.len(),
                    failure_count: entry.failure_count,
                    enabled: entry.enabled,
                    rate_limit: entry.rate_limit,
                }
            })
            .collect()
    }
}

impl KeyLease {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn key_id(&self) -> usize {
        self.key_id
    }

    /// Stable, config-level identifier for this key (matches
    /// `NimApiKey.id`). Use this — not [`KeyLease::key_id`] — when
    /// recording metrics so the totals survive pool rebuilds and key
    /// reorderings.
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }

    pub fn pool(&self) -> &KeyPool {
        &self.pool
    }

    /// Provider associated with the leased key. Lets the upstream client
    /// decide which protocol/path to use without re-querying the pool.
    pub fn provider(&self) -> ProviderKind {
        self.provider
    }

    /// Already-resolved base URL for this key (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for KeyLease {
    fn drop(&mut self) {
        self.pool.release(self.key_id);
    }
}

fn build_entries(input: Vec<KeyPoolEntry>, prev: &[KeyEntry]) -> Vec<KeyEntry> {
    // Match by `stable_id` first so editing the secret value of an
    // existing key still preserves live counters; fall back to a
    // value-based lookup for any legacy entries (or callers that
    // intentionally reuse the secret as the stable id).
    let by_stable: HashMap<&str, &KeyEntry> = prev
        .iter()
        .filter(|e| !e.stable_id.is_empty())
        .map(|e| (e.stable_id.as_str(), e))
        .collect();
    let by_value: HashMap<&str, &KeyEntry> = prev.iter().map(|e| (e.value.as_str(), e)).collect();
    input
        .into_iter()
        .filter(|entry| !entry.value.trim().is_empty())
        .enumerate()
        .map(|(id, entry)| {
            let prev = by_stable
                .get(entry.stable_id.as_str())
                .copied()
                .or_else(|| by_value.get(entry.value.as_str()).copied());
            match prev {
                Some(prev) => KeyEntry::merged(id, entry, prev),
                None => KeyEntry::fresh(id, entry),
            }
        })
        .collect()
}

fn refresh_key_state(entry: &mut KeyEntry, now: Instant, now_unix: i64) {
    // Disabled (auth-rejected) is sticky regardless of expiry; surfacing the
    // upstream rejection is more actionable than "expired".
    if entry.state != KeyState::Disabled {
        let expired = matches!(entry.expires_at, Some(t) if now_unix >= t);
        if expired {
            entry.state = KeyState::Expired;
            entry.cooldown_until = None;
            return;
        }
        if entry.state == KeyState::Expired {
            // User extended the expiry past `now`; un-expire.
            entry.state = KeyState::Healthy;
        }
    }

    if entry.state == KeyState::CoolingDown {
        if let Some(until) = entry.cooldown_until {
            if now >= until {
                entry.state = KeyState::Healthy;
                entry.cooldown_until = None;
            }
        }
    }
}

fn prune_requests(entry: &mut KeyEntry, now: Instant, window: Duration) {
    while entry
        .recent_requests
        .front()
        .is_some_and(|instant| now.duration_since(*instant) > window)
    {
        entry.recent_requests.pop_front();
    }
}

fn mask_key(key: &str) -> String {
    if key.len() <= 10 {
        return "********".to_string();
    }
    format!("{}...{}", &key[..6], &key[key.len() - 4..])
}

fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(values: &[&str]) -> KeyPool {
        KeyPool::new(
            values
                .iter()
                .map(|v| KeyPoolEntry {
                    rate_limit: Some(40),
                    ..KeyPoolEntry::new(*v)
                })
                .collect(),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn rotates_away_from_rate_limited_key() {
        let pool = pool(&["nvapi-first", "nvapi-second"]);
        let first = pool.acquire().expect("first key");
        let first_id = first.key_id();
        drop(first);

        pool.mark_rate_limited(first_id, Some(Duration::from_secs(60)));
        let next = pool.acquire().expect("second key");
        assert_ne!(next.key_id(), first_id);
    }

    #[test]
    fn disables_key_on_auth_failure() {
        let pool = pool(&["nvapi-first"]);
        let lease = pool.acquire().expect("key");
        let id = lease.key_id();
        drop(lease);

        pool.mark_auth_failed(id);
        assert!(pool.acquire().is_none());
        assert_eq!(pool.snapshots()[0].state, KeyState::Disabled);
    }

    /// The frontend matches on `state === "healthy"` etc. — keep the wire
    /// format snake_case so the case mismatch bug never returns.
    #[test]
    fn key_state_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&KeyState::Healthy).unwrap(),
            "\"healthy\""
        );
        assert_eq!(
            serde_json::to_string(&KeyState::CoolingDown).unwrap(),
            "\"cooling_down\""
        );
        assert_eq!(
            serde_json::to_string(&KeyState::Exhausted).unwrap(),
            "\"exhausted\""
        );
        assert_eq!(
            serde_json::to_string(&KeyState::Disabled).unwrap(),
            "\"disabled\""
        );
        assert_eq!(
            serde_json::to_string(&KeyState::Expired).unwrap(),
            "\"expired\""
        );
    }

    /// Keys whose user-configured expiry has passed are reported as Expired
    /// and excluded from acquire().
    #[test]
    fn expired_key_is_skipped_by_acquire() {
        let past = current_unix_secs() - 60;
        let future = current_unix_secs() + 600;
        let pool = KeyPool::new(
            vec![
                KeyPoolEntry {
                    value: "nvapi-old".to_string(),
                    label: Some("expired".into()),
                    expires_at: Some(past),
                    rate_limit: Some(40),
                    ..Default::default()
                },
                KeyPoolEntry {
                    value: "nvapi-new".to_string(),
                    label: None,
                    expires_at: Some(future),
                    rate_limit: Some(40),
                    ..Default::default()
                },
            ],
            Duration::from_secs(60),
        );

        let lease = pool.acquire().expect("non-expired key should be available");
        assert_eq!(lease.key(), "nvapi-new");
        drop(lease);

        let snaps = pool.snapshots();
        assert_eq!(snaps[0].state, KeyState::Expired);
        assert_eq!(snaps[1].state, KeyState::Healthy);
    }

    /// Editing a key (e.g. updating its label) must not reset live stats for
    /// keys whose secret value is unchanged.
    #[test]
    fn update_keys_preserves_stats_for_unchanged_value() {
        let pool = pool(&["nvapi-first", "nvapi-second"]);
        let lease = pool.acquire().expect("key");
        // simulate one in-flight + one recorded request
        let id_before = lease.key_id();
        drop(lease);

        // Re-submit the same values, but attach a label; first key keeps its
        // recent_requests counter (the snapshot must show 1).
        pool.update_keys(vec![
            KeyPoolEntry {
                value: "nvapi-first".to_string(),
                label: Some("primary".into()),
                expires_at: None,
                ..Default::default()
            },
            KeyPoolEntry {
                value: "nvapi-second".to_string(),
                label: None,
                expires_at: None,
                ..Default::default()
            },
        ]);
        let snaps = pool.snapshots();
        let first = &snaps[0];
        assert_eq!(first.label.as_deref(), Some("primary"));
        assert_eq!(first.recent_requests, 1, "should retain recent_requests");
        let _ = id_before;
    }

    /// `acquire_for(provider)` returns only keys with the matching protocol
    /// family, even if other providers also have healthy capacity.
    #[test]
    fn acquire_for_filters_by_provider() {
        let pool = KeyPool::new(
            vec![
                KeyPoolEntry {
                    value: "nvapi-x".into(),
                    provider: ProviderKind::Nim,
                    base_url: "https://integrate.api.nvidia.com/v1".into(),
                    rate_limit: Some(40),
                    ..Default::default()
                },
                KeyPoolEntry {
                    value: "sk-deepseek".into(),
                    provider: ProviderKind::OpenaiCompat,
                    base_url: "https://api.deepseek.com".into(),
                    ..Default::default()
                },
                KeyPoolEntry {
                    value: "sk-ant-x".into(),
                    provider: ProviderKind::AnthropicCompat,
                    base_url: "https://api.anthropic.com".into(),
                    ..Default::default()
                },
            ],
            Duration::from_secs(60),
        );

        let nim = pool.acquire_for(ProviderKind::Nim).unwrap();
        assert_eq!(nim.key(), "nvapi-x");
        assert_eq!(nim.provider(), ProviderKind::Nim);
        assert_eq!(nim.base_url(), "https://integrate.api.nvidia.com/v1");
        drop(nim);

        let oai = pool.acquire_for(ProviderKind::OpenaiCompat).unwrap();
        assert_eq!(oai.key(), "sk-deepseek");
        drop(oai);

        let anth = pool.acquire_for(ProviderKind::AnthropicCompat).unwrap();
        assert_eq!(anth.base_url(), "https://api.anthropic.com");
    }

    /// When no key exists for a given provider, acquire_for returns None
    /// instead of falling back to a different provider.
    #[test]
    fn acquire_for_returns_none_when_no_provider_match() {
        let pool = KeyPool::new(
            vec![KeyPoolEntry {
                value: "nvapi-x".into(),
                provider: ProviderKind::Nim,
                base_url: "https://integrate.api.nvidia.com/v1".into(),
                rate_limit: Some(40),
                ..Default::default()
            }],
            Duration::from_secs(60),
        );
        assert!(pool.acquire_for(ProviderKind::AnthropicCompat).is_none());
        assert!(pool.acquire_for(ProviderKind::OpenaiCompat).is_none());
        assert!(pool.acquire_for(ProviderKind::Nim).is_some());
    }

    /// A `None` per-key rate limit means the pool refuses to gate
    /// requests at all — useful for OpenAI/Anthropic-compat hosts
    /// whose quotas vary widely and are best left to the upstream.
    #[test]
    fn unlimited_key_is_not_capped_by_recent_requests() {
        let pool = KeyPool::new(
            vec![KeyPoolEntry {
                value: "sk-unlimited".into(),
                provider: ProviderKind::OpenaiCompat,
                base_url: "https://api.deepseek.com".into(),
                rate_limit: None,
                ..Default::default()
            }],
            Duration::from_secs(60),
        );
        // Burn a comfortable margin past any reasonable per-key cap;
        // an unlimited key must keep handing out leases regardless.
        for _ in 0..200 {
            let lease = pool
                .acquire()
                .expect("unlimited key should always be acquirable");
            drop(lease);
        }
        assert!(pool.snapshots()[0].rate_limit.is_none());
    }
}

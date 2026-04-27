use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
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
/// metadata to surface label/expiry on the dashboard without re-reading the
/// secrets file on every snapshot.
#[derive(Debug, Clone, Default)]
pub struct KeyPoolEntry {
    pub value: String,
    pub label: Option<String>,
    /// Unix epoch seconds. `None` means "never expires".
    pub expires_at: Option<i64>,
}

impl KeyPoolEntry {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: None,
            expires_at: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeySnapshot {
    pub id: usize,
    pub masked: String,
    pub label: Option<String>,
    pub expires_at: Option<i64>,
    pub state: KeyState,
    pub inflight: usize,
    pub recent_requests: usize,
    pub failure_count: usize,
}

#[derive(Debug)]
struct KeyEntry {
    id: usize,
    value: String,
    label: Option<String>,
    expires_at: Option<i64>,
    state: KeyState,
    inflight: usize,
    recent_requests: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
    failure_count: usize,
}

impl KeyEntry {
    fn fresh(id: usize, entry: KeyPoolEntry) -> Self {
        Self {
            id,
            value: entry.value,
            label: entry.label,
            expires_at: entry.expires_at,
            state: KeyState::Healthy,
            inflight: 0,
            recent_requests: VecDeque::new(),
            cooldown_until: None,
            failure_count: 0,
        }
    }

    /// Build a new entry that inherits live counters from `prev`. Used by
    /// [`KeyPool::update_keys`] so editing a label or expiry does not reset
    /// inflight / recent / failure stats for an unchanged key.
    fn merged(id: usize, entry: KeyPoolEntry, prev: &KeyEntry) -> Self {
        Self {
            id,
            value: entry.value,
            label: entry.label,
            expires_at: entry.expires_at,
            state: prev.state,
            inflight: prev.inflight,
            recent_requests: prev.recent_requests.clone(),
            cooldown_until: prev.cooldown_until,
            failure_count: prev.failure_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyPool {
    inner: Arc<Mutex<Vec<KeyEntry>>>,
    rate_limit: usize,
    rate_window: Duration,
}

#[derive(Debug)]
pub struct KeyLease {
    pool: KeyPool,
    key_id: usize,
    key: String,
}

impl KeyPool {
    pub fn new(keys: Vec<KeyPoolEntry>, rate_limit: usize, rate_window: Duration) -> Self {
        let entries = build_entries(keys, &[]);
        Self {
            inner: Arc::new(Mutex::new(entries)),
            rate_limit,
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
                entry.state == KeyState::Healthy && entry.recent_requests.len() < self.rate_limit
            })
            .min_by_key(|(_, entry)| (entry.inflight, entry.recent_requests.len()))
            .map(|(idx, _)| idx)?;

        let entry = &mut entries[candidate];
        entry.inflight += 1;
        entry.recent_requests.push_back(now);
        Some(KeyLease {
            pool: self.clone(),
            key_id: entry.id,
            key: entry.value.clone(),
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
                    masked: mask_key(&entry.value),
                    label: entry.label.clone(),
                    expires_at: entry.expires_at,
                    state: entry.state,
                    inflight: entry.inflight,
                    recent_requests: entry.recent_requests.len(),
                    failure_count: entry.failure_count,
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

    pub fn pool(&self) -> &KeyPool {
        &self.pool
    }
}

impl Drop for KeyLease {
    fn drop(&mut self) {
        self.pool.release(self.key_id);
    }
}

fn build_entries(input: Vec<KeyPoolEntry>, prev: &[KeyEntry]) -> Vec<KeyEntry> {
    let by_value: HashMap<&str, &KeyEntry> = prev.iter().map(|e| (e.value.as_str(), e)).collect();
    input
        .into_iter()
        .filter(|entry| !entry.value.trim().is_empty())
        .enumerate()
        .map(|(id, entry)| match by_value.get(entry.value.as_str()) {
            Some(prev) => KeyEntry::merged(id, entry, prev),
            None => KeyEntry::fresh(id, entry),
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
            values.iter().map(|v| KeyPoolEntry::new(*v)).collect(),
            40,
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
                },
                KeyPoolEntry {
                    value: "nvapi-new".to_string(),
                    label: None,
                    expires_at: Some(future),
                },
            ],
            40,
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
            },
            KeyPoolEntry {
                value: "nvapi-second".to_string(),
                label: None,
                expires_at: None,
            },
        ]);
        let snaps = pool.snapshots();
        let first = &snaps[0];
        assert_eq!(first.label.as_deref(), Some("primary"));
        assert_eq!(first.recent_requests, 1, "should retain recent_requests");
        let _ = id_before;
    }
}

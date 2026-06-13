//! Bounded, TTL'd set of strings — the shared primitive behind both inbound dedup and the
//! outbound sent-id cache (see `sent_ids.rs`). No background sweeper task: entries expire lazily on
//! access, and an opportunistic sweep on insert keeps the map from growing past `capacity`.
//!
//! Why dedup exists: GOWA times out each webhook delivery at 10s and retries up to 5× with
//! exponential backoff. Combined with ack-fast + async-forward, a slow agent turn would otherwise
//! cause GOWA to re-deliver the same message id and the agent to reply more than once. Dropping a
//! already-seen `id` here is what makes the at-least-once webhook delivery idempotent downstream.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// A thread-safe set of strings where each entry lives for `ttl` and the whole set is capped at
/// `capacity` live entries. Cheap to share behind an `Arc`.
pub struct TtlSet {
    inner: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
    capacity: usize,
}

impl TtlSet {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// Insert `key`, returning `true` if it was newly inserted (i.e. *not* already present and
    /// unexpired). A `false` return means "already seen" — the caller should treat it as a
    /// duplicate. An expired prior entry counts as absent, so it returns `true` and is refreshed.
    pub fn insert_new(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("ttl set poisoned");
        let live = map
            .get(key)
            .is_some_and(|seen| now.duration_since(*seen) < self.ttl);
        if live {
            return false;
        }
        map.insert(key.to_string(), now);
        self.sweep(&mut map, now);
        true
    }

    /// Record `key` as present (refreshing its timestamp), ignoring whether it already was. Used by
    /// the sent-id cache where "I sent this" is unconditional.
    pub fn record(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("ttl set poisoned");
        map.insert(key.to_string(), now);
        self.sweep(&mut map, now);
    }

    /// True if `key` is present and unexpired.
    pub fn contains(&self, key: &str) -> bool {
        let now = Instant::now();
        let map = self.inner.lock().expect("ttl set poisoned");
        map.get(key)
            .is_some_and(|seen| now.duration_since(*seen) < self.ttl)
    }

    /// Drop expired entries; if still over capacity, evict the oldest until within bounds. Called
    /// under the held lock on every mutating op, so the map never grows unbounded between accesses.
    fn sweep(&self, map: &mut HashMap<String, Instant>, now: Instant) {
        map.retain(|_, seen| now.duration_since(*seen) < self.ttl);
        while map.len() > self.capacity {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, seen)| **seen)
                .map(|(key, _)| key.clone())
            {
                map.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_new_is_true_once_then_false() {
        let set = TtlSet::new(Duration::from_secs(60), 100);
        assert!(set.insert_new("id-1"));
        assert!(!set.insert_new("id-1"));
        assert!(set.insert_new("id-2"));
    }

    #[test]
    fn expired_entry_is_absent_again() {
        let set = TtlSet::new(Duration::from_millis(1), 100);
        assert!(set.insert_new("id-1"));
        std::thread::sleep(Duration::from_millis(5));
        // Expired → treated as new again.
        assert!(set.insert_new("id-1"));
        assert!(set.contains("id-1"));
    }

    #[test]
    fn capacity_evicts_oldest() {
        let set = TtlSet::new(Duration::from_secs(60), 2);
        set.record("a");
        set.record("b");
        set.record("c");
        // "a" was oldest and should have been evicted to stay within capacity 2.
        assert!(!set.contains("a"));
        assert!(set.contains("b"));
        assert!(set.contains("c"));
    }
}

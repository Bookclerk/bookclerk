//! Small in-process TTL cache for Discover metadata / purchase hints.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cached value plus the monotonic instant after which [`TtlCache::get`] evicts it.
struct Entry<T> {
    /// Cloned out on a hit while still unexpired.
    value: T,
    /// Monotonic deadline; equal or earlier instants are treated as a miss.
    expires: Instant,
}

/// Bounded TTL map (FIFO-ish eviction of expired + oldest when over capacity).
pub struct TtlCache<T> {
    /// Keyed entries; a poisoned lock makes get/insert no-ops.
    inner: Mutex<HashMap<String, Entry<T>>>,
    /// Lifetime applied to each insert (from `Instant::now`).
    ttl: Duration,
    /// Capacity after which an arbitrary live entry is evicted (at least 16).
    max_entries: usize,
}

impl<T: Clone> TtlCache<T> {
    #[must_use]
    /// Builds a cache with `ttl` per entry and `max_entries` clamped to at least 16.
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            max_entries: max_entries.max(16),
        }
    }

    /// Returns a clone on a live hit; expired keys are removed and yield `None`.
    pub fn get(&self, key: &str) -> Option<T> {
        let mut guard = self.inner.lock().ok()?;
        let now = Instant::now();
        match guard.get(key) {
            Some(e) if e.expires > now => Some(e.value.clone()),
            Some(_) => {
                guard.remove(key);
                None
            }
            None => None,
        }
    }

    /// Inserts `value` with a fresh TTL, dropping expired keys and evicting when over capacity.
    pub fn insert(&self, key: String, value: T) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        guard.retain(|_, e| e.expires > now);
        if guard.len() >= self.max_entries {
            // Drop an arbitrary expired-or-oldest entry (HashMap iter order).
            if let Some(evict) = guard.keys().next().cloned() {
                guard.remove(&evict);
            }
        }
        guard.insert(
            key,
            Entry {
                value,
                expires: now + self.ttl,
            },
        );
    }
}

/// Stable cache key from arbitrary serializable fields.
///
/// # Arguments
///
/// * `parts` - String `parts` for this call.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn cache_key(parts: &[&str]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.trim().to_ascii_lowercase().hash(&mut hasher);
        b'\0'.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

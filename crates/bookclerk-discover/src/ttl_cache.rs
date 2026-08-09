//! Small in-process TTL cache for Discover metadata / purchase hints.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Entry<T> {
    value: T,
    expires: Instant,
}

/// Bounded TTL map (FIFO-ish eviction of expired + oldest when over capacity).
pub struct TtlCache<T> {
    inner: Mutex<HashMap<String, Entry<T>>>,
    ttl: Duration,
    max_entries: usize,
}

impl<T: Clone> TtlCache<T> {
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            max_entries: max_entries.max(16),
        }
    }

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
#[must_use]
pub fn cache_key(parts: &[&str]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.trim().to_ascii_lowercase().hash(&mut hasher);
        b'\0'.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

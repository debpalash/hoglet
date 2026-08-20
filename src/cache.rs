//! Result cache — LRU cache of query responses keyed by (token, IR hash,
//! data version). Avoids re-running DuckDB queries when the data hasn't
//! changed. In-memory only, rebuilt on restart.

use std::collections::HashMap;
use std::sync::Mutex;

pub const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;

pub struct ResultCache {
    state: Mutex<CacheState>,
    max_entries: usize,
    max_bytes: usize,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CacheKey {
    pub token: String,
    pub ir_hash: u64,
    pub data_version: u64,
}

struct CacheEntry {
    response_json: Vec<u8>,
    last_access: u64,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    response_bytes: usize,
    access_clock: u64,
}

impl CacheState {
    fn tick(&mut self) -> u64 {
        self.access_clock = self.access_clock.saturating_add(1);
        self.access_clock
    }
}

impl ResultCache {
    pub fn new(max_entries: usize) -> Self {
        Self::with_limits(max_entries, MAX_CACHE_BYTES)
    }

    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        ResultCache {
            state: Mutex::new(CacheState::default()),
            max_entries,
            max_bytes,
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        let access = state.tick();
        state.entries.get_mut(key).map(|entry| {
            entry.last_access = access;
            entry.response_json.clone()
        })
    }

    pub fn put(&self, key: CacheKey, response_json: Vec<u8>) {
        let mut state = self.state.lock().unwrap();

        if let Some(replaced) = state.entries.remove(&key) {
            state.response_bytes = state
                .response_bytes
                .saturating_sub(replaced.response_json.len());
        }

        // One response that cannot fit must not evict the useful cache or leave
        // a stale value for the same key behind.
        if self.max_entries == 0 || response_json.len() > self.max_bytes {
            return;
        }

        let response_len = response_json.len();
        let access = state.tick();
        state.entries.insert(
            key,
            CacheEntry {
                response_json,
                last_access: access,
            },
        );
        state.response_bytes += response_len;

        while state.entries.len() > self.max_entries || state.response_bytes > self.max_bytes {
            let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = state.entries.remove(&oldest) {
                state.response_bytes = state
                    .response_bytes
                    .saturating_sub(evicted.response_json.len());
            }
        }
    }

    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.entries.clear();
        state.response_bytes = 0;
    }
}

/// Hash the IR JSON for cache key generation.
pub fn hash_ir(ir: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ir.to_string().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: u64) -> CacheKey {
        CacheKey {
            token: "phc_test".into(),
            ir_hash: id,
            data_version: 1,
        }
    }

    #[test]
    fn entry_limit_evicts_the_least_recently_used_value() {
        let cache = ResultCache::with_limits(2, 1024);
        cache.put(key(1), vec![1]);
        cache.put(key(2), vec![2]);
        assert_eq!(cache.get(&key(1)), Some(vec![1]));

        cache.put(key(3), vec![3]);

        assert_eq!(cache.get(&key(1)), Some(vec![1]));
        assert_eq!(cache.get(&key(2)), None);
        assert_eq!(cache.get(&key(3)), Some(vec![3]));
    }

    #[test]
    fn response_bytes_are_bounded_independently_of_entry_count() {
        let cache = ResultCache::with_limits(10, 5);
        cache.put(key(1), vec![1; 3]);
        cache.put(key(2), vec![2; 3]);

        assert_eq!(cache.get(&key(1)), None);
        assert_eq!(cache.get(&key(2)), Some(vec![2; 3]));
    }

    #[test]
    fn replacement_updates_byte_accounting_and_oversize_values_are_not_cached() {
        let cache = ResultCache::with_limits(10, 5);
        cache.put(key(1), vec![1; 4]);
        cache.put(key(1), vec![1; 2]);
        cache.put(key(2), vec![2; 3]);

        assert_eq!(cache.get(&key(1)), Some(vec![1; 2]));
        assert_eq!(cache.get(&key(2)), Some(vec![2; 3]));

        cache.put(key(2), vec![9; 6]);
        assert_eq!(cache.get(&key(2)), None);
    }

    #[test]
    fn clear_resets_entries_and_byte_budget() {
        let cache = ResultCache::with_limits(10, 5);
        cache.put(key(1), vec![1; 5]);
        cache.clear();
        cache.put(key(2), vec![2; 5]);

        assert_eq!(cache.get(&key(1)), None);
        assert_eq!(cache.get(&key(2)), Some(vec![2; 5]));
    }
}

//! Result cache — LRU cache of query responses keyed by (token, IR hash,
//! data version). Avoids re-running DuckDB queries when the data hasn't
//! changed. In-memory only, rebuilt on restart.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

pub struct ResultCache {
    entries: Mutex<HashMap<CacheKey, CacheEntry>>,
    max_entries: usize,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CacheKey {
    pub token: String,
    pub ir_hash: u64,
    pub data_version: u64,
}

struct CacheEntry {
    response_json: Vec<u8>,
    inserted_at: Instant,
}

impl ResultCache {
    pub fn new(max_entries: usize) -> Self {
        ResultCache {
            entries: Mutex::new(HashMap::new()),
            max_entries,
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        let map = self.entries.lock().unwrap();
        map.get(key).map(|e| e.response_json.clone())
    }

    pub fn put(&self, key: CacheKey, response_json: Vec<u8>) {
        let mut map = self.entries.lock().unwrap();
        if map.len() >= self.max_entries {
            // Evict oldest
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, v)| v.inserted_at)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(key, CacheEntry {
            response_json,
            inserted_at: Instant::now(),
        });
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

/// Hash the IR JSON for cache key generation.
pub fn hash_ir(ir: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ir.to_string().hash(&mut hasher);
    hasher.finish()
}

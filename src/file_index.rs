//! In-memory file index — avoids filesystem directory listing in the
//! query path. Updated by the flusher after writes + compaction.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

pub struct FileIndex {
    files: RwLock<HashMap<String, BTreeMap<String, Vec<String>>>>,
    version: std::sync::atomic::AtomicU64,
}

impl FileIndex {
    pub fn new() -> Self {
        FileIndex {
            files: RwLock::new(HashMap::new()),
            version: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn add_files(&self, token: &str, date: &str, file_names: &[String]) {
        let mut map = self.files.write().unwrap();
        map.entry(token.to_string())
            .or_default()
            .entry(date.to_string())
            .or_default()
            .extend(file_names.iter().cloned());
        self.version.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn bump(&self) {
        self.version.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn read_version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn list_files(&self, token: &str) -> Vec<String> {
        let map = self.files.read().unwrap();
        let mut paths = Vec::new();
        if let Some(dates) = map.get(token) {
            for (_date, files) in dates {
                paths.extend(files.clone());
            }
        }
        paths
    }

    pub fn rebuild_from_dir(&self, events_dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(events_dir) else { return };
        let mut map: HashMap<String, BTreeMap<String, Vec<String>>> = HashMap::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".parquet") {
                map.entry("*".to_string()).or_default().entry("*".to_string()).or_default().push(name);
            }
        }
        if let Ok(mut m) = self.files.write() {
            *m = map;
        }
        self.bump();
    }
}

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
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn bump(&self) {
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn read_version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Files for one token, plus any unpartitioned legacy files.
    pub fn list_files(&self, token: &str) -> Vec<String> {
        let map = self.files.read().unwrap();
        let mut paths = Vec::new();
        for key in [token, "*"] {
            if let Some(dates) = map.get(key) {
                for (_date, files) in dates {
                    paths.extend(files.clone());
                }
            }
        }
        paths
    }

    /// Rebuild from the on-disk layout `{token}/{date}/*.parquet`.
    ///
    /// Files left at the top level predate partitioning; their token and date
    /// are unknown, so they go under `*`/`*` and every token's listing must
    /// include them — dropping a file whose partition is unknown would silently
    /// drop rows.
    pub fn rebuild_from_dir(&self, events_dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(events_dir) else {
            return;
        };
        let mut map: HashMap<String, BTreeMap<String, Vec<String>>> = HashMap::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if path.is_file() && name.ends_with(".parquet") {
                map.entry("*".into())
                    .or_default()
                    .entry("*".into())
                    .or_default()
                    .push(name);
                continue;
            }
            if !path.is_dir() {
                continue;
            }
            let Ok(dates) = std::fs::read_dir(&path) else {
                continue;
            };
            for date_entry in dates.flatten() {
                let date = date_entry.file_name().to_string_lossy().to_string();
                if !date_entry.path().is_dir() {
                    continue;
                }
                let Ok(files) = std::fs::read_dir(date_entry.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let fname = f.file_name().to_string_lossy().to_string();
                    if fname.ends_with(".parquet") {
                        map.entry(name.clone())
                            .or_default()
                            .entry(date.clone())
                            .or_default()
                            .push(f.path().to_string_lossy().to_string());
                    }
                }
            }
        }
        if let Ok(mut m) = self.files.write() {
            *m = map;
        }
        self.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rebuild_reads_the_partition_layout() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("phc_x/2026-07-21")).unwrap();
        fs::create_dir_all(dir.path().join("phc_y/2026-07-21")).unwrap();
        fs::write(dir.path().join("phc_x/2026-07-21/a.parquet"), b"").unwrap();
        fs::write(dir.path().join("phc_x/2026-07-21/b.parquet"), b"").unwrap();
        fs::write(dir.path().join("phc_y/2026-07-21/c.parquet"), b"").unwrap();
        // A pre-partitioning file, token unknown.
        fs::write(dir.path().join("legacy.parquet"), b"").unwrap();

        let idx = FileIndex::new();
        let before = idx.read_version();
        idx.rebuild_from_dir(dir.path());
        assert!(
            idx.read_version() > before,
            "version must move so caches invalidate"
        );

        // Own partition + the unattributable legacy file, never another token's.
        assert_eq!(idx.list_files("phc_x").len(), 3);
        assert_eq!(idx.list_files("phc_y").len(), 2);
        assert_eq!(idx.list_files("phc_unknown").len(), 1);
        assert!(idx.list_files("phc_x").iter().all(|f| !f.contains("phc_y")));
    }
}

//! Event store — Parquet segments on disk, DuckDB's read-only substrate
//! (stack.md). Files are published atomically: write tmp → fsync → rename →
//! fsync dir, so a crash never leaves a half-written Parquet file visible.

pub mod parquet;

use std::path::{Path, PathBuf};

use crate::capture::event::CapturedEvent;

/// Parquet files smaller than this are compaction candidates — many tiny
/// segments degrade query performance (stack.md).
pub const COMPACT_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Compaction merges at most this many files per run — bounded work per
/// cycle.
pub const COMPACT_MAX_FILES: usize = 64;

pub struct EventStore {
    dir: PathBuf,
    counter: std::sync::atomic::AtomicU64,
}

impl EventStore {
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(Self {
            dir,
            counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write a batch of events as one Parquet file, atomically published.
    pub fn write_events(&self, events: &[CapturedEvent]) -> std::io::Result<PathBuf> {
        assert!(!events.is_empty(), "caller must not flush empty batches");
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Name by first event time + uuid: sortable, collision-free.
        let name = format!(
            "{}-{:06}-{}.parquet",
            events[0].timestamp.format("%Y%m%dT%H%M%S"),
            n,
            uuid::Uuid::new_v4().simple()
        );
        let final_path = self.dir.join(&name);
        let tmp_path = self.dir.join(format!("{name}.tmp"));

        parquet::write_file(events, &tmp_path)?;
        std::fs::rename(&tmp_path, &final_path)?;
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(final_path)
    }

    pub fn list_files(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "parquet") {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    /// Delete Parquet files whose newest event is older than `cutoff`
    /// (SPEC.md "Operational contract" retention). Whole-file only: a file
    /// straddling the cutoff is kept until all its events expire — bounded,
    /// and never drops a live event. Returns files deleted.
    pub fn enforce_retention(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> std::io::Result<usize> {
        let mut deleted = 0;
        for path in self.list_files()? {
            let events = parquet::read_file(&path)?;
            let max_ts = events.iter().map(|e| e.timestamp).max();
            if let Some(max_ts) = max_ts
                && max_ts < cutoff
            {
                std::fs::remove_file(&path)?;
                deleted += 1;
            }
        }
        if deleted > 0 {
            std::fs::File::open(&self.dir)?.sync_all()?;
        }
        Ok(deleted)
    }

    /// Merge small Parquet files into one. Returns the number of files
    /// merged (0 = nothing to do).
    pub fn compact(&self) -> std::io::Result<usize> {
        let mut small: Vec<PathBuf> = Vec::new();
        for path in self.list_files()? {
            if std::fs::metadata(&path)?.len() < COMPACT_THRESHOLD_BYTES {
                small.push(path);
            }
            if small.len() >= COMPACT_MAX_FILES {
                break;
            }
        }
        if small.len() < 2 {
            return Ok(0);
        }

        let mut events = Vec::new();
        for path in &small {
            events.extend(parquet::read_file(path)?);
        }
        // Publish the merged file first, durably — only then delete inputs.
        // A crash in between leaves duplicates, never loss; dedupe by uuid
        // happens at query time.
        self.write_events(&events)?;
        for path in &small {
            std::fs::remove_file(path)?;
        }
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(small.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::Map;
    use uuid::Uuid;

    fn event(name: &str) -> CapturedEvent {
        let mut properties = Map::new();
        properties.insert("plan".into(), serde_json::json!("pro"));
        CapturedEvent {
            uuid: Uuid::new_v4(),
            event: name.into(),
            distinct_id: "u1".into(),
            token: "phc_t".into(),
            timestamp: Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap(),
            properties,
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        let events = vec![event("a"), event("b")];
        let path = store.write_events(&events).unwrap();

        let back = parquet::read_file(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].event, "a");
        assert_eq!(back[0].distinct_id, "u1");
        assert_eq!(back[0].timestamp, events[0].timestamp);
        assert_eq!(back[0].uuid, events[0].uuid);
        assert_eq!(back[0].properties["plan"], "pro");
    }

    #[test]
    fn no_tmp_files_left_visible() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store.write_events(&[event("a")]).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn compact_merges_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        for i in 0..3 {
            store.write_events(&[event(&format!("e{i}"))]).unwrap();
        }
        assert_eq!(store.list_files().unwrap().len(), 3);

        let merged = store.compact().unwrap();
        assert_eq!(merged, 3);
        let files = store.list_files().unwrap();
        assert_eq!(files.len(), 1);

        let all = parquet::read_file(&files[0]).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn compact_with_one_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store.write_events(&[event("a")]).unwrap();
        assert_eq!(store.compact().unwrap(), 0);
        assert_eq!(store.list_files().unwrap().len(), 1);
    }

    #[test]
    fn retention_deletes_only_fully_expired_files() {
        use chrono::{Duration, Utc};
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();

        let mut old = event("old");
        old.timestamp = Utc::now() - Duration::days(40);
        let mut recent = event("recent");
        recent.timestamp = Utc::now();
        store.write_events(&[old]).unwrap(); // file 1: all old
        store.write_events(&[recent]).unwrap(); // file 2: fresh

        let cutoff = Utc::now() - Duration::days(30);
        assert_eq!(store.enforce_retention(cutoff).unwrap(), 1);
        // Fresh file survives.
        let remaining: Vec<_> = store
            .list_files()
            .unwrap()
            .iter()
            .flat_map(|p| parquet::read_file(p).unwrap())
            .map(|e| e.event)
            .collect();
        assert_eq!(remaining, vec!["recent"]);
    }

    #[test]
    fn parquet_carries_schema_version() {
        // `parquet` here is our submodule; `::parquet` is the crate.
        use ::parquet::file::reader::{FileReader, SerializedFileReader};
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        let path = store.write_events(&[event("a")]).unwrap();
        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let kv = reader.metadata().file_metadata().key_value_metadata().unwrap();
        let ver = kv.iter().find(|k| k.key == "hoglet_schema_version").unwrap();
        assert_eq!(ver.value.as_deref(), Some(parquet::SCHEMA_VERSION));
    }
}

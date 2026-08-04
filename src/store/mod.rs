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

/// Map a project token to a single path segment.
///
/// The token arrives from an HTTP request and becomes a directory name, so it
/// cannot be trusted: `../../etc` must not escape the events directory. Safe
/// characters pass through so partitions stay readable; everything else is
/// percent-escaped, which is injective — two tokens can never collide on one
/// directory.
pub fn token_dir(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for c in token.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            for b in c.to_string().as_bytes() {
                out.push_str(&format!("%{b:02x}"));
            }
        }
    }
    if out.is_empty() { "_".into() } else { out }
}

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

    /// Write a batch, partitioned by (token, event date) — `{token}/{date}/`.
    ///
    /// A batch spanning two projects or two days produces one file per
    /// partition, so a query for one project on one day can be answered by
    /// listing one directory instead of scanning the whole store
    /// (`spec/scale.md` §1). Each file is still published atomically.
    ///
    /// Returns every file written, newest-batch-first order not guaranteed.
    pub fn write_events(&self, events: &[CapturedEvent]) -> std::io::Result<Vec<PathBuf>> {
        assert!(!events.is_empty(), "caller must not flush empty batches");

        // Group first so each output file holds exactly one partition.
        let mut parts: std::collections::BTreeMap<(String, String), Vec<CapturedEvent>> =
            std::collections::BTreeMap::new();
        for e in events {
            let key = (
                token_dir(&e.token),
                e.timestamp.format("%Y-%m-%d").to_string(),
            );
            parts.entry(key).or_default().push(e.clone());
        }

        let mut written = Vec::with_capacity(parts.len());
        for ((token, date), batch) in parts {
            let part_dir = self.dir.join(&token).join(&date);
            std::fs::create_dir_all(&part_dir)?;

            let n = self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Name by first event time + uuid: sortable, collision-free.
            let name = format!(
                "{}-{:06}-{}.parquet",
                batch[0].timestamp.format("%Y%m%dT%H%M%S"),
                n,
                uuid::Uuid::new_v4().simple()
            );
            let final_path = part_dir.join(&name);
            let tmp_path = part_dir.join(format!("{name}.tmp"));

            parquet::write_file(&batch, &tmp_path)?;
            std::fs::rename(&tmp_path, &final_path)?;
            std::fs::File::open(&part_dir)?.sync_all()?;
            written.push(final_path);
        }
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(written)
    }

    /// Every Parquet file in the store, partitioned or not.
    ///
    /// Stores written before partitioning have their files at the top level;
    /// they are still listed, so an upgrade needs no migration step and no
    /// query silently loses history.
    pub fn list_files(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        collect_parquet(&self.dir, &mut files)?;
        files.sort();
        Ok(files)
    }

    /// Files that could hold events for `token` between `from` and `to`
    /// (inclusive, UTC dates). This is the pruning the partition layout exists
    /// for: unrelated projects and out-of-range days are never opened.
    ///
    /// Legacy top-level files are always included — their partition is unknown,
    /// so excluding them could drop real rows. Correctness first; they stop
    /// appearing once compaction has rewritten them into partitions.
    pub fn files_for(
        &self,
        token: &str,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> std::io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        // Legacy flat files (pre-partitioning).
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "parquet") {
                    files.push(path);
                }
            }
        }

        let token_root = self.dir.join(token_dir(token));
        if let Ok(dates) = std::fs::read_dir(&token_root) {
            for entry in dates.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let in_range = match chrono::NaiveDate::parse_from_str(&name, "%Y-%m-%d") {
                    Ok(d) => from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t),
                    // An unparseable directory name is not proof of absence.
                    Err(_) => true,
                };
                if in_range {
                    collect_parquet(&path, &mut files)?;
                }
            }
        }

        files.sort();
        Ok(files)
    }

    /// Delete Parquet files whose newest event is older than `cutoff`
    /// (spec/README.md "Operational contract" retention). Whole-file only: a file
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

    /// Physically remove every event for `distinct_id` under `token`
    /// (spec/README.md "PII" / GDPR). Rewrites each Parquet file without the matching
    /// rows; deletes a file that becomes empty. Synchronous and complete — the
    /// person's events are gone when this returns. Returns rows removed.
    pub fn purge_distinct_id(&self, token: &str, distinct_id: &str) -> std::io::Result<usize> {
        let mut removed = 0;
        for path in self.list_files()? {
            let events = parquet::read_file(&path)?;
            let before = events.len();
            let kept: Vec<CapturedEvent> = events
                .into_iter()
                .filter(|e| !(e.token == token && e.distinct_id == distinct_id))
                .collect();
            if kept.len() == before {
                continue; // nothing to purge in this file
            }
            removed += before - kept.len();
            std::fs::remove_file(&path)?;
            if !kept.is_empty() {
                self.write_events(&kept)?;
            }
        }
        if removed > 0 {
            std::fs::File::open(&self.dir)?.sync_all()?;
        }
        Ok(removed)
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

/// Recursively gather `.parquet` files under `dir` (skipping `.tmp`).
fn collect_parquet(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_parquet(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "parquet") {
            out.push(path);
        }
    }
    Ok(())
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
        let paths = store.write_events(&events).unwrap();
        assert_eq!(paths.len(), 1, "one token, one day → one file");

        let back = parquet::read_file(&paths[0]).unwrap();
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
    fn purge_removes_only_that_person() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        let mut a = event("keep");
        a.distinct_id = "keep-me".into();
        let mut b = event("gdpr");
        b.distinct_id = "forget-me".into();
        store.write_events(&[a]).unwrap();
        store.write_events(&[b.clone(), b]).unwrap();

        let removed = store.purge_distinct_id("phc_t", "forget-me").unwrap();
        assert_eq!(removed, 2);
        let survivors: Vec<String> = store
            .list_files()
            .unwrap()
            .iter()
            .flat_map(|p| parquet::read_file(p).unwrap())
            .map(|e| e.distinct_id)
            .collect();
        assert!(survivors.iter().all(|d| d == "keep-me"));
        assert_eq!(survivors.len(), 1);
    }

    fn event_at(name: &str, token: &str, y: i32, m: u32, d: u32) -> CapturedEvent {
        CapturedEvent {
            token: token.into(),
            timestamp: Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap(),
            ..event(name)
        }
    }

    /// One file per (token, day): the unit a query can skip.
    #[test]
    fn batch_splits_into_one_file_per_partition() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        let paths = store
            .write_events(&[
                event_at("a", "phc_x", 2026, 7, 21),
                event_at("b", "phc_x", 2026, 7, 21),
                event_at("c", "phc_x", 2026, 7, 22),
                event_at("d", "phc_y", 2026, 7, 21),
            ])
            .unwrap();
        assert_eq!(paths.len(), 3, "2 days x 1 token + 1 day x 1 token");

        assert!(dir.path().join("phc_x/2026-07-21").is_dir());
        assert!(dir.path().join("phc_x/2026-07-22").is_dir());
        assert!(dir.path().join("phc_y/2026-07-21").is_dir());
        assert_eq!(store.list_files().unwrap().len(), 3);
    }

    /// The token becomes a directory name and arrives over HTTP, so it must not
    /// be able to walk out of the events directory.
    #[test]
    fn hostile_tokens_stay_inside_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        for token in ["../../etc", "..", "a/b", "", "with space"] {
            let paths = store.write_events(&[event_at("x", token, 2026, 7, 21)]).unwrap();
            for path in paths {
                assert!(
                    path.canonicalize().unwrap().starts_with(dir.path().canonicalize().unwrap()),
                    "{token:?} escaped the store: {path:?}"
                );
            }
        }
        // Distinct tokens never share a partition, however they are spelled.
        assert_ne!(token_dir("a/b"), token_dir("a_b"));
    }

    /// Pruning is the whole point of the layout: another project's files and
    /// out-of-range days must not be handed to DuckDB.
    #[test]
    fn files_for_prunes_by_token_and_date() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store.write_events(&[
            event_at("a", "phc_x", 2026, 7, 20),
            event_at("b", "phc_x", 2026, 7, 21),
            event_at("c", "phc_x", 2026, 7, 22),
            event_at("d", "phc_y", 2026, 7, 21),
        ]).unwrap();

        let d = |y, m, day| chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap();

        assert_eq!(store.files_for("phc_x", None, None).unwrap().len(), 3, "other project leaked in");
        assert_eq!(store.files_for("phc_y", None, None).unwrap().len(), 1);
        assert_eq!(
            store.files_for("phc_x", Some(d(2026, 7, 21)), Some(d(2026, 7, 21))).unwrap().len(),
            1,
            "date window not pruned"
        );
        assert_eq!(
            store.files_for("phc_x", Some(d(2026, 7, 21)), None).unwrap().len(),
            2,
            "open-ended window wrong"
        );
        assert_eq!(store.files_for("phc_unknown", None, None).unwrap().len(), 0);
    }

    /// A store written before partitioning keeps its files at the top level.
    /// Those rows must stay visible — an upgrade cannot lose history, and a
    /// pruned query cannot exclude a file whose partition is unknown.
    #[test]
    fn legacy_flat_files_are_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();

        // Simulate the old layout by hand.
        let legacy = dir.path().join("20260721T120000-000000-legacy.parquet");
        parquet::write_file(&[event_at("old", "phc_x", 2026, 7, 21)], &legacy).unwrap();

        store.write_events(&[event_at("new", "phc_x", 2026, 7, 22)]).unwrap();

        assert_eq!(store.list_files().unwrap().len(), 2, "legacy file not listed");
        assert!(
            store.files_for("phc_x", None, None).unwrap().contains(&legacy),
            "legacy file pruned away"
        );
        // Even a window that excludes its date keeps it: its partition is unknown.
        let d = chrono::NaiveDate::from_ymd_opt(2026, 7, 22).unwrap();
        assert!(store.files_for("phc_x", Some(d), None).unwrap().contains(&legacy));
    }

    #[test]
    fn parquet_carries_schema_version() {
        // `parquet` here is our submodule; `::parquet` is the crate.
        use ::parquet::file::reader::{FileReader, SerializedFileReader};
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        let paths = store.write_events(&[event("a")]).unwrap();
        let reader = SerializedFileReader::new(std::fs::File::open(&paths[0]).unwrap()).unwrap();
        let kv = reader.metadata().file_metadata().key_value_metadata().unwrap();
        let ver = kv.iter().find(|k| k.key == "hoglet_schema_version").unwrap();
        assert_eq!(ver.value.as_deref(), Some(parquet::SCHEMA_VERSION));
    }
}

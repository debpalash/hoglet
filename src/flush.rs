//! Flusher — moves sealed WAL segments into the Parquet store.
//!
//! Order is the invariant: Parquet file durable *first*, WAL segment deleted
//! *second*. A crash in between replays the segment and produces duplicates
//! (deduped by uuid at query time) — never loss.

use std::sync::Arc;
use std::time::Duration;

use crate::store::EventStore;
use crate::wal::{self, Wal};

pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Run retention at most this often, independent of the faster flush tick.
const RETENTION_EVERY: Duration = Duration::from_secs(3600);

/// Spawn the background flush loop. Runs until the WAL closes. When
/// `retention_days` is set, fully-expired Parquet files are dropped hourly
/// (spec/README.md "Operational contract").
pub fn spawn(
    wal: Wal,
    store: Arc<EventStore>,
    retention_days: Option<i64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut retention = tokio::time::interval(RETENTION_EVERY);
        retention.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => match flush_once(&wal, &store).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(events = n, "flushed WAL to Parquet"),
                    Err(FlushError::WalClosed) => return,
                    Err(FlushError::Io(e)) => {
                        // Leave segments in place; next tick retries. Nothing
                        // is acked-but-lost — the WAL still holds it all.
                        tracing::error!("flush failed, will retry: {e}");
                    }
                },
                _ = retention.tick() => {
                    if let Some(days) = retention_days {
                        let store = store.clone();
                        let dropped = tokio::task::spawn_blocking(move || {
                            let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
                            store.enforce_retention(cutoff)
                        }).await;
                        match dropped {
                            Ok(Ok(n)) if n > 0 => tracing::info!(files = n, "retention dropped expired segments"),
                            Ok(Err(e)) => tracing::error!("retention failed: {e}"),
                            _ => {}
                        }
                    }
                }
            }
        }
    })
}

#[derive(Debug)]
pub enum FlushError {
    WalClosed,
    Io(std::io::Error),
}

/// One flush cycle: seal, move each sealed segment to Parquet, delete it,
/// then compact. Returns events flushed.
pub async fn flush_once(wal: &Wal, store: &Arc<EventStore>) -> Result<usize, FlushError> {
    let sealed = wal.seal().await.map_err(|_| FlushError::WalClosed)?;
    if sealed.is_empty() {
        return Ok(0);
    }
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut flushed = 0usize;
        for path in sealed {
            let events = wal::read_segment_events(&path)?;
            if !events.is_empty() {
                store.write_events(&events)?;
                flushed += events.len();
            }
            // Parquet is durable; the segment may now disappear.
            std::fs::remove_file(&path)?;
        }
        store.compact()?;
        Ok(flushed)
    })
    .await
    .expect("flush task panicked")
    .map_err(FlushError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::parquet;
    use crate::wal::Wal;
    use chrono::{TimeZone, Utc};
    use serde_json::Map;
    use uuid::Uuid;

    fn event(name: &str) -> crate::capture::event::CapturedEvent {
        crate::capture::event::CapturedEvent {
            uuid: Uuid::new_v4(),
            event: name.into(),
            distinct_id: "u1".into(),
            token: "phc_t".into(),
            timestamp: Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap(),
            properties: Map::new(),
        }
    }

    #[tokio::test]
    async fn wal_events_end_up_in_parquet_and_wal_empties() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        let store = Arc::new(EventStore::open(dir.path().join("events")).unwrap());

        let (wal, runtime, _) = Wal::open(wal_dir.clone()).unwrap();
        wal.append(vec![event("a"), event("b")]).await.unwrap();
        wal.append(vec![event("c")]).await.unwrap();

        let flushed = flush_once(&wal, &store).await.unwrap();
        assert_eq!(flushed, 3);

        // Parquet holds all three.
        let files = store.list_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(parquet::read_file(&files[0]).unwrap().len(), 3);

        // A reopen recovers nothing — flushed segments are gone.
        drop(wal);
        runtime.close();
        let (w2, r2, recovered) = Wal::open(wal_dir).unwrap();
        assert!(recovered.events.is_empty());
        drop(w2);
        r2.close();
    }

    #[tokio::test]
    async fn flush_with_nothing_pending_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(EventStore::open(dir.path().join("events")).unwrap());
        let (wal, runtime, _) = Wal::open(dir.path().join("wal")).unwrap();
        assert_eq!(flush_once(&wal, &store).await.unwrap(), 0);
        assert!(store.list_files().unwrap().is_empty());
        drop(wal);
        runtime.close();
    }

    #[tokio::test]
    async fn unflushed_events_from_previous_run_get_flushed() {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        let store = Arc::new(EventStore::open(dir.path().join("events")).unwrap());

        // First run: append, crash without flushing.
        let (w1, r1, _) = Wal::open(wal_dir.clone()).unwrap();
        w1.append(vec![event("survivor")]).await.unwrap();
        drop(w1);
        r1.close();

        // Second run: flusher picks up the sealed segment.
        let (w2, r2, recovered) = Wal::open(wal_dir).unwrap();
        assert_eq!(recovered.events.len(), 1);
        let flushed = flush_once(&w2, &store).await.unwrap();
        assert_eq!(flushed, 1);
        let files = store.list_files().unwrap();
        let events = parquet::read_file(&files[0]).unwrap();
        assert_eq!(events[0].event, "survivor");
        drop(w2);
        r2.close();
    }
}

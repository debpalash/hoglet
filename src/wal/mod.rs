//! Write-ahead log — the durability boundary (claims.md claim 2).
//!
//! Contract: an event batch is acknowledged only after its record is written
//! *and fsynced* to the active segment. The writer is a single dedicated
//! thread doing group commit: it drains whatever is queued, writes one fsync's
//! worth, syncs, then acks every batch in the group. Callers reach it through
//! a bounded channel — backpressure surfaces as `SinkFull` → 503 → SDK retry,
//! never an unbounded queue (CLAUDE.md engineering standard).

pub mod segment;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};

use crate::capture::event::CapturedEvent;
use crate::sink::{EventSink, SinkError};

/// Maximum queued-but-not-yet-durable batches. Beyond this we shed load with
/// 503 rather than queue without bound.
pub const MAX_QUEUED_BATCHES: usize = 1024;

/// Maximum batches folded into one fsync (group commit ceiling).
const MAX_GROUP_BATCHES: usize = 256;

enum WalRequest {
    Append {
        events: Vec<CapturedEvent>,
        ack: oneshot::Sender<Result<(), SinkError>>,
    },
    /// Roll the active segment (if non-empty) and report every sealed
    /// segment — segments the writer will never touch again, safe to flush
    /// to Parquet and delete.
    Seal {
        ack: oneshot::Sender<Vec<std::path::PathBuf>>,
    },
}

/// Handle for appending; cheap to clone.
#[derive(Clone)]
pub struct Wal {
    tx: mpsc::Sender<WalRequest>,
}

/// Owns the writer thread. `close` joins it — the writer exits when the last
/// `Wal` clone (sender) is dropped, so drop all handles before closing.
pub struct WalRuntime {
    writer: Option<JoinHandle<()>>,
}

/// Events recovered from disk at startup, in write order.
pub struct Recovered {
    pub events: Vec<CapturedEvent>,
    /// True if any segment had a torn/corrupt tail that was truncated away.
    pub truncated: bool,
}

impl Wal {
    /// Open the WAL in `dir`: recover existing segments (truncating any
    /// invalid tail), then start the writer thread on the next segment.
    ///
    /// Returns the runtime and everything recovered — the caller decides what
    /// to do with replayed events (task 4: flush to Parquet).
    pub fn open(dir: PathBuf) -> std::io::Result<(Wal, WalRuntime, Recovered)> {
        segment::create_dir_durable(&dir)?;

        let mut events = Vec::new();
        let mut truncated = false;
        let mut last_seq = 0u64;
        for (seq, path) in segment::list_segments(&dir)? {
            let scanned = segment::scan_and_truncate(&path)?;
            truncated |= scanned.truncated;
            events.extend(decode_records(scanned.records)?);
            last_seq = seq;
        }

        let (tx, rx) = mpsc::channel(MAX_QUEUED_BATCHES);
        let start_seq = last_seq + 1;
        let writer = std::thread::Builder::new()
            .name("hoglet-wal".into())
            .spawn(move || writer_loop(dir, start_seq, rx))?;

        Ok((
            Wal { tx },
            WalRuntime {
                writer: Some(writer),
            },
            Recovered { events, truncated },
        ))
    }

    /// Append a batch; resolves once the batch is durable on disk.
    pub async fn append(&self, events: Vec<CapturedEvent>) -> Result<(), SinkError> {
        let (ack, ack_rx) = oneshot::channel();
        self.tx
            .try_send(WalRequest::Append { events, ack })
            .map_err(|_| SinkError::Retryable)?;
        ack_rx.await.map_err(|_| SinkError::Retryable)?
    }

    /// Seal the active segment and return all sealed segment paths, oldest
    /// first. The writer never appends to a sealed segment again.
    pub async fn seal(&self) -> Result<Vec<std::path::PathBuf>, SinkError> {
        let (ack, ack_rx) = oneshot::channel();
        self.tx
            .send(WalRequest::Seal { ack })
            .await
            .map_err(|_| SinkError::Retryable)?;
        ack_rx.await.map_err(|_| SinkError::Retryable)
    }
}

impl WalRuntime {
    /// Join the writer thread. It exits once every `Wal` handle is dropped;
    /// queued requests are drained first.
    pub fn close(mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn writer_loop(dir: PathBuf, mut seq: u64, mut rx: mpsc::Receiver<WalRequest>) {
    let mut current = match open_segment(&dir, seq) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!("wal: cannot open segment: {e}");
            fail_all(&mut rx);
            return;
        }
    };

    let mut group: Vec<WalRequest> = Vec::with_capacity(MAX_GROUP_BATCHES);
    let mut buf: Vec<u8> = Vec::new();

    while let Some(first) = rx.blocking_recv() {
        let mut pending_seal: Option<oneshot::Sender<Vec<PathBuf>>> = None;

        match first {
            WalRequest::Seal { ack } => {
                seal(&dir, &mut seq, &mut current, ack);
                continue;
            }
            append => group.push(append),
        }
        while group.len() < MAX_GROUP_BATCHES {
            match rx.try_recv() {
                Ok(WalRequest::Seal { ack }) => {
                    // Handle after this group commits so its batches land in
                    // the segment being sealed.
                    pending_seal = Some(ack);
                    break;
                }
                Ok(append) => group.push(append),
                Err(_) => break,
            }
        }

        buf.clear();
        let mut encodable: Vec<bool> = Vec::with_capacity(group.len());
        for req in &group {
            let WalRequest::Append { events, .. } = req else {
                unreachable!("group holds only appends");
            };
            match serde_json::to_vec(events) {
                Ok(payload) if payload.len() <= segment::MAX_RECORD_BYTES => {
                    segment::encode_record(&payload, &mut buf);
                    encodable.push(true);
                }
                _ => encodable.push(false),
            }
        }

        let result = segment::append_durable(&mut current.0, &buf);
        match &result {
            Ok(()) => current.1 += buf.len() as u64,
            Err(e) => tracing::error!("wal: write failed: {e}"),
        }

        for (req, ok) in group.drain(..).zip(encodable) {
            let WalRequest::Append { ack, .. } = req else {
                unreachable!("group holds only appends");
            };
            let outcome = match (&result, ok) {
                (Ok(()), true) => Ok(()),
                // Unencodable batch: our bug, not retryable by the client.
                (Ok(()), false) => Err(SinkError::Fatal),
                (Err(_), _) => Err(SinkError::Retryable),
            };
            let _ = ack.send(outcome);
        }

        // On write failure, reopen a fresh segment: never keep appending
        // after an unknown partial write.
        if result.is_err() || current.1 >= segment::MAX_SEGMENT_BYTES {
            seq += 1;
            match open_segment(&dir, seq) {
                Ok(pair) => current = pair,
                Err(e) => {
                    tracing::error!("wal: cannot roll segment: {e}");
                    fail_all(&mut rx);
                    return;
                }
            }
        }

        if let Some(ack) = pending_seal.take() {
            seal(&dir, &mut seq, &mut current, ack);
        }
    }
}

/// Roll the active segment if it has data, then report every segment with
/// seq < active — those are sealed and safe to flush + delete.
fn seal(
    dir: &std::path::Path,
    seq: &mut u64,
    current: &mut (std::fs::File, u64),
    ack: oneshot::Sender<Vec<PathBuf>>,
) {
    if current.1 > 0 {
        match open_segment(dir, *seq + 1) {
            Ok(pair) => {
                *seq += 1;
                *current = pair;
            }
            Err(e) => {
                // Keep writing to the old segment; report only what's
                // already sealed.
                tracing::error!("wal: cannot roll segment on seal: {e}");
            }
        }
    }
    let sealed = match segment::list_segments(dir) {
        Ok(all) => all
            .into_iter()
            .filter(|(s, _)| *s < *seq)
            .map(|(_, p)| p)
            .collect(),
        Err(e) => {
            tracing::error!("wal: cannot list segments: {e}");
            Vec::new()
        }
    };
    let _ = ack.send(sealed);
}

fn open_segment(dir: &std::path::Path, seq: u64) -> std::io::Result<(std::fs::File, u64)> {
    let path = segment::segment_path(dir, seq);
    let file = segment::open_for_append(&path)?;
    // Make the directory entry durable so the segment survives crash.
    std::fs::File::open(dir)?.sync_all()?;
    Ok(file)
}

fn fail_all(rx: &mut mpsc::Receiver<WalRequest>) {
    while let Ok(req) = rx.try_recv() {
        match req {
            WalRequest::Append { ack, .. } => {
                let _ = ack.send(Err(SinkError::Retryable));
            }
            WalRequest::Seal { ack } => {
                let _ = ack.send(Vec::new());
            }
        }
    }
}

fn decode_records(records: Vec<Vec<u8>>) -> std::io::Result<Vec<CapturedEvent>> {
    let mut events = Vec::new();
    for record in records {
        let batch: Vec<CapturedEvent> = serde_json::from_slice(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        events.extend(batch);
    }
    Ok(events)
}

/// Read every event in a sealed segment (flusher path). Truncates a torn
/// tail exactly like startup recovery does.
pub fn read_segment_events(path: &std::path::Path) -> std::io::Result<Vec<CapturedEvent>> {
    let scanned = segment::scan_and_truncate(path)?;
    decode_records(scanned.records)
}

/// The WAL as the capture pipeline's sink.
pub struct WalSink(pub Wal);

#[async_trait::async_trait]
impl EventSink for WalSink {
    async fn append(&self, events: Vec<CapturedEvent>) -> Result<(), SinkError> {
        self.0.append(events).await
    }
}

/// Convenience: open in `dir` and return an Arc'd sink plus runtime +
/// recovery info.
pub fn open_sink(dir: PathBuf) -> std::io::Result<(Arc<dyn EventSink>, WalRuntime, Recovered)> {
    let (wal, runtime, recovered) = Wal::open(dir)?;
    let sink = Arc::new(WalSink(wal));
    Ok((sink, runtime, recovered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use serde_json::Map;
    use uuid::Uuid;

    fn event(name: &str) -> CapturedEvent {
        CapturedEvent {
            uuid: Uuid::new_v4(),
            event: name.into(),
            distinct_id: "u1".into(),
            token: "phc_t".into(),
            timestamp: Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap(),
            properties: Map::new(),
        }
    }

    fn close(wal: Wal, runtime: WalRuntime) {
        drop(wal);
        runtime.close();
    }

    #[tokio::test]
    async fn ack_then_recover_after_close() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, runtime, recovered) = Wal::open(dir.path().to_path_buf()).unwrap();
        assert!(recovered.events.is_empty());

        wal.append(vec![event("a"), event("b")]).await.unwrap();
        wal.append(vec![event("c")]).await.unwrap();
        close(wal, runtime);

        // Reopen: every acked event must come back.
        let (wal2, runtime2, recovered) = Wal::open(dir.path().to_path_buf()).unwrap();
        let names: Vec<&str> = recovered.events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert!(!recovered.truncated);
        close(wal2, runtime2);
    }

    #[tokio::test]
    async fn torn_tail_on_disk_recovers_acked_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, runtime, _) = Wal::open(dir.path().to_path_buf()).unwrap();
        wal.append(vec![event("a")]).await.unwrap();
        wal.append(vec![event("b")]).await.unwrap();
        close(wal, runtime);

        // Simulate a crash mid-write: append garbage half-record to the
        // active segment.
        let segments = segment::list_segments(dir.path()).unwrap();
        let (_, last) = segments.last().unwrap();
        let mut bytes = std::fs::read(last).unwrap();
        bytes.extend_from_slice(&[42u8; 5]);
        std::fs::write(last, &bytes).unwrap();

        let (wal2, runtime2, recovered) = Wal::open(dir.path().to_path_buf()).unwrap();
        let names: Vec<&str> = recovered.events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(recovered.truncated);
        close(wal2, runtime2);
    }

    #[tokio::test]
    async fn new_segment_after_reopen_continues_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let (w1, r1, _) = Wal::open(dir.path().to_path_buf()).unwrap();
        w1.append(vec![event("a")]).await.unwrap();
        close(w1, r1);
        let (w2, r2, _) = Wal::open(dir.path().to_path_buf()).unwrap();
        w2.append(vec![event("b")]).await.unwrap();
        close(w2, r2);

        let segments = segment::list_segments(dir.path()).unwrap();
        assert!(segments.len() >= 2, "each open starts a fresh segment");
        let (w3, r3, recovered) = Wal::open(dir.path().to_path_buf()).unwrap();
        let names: Vec<&str> = recovered.events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        close(w3, r3);
    }

    #[tokio::test]
    async fn concurrent_appends_all_durable() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, runtime, _) = Wal::open(dir.path().to_path_buf()).unwrap();

        let mut handles = Vec::new();
        for i in 0..50 {
            let wal = wal.clone();
            handles.push(tokio::spawn(async move {
                wal.append(vec![event(&format!("e{i}"))]).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        close(wal, runtime);

        let (w2, r2, recovered) = Wal::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(recovered.events.len(), 50);
        close(w2, r2);
    }
}

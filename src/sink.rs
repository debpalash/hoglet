//! Event sink — where accepted events go after the wire edge.
//!
//! The production implementation is [`DurableWalSink`]: `append`
//! returning `Ok` is the durability promise that justifies a 2xx to the
//! client, so no implementation may ack before its write is durable.
//! [`MemorySink`] exists for tests and [`LogSink`] for running without
//! persistence.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc as sync_mpsc};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::capture::event::CapturedEvent;
use crate::pipeline::publication::{PublicationCoordinator, PublicationError};
use crate::pipeline::wal::{CapturedBatch, Recovery, WalConfig, WalError, WriteAheadLog};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// Transient failure → 503, which posthog-js retries.
    Retryable,
    /// The batch itself can never be stored → 400, never retried.
    Fatal,
}

/// A parsed batch together with the durable authorization decisions made at
/// the capture seam. Production sinks must preserve these project bindings.
#[derive(Debug, Clone)]
pub struct AuthorizedEventBatch {
    pub events: Vec<CapturedEvent>,
    pub project_ids_by_token: BTreeMap<String, String>,
    pub historical_migration: bool,
}

#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    async fn append(&self, batch: AuthorizedEventBatch) -> Result<(), SinkError>;
}

/// Explicit bound: no unbounded growth in the ingest path, even in the stub.
pub const MEMORY_SINK_MAX_EVENTS: usize = 1_000_000;

#[derive(Default)]
pub struct MemorySink {
    events: Mutex<Vec<CapturedEvent>>,
}

impl MemorySink {
    pub fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EventSink for MemorySink {
    async fn append(&self, mut batch: AuthorizedEventBatch) -> Result<(), SinkError> {
        let mut events = self.events.lock().unwrap();
        if events.len() + batch.events.len() > MEMORY_SINK_MAX_EVENTS {
            return Err(SinkError::Retryable);
        }
        events.append(&mut batch.events);
        Ok(())
    }
}

/// Logs and drops. Only for running without persistence.
pub struct LogSink;

#[async_trait::async_trait]
impl EventSink for LogSink {
    async fn append(&self, batch: AuthorizedEventBatch) -> Result<(), SinkError> {
        for e in &batch.events {
            tracing::info!(event = %e.event, distinct_id = %e.distinct_id, "event (log sink, not persisted)");
        }
        Ok(())
    }
}

const DURABLE_QUEUE_BATCHES: usize = 64;
const DURABLE_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_UNRECLAIMED_WAL_BYTES: u64 = 1024 * 1024 * 1024;
const GROUP_COMMIT_BYTES: u64 = 8 * 1024 * 1024;
const GROUP_COMMIT_LATENCY: Duration = Duration::from_millis(2);
const PUBLICATION_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_PUBLICATION_WAL_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum DurablePipelineError {
    Wal(WalError),
    Publication(PublicationError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    MissingProjectBinding {
        event: uuid::Uuid,
    },
    WorkerPanicked,
}

impl fmt::Display for DurablePipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wal(error) => error.fmt(formatter),
            Self::Publication(error) => error.fmt(formatter),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::MissingProjectBinding { event } => {
                write!(formatter, "event {event} has no authorized project binding")
            }
            Self::WorkerPanicked => formatter.write_str("durable pipeline worker panicked"),
        }
    }
}

impl std::error::Error for DurablePipelineError {}

impl From<WalError> for DurablePipelineError {
    fn from(error: WalError) -> Self {
        Self::Wal(error)
    }
}

impl From<PublicationError> for DurablePipelineError {
    fn from(error: PublicationError) -> Self {
        Self::Publication(error)
    }
}

enum DurableRequest {
    Append {
        batch: CapturedBatch,
        encoded_bytes: u64,
        _queue_bytes: OwnedSemaphorePermit,
        ack: oneshot::Sender<Result<(), SinkError>>,
    },
    Shutdown {
        ack: oneshot::Sender<Result<(), DurablePipelineError>>,
    },
}

/// The production capture adapter. A bounded queue feeds one blocking WAL
/// writer; an append is acknowledged only after the v2 WAL record is fsynced.
pub struct DurableWalSink {
    sender: sync_mpsc::SyncSender<DurableRequest>,
    queue_bytes: Arc<Semaphore>,
}

pub struct DurableWalRuntime {
    sender: sync_mpsc::SyncSender<DurableRequest>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DurableWalSink {
    pub fn open(
        directory: impl AsRef<Path>,
        coordinator: PublicationCoordinator,
    ) -> Result<(Arc<Self>, DurableWalRuntime, Recovery), DurablePipelineError> {
        let directory = directory.as_ref().to_path_buf();
        let (mut wal, recovery) = WriteAheadLog::open(&directory, WalConfig::default())?;
        let publisher = PipelinePublisher { coordinator };
        // Recovery is part of startup, not a detached best-effort task. Any
        // sealed or recovered active prefix is queryable before readiness.
        publisher.publish_all(&mut wal)?;
        let (sender, receiver) = sync_mpsc::sync_channel(DURABLE_QUEUE_BATCHES);
        let worker = std::thread::Builder::new()
            .name("hoglet-v2-wal".to_owned())
            .spawn(move || durable_writer_loop(wal, directory, publisher, receiver))
            .map_err(|source| DurablePipelineError::Io {
                path: PathBuf::from("<hoglet-v2-wal-thread>"),
                source,
            })?;
        let queue_bytes = Arc::new(Semaphore::new(DURABLE_QUEUE_BYTES));
        Ok((
            Arc::new(Self {
                sender: sender.clone(),
                queue_bytes,
            }),
            DurableWalRuntime {
                sender,
                worker: Some(worker),
            },
            recovery,
        ))
    }
}

#[async_trait::async_trait]
impl EventSink for DurableWalSink {
    async fn append(&self, batch: AuthorizedEventBatch) -> Result<(), SinkError> {
        let batch = CapturedBatch::authorized(
            batch.events,
            batch.project_ids_by_token,
            batch.historical_migration,
        )
        .map_err(|_| SinkError::Fatal)?;
        let encoded_bytes = serde_json::to_vec(&batch)
            .map_err(|_| SinkError::Fatal)?
            .len();
        let permits = u32::try_from(encoded_bytes).map_err(|_| SinkError::Fatal)?;
        let queue_bytes = self
            .queue_bytes
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(|_| SinkError::Retryable)?;
        let (ack, receive) = oneshot::channel();
        self.sender
            .try_send(DurableRequest::Append {
                batch,
                encoded_bytes: encoded_bytes as u64,
                _queue_bytes: queue_bytes,
                ack,
            })
            .map_err(|_| SinkError::Retryable)?;
        receive.await.map_err(|_| SinkError::Retryable)?
    }
}

impl DurableWalRuntime {
    pub async fn shutdown(mut self) -> Result<(), DurablePipelineError> {
        let (ack, receive) = oneshot::channel();
        let sender = self.sender.clone();
        let sent =
            tokio::task::spawn_blocking(move || sender.send(DurableRequest::Shutdown { ack }))
                .await;
        let result = if matches!(sent, Ok(Ok(()))) {
            receive
                .await
                .unwrap_or(Err(DurablePipelineError::WorkerPanicked))
        } else {
            Err(DurablePipelineError::WorkerPanicked)
        };
        if let Some(worker) = self.worker.take() {
            if !matches!(
                tokio::task::spawn_blocking(move || worker.join()).await,
                Ok(Ok(()))
            ) {
                return Err(DurablePipelineError::WorkerPanicked);
            }
        }
        result
    }
}

fn durable_writer_loop(
    mut wal: WriteAheadLog,
    directory: PathBuf,
    publisher: PipelinePublisher,
    receiver: sync_mpsc::Receiver<DurableRequest>,
) {
    let mut unpublished_bytes = 0_u64;
    let mut publish_deadline: Option<Instant> = None;
    let mut retrying_publication = false;
    loop {
        let request = match publish_deadline {
            Some(deadline) => {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => receiver
                .recv()
                .map_err(|_| sync_mpsc::RecvTimeoutError::Disconnected),
        };
        match request {
            Ok(DurableRequest::Append {
                batch,
                encoded_bytes,
                _queue_bytes,
                ack,
            }) => {
                let result = wal_directory_bytes(&directory)
                    .and_then(|bytes| {
                        if bytes.saturating_add(encoded_bytes) > MAX_UNRECLAIMED_WAL_BYTES {
                            Err(std::io::Error::other("unreclaimed WAL limit reached"))
                        } else {
                            Ok(())
                        }
                    })
                    .map_err(|_| SinkError::Retryable)
                    .and_then(|()| wal.append(batch).map(|_| ()).map_err(map_wal_error));
                drop(_queue_bytes);
                let appended = result.is_ok();
                let _ = ack.send(result);
                if appended {
                    unpublished_bytes = unpublished_bytes.saturating_add(encoded_bytes);
                    publish_deadline.get_or_insert_with(|| Instant::now() + GROUP_COMMIT_LATENCY);
                }
                let publication_due =
                    publish_deadline.is_some_and(|deadline| Instant::now() >= deadline);
                if (!retrying_publication && unpublished_bytes >= GROUP_COMMIT_BYTES)
                    || publication_due
                {
                    if publish_or_log(&publisher, &mut wal) {
                        unpublished_bytes = 0;
                        publish_deadline = None;
                        retrying_publication = false;
                    } else {
                        unpublished_bytes = 0;
                        publish_deadline = Some(Instant::now() + PUBLICATION_RETRY_DELAY);
                        retrying_publication = true;
                    }
                }
            }
            Ok(DurableRequest::Shutdown { ack }) => {
                let result = publisher.publish_all(&mut wal);
                let _ = ack.send(result);
                break;
            }
            Err(sync_mpsc::RecvTimeoutError::Timeout) => {
                if publish_or_log(&publisher, &mut wal) {
                    unpublished_bytes = 0;
                    publish_deadline = None;
                    retrying_publication = false;
                } else {
                    unpublished_bytes = 0;
                    publish_deadline = Some(Instant::now() + PUBLICATION_RETRY_DELAY);
                    retrying_publication = true;
                }
            }
            Err(sync_mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(error) = publisher.publish_all(&mut wal) {
                    tracing::error!(%error, "durable pipeline shutdown publication failed");
                }
                break;
            }
        }
    }
}

struct PipelinePublisher {
    coordinator: PublicationCoordinator,
}

impl PipelinePublisher {
    fn publish_all(&self, wal: &mut WriteAheadLog) -> Result<(), DurablePipelineError> {
        loop {
            if self.publish_one(wal)? == 0 {
                return Ok(());
            }
        }
    }

    fn publish_one(&self, wal: &mut WriteAheadLog) -> Result<usize, DurablePipelineError> {
        wal.seal()?;
        // Retry any cleanup that failed after an earlier committed generation
        // before deciding whether new records are pending.
        wal.reclaim_through(self.coordinator.checkpoint()?)?;
        match self
            .coordinator
            .publish_pending(wal, MAX_PUBLICATION_WAL_BYTES)
        {
            Ok(Some(result)) => {
                tracing::debug!(
                    generation_id = result.generation_id.get(),
                    events = result.applied_events,
                    duplicates = result.duplicate_events,
                    "published v2 event generation"
                );
                Ok(result.published_records)
            }
            Ok(None) => Ok(0),
            Err(PublicationError::ReclamationAfterCommit {
                generation_id,
                checkpoint,
                source,
            }) => {
                // The generation and checkpoint are already authoritative.
                // Retaining extra WAL is safe and the next pass can reclaim it.
                tracing::warn!(
                    generation_id = generation_id.get(),
                    segment = checkpoint.segment,
                    byte_offset = checkpoint.byte_offset,
                    %source,
                    "generation committed but WAL reclamation will be retried"
                );
                wal.reclaim_through(checkpoint)?;
                Ok(1)
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn publish_or_log(publisher: &PipelinePublisher, wal: &mut WriteAheadLog) -> bool {
    match publisher.publish_all(wal) {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(%error, "event generation publication failed; durable WAL will be retried");
            false
        }
    }
}

fn wal_directory_bytes(directory: &Path) -> std::io::Result<u64> {
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(bytes)
}

fn map_wal_error(error: WalError) -> SinkError {
    match error {
        WalError::EmptyBatch
        | WalError::InvalidProjectBinding
        | WalError::RecordTooLarge { .. } => SinkError::Fatal,
        _ => SinkError::Retryable,
    }
}

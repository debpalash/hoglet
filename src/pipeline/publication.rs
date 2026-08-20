//! Atomic publication of one ordered WAL prefix.
//!
//! The coordinator is the transactional seam between Event Truth and every
//! rebuildable analytical view. A successful call makes exactly one WAL prefix,
//! one projection state, and one immutable file manifest visible together.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::event_lake::{EventLakeError, GenerationId, PublishedFile, VersionedEventLake};
use crate::pipeline::wal::{WalCursor, WalError, WalRecord, WriteAheadLog};
use crate::projections::{ApplyOutcome, ProjectionError, apply_captured_event, initialize_schema};

/// Observable result of one committed publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationResult {
    pub generation_id: GenerationId,
    pub checkpoint: WalCursor,
    pub data_epoch: u64,
    pub applied_events: usize,
    pub duplicate_events: usize,
    pub published_records: usize,
    pub reclaimed_segments: Vec<u64>,
}

#[derive(Debug)]
pub enum PublicationError {
    EventLake(EventLakeError),
    Projection(ProjectionError),
    Wal(WalError),
    Database(rusqlite::Error),
    InvalidCheckpoint {
        segment: i64,
        byte_offset: i64,
    },
    InvalidDataEpoch(i64),
    DataEpochExhausted,
    NoSealedRecords,
    MissingProjectBinding(Uuid),
    ConcurrentStateChange,
    /// SQLite is already committed and the lease is refreshed. WAL remains
    /// intact because reclamation could not be proven durable.
    ReclamationAfterCommit {
        generation_id: GenerationId,
        checkpoint: WalCursor,
        source: WalError,
    },
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLake(error) => error.fmt(formatter),
            Self::Projection(error) => error.fmt(formatter),
            Self::Wal(error) => error.fmt(formatter),
            Self::Database(error) => write!(formatter, "projection database error: {error}"),
            Self::InvalidCheckpoint {
                segment,
                byte_offset,
            } => write!(
                formatter,
                "invalid committed WAL checkpoint {segment}:{byte_offset}"
            ),
            Self::InvalidDataEpoch(epoch) => {
                write!(formatter, "invalid committed data epoch {epoch}")
            }
            Self::DataEpochExhausted => formatter.write_str("data epoch exhausted"),
            Self::NoSealedRecords => formatter.write_str("no sealed WAL records to publish"),
            Self::MissingProjectBinding(uuid) => write!(
                formatter,
                "captured event {uuid} has no durable authorized-project binding"
            ),
            Self::ConcurrentStateChange => {
                formatter.write_str("projection state changed during publication")
            }
            Self::ReclamationAfterCommit {
                generation_id,
                checkpoint,
                source,
            } => write!(
                formatter,
                "generation {generation_id} committed through {}:{} but WAL reclamation failed: {source}",
                checkpoint.segment, checkpoint.byte_offset
            ),
        }
    }
}

impl std::error::Error for PublicationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EventLake(error) => Some(error),
            Self::Projection(error) => Some(error),
            Self::Wal(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::ReclamationAfterCommit { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<EventLakeError> for PublicationError {
    fn from(error: EventLakeError) -> Self {
        Self::EventLake(error)
    }
}

impl From<ProjectionError> for PublicationError {
    fn from(error: ProjectionError) -> Self {
        Self::Projection(error)
    }
}

impl From<WalError> for PublicationError {
    fn from(error: WalError) -> Self {
        Self::Wal(error)
    }
}

impl From<rusqlite::Error> for PublicationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Clone, Copy)]
struct ProjectionState {
    generation: i64,
    checkpoint: WalCursor,
    data_epoch: i64,
}

/// Owns the single SQLite transaction that joins ordered projections,
/// generation metadata, the exact WAL checkpoint, and the cache data epoch.
pub struct PublicationCoordinator {
    event_lake: Arc<VersionedEventLake>,
}

impl PublicationCoordinator {
    pub fn open(
        projections_database: impl AsRef<Path>,
        event_root: impl AsRef<Path>,
    ) -> Result<Self, PublicationError> {
        let event_lake = Arc::new(VersionedEventLake::open(projections_database, event_root)?);
        {
            let connection = event_lake.lock_connection()?;
            initialize_schema(&connection)?;
        }
        Ok(Self { event_lake })
    }

    pub fn event_lake(&self) -> Arc<VersionedEventLake> {
        self.event_lake.clone()
    }

    /// Read the exact bounded WAL prefix following the committed projection
    /// checkpoint. The sole pipeline worker uses this to materialize Parquet
    /// before [`Self::publish_pending`] commits the same prefix.
    pub fn pending_records(
        &self,
        wal: &WriteAheadLog,
        max_wal_bytes: u64,
    ) -> Result<Vec<WalRecord>, PublicationError> {
        let connection = self.event_lake.lock_connection()?;
        let checkpoint = read_projection_state(&connection)?.checkpoint;
        drop(connection);
        wal.read_window(checkpoint, max_wal_bytes)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(PublicationError::Wal)
    }

    /// Return the durable projection checkpoint used for safe WAL cleanup.
    pub fn checkpoint(&self) -> Result<WalCursor, PublicationError> {
        let connection = self.event_lake.lock_connection()?;
        Ok(read_projection_state(&connection)?.checkpoint)
    }

    /// Clone the complete manifest visible to new queries.
    pub fn visible_files(&self) -> Vec<PublishedFile> {
        self.event_lake.visible_files()
    }

    /// Materialize and atomically publish the next exact sealed WAL prefix.
    ///
    /// The coordinator owns both sides of the seam: callers cannot supply a
    /// partial manifest independently of the records whose checkpoint will be
    /// advanced and reclaimed.
    pub fn publish_pending(
        &self,
        wal: &WriteAheadLog,
        max_wal_bytes: u64,
    ) -> Result<Option<PublicationResult>, PublicationError> {
        let records = self.pending_records(wal, max_wal_bytes)?;
        if records.is_empty() {
            return Ok(None);
        }
        let manifest = self.materialize(&records)?;
        self.commit(wal, manifest, max_wal_bytes).map(Some)
    }

    /// Publish at most `max_wal_bytes` of whole records and replace the
    /// complete visible manifest.
    ///
    /// File content is validated before SQLite is locked. Once the immediate
    /// transaction begins, every event is applied in WAL and batch order. WAL
    /// reclamation is deliberately last: every earlier error leaves Event
    /// Truth untouched for deterministic replay.
    fn commit(
        &self,
        wal: &WriteAheadLog,
        files: Vec<PublishedFile>,
        max_wal_bytes: u64,
    ) -> Result<PublicationResult, PublicationError> {
        let prepared = self.event_lake.prepare_publication(files)?;
        let mut connection = self.event_lake.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_projection_state(&transaction)?;

        let mut window = wal.read_window(state.checkpoint, max_wal_bytes)?;
        let mut records = Vec::new();
        while let Some(record) = window.next() {
            records.push(record?);
        }
        if records.is_empty() {
            return Err(PublicationError::NoSealedRecords);
        }
        let checkpoint = window.next_cursor();

        let mut applied_events = 0_usize;
        let mut duplicate_events = 0_usize;
        for record in &records {
            for event in &record.batch.events {
                let project_id = record
                    .batch
                    .project_id_for(event)
                    .ok_or(PublicationError::MissingProjectBinding(event.uuid))?;
                match apply_captured_event(&transaction, project_id, event, record.span.end)? {
                    ApplyOutcome::Applied => applied_events += 1,
                    ApplyOutcome::Duplicate => duplicate_events += 1,
                }
            }
        }

        let generation_id = self.event_lake.persist_prepared_generation(
            &transaction,
            state.generation,
            &prepared,
        )?;
        let data_epoch = state
            .data_epoch
            .checked_add(1)
            .ok_or(PublicationError::DataEpochExhausted)?;
        let segment =
            i64::try_from(checkpoint.segment).map_err(|_| PublicationError::InvalidCheckpoint {
                segment: -1,
                byte_offset: state.checkpoint.byte_offset as i64,
            })?;
        let byte_offset = i64::try_from(checkpoint.byte_offset).map_err(|_| {
            PublicationError::InvalidCheckpoint {
                segment,
                byte_offset: -1,
            }
        })?;
        let generation_raw = i64::try_from(generation_id.get())
            .map_err(|_| PublicationError::ConcurrentStateChange)?;
        let updated = transaction.execute(
            "UPDATE projection_state
             SET current_generation_id = ?1,
                 applied_wal_segment = ?2,
                 applied_wal_offset = ?3,
                 data_epoch = ?4,
                 updated_at = ?5
             WHERE singleton = 1
               AND current_generation_id = ?6
               AND applied_wal_segment = ?7
               AND applied_wal_offset = ?8
               AND data_epoch = ?9",
            params![
                generation_raw,
                segment,
                byte_offset,
                data_epoch,
                Utc::now().timestamp(),
                state.generation,
                i64::try_from(state.checkpoint.segment)
                    .map_err(|_| PublicationError::ConcurrentStateChange)?,
                i64::try_from(state.checkpoint.byte_offset)
                    .map_err(|_| PublicationError::ConcurrentStateChange)?,
                state.data_epoch,
            ],
        )?;
        if updated != 1 {
            return Err(PublicationError::ConcurrentStateChange);
        }
        transaction.commit()?;
        drop(connection);

        self.event_lake
            .activate_committed_generation(generation_id, &prepared);
        let reclaimed_segments = wal.reclaim_through(checkpoint).map_err(|source| {
            PublicationError::ReclamationAfterCommit {
                generation_id,
                checkpoint,
                source,
            }
        })?;

        Ok(PublicationResult {
            generation_id,
            checkpoint,
            data_epoch: u64::try_from(data_epoch)
                .map_err(|_| PublicationError::InvalidDataEpoch(data_epoch))?,
            applied_events,
            duplicate_events,
            published_records: records.len(),
            reclaimed_segments,
        })
    }

    fn materialize(&self, records: &[WalRecord]) -> Result<Vec<PublishedFile>, PublicationError> {
        let first = records
            .first()
            .map(|record| record.span.start)
            .ok_or(PublicationError::NoSealedRecords)?;
        let last = records
            .last()
            .map(|record| record.span.end)
            .ok_or(PublicationError::NoSealedRecords)?;
        let mut groups = BTreeMap::new();
        for record in records {
            for event in &record.batch.events {
                let project_id = record
                    .batch
                    .project_id_for(event)
                    .ok_or(PublicationError::MissingProjectBinding(event.uuid))?;
                groups
                    .entry((project_id.to_owned(), event.timestamp.date_naive()))
                    .or_insert_with(Vec::new)
                    .push(event.clone());
            }
        }

        let mut manifest = self.visible_files();
        for ((project_id, event_date), events) in groups {
            let path = self.write_group(&project_id, event_date, first, last, &events)?;
            let scope = crate::event_lake::FileScope::ProjectDate {
                project_id,
                event_date,
            };
            if manifest.iter().any(|file| file.path == path) {
                continue;
            }
            let digest = file_sha256(&path)?;
            let mut duplicate = None;
            for existing in &manifest {
                if file_sha256(&existing.path)? == digest {
                    duplicate = Some(existing);
                    break;
                }
            }
            if let Some(existing) = duplicate {
                if existing.scope != scope {
                    return Err(EventLakeError::InvalidPublication(
                        "identical event content has conflicting project attribution".to_owned(),
                    )
                    .into());
                }
                std::fs::remove_file(&path)
                    .map_err(|source| publication_io(path.clone(), source))?;
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
                continue;
            }
            manifest.push(PublishedFile::new(path, scope));
        }
        manifest.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(manifest)
    }

    fn write_group(
        &self,
        project_id: &str,
        event_date: NaiveDate,
        first: WalCursor,
        last: WalCursor,
        events: &[crate::capture::event::CapturedEvent],
    ) -> Result<PathBuf, PublicationError> {
        let project_directory = self.event_lake.event_root().join(format!(
            "project_id={}",
            crate::store::token_dir(project_id)
        ));
        create_directory_durable(&project_directory)?;
        let directory = project_directory.join(format!("date={event_date}"));
        create_directory_durable(&directory)?;
        let final_path = directory.join(format!(
            "wal-{:020}-{:020}-to-{:020}-{:020}.parquet",
            first.segment, first.byte_offset, last.segment, last.byte_offset
        ));
        if final_path.exists() {
            let stored = crate::store::parquet::read_file(&final_path)
                .map_err(|source| publication_io(final_path.clone(), source))?;
            if !same_events(&stored, events) {
                return Err(EventLakeError::InvalidPublication(format!(
                    "materialized WAL path {} contains different events",
                    final_path.display()
                ))
                .into());
            }
        } else {
            let temporary_path = final_path.with_extension("parquet.tmp");
            if temporary_path.exists() {
                std::fs::remove_file(&temporary_path)
                    .map_err(|source| publication_io(temporary_path.clone(), source))?;
            }
            crate::store::parquet::write_file(events, &temporary_path)
                .map_err(|source| publication_io(temporary_path.clone(), source))?;
            std::fs::rename(&temporary_path, &final_path)
                .map_err(|source| publication_io(final_path.clone(), source))?;
            sync_directory(&directory)?;
        }
        std::fs::canonicalize(&final_path).map_err(|source| publication_io(final_path, source))
    }
}

fn create_directory_durable(path: &Path) -> Result<(), PublicationError> {
    match std::fs::create_dir(path) {
        Ok(()) => {
            sync_directory(path)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(source) => Err(publication_io(path.to_path_buf(), source)),
    }
}

fn sync_directory(path: &Path) -> Result<(), PublicationError> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| publication_io(path.to_path_buf(), source))
}

fn publication_io(path: PathBuf, source: std::io::Error) -> PublicationError {
    EventLakeError::Io { path, source }.into()
}

fn file_sha256(path: &Path) -> Result<[u8; 32], PublicationError> {
    let mut file =
        std::fs::File::open(path).map_err(|source| publication_io(path.to_path_buf(), source))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| publication_io(path.to_path_buf(), source))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn same_events(
    stored: &[crate::capture::event::CapturedEvent],
    expected: &[crate::capture::event::CapturedEvent],
) -> bool {
    stored.len() == expected.len()
        && stored.iter().zip(expected).all(|(left, right)| {
            left.uuid == right.uuid
                && left.event == right.event
                && left.distinct_id == right.distinct_id
                && left.token == right.token
                && left.timestamp == right.timestamp
                && left.properties == right.properties
        })
}

fn read_projection_state(connection: &Connection) -> Result<ProjectionState, PublicationError> {
    let (generation, segment, byte_offset, data_epoch): (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT current_generation_id, applied_wal_segment,
                    applied_wal_offset, data_epoch
             FROM projection_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let checkpoint = match (u64::try_from(segment), u64::try_from(byte_offset)) {
        (Ok(segment), Ok(byte_offset)) if segment > 0 => WalCursor::new(segment, byte_offset),
        _ => {
            return Err(PublicationError::InvalidCheckpoint {
                segment,
                byte_offset,
            });
        }
    };
    if data_epoch < 0 {
        return Err(PublicationError::InvalidDataEpoch(data_epoch));
    }
    Ok(ProjectionState {
        generation,
        checkpoint,
        data_epoch,
    })
}

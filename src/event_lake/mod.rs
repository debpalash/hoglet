//! Versioned metadata for immutable event files.
//!
//! A publication replaces the complete visible file manifest in one SQLite
//! transaction. Readers acquire an `Arc`-backed generation lease, so publishing
//! a newer manifest cannot change the files underneath an in-flight query.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::storage_bootstrap::PROJECTIONS_APPLICATION_ID;

const EVENT_FILE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS event_files (
    id TEXT PRIMARY KEY NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    sha256 TEXT NOT NULL CHECK(id = sha256 AND length(sha256) = 64),
    project_id TEXT,
    partition_day TEXT,
    size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
    row_count INTEGER CHECK(row_count IS NULL OR row_count >= 0),
    CHECK (
        (project_id IS NOT NULL AND length(trim(project_id)) > 0 AND partition_day IS NOT NULL)
        OR (project_id IS NULL AND partition_day IS NULL)
    )
);

CREATE TABLE IF NOT EXISTS generation_files (
    generation_id INTEGER NOT NULL REFERENCES event_generations(id) ON DELETE CASCADE,
    file_id TEXT NOT NULL REFERENCES event_files(id),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    PRIMARY KEY (generation_id, file_id),
    UNIQUE (generation_id, ordinal)
);
"#;

const CANONICAL_EVENT_FILE_COLUMNS: &[&str] = &[
    "id",
    "partition_day",
    "project_id",
    "relative_path",
    "row_count",
    "sha256",
    "size_bytes",
];
const LEGACY_EVENT_FILE_COLUMNS: &[&str] = &[
    "checksum",
    "id",
    "partition_day",
    "path",
    "project_token",
    "row_count",
    "size_bytes",
];

/// Monotonically increasing identifier for a complete event-file manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(u64);

impl GenerationId {
    pub const EMPTY: Self = Self(0);

    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for GenerationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// File attribution recorded at publication time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileScope {
    ProjectDate {
        project_id: String,
        event_date: NaiveDate,
    },
    /// Pre-partitioning files whose project and day cannot be proven.
    /// These files must remain visible to every scoped query.
    LegacyUnknown,
}

/// One immutable event file in a generation manifest.
///
/// Publication accepts either an event-root-relative path or an absolute path
/// beneath the configured event root. Leases expose the validated canonical
/// absolute path; SQLite stores only the event-root-relative form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedFile {
    pub path: PathBuf,
    pub scope: FileScope,
}

impl PublishedFile {
    pub fn new(path: impl Into<PathBuf>, scope: FileScope) -> Self {
        Self {
            path: path.into(),
            scope,
        }
    }
}

/// Project and half-open date range requested by a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventScope {
    project_id: String,
    start_date: NaiveDate,
    end_date_exclusive: NaiveDate,
}

impl EventScope {
    pub fn new(
        project_id: impl Into<String>,
        start_date: NaiveDate,
        end_date_exclusive: NaiveDate,
    ) -> Result<Self, EventLakeError> {
        let project_id = project_id.into();
        if project_id.trim().is_empty() {
            return Err(EventLakeError::InvalidScope("project id is empty"));
        }
        if start_date >= end_date_exclusive {
            return Err(EventLakeError::InvalidScope(
                "start date must be before end date",
            ));
        }
        Ok(Self {
            project_id,
            start_date,
            end_date_exclusive,
        })
    }

    fn includes(&self, file: &PublishedFile) -> bool {
        match &file.scope {
            FileScope::ProjectDate {
                project_id,
                event_date,
            } => {
                project_id == &self.project_id
                    && event_date >= &self.start_date
                    && event_date < &self.end_date_exclusive
            }
            FileScope::LegacyUnknown => true,
        }
    }
}

#[derive(Debug)]
struct GenerationSnapshot {
    id: GenerationId,
    files: Vec<PublishedFile>,
}

/// Stable view of the files selected for one query.
///
/// The private snapshot field is deliberately retained even when the selected
/// list is empty: its `Arc` is the reader guard used by future file retirement.
#[derive(Debug, Clone)]
pub struct GenerationLease {
    snapshot: Arc<GenerationSnapshot>,
    selected: Vec<usize>,
}

impl GenerationLease {
    pub fn generation_id(&self) -> GenerationId {
        self.snapshot.id
    }

    pub fn files(&self) -> impl ExactSizeIterator<Item = &PublishedFile> {
        self.selected
            .iter()
            .map(|index| &self.snapshot.files[*index])
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.files().map(|file| file.path.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }
}

#[derive(Debug)]
pub enum EventLakeError {
    Database(rusqlite::Error),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidScope(&'static str),
    InvalidPublication(String),
    CorruptMetadata(String),
    GenerationExhausted,
    Unavailable,
}

impl std::fmt::Display for EventLakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "projection database error: {error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::InvalidScope(message) => write!(formatter, "invalid event scope: {message}"),
            Self::InvalidPublication(message) => {
                write!(formatter, "invalid event publication: {message}")
            }
            Self::CorruptMetadata(message) => {
                write!(formatter, "corrupt event lake metadata: {message}")
            }
            Self::GenerationExhausted => formatter.write_str("event generation id exhausted"),
            Self::Unavailable => formatter.write_str("event lake unavailable"),
        }
    }
}

impl std::error::Error for EventLakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for EventLakeError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

/// Durable generation metadata plus the current immutable in-memory snapshot.
pub struct VersionedEventLake {
    connection: Mutex<Connection>,
    event_root: PathBuf,
    visible: RwLock<Arc<GenerationSnapshot>>,
}

impl VersionedEventLake {
    /// Open an already-bootstrapped projections database for one event root.
    ///
    /// This method never creates a database. It validates the SQLite
    /// application id and `database_meta` role before installing or migrating
    /// EventLake-owned tables.
    pub fn open(
        database_path: impl AsRef<Path>,
        event_root: impl AsRef<Path>,
    ) -> Result<Self, EventLakeError> {
        let database_path = database_path.as_ref();
        let event_root = canonical_event_root(event_root.as_ref())?;
        let mut connection = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        validate_prebootstrapped_database(&connection)?;
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
        install_event_file_schema(&mut connection)?;
        let visible = Arc::new(load_visible_snapshot(&connection, &event_root)?);

        Ok(Self {
            connection: Mutex::new(connection),
            event_root,
            visible: RwLock::new(visible),
        })
    }

    /// Acquire a stable, query-scoped view of the currently visible generation.
    pub fn acquire(&self, scope: &EventScope) -> GenerationLease {
        let snapshot = self
            .visible
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let selected = snapshot
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| scope.includes(file).then_some(index))
            .collect();
        GenerationLease { snapshot, selected }
    }

    /// Return the identifier of the generation visible to new readers.
    pub fn current_generation_id(&self) -> GenerationId {
        self.visible
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .id
    }

    /// Clone the complete visible manifest for publication coordination.
    ///
    /// Query callers should use [`Self::acquire`] instead: it both scopes the
    /// manifest and retains the reader lease that protects an in-flight query.
    pub fn visible_files(&self) -> Vec<PublishedFile> {
        self.visible
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .files
            .clone()
    }

    pub(crate) fn event_root(&self) -> &Path {
        &self.event_root
    }

    /// Atomically publish a complete replacement manifest.
    ///
    /// This foundation intentionally updates metadata only. It never unlinks
    /// files; a later retirement layer may delete them only after old reader
    /// leases have been released.
    pub fn publish_generation(
        &self,
        files: Vec<PublishedFile>,
    ) -> Result<GenerationId, EventLakeError> {
        let prepared = self.prepare_publication(files)?;

        let mut connection = self
            .connection
            .lock()
            .map_err(|_| EventLakeError::Unavailable)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let (current_raw, current_epoch): (i64, i64) = transaction.query_row(
            "SELECT current_generation_id, data_epoch
             FROM projection_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let next = self.persist_prepared_generation(&transaction, current_raw, &prepared)?;
        let next_raw = encode_generation(next)?;
        let next_epoch = current_epoch
            .checked_add(1)
            .ok_or_else(|| EventLakeError::CorruptMetadata("data epoch exhausted".to_owned()))?;
        let published_at = Utc::now().timestamp();
        let updated = transaction.execute(
            "UPDATE projection_state
             SET current_generation_id = ?1, data_epoch = ?2, updated_at = ?3
             WHERE singleton = 1
               AND current_generation_id = ?4
               AND data_epoch = ?5",
            params![
                next_raw,
                next_epoch,
                published_at,
                current_raw,
                current_epoch
            ],
        )?;
        if updated != 1 {
            return Err(EventLakeError::Unavailable);
        }
        transaction.commit()?;
        drop(connection);

        self.activate_committed_generation(next, &prepared);
        Ok(next)
    }

    /// Validate and fingerprint a complete manifest before a publication
    /// transaction starts. The coordinator reuses this exact validation path;
    /// there is no lower-trust metadata-only insertion interface.
    pub(crate) fn prepare_publication(
        &self,
        files: Vec<PublishedFile>,
    ) -> Result<PreparedPublication, EventLakeError> {
        Ok(PreparedPublication {
            files: prepare_publication(&self.event_root, files)?,
        })
    }

    /// Borrow the one projections connection shared by manifest publication
    /// and ordered projection updates.
    pub(crate) fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, EventLakeError> {
        self.connection
            .lock()
            .map_err(|_| EventLakeError::Unavailable)
    }

    /// Insert a verified replacement manifest inside the caller's transaction.
    /// Visibility and `projection_state` remain the caller's responsibility.
    pub(crate) fn persist_prepared_generation(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        current_raw: i64,
        prepared: &PreparedPublication,
    ) -> Result<GenerationId, EventLakeError> {
        let current = decode_generation(current_raw)?;
        let next = GenerationId(
            current
                .get()
                .checked_add(1)
                .ok_or(EventLakeError::GenerationExhausted)?,
        );
        let next_raw = encode_generation(next)?;
        let published_at = Utc::now().timestamp();
        transaction.execute(
            "INSERT INTO event_generations(
                id, parent_generation_id, reason, manifest_checksum, created_at
             ) VALUES (?1, ?2, 'publish', ?3, ?4)",
            params![
                next_raw,
                current_raw,
                manifest_checksum(&prepared.files),
                published_at
            ],
        )?;

        for file in &prepared.files {
            persist_file_metadata(transaction, file)?;
        }

        let mut link = transaction.prepare(
            "INSERT INTO generation_files(generation_id, file_id, ordinal)
             VALUES (?1, ?2, ?3)",
        )?;
        for (ordinal, file) in prepared.files.iter().enumerate() {
            let ordinal = i64::try_from(ordinal)
                .map_err(|_| EventLakeError::InvalidPublication("too many files".to_owned()))?;
            link.execute(params![next_raw, file.file_id, ordinal])?;
        }
        Ok(next)
    }

    /// Make a committed generation available to new leases. Old leases keep
    /// their previous `Arc` snapshot.
    pub(crate) fn activate_committed_generation(
        &self,
        id: GenerationId,
        prepared: &PreparedPublication,
    ) {
        let files = prepared
            .files
            .iter()
            .map(|file| PublishedFile::new(file.absolute_path.clone(), file.scope.clone()))
            .collect();
        let mut visible = self
            .visible
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *visible = Arc::new(GenerationSnapshot { id, files });
    }
}

fn canonical_event_root(path: &Path) -> Result<PathBuf, EventLakeError> {
    let canonical = fs::canonicalize(path).map_err(|source| EventLakeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::metadata(&canonical).map_err(|source| EventLakeError::Io {
        path: canonical.clone(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(EventLakeError::InvalidPublication(format!(
            "event root is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn validate_prebootstrapped_database(connection: &Connection) -> Result<(), EventLakeError> {
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if application_id != PROJECTIONS_APPLICATION_ID {
        return Err(EventLakeError::CorruptMetadata(format!(
            "database application id {application_id:#x} is not the projections id {PROJECTIONS_APPLICATION_ID:#x}"
        )));
    }

    let metadata = connection
        .query_row(
            "SELECT pair_id, database_role FROM database_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let (pair_id, role) = metadata.ok_or_else(|| {
        EventLakeError::CorruptMetadata("database_meta singleton is absent".to_owned())
    })?;
    if pair_id.trim().is_empty() {
        return Err(EventLakeError::CorruptMetadata(
            "database pair id is empty".to_owned(),
        ));
    }
    if role != "projections" {
        return Err(EventLakeError::CorruptMetadata(format!(
            "database role is {role:?}, expected \"projections\""
        )));
    }

    let current_generation = connection
        .query_row(
            "SELECT state.current_generation_id
             FROM projection_state AS state
             JOIN event_generations AS generation
               ON generation.id = state.current_generation_id
             WHERE state.singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if current_generation.is_none() {
        return Err(EventLakeError::CorruptMetadata(
            "projection state is absent or references an unknown generation".to_owned(),
        ));
    }
    Ok(())
}

fn install_event_file_schema(connection: &mut Connection) -> Result<(), EventLakeError> {
    let event_files_exists = table_exists(connection, "event_files")?;
    let generation_files_exists = table_exists(connection, "generation_files")?;
    if !event_files_exists && generation_files_exists {
        return Err(EventLakeError::CorruptMetadata(
            "generation_files exists without event_files".to_owned(),
        ));
    }

    if event_files_exists {
        let columns = event_file_columns(connection)?;
        let canonical = column_set(CANONICAL_EVENT_FILE_COLUMNS);
        let legacy = column_set(LEGACY_EVENT_FILE_COLUMNS);
        if columns == legacy {
            let file_count: i64 =
                connection.query_row("SELECT count(*) FROM event_files", [], |row| row.get(0))?;
            let link_count = if generation_files_exists {
                connection.query_row("SELECT count(*) FROM generation_files", [], |row| {
                    row.get::<_, i64>(0)
                })?
            } else {
                0
            };
            if file_count != 0 || link_count != 0 {
                return Err(EventLakeError::CorruptMetadata(
                    "populated legacy event-file metadata requires offline migration".to_owned(),
                ));
            }

            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if generation_files_exists {
                transaction.execute("DROP TABLE generation_files", [])?;
            }
            transaction.execute("DROP TABLE event_files", [])?;
            transaction.execute_batch(EVENT_FILE_SCHEMA)?;
            transaction.commit()?;
            return Ok(());
        }
        if columns != canonical {
            return Err(EventLakeError::CorruptMetadata(format!(
                "unsupported event_files schema columns: {columns:?}"
            )));
        }
    }

    connection.execute_batch(EVENT_FILE_SCHEMA)?;
    Ok(())
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, EventLakeError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn event_file_columns(connection: &Connection) -> Result<BTreeSet<String>, EventLakeError> {
    let mut statement = connection.prepare("PRAGMA table_info(event_files)")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(EventLakeError::Database)
}

fn column_set(columns: &[&str]) -> BTreeSet<String> {
    columns.iter().map(|column| (*column).to_owned()).collect()
}

#[derive(Debug)]
pub(crate) struct PreparedPublication {
    files: Vec<PreparedFile>,
}

#[derive(Debug)]
struct PreparedFile {
    file_id: String,
    relative_path: String,
    sha256: String,
    size_bytes: i64,
    row_count: Option<i64>,
    scope: FileScope,
    absolute_path: PathBuf,
}

fn prepare_publication(
    event_root: &Path,
    files: Vec<PublishedFile>,
) -> Result<Vec<PreparedFile>, EventLakeError> {
    let mut relative_paths = HashSet::with_capacity(files.len());
    let mut file_ids = HashSet::with_capacity(files.len());
    let mut prepared = Vec::with_capacity(files.len());

    for file in files {
        if let FileScope::ProjectDate { project_id, .. } = &file.scope
            && project_id.trim().is_empty()
        {
            return Err(EventLakeError::InvalidPublication(
                "project id is empty".to_owned(),
            ));
        }
        let file = prepare_file(event_root, file)?;
        if !relative_paths.insert(file.relative_path.clone()) {
            return Err(EventLakeError::InvalidPublication(format!(
                "duplicate file path: {}",
                file.relative_path
            )));
        }
        if !file_ids.insert(file.file_id.clone()) {
            return Err(EventLakeError::InvalidPublication(format!(
                "different paths contain the same immutable content: {}",
                file.file_id
            )));
        }
        prepared.push(file);
    }
    Ok(prepared)
}

fn prepare_file(
    event_root: &Path,
    published: PublishedFile,
) -> Result<PreparedFile, EventLakeError> {
    if published.path.as_os_str().is_empty() {
        return Err(EventLakeError::InvalidPublication(
            "file path is empty".to_owned(),
        ));
    }
    let candidate = if published.path.is_absolute() {
        published.path.clone()
    } else {
        event_root.join(&published.path)
    };
    let requested_metadata = fs::symlink_metadata(&candidate)
        .map_err(|error| invalid_file_io("inspect", &candidate, error))?;
    if requested_metadata.file_type().is_symlink() {
        return Err(EventLakeError::InvalidPublication(format!(
            "symbolic links cannot be published: {}",
            candidate.display()
        )));
    }
    if !requested_metadata.is_file() {
        return Err(EventLakeError::InvalidPublication(format!(
            "event path is not a regular file: {}",
            candidate.display()
        )));
    }

    let absolute_path = fs::canonicalize(&candidate)
        .map_err(|error| invalid_file_io("canonicalize", &candidate, error))?;
    let relative = absolute_path.strip_prefix(event_root).map_err(|_| {
        EventLakeError::InvalidPublication(format!(
            "event file is outside the configured event root: {}",
            absolute_path.display()
        ))
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(EventLakeError::InvalidPublication(format!(
            "event file has an invalid relative path: {}",
            relative.display()
        )));
    }
    let relative_path = relative
        .to_str()
        .ok_or_else(|| {
            EventLakeError::InvalidPublication(format!(
                "event-root-relative path is not valid UTF-8: {}",
                relative.display()
            ))
        })?
        .to_owned();

    let mut input = File::open(&absolute_path)
        .map_err(|error| invalid_file_io("open", &absolute_path, error))?;
    let opened_metadata = input
        .metadata()
        .map_err(|error| invalid_file_io("inspect", &absolute_path, error))?;
    if !opened_metadata.is_file() {
        return Err(EventLakeError::InvalidPublication(format!(
            "event path is not a regular file: {}",
            absolute_path.display()
        )));
    }
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| invalid_file_io("read", &absolute_path, error))?;
        if read == 0 {
            break;
        }
        size = size.checked_add(read as u64).ok_or_else(|| {
            EventLakeError::InvalidPublication(format!(
                "event file is too large: {}",
                absolute_path.display()
            ))
        })?;
        digest.update(&buffer[..read]);
    }

    let current_metadata = fs::symlink_metadata(&candidate)
        .map_err(|error| invalid_file_io("reinspect", &candidate, error))?;
    let current_path = fs::canonicalize(&candidate)
        .map_err(|error| invalid_file_io("recanonicalize", &candidate, error))?;
    if current_metadata.file_type().is_symlink()
        || !current_metadata.is_file()
        || current_path != absolute_path
        || opened_metadata.len() != size
        || current_metadata.len() != size
    {
        return Err(EventLakeError::InvalidPublication(format!(
            "event file changed while it was being published: {}",
            candidate.display()
        )));
    }

    let size_bytes = i64::try_from(size).map_err(|_| {
        EventLakeError::InvalidPublication(format!(
            "event file is too large: {}",
            absolute_path.display()
        ))
    })?;
    let sha256 = hex::encode(digest.finalize());
    Ok(PreparedFile {
        file_id: sha256.clone(),
        relative_path,
        sha256,
        size_bytes,
        row_count: None,
        scope: published.scope,
        absolute_path,
    })
}

fn invalid_file_io(operation: &str, path: &Path, error: std::io::Error) -> EventLakeError {
    EventLakeError::InvalidPublication(format!("cannot {operation} {}: {error}", path.display()))
}

fn load_visible_snapshot(
    connection: &Connection,
    event_root: &Path,
) -> Result<GenerationSnapshot, EventLakeError> {
    let generation_raw: i64 = connection.query_row(
        "SELECT current_generation_id FROM projection_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let id = decode_generation(generation_raw)?;
    let mut statement = connection.prepare(
        "SELECT f.id, f.relative_path, f.sha256, f.project_id, f.partition_day,
                f.size_bytes, f.row_count
         FROM generation_files AS gf
         JOIN event_files AS f ON f.id = gf.file_id
         WHERE gf.generation_id = ?1
         ORDER BY gf.ordinal",
    )?;
    let rows = statement.query_map([generation_raw], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;

    let mut files = Vec::new();
    for row in rows {
        let (file_id, relative_path, sha256, project_id, event_date, size_bytes, row_count) = row?;
        let scope = match (project_id, event_date) {
            (Some(project_id), Some(event_date)) => {
                let event_date =
                    NaiveDate::parse_from_str(&event_date, "%Y-%m-%d").map_err(|_| {
                        EventLakeError::CorruptMetadata(format!(
                            "invalid event date {event_date:?} for {relative_path}"
                        ))
                    })?;
                FileScope::ProjectDate {
                    project_id,
                    event_date,
                }
            }
            (None, None) => FileScope::LegacyUnknown,
            _ => {
                return Err(EventLakeError::CorruptMetadata(format!(
                    "invalid scope for {relative_path}"
                )));
            }
        };
        if row_count.is_some_and(|count| count < 0) {
            return Err(EventLakeError::CorruptMetadata(format!(
                "negative row count for {relative_path}"
            )));
        }
        let prepared = prepare_file(
            event_root,
            PublishedFile::new(PathBuf::from(&relative_path), scope.clone()),
        )
        .map_err(|error| {
            EventLakeError::CorruptMetadata(format!(
                "published file {relative_path:?} is unavailable or unsafe: {error}"
            ))
        })?;
        if prepared.file_id != file_id
            || prepared.sha256 != sha256
            || prepared.relative_path != relative_path
            || prepared.size_bytes != size_bytes
        {
            return Err(EventLakeError::CorruptMetadata(format!(
                "published file metadata does not match content for {relative_path}"
            )));
        }
        files.push(PublishedFile::new(prepared.absolute_path, scope));
    }
    Ok(GenerationSnapshot { id, files })
}

#[derive(Debug, PartialEq, Eq)]
struct StoredFileMetadata {
    id: String,
    relative_path: String,
    sha256: String,
    project_id: Option<String>,
    partition_day: Option<String>,
    size_bytes: i64,
    row_count: Option<i64>,
}

fn persist_file_metadata(
    transaction: &rusqlite::Transaction<'_>,
    expected: &PreparedFile,
) -> Result<(), EventLakeError> {
    let stored_by_id = read_stored_file(transaction, "id", &expected.file_id)?;
    if let Some(stored) = stored_by_id {
        if stored == expected.stored_metadata() {
            return Ok(());
        }
        return Err(EventLakeError::InvalidPublication(format!(
            "immutable metadata changed for content {}",
            expected.file_id
        )));
    }

    if let Some(stored) = read_stored_file(transaction, "relative_path", &expected.relative_path)? {
        return Err(EventLakeError::InvalidPublication(format!(
            "immutable path {} was already assigned to content {}",
            expected.relative_path, stored.id
        )));
    }

    let metadata = expected.stored_metadata();
    transaction.execute(
        "INSERT INTO event_files(
            id, relative_path, sha256, project_id, partition_day, size_bytes, row_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            metadata.id,
            metadata.relative_path,
            metadata.sha256,
            metadata.project_id,
            metadata.partition_day,
            metadata.size_bytes,
            metadata.row_count,
        ],
    )?;
    Ok(())
}

impl PreparedFile {
    fn stored_metadata(&self) -> StoredFileMetadata {
        let (project_id, partition_day) = match &self.scope {
            FileScope::ProjectDate {
                project_id,
                event_date,
            } => (
                Some(project_id.clone()),
                Some(event_date.format("%Y-%m-%d").to_string()),
            ),
            FileScope::LegacyUnknown => (None, None),
        };
        StoredFileMetadata {
            id: self.file_id.clone(),
            relative_path: self.relative_path.clone(),
            sha256: self.sha256.clone(),
            project_id,
            partition_day,
            size_bytes: self.size_bytes,
            row_count: self.row_count,
        }
    }
}

fn read_stored_file(
    transaction: &rusqlite::Transaction<'_>,
    lookup_column: &str,
    value: &str,
) -> Result<Option<StoredFileMetadata>, EventLakeError> {
    let sql = match lookup_column {
        "id" => {
            "SELECT id, relative_path, sha256, project_id, partition_day, size_bytes, row_count
             FROM event_files WHERE id = ?1"
        }
        "relative_path" => {
            "SELECT id, relative_path, sha256, project_id, partition_day, size_bytes, row_count
             FROM event_files WHERE relative_path = ?1"
        }
        _ => return Err(EventLakeError::Unavailable),
    };
    transaction
        .query_row(sql, [value], |row| {
            Ok(StoredFileMetadata {
                id: row.get(0)?,
                relative_path: row.get(1)?,
                sha256: row.get(2)?,
                project_id: row.get(3)?,
                partition_day: row.get(4)?,
                size_bytes: row.get(5)?,
                row_count: row.get(6)?,
            })
        })
        .optional()
        .map_err(EventLakeError::Database)
}

fn manifest_checksum(files: &[PreparedFile]) -> String {
    let mut digest = Sha256::new();
    for file in files {
        digest.update(file.file_id.as_bytes());
        digest.update([0]);
        digest.update(file.relative_path.as_bytes());
        digest.update([0]);
        match &file.scope {
            FileScope::ProjectDate {
                project_id,
                event_date,
            } => {
                digest.update(b"project_date\0");
                digest.update(project_id.as_bytes());
                digest.update([0]);
                digest.update(event_date.format("%Y-%m-%d").to_string().as_bytes());
            }
            FileScope::LegacyUnknown => digest.update(b"legacy_unknown"),
        }
        digest.update([0]);
        digest.update(file.size_bytes.to_be_bytes());
        match file.row_count {
            Some(row_count) => {
                digest.update([1]);
                digest.update(row_count.to_be_bytes());
            }
            None => digest.update([0]),
        }
        digest.update([0xff]);
    }
    hex::encode(digest.finalize())
}

fn decode_generation(raw: i64) -> Result<GenerationId, EventLakeError> {
    u64::try_from(raw)
        .map(GenerationId)
        .map_err(|_| EventLakeError::CorruptMetadata(format!("invalid generation id {raw}")))
}

fn encode_generation(generation: GenerationId) -> Result<i64, EventLakeError> {
    i64::try_from(generation.get()).map_err(|_| EventLakeError::GenerationExhausted)
}

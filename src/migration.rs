//! Explicit, offline migration from the legacy multi-database layout.
//!
//! Migration builds and validates a paired v2 control/projections database in
//! shadow files. Publication is ordered: projections first, control second,
//! and the completion marker last. Legacy databases, Parquet files, and WAL
//! segments are opened read-only and are never repaired in place.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::event_lake::{EventLakeError, FileScope, PublishedFile, VersionedEventLake};
use crate::legacy_import::{
    LegacyImportError, LegacyImportReport, import_legacy_control_with_tokens,
};
use crate::legacy_resources::{LegacyResourceImportError, import_legacy_resources};
use crate::storage_bootstrap::{
    StorageBootstrapError, StorageDisposition, StorageMetadata, StoragePaths, bootstrap_storage,
    discover_legacy_tokens, inspect_storage, validate_storage_pair,
};

const MIGRATION_ID: &str = "legacy-v1-to-v2";
const MARKER_VERSION: u32 = 1;
const LEGACY_DATABASES: &[&str] = &[
    "auth.db",
    "projects.db",
    "identity.db",
    "catalog.db",
    "sessions.db",
    "flags.db",
    "dashboards.db",
    "cohorts.db",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationMarker {
    pub version: u32,
    pub migration: String,
    pub pair_id: String,
    pub discovered_tokens: usize,
    pub source_fingerprint: String,
    pub completed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub pair_id: String,
    pub discovered_tokens: usize,
    pub source_fingerprint: String,
    pub resumed: bool,
    pub already_complete: bool,
    pub imported: Option<LegacyImportReport>,
}

#[derive(Debug)]
pub enum MigrationError {
    NotLegacy(StorageDisposition),
    Locked(PathBuf),
    UnsupportedIncomplete(String),
    Storage(StorageBootstrapError),
    Import(LegacyImportError),
    Resources(LegacyResourceImportError),
    EventLake(EventLakeError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
    InvalidLegacyData {
        path: PathBuf,
        detail: String,
    },
    Validation(String),
    InvalidMarker(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLegacy(disposition) => {
                write!(
                    formatter,
                    "offline migration requires LegacyOnly storage, found {disposition:?}"
                )
            }
            Self::Locked(path) => write!(
                formatter,
                "offline migration is locked by {}; verify no migration is running before removing a stale lock",
                path.display()
            ),
            Self::UnsupportedIncomplete(detail) => {
                write!(
                    formatter,
                    "cannot safely resume incomplete migration: {detail}"
                )
            }
            Self::Storage(error) => write!(formatter, "storage migration failed: {error}"),
            Self::Import(error) => write!(formatter, "legacy control import failed: {error}"),
            Self::Resources(error) => write!(formatter, "legacy resource import failed: {error}"),
            Self::EventLake(error) => write!(formatter, "legacy event manifest failed: {error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Sqlite { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::InvalidLegacyData { path, detail } => {
                write!(
                    formatter,
                    "invalid legacy data in {}: {detail}",
                    path.display()
                )
            }
            Self::Validation(detail) => write!(formatter, "migration validation failed: {detail}"),
            Self::InvalidMarker(detail) => write!(formatter, "invalid migration marker: {detail}"),
        }
    }
}

impl Error for MigrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Import(error) => Some(error),
            Self::Resources(error) => Some(error),
            Self::EventLake(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<StorageBootstrapError> for MigrationError {
    fn from(value: StorageBootstrapError) -> Self {
        Self::Storage(value)
    }
}

impl From<LegacyImportError> for MigrationError {
    fn from(value: LegacyImportError) -> Self {
        Self::Import(value)
    }
}

impl From<EventLakeError> for MigrationError {
    fn from(value: EventLakeError) -> Self {
        Self::EventLake(value)
    }
}

struct LegacyDiscovery {
    tokens: BTreeSet<String>,
    parquet_files: Vec<PathBuf>,
    source_fingerprint: String,
}

/// Migrate one data directory while the Hoglet server is stopped.
///
/// The only resumable states are a complete shadow pair, projections already
/// published with a staged control database, or a fully published prepared
/// pair whose completion marker was not yet written. Any other partial state
/// is refused without deleting or overwriting it.
pub fn migrate_legacy_storage(paths: &StoragePaths) -> Result<MigrationReport, MigrationError> {
    let initial = inspect_storage(paths)?;
    if let StorageDisposition::ReadyV2(metadata) = &initial
        && completion_marker_path(paths).exists()
    {
        let marker = read_marker(paths)?;
        let source_fingerprint = source_fingerprint(paths)?;
        validate_marker(&marker, metadata, &source_fingerprint)?;
        return Ok(MigrationReport {
            pair_id: marker.pair_id,
            discovered_tokens: marker.discovered_tokens,
            source_fingerprint: marker.source_fingerprint,
            resumed: false,
            already_complete: true,
            imported: None,
        });
    }
    if matches!(&initial, StorageDisposition::Fresh) || !paths.has_legacy_artifacts() {
        return Err(MigrationError::NotLegacy(initial));
    }

    let _lock = MigrationLock::acquire(paths)?;
    let disposition = inspect_storage(paths)?;
    let resumed = !matches!(&disposition, StorageDisposition::LegacyOnly);
    let discovery = discover_all_legacy_tokens(paths)?;
    let tokens = &discovery.tokens;

    let shadow_control = paths.control_migrating();
    let shadow_projections = paths.projections_migrating();
    let inner_shadow_control = staging_path(&shadow_control);
    let final_control = paths.control();
    let final_projections = paths.projections();

    let (metadata, imported, validation_control, validation_projections) = match disposition {
        StorageDisposition::LegacyOnly => {
            let metadata = bootstrap_storage(&shadow_control, &shadow_projections)?;
            let imported =
                import_control_with_tokens(paths.data_dir(), &shadow_control, tokens.iter())?;
            record_prepared(&shadow_control, &metadata, tokens.len())?;
            let metadata = ensure_legacy_manifest(
                &shadow_projections,
                &paths.data_dir().join("events"),
                &discovery.parquet_files,
                metadata,
            )?;
            freeze_database(&shadow_control)?;
            freeze_database(&shadow_projections)?;
            (
                metadata,
                Some(imported),
                shadow_control.clone(),
                shadow_projections.clone(),
            )
        }
        StorageDisposition::MigrationIncomplete
            if shadow_control.exists()
                && shadow_projections.exists()
                && !final_control.exists()
                && !final_projections.exists() =>
        {
            let metadata = validate_storage_pair(&shadow_control, &shadow_projections)?;
            let imported =
                import_control_with_tokens(paths.data_dir(), &shadow_control, tokens.iter())?;
            record_prepared(&shadow_control, &metadata, tokens.len())?;
            let metadata = ensure_legacy_manifest(
                &shadow_projections,
                &paths.data_dir().join("events"),
                &discovery.parquet_files,
                metadata,
            )?;
            freeze_database(&shadow_control)?;
            freeze_database(&shadow_projections)?;
            (
                metadata,
                Some(imported),
                shadow_control.clone(),
                shadow_projections.clone(),
            )
        }
        StorageDisposition::MigrationIncomplete
            if !shadow_control.exists()
                && shadow_projections.exists()
                && inner_shadow_control.exists()
                && !final_control.exists()
                && !final_projections.exists() =>
        {
            let metadata = bootstrap_storage(&shadow_control, &shadow_projections)?;
            let imported =
                import_control_with_tokens(paths.data_dir(), &shadow_control, tokens.iter())?;
            record_prepared(&shadow_control, &metadata, tokens.len())?;
            let metadata = ensure_legacy_manifest(
                &shadow_projections,
                &paths.data_dir().join("events"),
                &discovery.parquet_files,
                metadata,
            )?;
            freeze_database(&shadow_control)?;
            freeze_database(&shadow_projections)?;
            (
                metadata,
                Some(imported),
                shadow_control.clone(),
                shadow_projections.clone(),
            )
        }
        StorageDisposition::MigrationIncomplete
            if shadow_control.exists()
                && !shadow_projections.exists()
                && !final_control.exists()
                && final_projections.exists() =>
        {
            let metadata = validate_storage_pair(&shadow_control, &final_projections)?;
            let imported =
                import_control_with_tokens(paths.data_dir(), &shadow_control, tokens.iter())?;
            record_prepared(&shadow_control, &metadata, tokens.len())?;
            let metadata = ensure_legacy_manifest(
                &final_projections,
                &paths.data_dir().join("events"),
                &discovery.parquet_files,
                metadata,
            )?;
            freeze_database(&shadow_control)?;
            freeze_database(&final_projections)?;
            (
                metadata,
                Some(imported),
                shadow_control.clone(),
                final_projections.clone(),
            )
        }
        StorageDisposition::ReadyV2(metadata) if !completion_marker_path(paths).exists() => {
            import_legacy_resources(paths.data_dir(), &final_control)
                .map_err(MigrationError::Resources)?;
            freeze_database(&final_control)?;
            validate_candidate(&final_control, &final_projections, &metadata, tokens)?;
            let metadata = ensure_legacy_manifest(
                &final_projections,
                &paths.data_dir().join("events"),
                &discovery.parquet_files,
                metadata,
            )?;
            freeze_database(&final_projections)?;
            (
                metadata,
                None,
                final_control.clone(),
                final_projections.clone(),
            )
        }
        StorageDisposition::MigrationIncomplete => {
            return Err(MigrationError::UnsupportedIncomplete(format!(
                "expected both shadow databases, or published projections plus staged control; found control={}, projections={}, staged control={}, staged projections={}",
                final_control.exists(),
                final_projections.exists(),
                shadow_control.exists(),
                shadow_projections.exists()
            )));
        }
        other => return Err(MigrationError::NotLegacy(other)),
    };

    validate_candidate(
        &validation_control,
        &validation_projections,
        &metadata,
        tokens,
    )?;
    verify_source_fingerprint(paths, &discovery.source_fingerprint)?;
    sync_file(&validation_control)?;
    sync_file(&validation_projections)?;

    if validation_projections == shadow_projections {
        rename_new(&shadow_projections, &final_projections)?;
        sync_directory(paths.data_dir())?;
    }
    if validation_control == shadow_control {
        rename_new(&shadow_control, &final_control)?;
        sync_directory(paths.data_dir())?;
    }

    let published = validate_storage_pair(&final_control, &final_projections)?;
    if published != metadata {
        return Err(MigrationError::Validation(
            "published storage metadata changed after validation".into(),
        ));
    }
    validate_candidate(&final_control, &final_projections, &published, tokens)?;
    validate_legacy_manifest(
        &final_projections,
        &paths.data_dir().join("events"),
        &discovery.parquet_files,
    )?;
    verify_source_fingerprint(paths, &discovery.source_fingerprint)?;

    let marker = MigrationMarker {
        version: MARKER_VERSION,
        migration: MIGRATION_ID.into(),
        pair_id: metadata.pair_id.clone(),
        discovered_tokens: tokens.len(),
        source_fingerprint: discovery.source_fingerprint.clone(),
        completed_at: unix_timestamp(),
    };
    write_marker_last(paths, &marker)?;

    Ok(MigrationReport {
        pair_id: metadata.pair_id,
        discovered_tokens: tokens.len(),
        source_fingerprint: discovery.source_fingerprint,
        resumed,
        already_complete: false,
        imported,
    })
}

pub fn completion_marker_path(paths: &StoragePaths) -> PathBuf {
    paths.data_dir().join("migration-v2-complete.json")
}

fn discover_all_legacy_tokens(paths: &StoragePaths) -> Result<LegacyDiscovery, MigrationError> {
    let database_paths = LEGACY_DATABASES
        .iter()
        .map(|name| paths.data_dir().join(name))
        .collect::<Vec<_>>();
    let mut tokens = discover_legacy_tokens(database_paths)?;

    let mut parquet_files = Vec::new();
    collect_parquet_tokens(
        &paths.data_dir().join("events"),
        &mut tokens,
        &mut parquet_files,
    )?;
    collect_wal_tokens(&paths.data_dir().join("wal"), &mut tokens)?;

    for token in &tokens {
        crate::token::validate(token).map_err(|error| MigrationError::InvalidLegacyData {
            path: paths.data_dir().to_path_buf(),
            detail: format!("a discovered capture token is invalid: {error:?}"),
        })?;
    }
    parquet_files.sort();
    Ok(LegacyDiscovery {
        tokens,
        parquet_files,
        source_fingerprint: source_fingerprint(paths)?,
    })
}

fn import_control_with_tokens<'a, I>(
    data_dir: &Path,
    control_path: &Path,
    tokens: I,
) -> Result<LegacyImportReport, MigrationError>
where
    I: IntoIterator<Item = &'a String>,
{
    let report =
        import_legacy_control_with_tokens(data_dir, control_path, tokens).map_err(|error| {
            match error {
                LegacyImportError::InvalidToken(_) => MigrationError::InvalidLegacyData {
                    path: data_dir.to_path_buf(),
                    detail: "a discovered capture token is invalid".into(),
                },
                other => MigrationError::Import(other),
            }
        })?;
    import_legacy_resources(data_dir, control_path).map_err(MigrationError::Resources)?;
    Ok(report)
}

fn ensure_legacy_manifest(
    projections_path: &Path,
    event_root: &Path,
    parquet_files: &[PathBuf],
    metadata: StorageMetadata,
) -> Result<StorageMetadata, MigrationError> {
    if metadata.current_generation_id == 0 && parquet_files.is_empty() {
        return Ok(metadata);
    }

    let lake = VersionedEventLake::open(projections_path, event_root)?;
    if metadata.current_generation_id == 0 {
        lake.publish_generation(
            parquet_files
                .iter()
                .cloned()
                .map(|path| PublishedFile::new(path, FileScope::LegacyUnknown))
                .collect(),
        )?;
    }
    drop(lake);

    validate_legacy_manifest(projections_path, event_root, parquet_files)?;
    read_projection_metadata(projections_path, metadata.pair_id)
}

fn read_projection_metadata(
    projections_path: &Path,
    pair_id: String,
) -> Result<StorageMetadata, MigrationError> {
    let connection = Connection::open_with_flags(
        projections_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| sqlite_error(projections_path, source))?;
    let (current_generation_id, data_epoch) = connection
        .query_row(
            "SELECT current_generation_id,data_epoch FROM projection_state WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|source| sqlite_error(projections_path, source))?;
    Ok(StorageMetadata {
        pair_id,
        current_generation_id,
        data_epoch,
    })
}

fn validate_legacy_manifest(
    projections_path: &Path,
    event_root: &Path,
    parquet_files: &[PathBuf],
) -> Result<(), MigrationError> {
    if parquet_files.is_empty() {
        let connection = Connection::open_with_flags(
            projections_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| sqlite_error(projections_path, source))?;
        let generation: i64 = connection
            .query_row(
                "SELECT current_generation_id FROM projection_state WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(|source| sqlite_error(projections_path, source))?;
        if generation == 0 {
            return Ok(());
        }
    }

    let lake = VersionedEventLake::open(projections_path, event_root)?;
    drop(lake);
    let root = fs::canonicalize(event_root).map_err(|source| io_error(event_root, source))?;
    let mut expected = parquet_files
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .map_err(|source| io_error(path, source))?
                .strip_prefix(&root)
                .map(|relative| relative.to_string_lossy().into_owned())
                .map_err(|_| MigrationError::Validation("legacy event escaped event root".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    expected.sort();

    let connection = Connection::open_with_flags(
        projections_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| sqlite_error(projections_path, source))?;
    let mut statement = connection
        .prepare(
            "SELECT files.relative_path,files.project_id,files.partition_day
             FROM generation_files AS links
             JOIN projection_state AS state ON state.current_generation_id=links.generation_id
             JOIN event_files AS files ON files.id=links.file_id
             WHERE state.singleton=1
             ORDER BY files.relative_path",
        )
        .map_err(|source| sqlite_error(projections_path, source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|source| sqlite_error(projections_path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error(projections_path, source))?;
    if rows
        .iter()
        .any(|(_, project_id, partition_day)| project_id.is_some() || partition_day.is_some())
    {
        return Err(MigrationError::Validation(
            "legacy generation contains a project-scoped file".into(),
        ));
    }
    let actual = rows.into_iter().map(|row| row.0).collect::<Vec<_>>();
    if actual != expected {
        return Err(MigrationError::Validation(
            "legacy generation does not cover exactly the verified Parquet files".into(),
        ));
    }
    Ok(())
}

fn collect_parquet_tokens(
    path: &Path,
    tokens: &mut BTreeSet<String>,
    parquet_files: &mut Vec<PathBuf>,
) -> Result<(), MigrationError> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension == "parquet")
        {
            let events = crate::store::parquet::read_file(path).map_err(|source| {
                MigrationError::InvalidLegacyData {
                    path: path.to_path_buf(),
                    detail: source.to_string(),
                }
            })?;
            tokens.extend(events.into_iter().map(|event| event.token));
            parquet_files.push(path.to_path_buf());
        }
        return Ok(());
    }

    let mut entries = fs::read_dir(path)
        .map_err(|source| io_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(path, source))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let child = entry.path();
        if child.is_dir()
            || child
                .extension()
                .is_some_and(|extension| extension == "parquet")
        {
            collect_parquet_tokens(&child, tokens, parquet_files)?;
        }
    }
    Ok(())
}

fn collect_wal_tokens(path: &Path, tokens: &mut BTreeSet<String>) -> Result<(), MigrationError> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Err(MigrationError::InvalidLegacyData {
            path: path.to_path_buf(),
            detail: "legacy WAL path is not a directory".into(),
        });
    }
    let mut segments = fs::read_dir(path)
        .map_err(|source| io_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(path, source))?;
    segments.sort_by_key(|entry| entry.file_name());
    for entry in segments {
        let segment = entry.path();
        if !segment.is_file()
            || segment
                .extension()
                .is_none_or(|extension| extension != "wal")
        {
            continue;
        }
        if crate::wal::segment::parse_seq(&segment).is_none() {
            return Err(MigrationError::InvalidLegacyData {
                path: segment,
                detail: "unsupported WAL segment name or format".into(),
            });
        }
        let events = crate::wal::read_segment_events_read_only(&segment).map_err(|source| {
            MigrationError::InvalidLegacyData {
                path: segment.clone(),
                detail: source.to_string(),
            }
        })?;
        tokens.extend(events.into_iter().map(|event| event.token));
    }
    Ok(())
}

fn verify_source_fingerprint(paths: &StoragePaths, expected: &str) -> Result<(), MigrationError> {
    let actual = source_fingerprint(paths)?;
    if actual != expected {
        return Err(MigrationError::Validation(
            "legacy source files changed during or after migration".into(),
        ));
    }
    Ok(())
}

fn source_fingerprint(paths: &StoragePaths) -> Result<String, MigrationError> {
    let mut files = Vec::new();
    for name in LEGACY_DATABASES {
        let database = paths.data_dir().join(name);
        if database.is_file() {
            files.push(database.clone());
        }
        let mut wal_name = database.as_os_str().to_owned();
        wal_name.push("-wal");
        let sqlite_wal = PathBuf::from(wal_name);
        if sqlite_wal.is_file() {
            files.push(sqlite_wal);
        }
    }
    collect_source_files(&paths.data_dir().join("events"), "parquet", &mut files)?;
    collect_source_files(&paths.data_dir().join("wal"), "wal", &mut files)?;
    files.sort();
    files.dedup();

    let mut fingerprint = Sha256::new();
    fingerprint.update(b"hoglet-legacy-source-v1\0");
    for path in files {
        let relative = path.strip_prefix(paths.data_dir()).map_err(|_| {
            MigrationError::Validation("legacy source escaped the data directory".into())
        })?;
        let relative = relative.to_string_lossy();
        fingerprint.update((relative.len() as u64).to_be_bytes());
        fingerprint.update(relative.as_bytes());

        let before = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        if before.file_type().is_symlink() || !before.is_file() {
            return Err(MigrationError::InvalidLegacyData {
                path,
                detail: "source is not a regular file".into(),
            });
        }
        fingerprint.update(before.len().to_be_bytes());
        let mut input = File::open(&path).map_err(|source| io_error(&path, source))?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut bytes_read = 0_u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|source| io_error(&path, source))?;
            if read == 0 {
                break;
            }
            bytes_read = bytes_read.checked_add(read as u64).ok_or_else(|| {
                MigrationError::InvalidLegacyData {
                    path: path.clone(),
                    detail: "source file is too large".into(),
                }
            })?;
            fingerprint.update(&buffer[..read]);
        }
        let after = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        if after.file_type().is_symlink()
            || !after.is_file()
            || before.len() != bytes_read
            || after.len() != bytes_read
        {
            return Err(MigrationError::InvalidLegacyData {
                path,
                detail: "source changed while it was fingerprinted".into(),
            });
        }
        fingerprint.update([0xff]);
    }
    Ok(hex::encode(fingerprint.finalize()))
}

fn collect_source_files(
    path: &Path,
    extension: &str,
    files: &mut Vec<PathBuf>,
) -> Result<(), MigrationError> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        if path
            .extension()
            .is_some_and(|candidate| candidate == extension)
        {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    let mut entries = fs::read_dir(path)
        .map_err(|source| io_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(path, source))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let child = entry.path();
        if child.is_dir()
            || child
                .extension()
                .is_some_and(|candidate| candidate == extension)
        {
            collect_source_files(&child, extension, files)?;
        }
    }
    Ok(())
}

fn record_prepared(
    control_path: &Path,
    metadata: &StorageMetadata,
    discovered_tokens: usize,
) -> Result<(), MigrationError> {
    let connection =
        Connection::open(control_path).map_err(|source| sqlite_error(control_path, source))?;
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS legacy_migrations (
                 migration TEXT PRIMARY KEY,
                 pair_id TEXT NOT NULL,
                 discovered_tokens INTEGER NOT NULL CHECK (discovered_tokens >= 0),
                 prepared_at INTEGER NOT NULL
             );",
        )
        .map_err(|source| sqlite_error(control_path, source))?;
    connection
        .execute(
            "INSERT INTO legacy_migrations(migration,pair_id,discovered_tokens,prepared_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(migration) DO UPDATE SET
                 pair_id=excluded.pair_id,
                 discovered_tokens=excluded.discovered_tokens,
                 prepared_at=excluded.prepared_at",
            rusqlite::params![
                MIGRATION_ID,
                metadata.pair_id,
                discovered_tokens as i64,
                unix_timestamp()
            ],
        )
        .map_err(|source| sqlite_error(control_path, source))?;
    Ok(())
}

fn freeze_database(path: &Path) -> Result<(), MigrationError> {
    let connection = Connection::open(path).map_err(|source| sqlite_error(path, source))?;
    connection
        .execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE);
             PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;",
        )
        .map_err(|source| sqlite_error(path, source))?;
    drop(connection);
    sync_file(path)
}

fn validate_candidate(
    control_path: &Path,
    projections_path: &Path,
    expected: &StorageMetadata,
    tokens: &BTreeSet<String>,
) -> Result<(), MigrationError> {
    let actual = validate_storage_pair(control_path, projections_path)?;
    if &actual != expected {
        return Err(MigrationError::Validation(
            "candidate pair metadata does not match its prepared metadata".into(),
        ));
    }
    validate_sqlite(control_path)?;
    validate_sqlite(projections_path)?;

    let control = Connection::open_with_flags(
        control_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| sqlite_error(control_path, source))?;
    let prepared = control
        .query_row(
            "SELECT pair_id,discovered_tokens FROM legacy_migrations WHERE migration=?1",
            [MIGRATION_ID],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|source| sqlite_error(control_path, source))?
        .ok_or_else(|| MigrationError::Validation("prepared migration record is absent".into()))?;
    if prepared.0 != expected.pair_id || prepared.1 != tokens.len() as i64 {
        return Err(MigrationError::Validation(
            "prepared migration record does not match the legacy source".into(),
        ));
    }
    for token in tokens {
        let present: bool = control
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE capture_token=?1)",
                [token],
                |row| row.get(0),
            )
            .map_err(|source| sqlite_error(control_path, source))?;
        if !present {
            return Err(MigrationError::Validation(
                "a discovered capture token has no control project".into(),
            ));
        }
    }
    Ok(())
}

fn validate_sqlite(path: &Path) -> Result<(), MigrationError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| sqlite_error(path, source))?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|source| sqlite_error(path, source))?;
    if integrity != "ok" {
        return Err(MigrationError::Validation(format!(
            "{} failed integrity_check: {integrity}",
            path.display()
        )));
    }
    let foreign_key_violation: Option<String> = connection
        .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
        .optional()
        .map_err(|source| sqlite_error(path, source))?;
    if let Some(table) = foreign_key_violation {
        return Err(MigrationError::Validation(format!(
            "{} has a foreign-key violation in {table}",
            path.display()
        )));
    }
    Ok(())
}

fn rename_new(from: &Path, to: &Path) -> Result<(), MigrationError> {
    if to.exists() {
        return Err(MigrationError::UnsupportedIncomplete(format!(
            "refusing to overwrite {}",
            to.display()
        )));
    }
    fs::rename(from, to).map_err(|source| io_error(to, source))
}

fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".migrating");
    PathBuf::from(name)
}

fn write_marker_last(paths: &StoragePaths, marker: &MigrationMarker) -> Result<(), MigrationError> {
    let final_path = completion_marker_path(paths);
    if final_path.exists() {
        let existing = read_marker(paths)?;
        validate_marker(
            &existing,
            &StorageMetadata {
                pair_id: marker.pair_id.clone(),
                current_generation_id: 0,
                data_epoch: 0,
            },
            &marker.source_fingerprint,
        )?;
        return Ok(());
    }
    let temporary = paths
        .data_dir()
        .join("migration-v2-complete.json.migrating");
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|error| MigrationError::InvalidMarker(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|source| io_error(&temporary, source))?;
    file.write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|source| io_error(&temporary, source))?;
    drop(file);
    fs::rename(&temporary, &final_path).map_err(|source| io_error(&final_path, source))?;
    sync_directory(paths.data_dir())
}

fn read_marker(paths: &StoragePaths) -> Result<MigrationMarker, MigrationError> {
    let path = completion_marker_path(paths);
    let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
    serde_json::from_slice(&bytes).map_err(|error| MigrationError::InvalidMarker(error.to_string()))
}

fn validate_marker(
    marker: &MigrationMarker,
    metadata: &StorageMetadata,
    source_fingerprint: &str,
) -> Result<(), MigrationError> {
    if marker.version != MARKER_VERSION
        || marker.migration != MIGRATION_ID
        || marker.pair_id != metadata.pair_id
        || marker.source_fingerprint != source_fingerprint
    {
        return Err(MigrationError::InvalidMarker(
            "version, migration id, database pair id, or legacy source fingerprint does not match"
                .into(),
        ));
    }
    Ok(())
}

struct MigrationLock {
    path: PathBuf,
    directory: PathBuf,
}

impl MigrationLock {
    fn acquire(paths: &StoragePaths) -> Result<Self, MigrationError> {
        let path = paths.data_dir().join("migration-v2.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    MigrationError::Locked(path.clone())
                } else {
                    io_error(&path, source)
                }
            })?;
        let lock = Self {
            path: path.clone(),
            directory: paths.data_dir().to_path_buf(),
        };
        writeln!(
            file,
            "{{\"pid\":{},\"started_at\":{}}}",
            std::process::id(),
            unix_timestamp()
        )
        .and_then(|_| file.sync_all())
        .map_err(|source| io_error(&path, source))?;
        sync_directory(paths.data_dir())?;
        Ok(lock)
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        if fs::remove_file(&self.path).is_ok() {
            let _ = File::open(&self.directory).and_then(|directory| directory.sync_all());
        }
    }
}

fn sync_file(path: &Path) -> Result<(), MigrationError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(path, source))
}

fn sync_directory(path: &Path) -> Result<(), MigrationError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> MigrationError {
    MigrationError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_error(path: &Path, source: rusqlite::Error) -> MigrationError {
    MigrationError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

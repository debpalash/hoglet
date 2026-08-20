//! Strict bootstrap and pair validation for Hoglet's control and projection databases.
//!
//! This module deliberately owns only database identity and the initial projection
//! generation. Operational control tables are initialized by `control`; keeping
//! that boundary avoids a second, subtly different control schema.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

/// SQLite application id for a Hoglet control database (`HCTL`).
pub const CONTROL_APPLICATION_ID: i64 = 0x4843_544c;
/// SQLite application id for a Hoglet projections database (`HPRJ`).
pub const PROJECTIONS_APPLICATION_ID: i64 = 0x4850_524a;
pub const CONTROL_SCHEMA_VERSION: i64 = 1;
pub const PROJECTIONS_SCHEMA_VERSION: i64 = 1;

const EMPTY_MANIFEST_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const DATABASE_META_SCHEMA: &str = "
CREATE TABLE database_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    pair_id TEXT NOT NULL CHECK (length(pair_id) > 0),
    database_role TEXT NOT NULL CHECK (database_role IN ('control', 'projections'))
);";

const PROJECTIONS_SCHEMA: &str = "
CREATE TABLE event_generations (
    id INTEGER PRIMARY KEY CHECK (id >= 0),
    parent_generation_id INTEGER REFERENCES event_generations(id),
    reason TEXT NOT NULL CHECK (reason IN ('initial', 'publish', 'compact', 'retention', 'erasure')),
    manifest_checksum TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE projection_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    current_generation_id INTEGER NOT NULL REFERENCES event_generations(id),
    applied_wal_segment INTEGER NOT NULL CHECK (applied_wal_segment >= 0),
    applied_wal_offset INTEGER NOT NULL CHECK (applied_wal_offset >= 0),
    data_epoch INTEGER NOT NULL CHECK (data_epoch >= 0),
    updated_at INTEGER NOT NULL
);";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseRole {
    Control,
    Projections,
}

impl DatabaseRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Projections => "projections",
        }
    }

    fn application_id(self) -> i64 {
        match self {
            Self::Control => CONTROL_APPLICATION_ID,
            Self::Projections => PROJECTIONS_APPLICATION_ID,
        }
    }

    fn schema_version(self) -> i64 {
        match self {
            Self::Control => CONTROL_SCHEMA_VERSION,
            Self::Projections => PROJECTIONS_SCHEMA_VERSION,
        }
    }
}

impl fmt::Display for DatabaseRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageMetadata {
    pub pair_id: String,
    pub current_generation_id: i64,
    pub data_epoch: i64,
}

/// Canonical storage locations derived from one Hoglet data directory.
///
/// Keeping path derivation here prevents callers from accidentally inspecting
/// one installation and opening databases from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePaths {
    data_dir: PathBuf,
}

impl StoragePaths {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn control(&self) -> PathBuf {
        self.data_dir.join("control.db")
    }

    pub fn projections(&self) -> PathBuf {
        self.data_dir.join("projections.db")
    }

    /// Staging path used by the offline migration. It is intentionally
    /// visible to storage owners, but never opened by normal startup.
    pub fn control_migrating(&self) -> PathBuf {
        migrating_path(&self.control())
    }

    /// Staging path used by the offline migration. Projections are published
    /// before control so a crash cannot expose a control database without its
    /// rebuildable companion.
    pub fn projections_migrating(&self) -> PathBuf {
        migrating_path(&self.projections())
    }

    /// Whether the directory contains any artifact from Hoglet's legacy
    /// multi-database/event layout. This check only inspects directory entries.
    pub fn has_legacy_artifacts(&self) -> bool {
        const LEGACY_ARTIFACTS: &[&str] = &[
            "auth.db",
            "projects.db",
            "flags.db",
            "dashboards.db",
            "cohorts.db",
            "identity.db",
            "catalog.db",
            "sessions.db",
            "events",
            "wal",
        ];

        LEGACY_ARTIFACTS
            .iter()
            .any(|name| self.data_dir.join(name).exists())
    }
}

/// Read-only startup classification for one data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageDisposition {
    /// No v2 or recognized legacy artifacts exist.
    Fresh,
    /// A complete, internally consistent v2 database pair exists.
    ReadyV2(StorageMetadata),
    /// Legacy state exists and no v2 migration has begun.
    LegacyOnly,
    /// A v2 pair or its crash-safe staging files are only partly present.
    MigrationIncomplete,
}

#[derive(Debug)]
pub enum StorageBootstrapError {
    IdenticalPaths,
    IncompletePair {
        present: DatabaseRole,
    },
    SchemaTooNew {
        role: DatabaseRole,
        found: i64,
        supported: i64,
    },
    SchemaTooOld {
        role: DatabaseRole,
        found: i64,
        supported: i64,
    },
    ApplicationIdMismatch {
        role: DatabaseRole,
        found: i64,
        expected: i64,
    },
    RoleMismatch {
        expected: DatabaseRole,
        found: String,
    },
    PairIdMismatch {
        control: String,
        projections: String,
    },
    InvalidMetadata {
        role: DatabaseRole,
        detail: &'static str,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

impl fmt::Display for StorageBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdenticalPaths => {
                formatter.write_str("control and projections paths are identical")
            }
            Self::IncompletePair { present } => {
                write!(
                    formatter,
                    "incomplete storage pair: only {present} database exists"
                )
            }
            Self::SchemaTooNew {
                role,
                found,
                supported,
            } => write!(
                formatter,
                "{role} database schema {found} is newer than supported schema {supported}"
            ),
            Self::SchemaTooOld {
                role,
                found,
                supported,
            } => write!(
                formatter,
                "{role} database schema {found} requires migration to schema {supported}"
            ),
            Self::ApplicationIdMismatch {
                role,
                found,
                expected,
            } => write!(
                formatter,
                "{role} database application id {found:#x} does not match {expected:#x}"
            ),
            Self::RoleMismatch { expected, found } => {
                write!(
                    formatter,
                    "expected {expected} database metadata, found role {found:?}"
                )
            }
            Self::PairIdMismatch {
                control,
                projections,
            } => write!(
                formatter,
                "control/projections pair ids differ ({control:?} != {projections:?})"
            ),
            Self::InvalidMetadata { role, detail } => {
                write!(formatter, "invalid {role} database metadata: {detail}")
            }
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Sqlite { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl Error for StorageBootstrapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Classify a data directory without creating or mutating anything.
///
/// This is the safe first startup operation. In particular it does not create
/// `data_dir`, recover staging files, change SQLite journal modes, or install
/// runtime pragmas. A complete pair is opened with SQLite's read-only flag and
/// fully validated before it is reported as ready.
pub fn inspect_storage(paths: &StoragePaths) -> Result<StorageDisposition, StorageBootstrapError> {
    let control_path = paths.control();
    let projections_path = paths.projections();

    if paths.control_migrating().exists() || paths.projections_migrating().exists() {
        return Ok(StorageDisposition::MigrationIncomplete);
    }

    match (control_path.exists(), projections_path.exists()) {
        (true, true) => validate_pair_read_only(&control_path, &projections_path)
            .map(StorageDisposition::ReadyV2),
        (true, false) | (false, true) => Ok(StorageDisposition::MigrationIncomplete),
        (false, false) if paths.has_legacy_artifacts() => Ok(StorageDisposition::LegacyOnly),
        (false, false) => Ok(StorageDisposition::Fresh),
    }
}

/// Validate a database pair without changing either database or its journal
/// mode. Offline migration uses this immediately before and after publication.
pub fn validate_storage_pair(
    control_path: impl AsRef<Path>,
    projections_path: impl AsRef<Path>,
) -> Result<StorageMetadata, StorageBootstrapError> {
    let control_path = control_path.as_ref();
    let projections_path = projections_path.as_ref();
    if control_path == projections_path {
        return Err(StorageBootstrapError::IdenticalPaths);
    }
    validate_pair_read_only(control_path, projections_path)
}

/// Create a fresh pair, validate an existing pair, or finish the one supported
/// crash-recovery state (published projections plus a validated staged control
/// database).
///
/// Callers that own a data directory should run [`inspect_storage`] first and
/// invoke this only for `Fresh` or `ReadyV2`. This compatibility API does not
/// classify legacy artifacts before creating a pair. Arbitrary incomplete pairs
/// are rejected; the staged-control recovery is accepted only after strict pair
/// validation.
pub fn bootstrap_storage(
    control_path: impl AsRef<Path>,
    projections_path: impl AsRef<Path>,
) -> Result<StorageMetadata, StorageBootstrapError> {
    let control_path = control_path.as_ref();
    let projections_path = projections_path.as_ref();
    if control_path == projections_path {
        return Err(StorageBootstrapError::IdenticalPaths);
    }
    let control_migrating = migrating_path(control_path);
    let projections_migrating = migrating_path(projections_path);

    match (control_path.exists(), projections_path.exists()) {
        (true, false) => {
            return Err(StorageBootstrapError::IncompletePair {
                present: DatabaseRole::Control,
            });
        }
        (false, true) if control_migrating.exists() => {
            validate_pair_read_only(&control_migrating, projections_path)?;
            fs::rename(&control_migrating, control_path).map_err(|source| {
                StorageBootstrapError::Io {
                    path: control_path.to_path_buf(),
                    source,
                }
            })?;
            sync_parent(control_path)?;
        }
        (false, true) => {
            return Err(StorageBootstrapError::IncompletePair {
                present: DatabaseRole::Projections,
            });
        }
        (false, false) => {
            remove_generated_temporary(&control_migrating)?;
            remove_generated_temporary(&projections_migrating)?;
            create_pair(control_path, projections_path)?;
        }
        (true, true) => {}
    }

    let metadata = validate_pair_read_only(control_path, projections_path)?;
    install_runtime_pragmas(control_path, projections_path)?;
    Ok(metadata)
}

fn create_pair(control_path: &Path, projections_path: &Path) -> Result<(), StorageBootstrapError> {
    ensure_parent(control_path)?;
    ensure_parent(projections_path)?;

    let control_migrating = migrating_path(control_path);
    let projections_migrating = migrating_path(projections_path);
    let pair_id = uuid::Uuid::new_v4().to_string();
    create_control_database(&control_migrating, &pair_id)?;
    create_projections_database(&projections_migrating, &pair_id)?;
    harden_file_permissions(&control_migrating)?;
    harden_file_permissions(&projections_migrating)?;
    sync_file(&control_migrating)?;
    sync_file(&projections_migrating)?;
    fs::rename(&projections_migrating, projections_path).map_err(|source| {
        StorageBootstrapError::Io {
            path: projections_path.to_path_buf(),
            source,
        }
    })?;
    sync_parent(projections_path)?;
    fs::rename(&control_migrating, control_path).map_err(|source| StorageBootstrapError::Io {
        path: control_path.to_path_buf(),
        source,
    })?;
    sync_parent(control_path)?;
    Ok(())
}

fn migrating_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".migrating");
    PathBuf::from(name)
}

fn remove_generated_temporary(path: &Path) -> Result<(), StorageBootstrapError> {
    if path.exists() {
        fs::remove_file(path).map_err(|source| StorageBootstrapError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<(), StorageBootstrapError> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| StorageBootstrapError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn sync_parent(path: &Path) -> Result<(), StorageBootstrapError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StorageBootstrapError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<(), StorageBootstrapError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    let existed = parent.exists();
    fs::create_dir_all(parent).map_err(|source| StorageBootstrapError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    if !existed {
        harden_directory_permissions(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn harden_file_permissions(path: &Path) -> Result<(), StorageBootstrapError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        StorageBootstrapError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn harden_file_permissions(_path: &Path) -> Result<(), StorageBootstrapError> {
    Ok(())
}

#[cfg(unix)]
fn harden_directory_permissions(path: &Path) -> Result<(), StorageBootstrapError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        StorageBootstrapError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn harden_directory_permissions(_path: &Path) -> Result<(), StorageBootstrapError> {
    Ok(())
}

fn create_control_database(path: &Path, pair_id: &str) -> Result<(), StorageBootstrapError> {
    let mut connection = open_database(path)?;
    initialize_pragmas(&connection, DatabaseRole::Control)?;
    let transaction = connection
        .transaction()
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute_batch(DATABASE_META_SCHEMA)
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute(
            "INSERT INTO database_meta (singleton, pair_id, database_role) VALUES (1, ?1, 'control')",
            [pair_id],
        )
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .commit()
        .map_err(|source| sqlite_error(path, source))
}

fn create_projections_database(path: &Path, pair_id: &str) -> Result<(), StorageBootstrapError> {
    let mut connection = open_database(path)?;
    initialize_pragmas(&connection, DatabaseRole::Projections)?;
    let now = unix_timestamp();
    let transaction = connection
        .transaction()
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute_batch(DATABASE_META_SCHEMA)
        .and_then(|_| transaction.execute_batch(PROJECTIONS_SCHEMA))
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute(
            "INSERT INTO database_meta (singleton, pair_id, database_role)
             VALUES (1, ?1, 'projections')",
            [pair_id],
        )
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute(
            "INSERT INTO event_generations
                (id, parent_generation_id, reason, manifest_checksum, created_at)
             VALUES (0, NULL, 'initial', ?1, ?2)",
            rusqlite::params![EMPTY_MANIFEST_SHA256, now],
        )
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .execute(
            "INSERT INTO projection_state
                (singleton, current_generation_id, applied_wal_segment, applied_wal_offset,
                 data_epoch, updated_at)
             VALUES (1, 0, 1, 0, 0, ?1)",
            [now],
        )
        .map_err(|source| sqlite_error(path, source))?;
    transaction
        .commit()
        .map_err(|source| sqlite_error(path, source))
}

fn initialize_pragmas(
    connection: &Connection,
    role: DatabaseRole,
) -> Result<(), StorageBootstrapError> {
    let path = PathBuf::from(connection.path().unwrap_or("<sqlite>"));
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;",
        )
        .map_err(|source| StorageBootstrapError::Sqlite {
            path: path.clone(),
            source,
        })?;
    connection
        .pragma_update(None, "application_id", role.application_id())
        .and_then(|_| connection.pragma_update(None, "user_version", role.schema_version()))
        .map_err(|source| StorageBootstrapError::Sqlite { path, source })
}

fn validate_pair_read_only(
    control_path: &Path,
    projections_path: &Path,
) -> Result<StorageMetadata, StorageBootstrapError> {
    let control = open_database_read_only(control_path)?;
    let projections = open_database_read_only(projections_path)?;

    validate_header(&control, control_path, DatabaseRole::Control)?;
    validate_header(&projections, projections_path, DatabaseRole::Projections)?;

    let control_pair = read_pair_id(&control, control_path, DatabaseRole::Control)?;
    let projections_pair = read_pair_id(&projections, projections_path, DatabaseRole::Projections)?;
    if control_pair != projections_pair {
        return Err(StorageBootstrapError::PairIdMismatch {
            control: control_pair,
            projections: projections_pair,
        });
    }

    let generation_zero: Option<i64> = projections
        .query_row(
            "SELECT id FROM event_generations WHERE id = 0 AND reason = 'initial'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| sqlite_error(projections_path, source))?;
    if generation_zero.is_none() {
        return Err(StorageBootstrapError::InvalidMetadata {
            role: DatabaseRole::Projections,
            detail: "generation zero is absent or not initial",
        });
    }

    let state = projections
        .query_row(
            "SELECT state.current_generation_id, state.data_epoch
             FROM projection_state AS state
             JOIN event_generations AS generation
               ON generation.id = state.current_generation_id
             WHERE state.singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|source| sqlite_error(projections_path, source))?
        .ok_or(StorageBootstrapError::InvalidMetadata {
            role: DatabaseRole::Projections,
            detail: "projection state is absent or references an unknown generation",
        })?;

    Ok(StorageMetadata {
        pair_id: control_pair,
        current_generation_id: state.0,
        data_epoch: state.1,
    })
}

fn install_runtime_pragmas(
    control_path: &Path,
    projections_path: &Path,
) -> Result<(), StorageBootstrapError> {
    let control = open_database(control_path)?;
    let projections = open_database(projections_path)?;
    set_runtime_pragmas(&control, control_path)?;
    set_runtime_pragmas(&projections, projections_path)
}

fn validate_header(
    connection: &Connection,
    path: &Path,
    role: DatabaseRole,
) -> Result<(), StorageBootstrapError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| sqlite_error(path, source))?;
    let supported = role.schema_version();
    if version > supported {
        return Err(StorageBootstrapError::SchemaTooNew {
            role,
            found: version,
            supported,
        });
    }
    if version < supported {
        return Err(StorageBootstrapError::SchemaTooOld {
            role,
            found: version,
            supported,
        });
    }

    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|source| sqlite_error(path, source))?;
    let expected = role.application_id();
    if application_id != expected {
        return Err(StorageBootstrapError::ApplicationIdMismatch {
            role,
            found: application_id,
            expected,
        });
    }
    Ok(())
}

fn set_runtime_pragmas(connection: &Connection, path: &Path) -> Result<(), StorageBootstrapError> {
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(|source| sqlite_error(path, source))
}

fn read_pair_id(
    connection: &Connection,
    path: &Path,
    role: DatabaseRole,
) -> Result<String, StorageBootstrapError> {
    let metadata = connection
        .query_row(
            "SELECT pair_id, database_role FROM database_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|source| sqlite_error(path, source))?
        .ok_or(StorageBootstrapError::InvalidMetadata {
            role,
            detail: "database_meta singleton is absent",
        })?;
    if metadata.0.is_empty() {
        return Err(StorageBootstrapError::InvalidMetadata {
            role,
            detail: "pair id is empty",
        });
    }
    if metadata.1 != role.as_str() {
        return Err(StorageBootstrapError::RoleMismatch {
            expected: role,
            found: metadata.1,
        });
    }
    Ok(metadata.0)
}

fn open_database(path: &Path) -> Result<Connection, StorageBootstrapError> {
    Connection::open(path).map_err(|source| sqlite_error(path, source))
}

fn open_database_read_only(path: &Path) -> Result<Connection, StorageBootstrapError> {
    // `READ_ONLY` alone may still create `-wal`/`-shm` files when the database
    // header records WAL mode. SQLite's immutable URI mode forbids all writes,
    // which is the contract startup inspection and offline validation require.
    let uri = format!("file:{}?immutable=1", sqlite_uri_path(path));
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|source| sqlite_error(path, source))
}

fn sqlite_uri_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn sqlite_error(path: &Path, source: rusqlite::Error) -> StorageBootstrapError {
    StorageBootstrapError::Sqlite {
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

/// Read project tokens from known legacy SQLite tables without creating or
/// mutating any database. Missing paths and absent known tables are ignored.
pub fn discover_legacy_tokens<I, P>(paths: I) -> Result<BTreeSet<String>, StorageBootstrapError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    const SOURCES: &[(&str, &str)] = &[
        ("projects", "token"),
        ("projects", "capture_token"),
        ("persons", "token"),
        ("distinct_ids", "token"),
        ("event_names", "token"),
        ("property_keys", "token"),
        ("property_values", "token"),
        ("sessions", "token"),
        ("saved_insights", "token"),
        ("dashboards", "token"),
        ("cohorts", "token"),
        ("feature_flags", "token"),
    ];

    let mut tokens = BTreeSet::new();
    for path in paths {
        let path = path.as_ref();
        if !path.exists() {
            continue;
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| sqlite_error(path, source))?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|source| sqlite_error(path, source))?;

        for &(table, column) in SOURCES {
            if !known_column_exists(&connection, path, table, column)? {
                continue;
            }
            // `table` and `column` come only from the static allowlist above.
            let sql = format!(
                "SELECT DISTINCT {column} FROM {table}
                 WHERE typeof({column}) = 'text' AND length(trim({column})) > 0"
            );
            let mut statement = connection
                .prepare(&sql)
                .map_err(|source| sqlite_error(path, source))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|source| sqlite_error(path, source))?;
            for token in rows {
                tokens.insert(token.map_err(|source| sqlite_error(path, source))?);
            }
        }
    }
    Ok(tokens)
}

fn known_column_exists(
    connection: &Connection,
    path: &Path,
    table: &str,
    column: &str,
) -> Result<bool, StorageBootstrapError> {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2
             )",
            [table, column],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|source| sqlite_error(path, source))
}

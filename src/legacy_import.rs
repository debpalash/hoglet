//! Offline import of legacy authoritative account state into `control.db`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};

use crate::control::{AccessError, ProjectAccess};
use crate::storage_bootstrap::{StorageBootstrapError, discover_legacy_tokens};

const IMPORTED_ORGANIZATION_ID: &str = "00000000-0000-0000-0000-000000000001";
const LEGACY_DATABASES: &[&str] = &[
    "auth.db",
    "projects.db",
    "identity.db",
    "catalog.db",
    "sessions.db",
    "flags.db",
    "dashboards.db",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyImportReport {
    pub users: usize,
    pub organizations: usize,
    pub projects: usize,
    pub personal_keys: usize,
    pub sessions: usize,
}

#[derive(Debug)]
pub enum LegacyImportError {
    Control(AccessError),
    Discovery(StorageBootstrapError),
    Database {
        path: PathBuf,
        source: rusqlite::Error,
    },
    InvalidToken(String),
}

impl fmt::Display for LegacyImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => write!(formatter, "cannot initialize control state: {error}"),
            Self::Discovery(error) => write!(formatter, "cannot discover legacy projects: {error}"),
            Self::Database { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::InvalidToken(_) => formatter.write_str("a legacy project token is invalid"),
        }
    }
}

impl Error for LegacyImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Control(error) => Some(error),
            Self::Discovery(error) => Some(error),
            Self::Database { source, .. } => Some(source),
            Self::InvalidToken(_) => None,
        }
    }
}

/// Imports legacy account state and synthesizes Control State for orphan tokens.
///
/// The caller must bootstrap the control/projections pair first. All imported
/// rows are committed in one foreign-key-checked, FULL-synchronous transaction;
/// rerunning the import is safe.
pub fn import_legacy_control(
    data_dir: impl AsRef<Path>,
    control_path: impl AsRef<Path>,
) -> Result<LegacyImportReport, LegacyImportError> {
    import_legacy_control_with_tokens(data_dir, control_path, std::iter::empty::<String>())
}

/// Import legacy account state plus capture tokens found in immutable event
/// files or WAL segments. The extra tokens are validated before the control
/// transaction starts, then receive deterministic imported projects.
pub fn import_legacy_control_with_tokens<I, S>(
    data_dir: impl AsRef<Path>,
    control_path: impl AsRef<Path>,
    extra_tokens: I,
) -> Result<LegacyImportReport, LegacyImportError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let data_dir = data_dir.as_ref();
    let control_path = control_path.as_ref();
    let auth_path = data_dir.join("auth.db");
    let registry_path = data_dir.join("projects.db");

    ensure_control_schema(control_path)?;
    let legacy = load_legacy_auth(&auth_path)?;
    let registry_names = load_registry_names(&registry_path)?;
    let discovery_paths = LEGACY_DATABASES
        .iter()
        .map(|name| data_dir.join(name))
        .collect::<Vec<_>>();
    let mut tokens =
        discover_legacy_tokens(&discovery_paths).map_err(LegacyImportError::Discovery)?;
    tokens.extend(legacy.projects.iter().map(|project| project.token.clone()));
    tokens.extend(
        extra_tokens
            .into_iter()
            .map(|token| token.as_ref().to_owned()),
    );
    for token in &tokens {
        crate::token::validate(token)
            .map_err(|_| LegacyImportError::InvalidToken(token.clone()))?;
    }

    let mut connection =
        Connection::open(control_path).map_err(|source| database_error(control_path, source))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|source| database_error(control_path, source))?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(|source| database_error(control_path, source))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error(control_path, source))?;
    import_auth_rows(&transaction, &legacy)
        .map_err(|source| database_error(control_path, source))?;
    import_orphan_projects(&transaction, &tokens, &registry_names)
        .map_err(|source| database_error(control_path, source))?;
    let report =
        import_report(&transaction).map_err(|source| database_error(control_path, source))?;
    transaction
        .commit()
        .map_err(|source| database_error(control_path, source))?;
    Ok(report)
}

fn ensure_control_schema(control_path: &Path) -> Result<(), LegacyImportError> {
    let (access, runtime) =
        ProjectAccess::open(control_path.to_path_buf()).map_err(LegacyImportError::Control)?;
    drop(access);
    runtime.close();
    Ok(())
}

#[derive(Default)]
struct LegacyAuth {
    users: Vec<LegacyUser>,
    organizations: Vec<LegacyOrganization>,
    memberships: Vec<LegacyMembership>,
    projects: Vec<LegacyProject>,
    personal_keys: Vec<LegacyPersonalKey>,
    sessions: Vec<LegacySession>,
}

struct LegacyUser {
    id: String,
    email: String,
    password_hash: String,
    name: String,
    created_at: i64,
}

struct LegacyOrganization {
    id: String,
    name: String,
    created_at: i64,
}

struct LegacyMembership {
    organization_id: String,
    user_id: String,
    role: String,
}

struct LegacyProject {
    id: String,
    organization_id: String,
    name: String,
    token: String,
    created_at: i64,
}

struct LegacyPersonalKey {
    id: String,
    user_id: String,
    name: String,
    key_hash: String,
    key_prefix: String,
    last_used: Option<i64>,
    created_at: i64,
}

struct LegacySession {
    id: String,
    user_id: String,
    created_at: i64,
    expires_at: i64,
}

fn load_legacy_auth(path: &Path) -> Result<LegacyAuth, LegacyImportError> {
    if !path.exists() {
        return Ok(LegacyAuth::default());
    }
    let connection = open_legacy(path)?;
    Ok(LegacyAuth {
        users: if table_exists(&connection, path, "users")? {
            collect_rows(
                &connection,
                path,
                "SELECT id,email,pw_hash,name,created_at FROM users ORDER BY created_at,id",
                |row| {
                    Ok(LegacyUser {
                        id: row.get(0)?,
                        email: row.get(1)?,
                        password_hash: row.get(2)?,
                        name: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )?
        } else {
            Vec::new()
        },
        organizations: if table_exists(&connection, path, "orgs")? {
            collect_rows(
                &connection,
                path,
                "SELECT id,name,created_at FROM orgs ORDER BY created_at,id",
                |row| {
                    Ok(LegacyOrganization {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )?
        } else {
            Vec::new()
        },
        memberships: if table_exists(&connection, path, "org_members")? {
            collect_rows(
                &connection,
                path,
                "SELECT org_id,user_id,role FROM org_members ORDER BY org_id,user_id",
                |row| {
                    Ok(LegacyMembership {
                        organization_id: row.get(0)?,
                        user_id: row.get(1)?,
                        role: row.get(2)?,
                    })
                },
            )?
        } else {
            Vec::new()
        },
        projects: if table_exists(&connection, path, "projects")? {
            collect_rows(
                &connection,
                path,
                "SELECT id,org_id,name,token,created_at FROM projects ORDER BY created_at,id",
                |row| {
                    Ok(LegacyProject {
                        id: row.get(0)?,
                        organization_id: row.get(1)?,
                        name: row.get(2)?,
                        token: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )?
        } else {
            Vec::new()
        },
        personal_keys: if table_exists(&connection, path, "personal_api_keys")? {
            collect_rows(
                &connection,
                path,
                "SELECT id,user_id,name,key_hash,key_prefix,last_used,created_at
                 FROM personal_api_keys ORDER BY created_at,id",
                |row| {
                    Ok(LegacyPersonalKey {
                        id: row.get(0)?,
                        user_id: row.get(1)?,
                        name: row.get(2)?,
                        key_hash: row.get(3)?,
                        key_prefix: row.get(4)?,
                        last_used: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                },
            )?
        } else {
            Vec::new()
        },
        sessions: if table_exists(&connection, path, "sessions")? {
            collect_rows(
                &connection,
                path,
                "SELECT id,user_id,created_at,expires_at FROM sessions ORDER BY created_at,id",
                |row| {
                    Ok(LegacySession {
                        id: row.get(0)?,
                        user_id: row.get(1)?,
                        created_at: row.get(2)?,
                        expires_at: row.get(3)?,
                    })
                },
            )?
        } else {
            Vec::new()
        },
    })
}

fn load_registry_names(path: &Path) -> Result<BTreeMap<String, String>, LegacyImportError> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let connection = open_legacy(path)?;
    if !table_exists(&connection, path, "projects")? {
        return Ok(BTreeMap::new());
    }
    let rows = collect_rows(
        &connection,
        path,
        "SELECT token,name FROM projects ORDER BY token",
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    Ok(rows.into_iter().collect())
}

fn open_legacy(path: &Path) -> Result<Connection, LegacyImportError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| database_error(path, source))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|source| database_error(path, source))?;
    Ok(connection)
}

fn table_exists(
    connection: &Connection,
    path: &Path,
    table: &str,
) -> Result<bool, LegacyImportError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|source| database_error(path, source))
}

fn collect_rows<T, F>(
    connection: &Connection,
    path: &Path,
    sql: &str,
    map: F,
) -> Result<Vec<T>, LegacyImportError>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut statement = connection
        .prepare(sql)
        .map_err(|source| database_error(path, source))?;
    statement
        .query_map([], map)
        .map_err(|source| database_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| database_error(path, source))
}

fn import_auth_rows(transaction: &Transaction<'_>, legacy: &LegacyAuth) -> rusqlite::Result<()> {
    for user in &legacy.users {
        transaction.execute(
            "INSERT OR IGNORE INTO users(id,email,password_hash,name,created_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                user.id,
                user.email,
                user.password_hash,
                user.name,
                user.created_at
            ],
        )?;
    }
    for organization in &legacy.organizations {
        transaction.execute(
            "INSERT OR IGNORE INTO organizations(id,name,created_at,imported)
             VALUES (?1,?2,?3,0)",
            params![organization.id, organization.name, organization.created_at],
        )?;
    }
    for membership in &legacy.memberships {
        transaction.execute(
            "INSERT OR IGNORE INTO organization_members(organization_id,user_id,role)
             VALUES (?1,?2,?3)",
            params![
                membership.organization_id,
                membership.user_id,
                membership.role
            ],
        )?;
    }
    for project in &legacy.projects {
        transaction.execute(
            "INSERT INTO projects(id,organization_id,name,capture_token,created_at,imported)
             VALUES (?1,?2,?3,?4,?5,0)
             ON CONFLICT(capture_token) DO UPDATE SET
                 organization_id=excluded.organization_id,
                 name=excluded.name,
                 created_at=excluded.created_at,
                 imported=0",
            params![
                project.id,
                project.organization_id,
                project.name,
                project.token,
                project.created_at
            ],
        )?;
    }
    for key in &legacy.personal_keys {
        transaction.execute(
            "INSERT OR IGNORE INTO personal_api_keys(
                 id,user_id,name,key_hash,key_prefix,last_used,created_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                key.id,
                key.user_id,
                key.name,
                key.key_hash,
                key.key_prefix,
                key.last_used,
                key.created_at
            ],
        )?;
    }
    for session in &legacy.sessions {
        transaction.execute(
            "INSERT OR IGNORE INTO auth_sessions(id,user_id,created_at,expires_at)
             VALUES (?1,?2,?3,?4)",
            params![
                session.id,
                session.user_id,
                session.created_at,
                session.expires_at
            ],
        )?;
    }
    Ok(())
}

fn import_orphan_projects(
    transaction: &Transaction<'_>,
    tokens: &std::collections::BTreeSet<String>,
    registry_names: &BTreeMap<String, String>,
) -> rusqlite::Result<()> {
    let mut orphan_tokens = Vec::new();
    for token in tokens {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE capture_token=?1)",
            [token],
            |row| row.get(0),
        )?;
        if !exists {
            orphan_tokens.push(token);
        }
    }
    if orphan_tokens.is_empty() {
        return Ok(());
    }

    transaction.execute(
        "INSERT OR IGNORE INTO organizations(id,name,created_at,imported)
         VALUES (?1,'Imported Projects',0,1)",
        [IMPORTED_ORGANIZATION_ID],
    )?;
    let owner_id = transaction
        .query_row(
            "SELECT id FROM users ORDER BY created_at,id LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(owner_id) = owner_id {
        transaction.execute(
            "INSERT OR IGNORE INTO organization_members(organization_id,user_id,role)
             VALUES (?1,?2,'owner')",
            params![IMPORTED_ORGANIZATION_ID, owner_id],
        )?;
    }
    for token in orphan_tokens {
        let name = registry_names
            .get(token)
            .map(String::as_str)
            .unwrap_or("Imported Project");
        transaction.execute(
            "INSERT OR IGNORE INTO projects(
                 id,organization_id,name,capture_token,created_at,imported
             ) VALUES (?1,?2,?3,?4,0,1)",
            params![
                deterministic_project_id(token),
                IMPORTED_ORGANIZATION_ID,
                name,
                token
            ],
        )?;
    }
    Ok(())
}

fn deterministic_project_id(token: &str) -> String {
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    format!(
        "{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    )
}

fn import_report(transaction: &Transaction<'_>) -> rusqlite::Result<LegacyImportReport> {
    Ok(LegacyImportReport {
        users: row_count(transaction, "users")?,
        organizations: row_count(transaction, "organizations")?,
        projects: row_count(transaction, "projects")?,
        personal_keys: row_count(transaction, "personal_api_keys")?,
        sessions: row_count(transaction, "auth_sessions")?,
    })
}

fn row_count(transaction: &Transaction<'_>, table: &str) -> rusqlite::Result<usize> {
    // `table` is supplied only by the static call sites above.
    let count: i64 =
        transaction.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    Ok(count as usize)
}

fn database_error(path: &Path, source: rusqlite::Error) -> LegacyImportError {
    LegacyImportError::Database {
        path: path.to_path_buf(),
        source,
    }
}

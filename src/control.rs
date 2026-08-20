//! Authoritative project access and workspace state.
//!
//! One dedicated SQLite thread owns `control.db`. Callers receive domain
//! values (`Principal`, `AuthorizedProject`, `Workspace`) rather than raw
//! database handles or untrusted project tokens.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::token;

const COMMAND_CAPACITY: usize = 128;
const SESSION_TTL_SECONDS: i64 = 7 * 24 * 3600;
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY NOT NULL,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS organizations (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    imported INTEGER NOT NULL DEFAULT 0 CHECK(imported IN (0,1))
);
CREATE TABLE IF NOT EXISTS organization_members (
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK(role IN ('owner', 'admin', 'member')),
    PRIMARY KEY (organization_id, user_id)
);
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY NOT NULL,
    organization_id TEXT NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    capture_token TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    imported INTEGER NOT NULL DEFAULT 0 CHECK(imported IN (0,1))
);
CREATE TABLE IF NOT EXISTS auth_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS personal_api_keys (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    key_hash TEXT NOT NULL UNIQUE,
    key_prefix TEXT NOT NULL,
    last_used INTEGER,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON auth_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON auth_sessions(expires_at);
CREATE INDEX IF NOT EXISTS idx_projects_organization ON projects(organization_id);
CREATE INDEX IF NOT EXISTS idx_personal_keys_user ON personal_api_keys(user_id,created_at DESC,id);
CREATE INDEX IF NOT EXISTS idx_personal_keys_hash ON personal_api_keys(key_hash);
"#;

#[derive(Debug)]
pub enum AccessError {
    Database(rusqlite::Error),
    InvalidToken,
    InvalidRequest,
    InvalidCredentials,
    SetupComplete,
    Unauthorized,
    Forbidden,
    NotFound,
    Unavailable,
    InvalidStorage,
    Incompatible { found: i64, supported: i64 },
}

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(f, "control database error: {error}"),
            Self::InvalidToken => f.write_str("invalid project token"),
            Self::InvalidRequest => f.write_str("invalid request"),
            Self::InvalidCredentials => f.write_str("invalid credentials"),
            Self::SetupComplete => f.write_str("setup already completed"),
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::Forbidden => f.write_str("forbidden"),
            Self::NotFound => f.write_str("not found"),
            Self::Unavailable => f.write_str("project access unavailable"),
            Self::InvalidStorage => f.write_str("not a validated Hoglet control database"),
            Self::Incompatible { found, supported } => {
                write!(
                    f,
                    "control schema {found} is newer than supported {supported}"
                )
            }
        }
    }
}

impl std::error::Error for AccessError {}

impl From<rusqlite::Error> for AccessError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Clone)]
pub struct SetupRequest {
    pub email: String,
    pub password: String,
    pub organization_name: String,
    pub project_name: String,
    pub existing_project_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub user: User,
    pub organizations: Vec<Organization>,
}

#[derive(Debug, Clone)]
pub struct SetupResult {
    pub workspace: Workspace,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonalApiKey {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub last_used: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct CreatedPersonalApiKey {
    pub key: PersonalApiKey,
    pub secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    fn parse(value: &str) -> Result<Self, AccessError> {
        match value {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            _ => Err(AccessError::Forbidden),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authentication {
    Session,
    PersonalKey,
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub authentication: Authentication,
}

#[derive(Debug, Clone)]
pub struct AuthorizedProject {
    pub project_id: String,
    pub organization_id: String,
    pub capture_token: String,
    pub role: Role,
    pub principal: Principal,
}

#[derive(Debug, Clone)]
pub struct CaptureProject {
    pub project_id: Option<String>,
    pub token: String,
}

enum Command {
    Shutdown {
        response: std::sync::mpsc::SyncSender<()>,
    },
    SetupRequired {
        response: oneshot::Sender<Result<bool, AccessError>>,
    },
    Setup {
        request: SetupRequest,
        response: oneshot::Sender<Result<SetupResult, AccessError>>,
    },
    Login {
        request: LoginRequest,
        response: oneshot::Sender<Result<SetupResult, AccessError>>,
    },
    Logout {
        session_id: String,
        response: oneshot::Sender<Result<(), AccessError>>,
    },
    CreatePersonalKey {
        principal: Principal,
        name: String,
        response: oneshot::Sender<Result<CreatedPersonalApiKey, AccessError>>,
    },
    ValidatePersonalKey {
        secret: String,
        response: oneshot::Sender<Result<Principal, AccessError>>,
    },
    ListPersonalKeys {
        principal: Principal,
        response: oneshot::Sender<Result<Vec<PersonalApiKey>, AccessError>>,
    },
    RevokePersonalKey {
        principal: Principal,
        key_id: String,
        response: oneshot::Sender<Result<(), AccessError>>,
    },
    CreateOrganization {
        principal: Principal,
        name: String,
        response: oneshot::Sender<Result<Organization, AccessError>>,
    },
    CreateProject {
        principal: Principal,
        organization_id: String,
        name: String,
        response: oneshot::Sender<Result<Project, AccessError>>,
    },
    ValidateSession {
        session_id: String,
        response: oneshot::Sender<Result<Principal, AccessError>>,
    },
    Workspace {
        principal: Principal,
        response: oneshot::Sender<Result<Workspace, AccessError>>,
    },
    AuthorizeProject {
        principal: Principal,
        project_id: String,
        response: oneshot::Sender<Result<AuthorizedProject, AccessError>>,
    },
    AuthorizeCapture {
        token: String,
        response: oneshot::Sender<Result<CaptureProject, AccessError>>,
    },
}

#[derive(Clone)]
pub struct ProjectAccess {
    tx: mpsc::Sender<Command>,
}

pub struct ProjectAccessRuntime {
    tx: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl ProjectAccess {
    pub fn open(path: PathBuf) -> Result<(Self, ProjectAccessRuntime), AccessError> {
        let connection = open_connection(path)?;
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("hoglet-control".into())
            .spawn(move || worker_loop(connection, rx))
            .map_err(|_| AccessError::Unavailable)?;
        Ok((
            Self { tx: tx.clone() },
            ProjectAccessRuntime {
                tx,
                worker: Some(worker),
            },
        ))
    }

    pub async fn setup(&self, request: SetupRequest) -> Result<SetupResult, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::Setup { request, response })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn setup_required(&self) -> Result<bool, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::SetupRequired { response })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn login(&self, request: LoginRequest) -> Result<SetupResult, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::Login { request, response })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn logout(&self, session_id: &str) -> Result<(), AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::Logout {
                session_id: session_id.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn create_personal_key(
        &self,
        principal: &Principal,
        name: &str,
    ) -> Result<CreatedPersonalApiKey, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::CreatePersonalKey {
                principal: principal.clone(),
                name: name.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn validate_personal_key(&self, secret: &str) -> Result<Principal, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::ValidatePersonalKey {
                secret: secret.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn list_personal_keys(
        &self,
        principal: &Principal,
    ) -> Result<Vec<PersonalApiKey>, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::ListPersonalKeys {
                principal: principal.clone(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn revoke_personal_key(
        &self,
        principal: &Principal,
        key_id: &str,
    ) -> Result<(), AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::RevokePersonalKey {
                principal: principal.clone(),
                key_id: key_id.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn create_organization(
        &self,
        principal: &Principal,
        name: &str,
    ) -> Result<Organization, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::CreateOrganization {
                principal: principal.clone(),
                name: name.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn create_project(
        &self,
        principal: &Principal,
        organization_id: &str,
        name: &str,
    ) -> Result<Project, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::CreateProject {
                principal: principal.clone(),
                organization_id: organization_id.into(),
                name: name.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn validate_session(&self, session_id: &str) -> Result<Principal, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::ValidateSession {
                session_id: session_id.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn workspace(&self, principal: &Principal) -> Result<Workspace, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::Workspace {
                principal: principal.clone(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn authorize_project(
        &self,
        principal: &Principal,
        project_id: &str,
    ) -> Result<AuthorizedProject, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::AuthorizeProject {
                principal: principal.clone(),
                project_id: project_id.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }

    pub async fn authorize_capture(&self, token: &str) -> Result<CaptureProject, AccessError> {
        let (response, receive) = oneshot::channel();
        self.tx
            .send(Command::AuthorizeCapture {
                token: token.into(),
                response,
            })
            .await
            .map_err(|_| AccessError::Unavailable)?;
        receive.await.map_err(|_| AccessError::Unavailable)?
    }
}

impl ProjectAccessRuntime {
    pub fn close(mut self) {
        // Runtime ownership, rather than the lifetime of every adapter/router
        // clone, decides when the control worker exits. `try_send` avoids
        // Tokio's `blocking_send` panic when shutdown is invoked by an async
        // application task; a full queue is drained by the dedicated worker.
        let (response, receive) = std::sync::mpsc::sync_channel(1);
        let mut shutdown = Command::Shutdown { response };
        loop {
            match self.tx.try_send(shutdown) {
                Ok(()) => {
                    let _ = receive.recv();
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
                    shutdown = command;
                    std::thread::yield_now();
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn open_connection(path: PathBuf) -> Result<Connection, AccessError> {
    if !path.is_file() {
        return Err(AccessError::InvalidStorage);
    }
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if application_id != crate::storage_bootstrap::CONTROL_APPLICATION_ID {
        return Err(AccessError::InvalidStorage);
    }
    let metadata: Option<(String, String)> = connection
        .query_row(
            "SELECT pair_id,database_role FROM database_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| AccessError::InvalidStorage)?;
    if !matches!(metadata, Some((ref pair_id, ref role)) if !pair_id.is_empty() && role == "control")
    {
        return Err(AccessError::InvalidStorage);
    }
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(AccessError::Incompatible {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    connection.execute_batch(SCHEMA)?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(connection)
}

fn worker_loop(mut connection: Connection, mut rx: mpsc::Receiver<Command>) {
    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::Shutdown { response } => {
                let _ = response.send(());
                break;
            }
            Command::SetupRequired { response } => {
                let result = connection
                    .query_row("SELECT count(*) = 0 FROM users", [], |row| row.get(0))
                    .map_err(AccessError::from);
                let _ = response.send(result);
            }
            Command::Setup { request, response } => {
                let _ = response.send(setup(&mut connection, request));
            }
            Command::Login { request, response } => {
                let _ = response.send(login(&mut connection, request));
            }
            Command::Logout {
                session_id,
                response,
            } => {
                let result = connection
                    .execute("DELETE FROM auth_sessions WHERE id=?1", [&session_id])
                    .map(|_| ())
                    .map_err(AccessError::from);
                let _ = response.send(result);
            }
            Command::CreatePersonalKey {
                principal,
                name,
                response,
            } => {
                let _ = response.send(create_personal_key(&connection, &principal, &name));
            }
            Command::ValidatePersonalKey { secret, response } => {
                let _ = response.send(validate_personal_key(&connection, &secret));
            }
            Command::ListPersonalKeys {
                principal,
                response,
            } => {
                let _ = response.send(list_personal_keys(&connection, &principal));
            }
            Command::RevokePersonalKey {
                principal,
                key_id,
                response,
            } => {
                let result = require_session(&principal).and_then(|()| {
                    match connection.execute(
                        "DELETE FROM personal_api_keys WHERE id=?1 AND user_id=?2",
                        rusqlite::params![key_id, principal.user_id],
                    ) {
                        Ok(0) => Err(AccessError::NotFound),
                        Ok(_) => Ok(()),
                        Err(error) => Err(AccessError::Database(error)),
                    }
                });
                let _ = response.send(result);
            }
            Command::CreateOrganization {
                principal,
                name,
                response,
            } => {
                let _ = response.send(create_organization(&mut connection, &principal, &name));
            }
            Command::CreateProject {
                principal,
                organization_id,
                name,
                response,
            } => {
                let _ = response.send(create_project(
                    &connection,
                    &principal,
                    &organization_id,
                    &name,
                ));
            }
            Command::ValidateSession {
                session_id,
                response,
            } => {
                let _ = response.send(validate_session(&connection, &session_id));
            }
            Command::Workspace {
                principal,
                response,
            } => {
                let _ = response.send(workspace_for(&connection, &principal.user_id));
            }
            Command::AuthorizeProject {
                principal,
                project_id,
                response,
            } => {
                let _ = response.send(authorize_project(&connection, principal, &project_id));
            }
            Command::AuthorizeCapture { token, response } => {
                let _ = response.send(authorize_capture(&connection, &token));
            }
        }
    }
}

fn setup(connection: &mut Connection, request: SetupRequest) -> Result<SetupResult, AccessError> {
    if let Some(value) = request.existing_project_token.as_deref() {
        token::validate(value).map_err(|_| AccessError::InvalidToken)?;
    }
    validate_email(&request.email)?;
    validate_password(&request.password)?;
    validate_name(&request.organization_name)?;
    validate_name(&request.project_name)?;
    let now = Utc::now().timestamp();
    let created_at = Utc::now().timestamp_millis();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let password_hash = hash_password(&request.password)?;
    let email = request.email.trim().to_ascii_lowercase();

    // Hold the SQLite write lock while deciding whether setup is still
    // available, so two Hoglet processes cannot both create the first user.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let user_count: i64 =
        transaction.query_row("SELECT count(*) FROM users", [], |row| row.get(0))?;
    if user_count != 0 {
        return Err(AccessError::SetupComplete);
    }
    let claimed_project: Option<(String, String)> = request
        .existing_project_token
        .as_deref()
        .map(|capture_token| {
            transaction
                .query_row(
                    "SELECT id,organization_id FROM projects WHERE capture_token=?1",
                    [capture_token],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
        })
        .transpose()?
        .flatten();
    if request.existing_project_token.is_some() && claimed_project.is_none() {
        let project_count: i64 =
            transaction.query_row("SELECT count(*) FROM projects", [], |row| row.get(0))?;
        if project_count != 0 {
            return Err(AccessError::NotFound);
        }
    }

    transaction.execute(
        "INSERT INTO users(id,email,password_hash,name,created_at) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![user_id, email, password_hash, email, created_at],
    )?;
    if let Some((_, organization_id)) = &claimed_project {
        transaction.execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES (?1,?2,'owner')",
            rusqlite::params![organization_id, user_id],
        )?;
    } else {
        let organization_id = Uuid::new_v4().to_string();
        let project_id = Uuid::new_v4().to_string();
        let capture_token = request
            .existing_project_token
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("phc_{}", Uuid::new_v4().simple()));
        transaction.execute(
            "INSERT INTO organizations(id,name,created_at) VALUES (?1,?2,?3)",
            rusqlite::params![organization_id, request.organization_name, created_at],
        )?;
        transaction.execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES (?1,?2,'owner')",
            rusqlite::params![organization_id, user_id],
        )?;
        transaction.execute(
            "INSERT INTO projects(id,organization_id,name,capture_token,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                project_id,
                organization_id,
                request.project_name,
                capture_token,
                created_at
            ],
        )?;
    }
    transaction.execute(
        "INSERT OR IGNORE INTO organization_members(organization_id,user_id,role)
         SELECT id,?1,'owner' FROM organizations WHERE imported=1",
        [&user_id],
    )?;
    transaction.execute(
        "INSERT INTO auth_sessions(id,user_id,created_at,expires_at) VALUES (?1,?2,?3,?4)",
        rusqlite::params![session_id, user_id, now, now + SESSION_TTL_SECONDS],
    )?;
    transaction.commit()?;

    let workspace = workspace_for(connection, &user_id)?;
    Ok(SetupResult {
        workspace,
        session_id,
    })
}

fn login(connection: &mut Connection, request: LoginRequest) -> Result<SetupResult, AccessError> {
    validate_email(&request.email).map_err(|_| AccessError::InvalidCredentials)?;
    validate_password(&request.password).map_err(|_| AccessError::InvalidCredentials)?;
    let email = request.email.trim().to_ascii_lowercase();
    let credentials: Option<(String, String)> = connection
        .query_row(
            "SELECT id,password_hash FROM users WHERE email=?1",
            [&email],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (user_id, password_hash) = credentials.ok_or(AccessError::InvalidCredentials)?;
    verify_password(&password_hash, &request.password)?;

    let now = Utc::now().timestamp();
    let session_id = Uuid::new_v4().to_string();
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM auth_sessions WHERE expires_at<=?1", [now])?;
    transaction.execute(
        "INSERT INTO auth_sessions(id,user_id,created_at,expires_at) VALUES (?1,?2,?3,?4)",
        rusqlite::params![session_id, user_id, now, now + SESSION_TTL_SECONDS],
    )?;
    transaction.commit()?;

    Ok(SetupResult {
        workspace: workspace_for(connection, &user_id)?,
        session_id,
    })
}

fn validate_session(connection: &Connection, session_id: &str) -> Result<Principal, AccessError> {
    let now = Utc::now().timestamp();
    let user_id: Option<String> = connection
        .query_row(
            "SELECT user_id FROM auth_sessions WHERE id=?1 AND expires_at>?2",
            rusqlite::params![session_id, now],
            |row| row.get(0),
        )
        .optional()?;
    if user_id.is_none() {
        connection.execute(
            "DELETE FROM auth_sessions WHERE id=?1 AND expires_at<=?2",
            rusqlite::params![session_id, now],
        )?;
    }
    user_id
        .map(|user_id| Principal {
            user_id,
            authentication: Authentication::Session,
        })
        .ok_or(AccessError::Unauthorized)
}

fn create_personal_key(
    connection: &Connection,
    principal: &Principal,
    name: &str,
) -> Result<CreatedPersonalApiKey, AccessError> {
    require_session(principal)?;
    let name = name.trim();
    validate_name(name)?;
    let secret = format!("phx_{}", Uuid::new_v4().simple());
    let key_hash = hex::encode(Sha256::digest(secret.as_bytes()));
    let key = PersonalApiKey {
        id: Uuid::new_v4().to_string(),
        name: name.into(),
        key_prefix: secret.chars().take(12).collect(),
        last_used: None,
        created_at: Utc::now().timestamp(),
    };
    connection.execute(
        "INSERT INTO personal_api_keys(id,user_id,name,key_hash,key_prefix,last_used,created_at)
         VALUES (?1,?2,?3,?4,?5,NULL,?6)",
        rusqlite::params![
            key.id,
            principal.user_id,
            key.name,
            key_hash,
            key.key_prefix,
            key.created_at
        ],
    )?;
    Ok(CreatedPersonalApiKey { key, secret })
}

fn validate_personal_key(connection: &Connection, secret: &str) -> Result<Principal, AccessError> {
    if !secret.starts_with("phx_") || secret.len() != 36 {
        return Err(AccessError::Unauthorized);
    }
    let key_hash = hex::encode(Sha256::digest(secret.as_bytes()));
    let user_id: Option<String> = connection
        .query_row(
            "SELECT user_id FROM personal_api_keys WHERE key_hash=?1",
            [&key_hash],
            |row| row.get(0),
        )
        .optional()?;
    let user_id = user_id.ok_or(AccessError::Unauthorized)?;
    connection.execute(
        "UPDATE personal_api_keys SET last_used=?1 WHERE key_hash=?2",
        rusqlite::params![Utc::now().timestamp(), key_hash],
    )?;
    Ok(Principal {
        user_id,
        authentication: Authentication::PersonalKey,
    })
}

fn list_personal_keys(
    connection: &Connection,
    principal: &Principal,
) -> Result<Vec<PersonalApiKey>, AccessError> {
    require_session(principal)?;
    let mut statement = connection.prepare(
        "SELECT id,name,key_prefix,last_used,created_at FROM personal_api_keys
         WHERE user_id=?1 ORDER BY created_at DESC,id",
    )?;
    statement
        .query_map([&principal.user_id], |row| {
            Ok(PersonalApiKey {
                id: row.get(0)?,
                name: row.get(1)?,
                key_prefix: row.get(2)?,
                last_used: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(AccessError::from)
}

fn create_organization(
    connection: &mut Connection,
    principal: &Principal,
    name: &str,
) -> Result<Organization, AccessError> {
    require_session(principal)?;
    let name = name.trim();
    validate_name(name)?;
    let id = Uuid::new_v4().to_string();
    let transaction = connection.transaction()?;
    let latest: Option<i64> =
        transaction.query_row("SELECT max(created_at) FROM organizations", [], |row| {
            row.get(0)
        })?;
    let created_at = latest
        .and_then(|value| value.checked_add(1))
        .unwrap_or(i64::MIN)
        .max(Utc::now().timestamp_millis());
    transaction.execute(
        "INSERT INTO organizations(id,name,created_at) VALUES (?1,?2,?3)",
        rusqlite::params![id, name, created_at],
    )?;
    transaction.execute(
        "INSERT INTO organization_members(organization_id,user_id,role) VALUES (?1,?2,'owner')",
        rusqlite::params![id, principal.user_id],
    )?;
    transaction.commit()?;
    Ok(Organization {
        id,
        name: name.into(),
        role: Role::Owner,
        projects: Vec::new(),
    })
}

fn create_project(
    connection: &Connection,
    principal: &Principal,
    organization_id: &str,
    name: &str,
) -> Result<Project, AccessError> {
    require_session(principal)?;
    let name = name.trim();
    validate_name(name)?;
    let role: Option<String> = connection
        .query_row(
            "SELECT role FROM organization_members WHERE organization_id=?1 AND user_id=?2",
            rusqlite::params![organization_id, principal.user_id],
            |row| row.get(0),
        )
        .optional()?;
    match role.as_deref().map(Role::parse).transpose()? {
        Some(Role::Owner | Role::Admin) => {}
        Some(Role::Member) | None => return Err(AccessError::Forbidden),
    }
    let project = Project {
        id: Uuid::new_v4().to_string(),
        name: name.into(),
        token: format!("phc_{}", Uuid::new_v4().simple()),
    };
    let latest: Option<i64> = connection.query_row(
        "SELECT max(created_at) FROM projects WHERE organization_id=?1",
        [organization_id],
        |row| row.get(0),
    )?;
    let created_at = latest
        .and_then(|value| value.checked_add(1))
        .unwrap_or(i64::MIN)
        .max(Utc::now().timestamp_millis());
    connection.execute(
        "INSERT INTO projects(id,organization_id,name,capture_token,created_at)
         VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![
            project.id,
            organization_id,
            project.name,
            project.token,
            created_at
        ],
    )?;
    Ok(project)
}

fn authorize_capture(connection: &Connection, value: &str) -> Result<CaptureProject, AccessError> {
    token::validate(value).map_err(|_| AccessError::InvalidToken)?;
    let project_id: Option<String> = connection
        .query_row(
            "SELECT id FROM projects WHERE capture_token=?1",
            [value],
            |row| row.get(0),
        )
        .optional()?;
    project_id
        .map(|project_id| CaptureProject {
            project_id: Some(project_id),
            token: value.into(),
        })
        .ok_or(AccessError::Unauthorized)
}

fn authorize_project(
    connection: &Connection,
    principal: Principal,
    project_id: &str,
) -> Result<AuthorizedProject, AccessError> {
    let row: Option<(String, String, String)> = connection
        .query_row(
            "SELECT p.organization_id,p.capture_token,m.role
             FROM projects p
             JOIN organization_members m ON m.organization_id=p.organization_id
             WHERE p.id=?1 AND m.user_id=?2",
            rusqlite::params![project_id, principal.user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (organization_id, capture_token, role) = row.ok_or(AccessError::Forbidden)?;
    Ok(AuthorizedProject {
        project_id: project_id.into(),
        organization_id,
        capture_token,
        role: Role::parse(&role)?,
        principal,
    })
}

fn workspace_for(connection: &Connection, user_id: &str) -> Result<Workspace, AccessError> {
    let user: User = connection
        .query_row(
            "SELECT id,email,name FROM users WHERE id=?1",
            [user_id],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    email: row.get(1)?,
                    name: row.get(2)?,
                })
            },
        )
        .optional()?
        .ok_or(AccessError::Unauthorized)?;

    let mut organization_statement = connection.prepare(
        "SELECT o.id,o.name,m.role
         FROM organizations o
         JOIN organization_members m ON m.organization_id=o.id
         WHERE m.user_id=?1 ORDER BY o.created_at,o.id",
    )?;
    let organization_rows = organization_statement.query_map([user_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut organizations = Vec::new();
    for row in organization_rows {
        let (id, name, role) = row?;
        let mut project_statement = connection.prepare(
            "SELECT id,name,capture_token FROM projects WHERE organization_id=?1 ORDER BY created_at,id",
        )?;
        let projects = project_statement
            .query_map([&id], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    token: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        organizations.push(Organization {
            id,
            name,
            role: Role::parse(&role)?,
            projects,
        });
    }
    Ok(Workspace {
        user,
        organizations,
    })
}

fn hash_password(password: &str) -> Result<String, AccessError> {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| AccessError::InvalidCredentials)
}

fn validate_email(email: &str) -> Result<(), AccessError> {
    let email = email.trim();
    if email.is_empty() || email.len() > 254 || !email.contains('@') {
        return Err(AccessError::InvalidRequest);
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), AccessError> {
    if !(12..=1024).contains(&password.len()) {
        return Err(AccessError::InvalidRequest);
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), AccessError> {
    if name.trim().is_empty() || name.len() > 128 {
        return Err(AccessError::InvalidRequest);
    }
    Ok(())
}

fn require_session(principal: &Principal) -> Result<(), AccessError> {
    if principal.authentication != Authentication::Session {
        return Err(AccessError::Forbidden);
    }
    Ok(())
}

fn verify_password(hash: &str, password: &str) -> Result<(), AccessError> {
    let parsed = PasswordHash::new(hash).map_err(|_| AccessError::InvalidCredentials)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AccessError::InvalidCredentials)
}

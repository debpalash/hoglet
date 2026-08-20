//! Authentication store — users, orgs, projects, sessions, API keys.
//! Single SQLite database behind a mutex.

use std::path::Path;
use std::sync::Mutex;

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use chrono::Utc;
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ── Types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Org {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrgMember {
    pub org_id: String,
    pub user_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonalApiKey {
    pub id: String,
    pub name: String,
    pub key_prefix: String,
    pub last_used: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeResponse {
    pub user: Option<User>,
    pub orgs: Vec<Org>,
}

#[derive(Debug)]
pub enum AuthError {
    Db(rusqlite::Error),
    InvalidPassword,
    UserNotFound,
    EmailTaken,
    Unauthorized,
    NotFound,
    Forbidden,
}

impl From<rusqlite::Error> for AuthError {
    fn from(e: rusqlite::Error) -> Self {
        AuthError::Db(e)
    }
}

// ── AuthStore ─────────────────────────────────────────────────────

pub struct AuthStore {
    conn: Mutex<Connection>,
}

const SESSION_TTL_SECONDS: i64 = 7 * 24 * 3600;

impl AuthStore {
    pub fn open(path: &Path) -> Result<Self, AuthError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Self::migrate(&conn)?;
        Ok(AuthStore {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, AuthError> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(AuthStore {
            conn: Mutex::new(conn),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), AuthError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT NOT NULL PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                pw_hash TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS orgs (
                id TEXT NOT NULL PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS org_members (
                org_id TEXT NOT NULL REFERENCES orgs(id),
                user_id TEXT NOT NULL REFERENCES users(id),
                role TEXT NOT NULL DEFAULT 'member',
                PRIMARY KEY (org_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT NOT NULL PRIMARY KEY,
                org_id TEXT NOT NULL REFERENCES orgs(id),
                name TEXT NOT NULL,
                token TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS personal_api_keys (
                id TEXT NOT NULL PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(id),
                name TEXT NOT NULL,
                key_hash TEXT NOT NULL,
                key_prefix TEXT NOT NULL,
                last_used INTEGER,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT NOT NULL PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(id),
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    pub fn is_empty(&self) -> Result<bool, AuthError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
        Ok(count == 0)
    }

    // ── First-run setup ──────────────────────────────────────

    pub fn setup(
        &self,
        email: &str,
        password: &str,
        org_name: &str,
    ) -> Result<(User, Org, Project), AuthError> {
        let conn = self.conn.lock().unwrap();

        let count: i64 = conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
        if count > 0 {
            return Err(AuthError::Forbidden);
        }

        let now = Utc::now().timestamp();
        let uid = Uuid::new_v4().to_string();
        let pw_hash = hash_password(password);
        let oid = Uuid::new_v4().to_string();
        let pid = Uuid::new_v4().to_string();
        let token = format!("phc_{}", Uuid::new_v4().to_string().replace('-', ""));

        conn.execute(
            "INSERT INTO users (id, email, pw_hash, name, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![uid, email, pw_hash, email, now],
        )?;
        conn.execute(
            "INSERT INTO orgs (id, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![oid, org_name, now],
        )?;
        conn.execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, 'owner')",
            rusqlite::params![oid, uid],
        )?;
        conn.execute(
            "INSERT INTO projects (id, org_id, name, token, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![pid, oid, "Default", token, now],
        )?;

        Ok((
            User {
                id: uid.clone(),
                email: email.into(),
                name: email.into(),
            },
            Org {
                id: oid.clone(),
                name: org_name.into(),
            },
            Project {
                id: pid,
                org_id: oid,
                name: "Default".into(),
                token,
            },
        ))
    }

    // ── Login / sessions ─────────────────────────────────────

    pub fn login(&self, email: &str, password: &str) -> Result<(User, String), AuthError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT id, email, pw_hash, name FROM users WHERE email = ?1")?;
        let (uid, uemail, pw_hash, uname): (String, String, String, String) = stmt
            .query_row([email], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(|_| AuthError::InvalidPassword)?;

        verify_password(&pw_hash, password).map_err(|_| AuthError::InvalidPassword)?;

        let now = Utc::now().timestamp();
        let sid = Uuid::new_v4().to_string();
        let expires = now + SESSION_TTL_SECONDS;
        conn.execute(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![sid, uid, now, expires],
        )?;

        Ok((
            User {
                id: uid,
                email: uemail,
                name: uname,
            },
            sid,
        ))
    }

    pub fn validate_session(&self, session_id: &str) -> Result<User, AuthError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        let mut stmt = conn.prepare_cached(
            "SELECT u.id, u.email, u.name FROM sessions s
             JOIN users u ON s.user_id = u.id
             WHERE s.id = ?1 AND s.expires_at > ?2",
        )?;
        stmt.query_row(rusqlite::params![session_id, now], |r| {
            Ok(User {
                id: r.get(0)?,
                email: r.get(1)?,
                name: r.get(2)?,
            })
        })
        .map_err(|_| AuthError::Unauthorized)
    }

    pub fn logout(&self, session_id: &str) -> Result<(), AuthError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }

    // ── Personal API keys ────────────────────────────────────

    pub fn create_api_key(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<(PersonalApiKey, String), AuthError> {
        let conn = self.conn.lock().unwrap();
        let key = format!("phx_{}", Uuid::new_v4().to_string().replace('-', ""));
        let key_prefix = key[..12].to_string();
        let key_hash = hex::encode(Sha256::digest(key.as_bytes()));
        let now = Utc::now().timestamp();
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT INTO personal_api_keys (id, user_id, name, key_hash, key_prefix, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, user_id, name, key_hash, key_prefix, now],
        )?;

        Ok((
            PersonalApiKey {
                id,
                name: name.into(),
                key_prefix,
                last_used: None,
                created_at: now,
            },
            key,
        ))
    }

    pub fn validate_api_key(&self, key: &str) -> Result<User, AuthError> {
        if !key.starts_with("phx_") || key.len() < 20 {
            return Err(AuthError::Unauthorized);
        }
        let conn = self.conn.lock().unwrap();
        let key_hash = hex::encode(Sha256::digest(key.as_bytes()));
        let now = Utc::now().timestamp();

        conn.execute(
            "UPDATE personal_api_keys SET last_used = ?1 WHERE key_hash = ?2",
            rusqlite::params![now, key_hash],
        )?;

        let mut stmt = conn.prepare_cached(
            "SELECT u.id, u.email, u.name FROM personal_api_keys k
             JOIN users u ON k.user_id = u.id
             WHERE k.key_hash = ?1",
        )?;
        stmt.query_row([key_hash], |r| {
            Ok(User {
                id: r.get(0)?,
                email: r.get(1)?,
                name: r.get(2)?,
            })
        })
        .map_err(|_| AuthError::Unauthorized)
    }

    pub fn list_api_keys(&self, user_id: &str) -> Result<Vec<PersonalApiKey>, AuthError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, name, key_prefix, last_used, created_at
             FROM personal_api_keys WHERE user_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(PersonalApiKey {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    key_prefix: r.get(2)?,
                    last_used: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn revoke_api_key(&self, user_id: &str, key_id: &str) -> Result<(), AuthError> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "DELETE FROM personal_api_keys WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![key_id, user_id],
        )?;
        if affected == 0 {
            Err(AuthError::NotFound)
        } else {
            Ok(())
        }
    }

    // ── Orgs ─────────────────────────────────────────────────

    pub fn list_orgs(&self, user_id: &str) -> Result<Vec<Org>, AuthError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT o.id, o.name FROM orgs o
             JOIN org_members m ON o.id = m.org_id
             WHERE m.user_id = ?1 ORDER BY o.created_at",
        )?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(Org {
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn create_org(&self, user_id: &str, name: &str) -> Result<Org, AuthError> {
        let conn = self.conn.lock().unwrap();
        let oid = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO orgs (id, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![oid, name, now],
        )?;
        conn.execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, 'owner')",
            rusqlite::params![oid, user_id],
        )?;
        Ok(Org {
            id: oid,
            name: name.into(),
        })
    }

    pub fn list_projects(&self, org_id: &str) -> Result<Vec<Project>, AuthError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT id, org_id, name, token, created_at FROM projects WHERE org_id = ?1 ORDER BY created_at")?;
        let rows = stmt
            .query_map([org_id], |r| {
                Ok(Project {
                    id: r.get(0)?,
                    org_id: r.get(1)?,
                    name: r.get(2)?,
                    token: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn create_project(&self, org_id: &str, name: &str) -> Result<Project, AuthError> {
        let conn = self.conn.lock().unwrap();
        let pid = Uuid::new_v4().to_string();
        let token = format!("phc_{}", Uuid::new_v4().to_string().replace('-', ""));
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO projects (id, org_id, name, token, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![pid, org_id, name, token, now],
        )?;
        Ok(Project {
            id: pid,
            org_id: org_id.into(),
            name: name.into(),
            token,
        })
    }

    pub fn get_user_role(&self, user_id: &str, org_id: &str) -> Result<String, AuthError> {
        let conn = self.conn.lock().unwrap();
        let role: String = conn
            .query_row(
                "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
                rusqlite::params![org_id, user_id],
                |r| r.get(0),
            )
            .map_err(|_| AuthError::Forbidden)?;
        Ok(role)
    }
}

// ── Helpers ───────────────────────────────────────────────────────

fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hash")
        .to_string()
}

fn verify_password(hash: &str, password: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(hash).map_err(|_| AuthError::InvalidPassword)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::InvalidPassword)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_creates_user_org_project() {
        let store = AuthStore::open_in_memory().unwrap();
        assert!(store.is_empty().unwrap());
        let (user, org, project) = store.setup("a@b.com", "pass123", "MyOrg").unwrap();
        assert_eq!(user.email, "a@b.com");
        assert_eq!(org.name, "MyOrg");
        assert!(project.token.starts_with("phc_"));
        assert!(!store.is_empty().unwrap());
    }

    #[test]
    fn setup_fails_when_users_exist() {
        let store = AuthStore::open_in_memory().unwrap();
        store.setup("a@b.com", "p", "O").unwrap();
        assert!(store.setup("b@c.com", "p", "O").is_err());
    }

    #[test]
    fn login_creates_session() {
        let store = AuthStore::open_in_memory().unwrap();
        store.setup("a@b.com", "pass123", "O").unwrap();
        let (user, sid) = store.login("a@b.com", "pass123").unwrap();
        assert_eq!(user.email, "a@b.com");

        let me = store.validate_session(&sid).unwrap();
        assert_eq!(me.email, "a@b.com");

        store.logout(&sid).unwrap();
        assert!(store.validate_session(&sid).is_err());
    }

    #[test]
    fn bad_password_rejected() {
        let store = AuthStore::open_in_memory().unwrap();
        store.setup("a@b.com", "pass123", "O").unwrap();
        assert!(store.login("a@b.com", "wrong").is_err());
    }

    #[test]
    fn api_key_roundtrip() {
        let store = AuthStore::open_in_memory().unwrap();
        let (user, _, _) = store.setup("a@b.com", "p", "O").unwrap();
        let (_pk, full_key) = store.create_api_key(&user.id, "my key").unwrap();
        assert!(full_key.starts_with("phx_"));

        let me = store.validate_api_key(&full_key).unwrap();
        assert_eq!(me.email, "a@b.com");

        let keys = store.list_api_keys(&user.id).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key_prefix, full_key[..12]);

        store.revoke_api_key(&user.id, &keys[0].id).unwrap();
        assert!(store.validate_api_key(&full_key).is_err());
    }

    #[test]
    fn orgs_and_projects() {
        let store = AuthStore::open_in_memory().unwrap();
        let (user, _, _) = store.setup("a@b.com", "p", "O").unwrap();

        let orgs = store.list_orgs(&user.id).unwrap();
        assert_eq!(orgs.len(), 1);

        let org2 = store.create_org(&user.id, "Org2").unwrap();
        assert_eq!(org2.name, "Org2");

        let proj = store.create_project(&org2.id, "MyProject").unwrap();
        assert!(proj.token.starts_with("phc_"));

        let projects = store.list_projects(&org2.id).unwrap();
        assert_eq!(projects.len(), 1);
    }
}

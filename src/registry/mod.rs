//! Project/token registry (spec/README.md "Security and tenancy").
//!
//! Turns a token from an opaque namespace into an authenticated project.
//! Stored in SQLite alongside identity.
//!
//! Compatibility-preserving default: **open mode**. With zero projects
//! registered, any shape-valid token is accepted — a single-tenant hobby
//! install needs no setup. The moment one project is created, the registry
//! is authoritative: only registered tokens are accepted. This keeps the
//! drop-in promise for hobby users and gives real auth to multi-project ones
//! without a mode flag.

use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::token;

pub struct Registry {
    conn: Mutex<Connection>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Token is accepted for ingest.
    Accept,
    /// Token is shape-invalid or not a known project (in closed mode).
    Reject,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    token TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL
);
";

impl Registry {
    pub fn open(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::open(Connection::open_in_memory()?)
    }

    pub fn project_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Register a project. `created_at` is caller-supplied (no clock in the
    /// registry) as RFC3339.
    pub fn create_project(&self, tok: &str, name: &str, created_at: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO projects (token, name, created_at) VALUES (?1, ?2, ?3)",
            params![tok, name, created_at],
        )?;
        Ok(())
    }

    fn is_registered(&self, tok: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM projects WHERE token = ?1",
            params![tok],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    /// The gate: shape must always be valid; authenticity is required only
    /// once at least one project exists (closed mode).
    pub fn check(&self, tok: &str) -> Decision {
        if token::validate(tok).is_err() {
            return Decision::Reject;
        }
        // Open mode: no projects → accept any valid-shape token.
        if self.project_count() == 0 {
            return Decision::Accept;
        }
        if self.is_registered(tok) {
            Decision::Accept
        } else {
            Decision::Reject
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_mode_accepts_any_valid_token() {
        let r = Registry::in_memory().unwrap();
        assert_eq!(r.check("phc_anything"), Decision::Accept);
    }

    #[test]
    fn open_mode_still_rejects_bad_shape() {
        let r = Registry::in_memory().unwrap();
        assert_eq!(r.check("phx_personal_key"), Decision::Reject);
        assert_eq!(r.check(""), Decision::Reject);
    }

    #[test]
    fn closed_mode_requires_registration() {
        let r = Registry::in_memory().unwrap();
        r.create_project("phc_known", "Acme", "2026-07-21T00:00:00Z").unwrap();
        assert_eq!(r.check("phc_known"), Decision::Accept);
        assert_eq!(r.check("phc_unknown"), Decision::Reject);
    }

    #[test]
    fn creating_project_flips_to_closed_mode() {
        let r = Registry::in_memory().unwrap();
        assert_eq!(r.check("phc_x"), Decision::Accept); // open
        r.create_project("phc_y", "Y", "2026-07-21T00:00:00Z").unwrap();
        assert_eq!(r.check("phc_x"), Decision::Reject); // now closed
        assert_eq!(r.check("phc_y"), Decision::Accept);
    }
}

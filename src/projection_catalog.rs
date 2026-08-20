//! Read-only catalog queries over rebuildable `projections.db` state.
//!
//! Publication owns writes and exactly-once ordering. This module validates the
//! database role before opening a separate read connection and exposes only
//! project-scoped autocomplete operations.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::storage_bootstrap::PROJECTIONS_APPLICATION_ID;

#[derive(Debug)]
pub enum ProjectionCatalogError {
    InvalidStorage,
    InvalidSource,
    Database(rusqlite::Error),
    Unavailable,
}

impl std::fmt::Display for ProjectionCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStorage => formatter.write_str("not a validated projections database"),
            Self::InvalidSource => formatter.write_str("catalog source must be event or person"),
            Self::Database(error) => {
                write!(formatter, "projection catalog database error: {error}")
            }
            Self::Unavailable => formatter.write_str("projection catalog unavailable"),
        }
    }
}

impl std::error::Error for ProjectionCatalogError {}

impl From<rusqlite::Error> for ProjectionCatalogError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PropertyKey {
    pub key: String,
    pub source: String,
    pub type_guess: String,
    pub count: i64,
}

pub struct ProjectionCatalog {
    connection: Mutex<Connection>,
}

impl ProjectionCatalog {
    pub fn open(path: &Path) -> Result<Self, ProjectionCatalogError> {
        if !path.is_file() {
            return Err(ProjectionCatalogError::InvalidStorage);
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        validate_projection_database(&connection)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        // Prove the publication-owned catalog schema exists without mutating it.
        connection
            .prepare("SELECT project_id,name FROM event_names LIMIT 0")
            .map_err(|_| ProjectionCatalogError::InvalidStorage)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn event_names(
        &self,
        project_id: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<String>, ProjectionCatalogError> {
        let connection = self.lock()?;
        let pattern = format!("{}%", escape_like(prefix));
        let limit = i64::try_from(limit.clamp(1, 200)).unwrap_or(200);
        let mut statement = connection.prepare_cached(
            "SELECT name FROM event_names
             WHERE project_id=?1 AND name LIKE ?2 ESCAPE '\\'
             ORDER BY count DESC,name LIMIT ?3",
        )?;
        Ok(statement
            .query_map(params![project_id, pattern, limit], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn property_keys(
        &self,
        project_id: &str,
        source: &str,
    ) -> Result<Vec<PropertyKey>, ProjectionCatalogError> {
        if !matches!(source, "event" | "person") {
            return Err(ProjectionCatalogError::InvalidSource);
        }
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT key,source,type_guess,count FROM property_keys
             WHERE project_id=?1 AND source=?2 ORDER BY count DESC,key LIMIT 500",
        )?;
        Ok(statement
            .query_map(params![project_id, source], |row| {
                Ok(PropertyKey {
                    key: row.get(0)?,
                    source: row.get(1)?,
                    type_guess: row.get(2)?,
                    count: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn property_values(
        &self,
        project_id: &str,
        key: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<String>, ProjectionCatalogError> {
        let connection = self.lock()?;
        let pattern = format!("{}%", escape_like(prefix));
        let limit = i64::try_from(limit.clamp(1, 200)).unwrap_or(200);
        let mut statement = connection.prepare_cached(
            "SELECT value FROM property_values
             WHERE project_id=?1 AND key=?2 AND value LIKE ?3 ESCAPE '\\'
             ORDER BY count DESC,value LIMIT ?4",
        )?;
        Ok(statement
            .query_map(params![project_id, key, pattern, limit], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ProjectionCatalogError> {
        self.connection
            .lock()
            .map_err(|_| ProjectionCatalogError::Unavailable)
    }
}

fn validate_projection_database(connection: &Connection) -> Result<(), ProjectionCatalogError> {
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|_| ProjectionCatalogError::InvalidStorage)?;
    if application_id != PROJECTIONS_APPLICATION_ID {
        return Err(ProjectionCatalogError::InvalidStorage);
    }
    let metadata: Option<(String, String)> = connection
        .query_row(
            "SELECT pair_id,database_role FROM database_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| ProjectionCatalogError::InvalidStorage)?;
    if !matches!(metadata, Some((ref pair_id, ref role)) if !pair_id.is_empty() && role == "projections")
    {
        return Err(ProjectionCatalogError::InvalidStorage);
    }
    Ok(())
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

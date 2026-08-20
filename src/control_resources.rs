//! Authoritative project resources stored in the validated `control.db`.
//!
//! This module is the single project-scoping seam for feature flags, saved
//! insights, dashboards, tiles, and share links. Capture tokens are deliberately
//! absent from its interface: callers must arrive with an Authorized Project's
//! stable project id.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::flags::Variant;
use crate::query::ir::Query;
use crate::query::supported::SupportedQuery;
use crate::storage_bootstrap::CONTROL_APPLICATION_ID;

const MAX_KEY_BYTES: usize = 256;
const MAX_NAME_BYTES: usize = 512;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS feature_flags (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    active INTEGER NOT NULL CHECK(active IN (0, 1)),
    rollout_percentage REAL NOT NULL CHECK(rollout_percentage >= 0 AND rollout_percentage <= 100),
    variants TEXT NOT NULL DEFAULT '[]',
    payload TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(project_id, key)
);

CREATE TABLE IF NOT EXISTS saved_insights (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    query_ir TEXT NOT NULL,
    query_supported INTEGER NOT NULL CHECK(query_supported IN (0, 1)),
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(project_id, id)
);

CREATE TABLE IF NOT EXISTS dashboards (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(project_id, id)
);

CREATE TABLE IF NOT EXISTS dashboard_tiles (
    project_id TEXT NOT NULL,
    dashboard_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK(position >= 0),
    insight_id TEXT NOT NULL,
    grid_x INTEGER NOT NULL CHECK(grid_x >= 0),
    grid_y INTEGER NOT NULL CHECK(grid_y >= 0),
    grid_w INTEGER NOT NULL CHECK(grid_w > 0),
    grid_h INTEGER NOT NULL CHECK(grid_h > 0),
    PRIMARY KEY(project_id, dashboard_id, position),
    FOREIGN KEY(project_id, dashboard_id)
        REFERENCES dashboards(project_id, id) ON DELETE CASCADE,
    FOREIGN KEY(project_id, insight_id)
        REFERENCES saved_insights(project_id, id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS share_links (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    token TEXT NOT NULL UNIQUE,
    insight_id TEXT,
    dashboard_id TEXT,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    PRIMARY KEY(project_id, id),
    CHECK(
        (insight_id IS NOT NULL AND dashboard_id IS NULL) OR
        (insight_id IS NULL AND dashboard_id IS NOT NULL)
    ),
    FOREIGN KEY(project_id, insight_id)
        REFERENCES saved_insights(project_id, id) ON DELETE CASCADE,
    FOREIGN KEY(project_id, dashboard_id)
        REFERENCES dashboards(project_id, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_feature_flags_project
    ON feature_flags(project_id, key);
CREATE INDEX IF NOT EXISTS idx_saved_insights_project_updated
    ON saved_insights(project_id, updated_at DESC, id);
CREATE INDEX IF NOT EXISTS idx_dashboards_project_updated
    ON dashboards(project_id, updated_at DESC, id);
CREATE INDEX IF NOT EXISTS idx_dashboard_tiles_insight
    ON dashboard_tiles(project_id, insight_id);
CREATE INDEX IF NOT EXISTS idx_share_links_project_created
    ON share_links(project_id, created_at DESC, id);
"#;

#[derive(Debug)]
pub enum ControlResourceError {
    Database(rusqlite::Error),
    InvalidStorage,
    Unavailable,
    NotFound,
    Conflict,
    InvalidResource {
        field: &'static str,
        message: String,
    },
    InvalidQuery {
        field: String,
        message: String,
    },
}

impl std::fmt::Display for ControlResourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "control resource database error: {error}"),
            Self::InvalidStorage => formatter.write_str("not a validated Hoglet control database"),
            Self::Unavailable => formatter.write_str("control resources unavailable"),
            Self::NotFound => formatter.write_str("resource not found"),
            Self::Conflict => formatter.write_str("resource conflict"),
            Self::InvalidResource { field, message } => {
                write!(formatter, "invalid {field}: {message}")
            }
            Self::InvalidQuery { field, message } => {
                write!(formatter, "invalid query at {field}: {message}")
            }
        }
    }
}

impl std::error::Error for ControlResourceError {}

impl From<rusqlite::Error> for ControlResourceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureFlag {
    pub key: String,
    pub active: bool,
    pub rollout_percentage: f64,
    #[serde(default)]
    pub variants: Vec<Variant>,
    pub payload: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedFeatureFlag {
    pub flag: FeatureFlag,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsightDraft {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub query_ir: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedInsight {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub query_ir: Value,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedInsight {
    pub id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub query_ir: Value,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardDraft {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedDashboard {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tiles: Vec<DashboardTileInput>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardTileInput {
    pub insight_id: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default = "default_tile_width")]
    pub w: i32,
    #[serde(default = "default_tile_height")]
    pub h: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardTile {
    pub insight_id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insight: Option<SavedInsight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub tiles: Vec<DashboardTile>,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareTarget {
    Insight,
    Dashboard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareLink {
    pub id: String,
    pub project_id: String,
    pub object_type: ShareTarget,
    pub object_id: String,
    pub token: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedShareLink {
    pub id: String,
    pub token: String,
    pub target: ShareTarget,
    pub object_id: String,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

pub struct ControlResources {
    connection: Mutex<Connection>,
}

impl ControlResources {
    /// Open a database whose identity has already been established by storage
    /// bootstrap. Validation precedes pragmas and schema writes, so passing an
    /// arbitrary SQLite file never upgrades it into a control database.
    pub fn open(path: &Path) -> Result<Self, ControlResourceError> {
        if !path.is_file() {
            return Err(ControlResourceError::InvalidStorage);
        }
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        validate_control_database(&connection)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(SCHEMA)?;
        transaction.commit()?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn list_flags(&self, project_id: &str) -> Result<Vec<FeatureFlag>, ControlResourceError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT key,active,rollout_percentage,variants,payload
             FROM feature_flags WHERE project_id=?1 ORDER BY key",
        )?;
        let flags = statement
            .query_map([project_id], row_to_flag)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(flags)
    }

    pub fn get_flag(
        &self,
        project_id: &str,
        key: &str,
    ) -> Result<FeatureFlag, ControlResourceError> {
        let connection = self.lock()?;
        load_flag(&connection, project_id, key)
    }

    pub fn create_flag(
        &self,
        project_id: &str,
        flag: &FeatureFlag,
    ) -> Result<FeatureFlag, ControlResourceError> {
        validate_flag(flag)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project(&transaction, project_id)?;
        let now = Utc::now().timestamp();
        let variants = serde_json::to_string(&flag.variants).map_err(invalid_resource_json)?;
        transaction
            .execute(
                "INSERT INTO feature_flags(
                    project_id,key,active,rollout_percentage,variants,payload,created_at,updated_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
                params![
                    project_id,
                    flag.key,
                    flag.active,
                    flag.rollout_percentage,
                    variants,
                    flag.payload,
                    now
                ],
            )
            .map_err(map_write_error)?;
        transaction.commit()?;
        load_flag(&connection, project_id, &flag.key)
    }

    pub fn update_flag(
        &self,
        project_id: &str,
        key: &str,
        flag: &FeatureFlag,
    ) -> Result<FeatureFlag, ControlResourceError> {
        let connection = self.lock()?;
        ensure_flag(&connection, project_id, key)?;
        if key != flag.key {
            return Err(ControlResourceError::Conflict);
        }
        validate_flag(flag)?;
        let variants = serde_json::to_string(&flag.variants).map_err(invalid_resource_json)?;
        connection.execute(
            "UPDATE feature_flags SET
                active=?3,rollout_percentage=?4,variants=?5,payload=?6,updated_at=?7
             WHERE project_id=?1 AND key=?2",
            params![
                project_id,
                key,
                flag.active,
                flag.rollout_percentage,
                variants,
                flag.payload,
                Utc::now().timestamp()
            ],
        )?;
        load_flag(&connection, project_id, key)
    }

    pub fn delete_flag(&self, project_id: &str, key: &str) -> Result<(), ControlResourceError> {
        let connection = self.lock()?;
        deleted(connection.execute(
            "DELETE FROM feature_flags WHERE project_id=?1 AND key=?2",
            params![project_id, key],
        )?)
    }

    /// Migration-only upsert preserving the historical timestamps.
    pub fn import_flag(
        &self,
        project_id: &str,
        imported: ImportedFeatureFlag,
    ) -> Result<FeatureFlag, ControlResourceError> {
        validate_flag(&imported.flag)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project(&transaction, project_id)?;
        let variants =
            serde_json::to_string(&imported.flag.variants).map_err(invalid_resource_json)?;
        transaction
            .execute(
                "INSERT INTO feature_flags(
                    project_id,key,active,rollout_percentage,variants,payload,created_at,updated_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(project_id,key) DO UPDATE SET
                    active=excluded.active,
                    rollout_percentage=excluded.rollout_percentage,
                    variants=excluded.variants,
                    payload=excluded.payload,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at",
                params![
                    project_id,
                    imported.flag.key,
                    imported.flag.active,
                    imported.flag.rollout_percentage,
                    variants,
                    imported.flag.payload,
                    imported.created_at,
                    imported.updated_at
                ],
            )
            .map_err(map_write_error)?;
        transaction.commit()?;
        load_flag(&connection, project_id, &imported.flag.key)
    }

    pub fn list_insights(
        &self,
        project_id: &str,
    ) -> Result<Vec<SavedInsight>, ControlResourceError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT id,project_id,name,description,query_ir,created_by,created_at,updated_at
             FROM saved_insights WHERE project_id=?1 ORDER BY updated_at DESC,id",
        )?;
        let insights = statement
            .query_map([project_id], row_to_insight)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(insights)
    }

    pub fn get_insight(
        &self,
        project_id: &str,
        insight_id: &str,
    ) -> Result<SavedInsight, ControlResourceError> {
        let connection = self.lock()?;
        load_insight(&connection, project_id, insight_id)
    }

    pub fn create_insight(
        &self,
        project_id: &str,
        created_by: &str,
        draft: &InsightDraft,
    ) -> Result<SavedInsight, ControlResourceError> {
        validate_name(&draft.name)?;
        validate_required("created_by", created_by, MAX_NAME_BYTES)?;
        let query_ir = supported_query_json(&draft.query_ir)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project(&transaction, project_id)?;
        let id = Uuid::now_v7().to_string();
        let now = Utc::now().timestamp();
        transaction.execute(
            "INSERT INTO saved_insights(
                project_id,id,name,description,query_ir,query_supported,
                created_by,created_at,updated_at
             ) VALUES (?1,?2,?3,?4,?5,1,?6,?7,?7)",
            params![
                project_id,
                id,
                draft.name,
                draft.description,
                query_ir,
                created_by,
                now
            ],
        )?;
        transaction.commit()?;
        load_insight(&connection, project_id, &id)
    }

    pub fn update_insight(
        &self,
        project_id: &str,
        insight_id: &str,
        draft: &InsightDraft,
    ) -> Result<SavedInsight, ControlResourceError> {
        let connection = self.lock()?;
        ensure_insight(&connection, project_id, insight_id)?;
        validate_name(&draft.name)?;
        let query_ir = supported_query_json(&draft.query_ir)?;
        connection.execute(
            "UPDATE saved_insights SET
                name=?3,description=?4,query_ir=?5,query_supported=1,updated_at=?6
             WHERE project_id=?1 AND id=?2",
            params![
                project_id,
                insight_id,
                draft.name,
                draft.description,
                query_ir,
                Utc::now().timestamp()
            ],
        )?;
        load_insight(&connection, project_id, insight_id)
    }

    pub fn delete_insight(
        &self,
        project_id: &str,
        insight_id: &str,
    ) -> Result<(), ControlResourceError> {
        let connection = self.lock()?;
        deleted(connection.execute(
            "DELETE FROM saved_insights WHERE project_id=?1 AND id=?2",
            params![project_id, insight_id],
        )?)
    }

    /// Migration-only seam for preserving historical query kinds that the live
    /// query lane cannot yet execute. The value must still deserialize as the
    /// broad Query IR; only the SupportedQuery requirement is relaxed.
    pub fn import_insight(
        &self,
        project_id: &str,
        insight: ImportedInsight,
    ) -> Result<SavedInsight, ControlResourceError> {
        validate_required("id", &insight.id, MAX_NAME_BYTES)?;
        validate_name(&insight.name)?;
        validate_required("created_by", &insight.created_by, MAX_NAME_BYTES)?;
        let (query_ir, supported) = imported_query_json(&insight.query_ir)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project(&transaction, project_id)?;
        transaction
            .execute(
                "INSERT INTO saved_insights(
                    project_id,id,name,description,query_ir,query_supported,
                    created_by,created_at,updated_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(project_id,id) DO UPDATE SET
                    name=excluded.name,
                    description=excluded.description,
                    query_ir=excluded.query_ir,
                    query_supported=excluded.query_supported,
                    created_by=excluded.created_by,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at",
                params![
                    project_id,
                    insight.id,
                    insight.name,
                    insight.description,
                    query_ir,
                    supported,
                    insight.created_by,
                    insight.created_at,
                    insight.updated_at
                ],
            )
            .map_err(map_write_error)?;
        transaction.commit()?;
        load_insight(&connection, project_id, &insight.id)
    }

    pub fn list_dashboards(
        &self,
        project_id: &str,
    ) -> Result<Vec<Dashboard>, ControlResourceError> {
        let connection = self.lock()?;
        let ids = {
            let mut statement = connection.prepare_cached(
                "SELECT id FROM dashboards
                 WHERE project_id=?1 ORDER BY updated_at DESC,id",
            )?;
            statement
                .query_map([project_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        ids.into_iter()
            .map(|id| load_dashboard(&connection, project_id, &id))
            .collect()
    }

    pub fn get_dashboard(
        &self,
        project_id: &str,
        dashboard_id: &str,
    ) -> Result<Dashboard, ControlResourceError> {
        let connection = self.lock()?;
        load_dashboard(&connection, project_id, dashboard_id)
    }

    pub fn create_dashboard(
        &self,
        project_id: &str,
        created_by: &str,
        draft: &DashboardDraft,
    ) -> Result<Dashboard, ControlResourceError> {
        validate_name(&draft.name)?;
        validate_required("created_by", created_by, MAX_NAME_BYTES)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project(&transaction, project_id)?;
        let id = Uuid::now_v7().to_string();
        let now = Utc::now().timestamp();
        transaction.execute(
            "INSERT INTO dashboards(project_id,id,name,created_by,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?5)",
            params![project_id, id, draft.name, created_by, now],
        )?;
        transaction.commit()?;
        load_dashboard(&connection, project_id, &id)
    }

    pub fn update_dashboard(
        &self,
        project_id: &str,
        dashboard_id: &str,
        draft: &DashboardDraft,
    ) -> Result<Dashboard, ControlResourceError> {
        let connection = self.lock()?;
        ensure_dashboard(&connection, project_id, dashboard_id)?;
        validate_name(&draft.name)?;
        connection.execute(
            "UPDATE dashboards SET name=?3,updated_at=?4
             WHERE project_id=?1 AND id=?2",
            params![project_id, dashboard_id, draft.name, Utc::now().timestamp()],
        )?;
        load_dashboard(&connection, project_id, dashboard_id)
    }

    pub fn replace_dashboard_tiles(
        &self,
        project_id: &str,
        dashboard_id: &str,
        tiles: &[DashboardTileInput],
    ) -> Result<Dashboard, ControlResourceError> {
        for tile in tiles {
            validate_tile(tile)?;
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_dashboard(&transaction, project_id, dashboard_id)?;
        for tile in tiles {
            ensure_insight(&transaction, project_id, &tile.insight_id)?;
        }
        transaction.execute(
            "DELETE FROM dashboard_tiles WHERE project_id=?1 AND dashboard_id=?2",
            params![project_id, dashboard_id],
        )?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO dashboard_tiles(
                    project_id,dashboard_id,position,insight_id,grid_x,grid_y,grid_w,grid_h
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for (position, tile) in tiles.iter().enumerate() {
                insert.execute(params![
                    project_id,
                    dashboard_id,
                    position as i64,
                    tile.insight_id,
                    tile.x,
                    tile.y,
                    tile.w,
                    tile.h
                ])?;
            }
        }
        transaction.execute(
            "UPDATE dashboards SET updated_at=?3 WHERE project_id=?1 AND id=?2",
            params![project_id, dashboard_id, Utc::now().timestamp()],
        )?;
        transaction.commit()?;
        load_dashboard(&connection, project_id, dashboard_id)
    }

    pub fn delete_dashboard(
        &self,
        project_id: &str,
        dashboard_id: &str,
    ) -> Result<(), ControlResourceError> {
        let connection = self.lock()?;
        deleted(connection.execute(
            "DELETE FROM dashboards WHERE project_id=?1 AND id=?2",
            params![project_id, dashboard_id],
        )?)
    }

    /// Migration-only dashboard upsert preserving ids, timestamps, and tile
    /// ordering. Every tile is checked against the same project transaction.
    pub fn import_dashboard(
        &self,
        project_id: &str,
        dashboard: ImportedDashboard,
    ) -> Result<Dashboard, ControlResourceError> {
        validate_required("id", &dashboard.id, MAX_NAME_BYTES)?;
        validate_name(&dashboard.name)?;
        validate_required("created_by", &dashboard.created_by, MAX_NAME_BYTES)?;
        for tile in &dashboard.tiles {
            validate_tile(tile)?;
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_project(&transaction, project_id)?;
        for tile in &dashboard.tiles {
            ensure_insight(&transaction, project_id, &tile.insight_id)?;
        }
        transaction
            .execute(
                "INSERT INTO dashboards(project_id,id,name,created_by,created_at,updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(project_id,id) DO UPDATE SET
                    name=excluded.name,
                    created_by=excluded.created_by,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at",
                params![
                    project_id,
                    dashboard.id,
                    dashboard.name,
                    dashboard.created_by,
                    dashboard.created_at,
                    dashboard.updated_at
                ],
            )
            .map_err(map_write_error)?;
        transaction.execute(
            "DELETE FROM dashboard_tiles WHERE project_id=?1 AND dashboard_id=?2",
            params![project_id, dashboard.id],
        )?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO dashboard_tiles(
                    project_id,dashboard_id,position,insight_id,grid_x,grid_y,grid_w,grid_h
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for (position, tile) in dashboard.tiles.iter().enumerate() {
                insert.execute(params![
                    project_id,
                    dashboard.id,
                    position as i64,
                    tile.insight_id,
                    tile.x,
                    tile.y,
                    tile.w,
                    tile.h
                ])?;
            }
        }
        transaction.commit()?;
        load_dashboard(&connection, project_id, &dashboard.id)
    }

    pub fn list_shares(&self, project_id: &str) -> Result<Vec<ShareLink>, ControlResourceError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT id,project_id,token,insight_id,dashboard_id,created_at,expires_at
             FROM share_links WHERE project_id=?1 ORDER BY created_at DESC,id",
        )?;
        let shares = statement
            .query_map([project_id], row_to_share)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(shares)
    }

    pub fn get_share(
        &self,
        project_id: &str,
        share_id: &str,
    ) -> Result<ShareLink, ControlResourceError> {
        let connection = self.lock()?;
        load_share(&connection, project_id, share_id)
    }

    /// Resolve the public share credential into a project-scoped reference.
    /// This is an authorization operation, not an unscoped resource get; the
    /// returned project and object ids must be passed back through the scoped
    /// dashboard/insight methods to load content.
    pub fn authorize_share_token(
        &self,
        token: &str,
        now: i64,
    ) -> Result<ShareLink, ControlResourceError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id,project_id,token,insight_id,dashboard_id,created_at,expires_at
                 FROM share_links
                 WHERE token=?1 AND (expires_at IS NULL OR expires_at>?2)",
                params![token, now],
                row_to_share,
            )
            .optional()?
            .ok_or(ControlResourceError::NotFound)
    }

    pub fn create_share(
        &self,
        project_id: &str,
        target: ShareTarget,
        object_id: &str,
        created_by: &str,
        expires_at: Option<i64>,
    ) -> Result<ShareLink, ControlResourceError> {
        validate_required("created_by", created_by, MAX_NAME_BYTES)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match target {
            ShareTarget::Insight => ensure_insight(&transaction, project_id, object_id)?,
            ShareTarget::Dashboard => ensure_dashboard(&transaction, project_id, object_id)?,
        }
        let id = Uuid::now_v7().to_string();
        let token = format!("phs_{}", Uuid::new_v4().simple());
        let now = Utc::now().timestamp();
        let (insight_id, dashboard_id) = match target {
            ShareTarget::Insight => (Some(object_id), None),
            ShareTarget::Dashboard => (None, Some(object_id)),
        };
        transaction
            .execute(
                "INSERT INTO share_links(
                    project_id,id,token,insight_id,dashboard_id,created_by,created_at,expires_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    project_id,
                    id,
                    token,
                    insight_id,
                    dashboard_id,
                    created_by,
                    now,
                    expires_at
                ],
            )
            .map_err(map_write_error)?;
        transaction.commit()?;
        load_share(&connection, project_id, &id)
    }

    pub fn update_share_expiry(
        &self,
        project_id: &str,
        share_id: &str,
        expires_at: Option<i64>,
    ) -> Result<ShareLink, ControlResourceError> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE share_links SET expires_at=?3 WHERE project_id=?1 AND id=?2",
            params![project_id, share_id, expires_at],
        )?;
        if changed == 0 {
            return Err(ControlResourceError::NotFound);
        }
        load_share(&connection, project_id, share_id)
    }

    /// Migration-only share-link upsert preserving the public credential.
    pub fn import_share(
        &self,
        project_id: &str,
        share: ImportedShareLink,
    ) -> Result<ShareLink, ControlResourceError> {
        validate_required("id", &share.id, MAX_NAME_BYTES)?;
        validate_required("token", &share.token, MAX_NAME_BYTES)?;
        validate_required("object_id", &share.object_id, MAX_NAME_BYTES)?;
        validate_required("created_by", &share.created_by, MAX_NAME_BYTES)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match share.target {
            ShareTarget::Insight => ensure_insight(&transaction, project_id, &share.object_id)?,
            ShareTarget::Dashboard => ensure_dashboard(&transaction, project_id, &share.object_id)?,
        }
        let (insight_id, dashboard_id) = match share.target {
            ShareTarget::Insight => (Some(share.object_id.as_str()), None),
            ShareTarget::Dashboard => (None, Some(share.object_id.as_str())),
        };
        transaction
            .execute(
                "INSERT INTO share_links(
                    project_id,id,token,insight_id,dashboard_id,created_by,created_at,expires_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(project_id,id) DO UPDATE SET
                    token=excluded.token,
                    insight_id=excluded.insight_id,
                    dashboard_id=excluded.dashboard_id,
                    created_by=excluded.created_by,
                    created_at=excluded.created_at,
                    expires_at=excluded.expires_at",
                params![
                    project_id,
                    share.id,
                    share.token,
                    insight_id,
                    dashboard_id,
                    share.created_by,
                    share.created_at,
                    share.expires_at
                ],
            )
            .map_err(map_write_error)?;
        transaction.commit()?;
        load_share(&connection, project_id, &share.id)
    }

    pub fn delete_share(
        &self,
        project_id: &str,
        share_id: &str,
    ) -> Result<(), ControlResourceError> {
        let connection = self.lock()?;
        deleted(connection.execute(
            "DELETE FROM share_links WHERE project_id=?1 AND id=?2",
            params![project_id, share_id],
        )?)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ControlResourceError> {
        self.connection
            .lock()
            .map_err(|_| ControlResourceError::Unavailable)
    }
}

fn default_tile_width() -> i32 {
    4
}

fn default_tile_height() -> i32 {
    3
}

fn validate_control_database(connection: &Connection) -> Result<(), ControlResourceError> {
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|_| ControlResourceError::InvalidStorage)?;
    if application_id != CONTROL_APPLICATION_ID {
        return Err(ControlResourceError::InvalidStorage);
    }
    let metadata: Option<(String, String)> = connection
        .query_row(
            "SELECT pair_id,database_role FROM database_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| ControlResourceError::InvalidStorage)?;
    if !matches!(metadata, Some((ref pair_id, ref role)) if !pair_id.is_empty() && role == "control")
    {
        return Err(ControlResourceError::InvalidStorage);
    }
    Ok(())
}

fn validate_required(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ControlResourceError> {
    if value.trim().is_empty() {
        return Err(ControlResourceError::InvalidResource {
            field,
            message: "must not be empty".into(),
        });
    }
    if value.len() > max_bytes {
        return Err(ControlResourceError::InvalidResource {
            field,
            message: format!("must be at most {max_bytes} bytes"),
        });
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), ControlResourceError> {
    validate_required("name", name, MAX_NAME_BYTES)
}

fn validate_flag(flag: &FeatureFlag) -> Result<(), ControlResourceError> {
    validate_required("key", &flag.key, MAX_KEY_BYTES)?;
    if !flag.rollout_percentage.is_finite() || !(0.0..=100.0).contains(&flag.rollout_percentage) {
        return Err(ControlResourceError::InvalidResource {
            field: "rollout_percentage",
            message: "must be a finite number from 0 through 100".into(),
        });
    }
    let mut keys = BTreeSet::new();
    for variant in &flag.variants {
        validate_required("variants.key", &variant.key, MAX_KEY_BYTES)?;
        if !keys.insert(variant.key.as_str()) {
            return Err(ControlResourceError::InvalidResource {
                field: "variants.key",
                message: "variant keys must be unique".into(),
            });
        }
        if !variant.rollout.is_finite() || !(0.0..=100.0).contains(&variant.rollout) {
            return Err(ControlResourceError::InvalidResource {
                field: "variants.rollout",
                message: "must be a finite number from 0 through 100".into(),
            });
        }
    }
    Ok(())
}

fn validate_tile(tile: &DashboardTileInput) -> Result<(), ControlResourceError> {
    validate_required("insight_id", &tile.insight_id, MAX_NAME_BYTES)?;
    if tile.x < 0 || tile.y < 0 || tile.w <= 0 || tile.h <= 0 {
        return Err(ControlResourceError::InvalidResource {
            field: "tiles",
            message: "x/y must be non-negative and w/h must be positive".into(),
        });
    }
    Ok(())
}

fn supported_query_json(query_ir: &Value) -> Result<String, ControlResourceError> {
    let query: Query = serde_json::from_value(query_ir.clone()).map_err(|error| {
        ControlResourceError::InvalidQuery {
            field: "query_ir".into(),
            message: error.to_string(),
        }
    })?;
    let supported =
        SupportedQuery::try_from(query).map_err(|error| ControlResourceError::InvalidQuery {
            field: error.field,
            message: error.message,
        })?;
    serde_json::to_string(supported.as_query()).map_err(|error| {
        ControlResourceError::InvalidQuery {
            field: "query_ir".into(),
            message: error.to_string(),
        }
    })
}

fn imported_query_json(query_ir: &Value) -> Result<(String, bool), ControlResourceError> {
    let query: Query = serde_json::from_value(query_ir.clone()).map_err(|error| {
        ControlResourceError::InvalidQuery {
            field: "query_ir".into(),
            message: error.to_string(),
        }
    })?;
    let supported = SupportedQuery::try_from(query.clone()).is_ok();
    let encoded =
        serde_json::to_string(&query).map_err(|error| ControlResourceError::InvalidQuery {
            field: "query_ir".into(),
            message: error.to_string(),
        })?;
    Ok((encoded, supported))
}

fn invalid_resource_json(error: serde_json::Error) -> ControlResourceError {
    ControlResourceError::InvalidResource {
        field: "resource",
        message: error.to_string(),
    }
}

fn map_write_error(error: rusqlite::Error) -> ControlResourceError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            ControlResourceError::Conflict
        }
        other => ControlResourceError::Database(other),
    }
}

fn deleted(changed: usize) -> Result<(), ControlResourceError> {
    if changed == 0 {
        Err(ControlResourceError::NotFound)
    } else {
        Ok(())
    }
}

fn ensure_project(connection: &Connection, project_id: &str) -> Result<(), ControlResourceError> {
    ensure_exists(
        connection,
        "SELECT 1 FROM projects WHERE id=?1",
        params![project_id],
    )
}

fn ensure_flag(
    connection: &Connection,
    project_id: &str,
    key: &str,
) -> Result<(), ControlResourceError> {
    ensure_exists(
        connection,
        "SELECT 1 FROM feature_flags WHERE project_id=?1 AND key=?2",
        params![project_id, key],
    )
}

fn ensure_insight(
    connection: &Connection,
    project_id: &str,
    insight_id: &str,
) -> Result<(), ControlResourceError> {
    ensure_exists(
        connection,
        "SELECT 1 FROM saved_insights WHERE project_id=?1 AND id=?2",
        params![project_id, insight_id],
    )
}

fn ensure_dashboard(
    connection: &Connection,
    project_id: &str,
    dashboard_id: &str,
) -> Result<(), ControlResourceError> {
    ensure_exists(
        connection,
        "SELECT 1 FROM dashboards WHERE project_id=?1 AND id=?2",
        params![project_id, dashboard_id],
    )
}

fn ensure_exists(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<(), ControlResourceError> {
    let exists = connection
        .query_row(sql, parameters, |_| Ok(()))
        .optional()?;
    exists.ok_or(ControlResourceError::NotFound)
}

fn load_flag(
    connection: &Connection,
    project_id: &str,
    key: &str,
) -> Result<FeatureFlag, ControlResourceError> {
    connection
        .query_row(
            "SELECT key,active,rollout_percentage,variants,payload
             FROM feature_flags WHERE project_id=?1 AND key=?2",
            params![project_id, key],
            row_to_flag,
        )
        .optional()?
        .ok_or(ControlResourceError::NotFound)
}

fn row_to_flag(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeatureFlag> {
    let encoded: String = row.get(3)?;
    Ok(FeatureFlag {
        key: row.get(0)?,
        active: row.get(1)?,
        rollout_percentage: row.get(2)?,
        variants: decode_json(3, &encoded)?,
        payload: row.get(4)?,
    })
}

fn load_insight(
    connection: &Connection,
    project_id: &str,
    insight_id: &str,
) -> Result<SavedInsight, ControlResourceError> {
    connection
        .query_row(
            "SELECT id,project_id,name,description,query_ir,created_by,created_at,updated_at
             FROM saved_insights WHERE project_id=?1 AND id=?2",
            params![project_id, insight_id],
            row_to_insight,
        )
        .optional()?
        .ok_or(ControlResourceError::NotFound)
}

fn row_to_insight(row: &rusqlite::Row<'_>) -> rusqlite::Result<SavedInsight> {
    let query_ir: String = row.get(4)?;
    Ok(SavedInsight {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        query_ir: decode_json(4, &query_ir)?,
        created_by: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn load_dashboard(
    connection: &Connection,
    project_id: &str,
    dashboard_id: &str,
) -> Result<Dashboard, ControlResourceError> {
    let mut dashboard = connection
        .query_row(
            "SELECT id,project_id,name,created_by,created_at
             FROM dashboards WHERE project_id=?1 AND id=?2",
            params![project_id, dashboard_id],
            |row| {
                Ok(Dashboard {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    name: row.get(2)?,
                    tiles: Vec::new(),
                    created_by: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )
        .optional()?
        .ok_or(ControlResourceError::NotFound)?;

    let mut statement = connection.prepare_cached(
        "SELECT
            t.insight_id,t.grid_x,t.grid_y,t.grid_w,t.grid_h,
            i.id,i.project_id,i.name,i.description,i.query_ir,
            i.created_by,i.created_at,i.updated_at
         FROM dashboard_tiles t
         JOIN saved_insights i
           ON i.project_id=t.project_id AND i.id=t.insight_id
         WHERE t.project_id=?1 AND t.dashboard_id=?2
         ORDER BY t.position",
    )?;
    dashboard.tiles = statement
        .query_map(params![project_id, dashboard_id], |row| {
            let query_ir: String = row.get(9)?;
            Ok(DashboardTile {
                insight_id: row.get(0)?,
                x: row.get(1)?,
                y: row.get(2)?,
                w: row.get(3)?,
                h: row.get(4)?,
                insight: Some(SavedInsight {
                    id: row.get(5)?,
                    project_id: row.get(6)?,
                    name: row.get(7)?,
                    description: row.get(8)?,
                    query_ir: decode_json(9, &query_ir)?,
                    created_by: row.get(10)?,
                    created_at: row.get(11)?,
                    updated_at: row.get(12)?,
                }),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(dashboard)
}

fn load_share(
    connection: &Connection,
    project_id: &str,
    share_id: &str,
) -> Result<ShareLink, ControlResourceError> {
    connection
        .query_row(
            "SELECT id,project_id,token,insight_id,dashboard_id,created_at,expires_at
             FROM share_links WHERE project_id=?1 AND id=?2",
            params![project_id, share_id],
            row_to_share,
        )
        .optional()?
        .ok_or(ControlResourceError::NotFound)
}

fn row_to_share(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShareLink> {
    let insight_id: Option<String> = row.get(3)?;
    let dashboard_id: Option<String> = row.get(4)?;
    let (object_type, object_id) = match (insight_id, dashboard_id) {
        (Some(id), None) => (ShareTarget::Insight, id),
        (None, Some(id)) => (ShareTarget::Dashboard, id),
        _ => {
            return Err(rusqlite::Error::InvalidColumnType(
                3,
                "share target".into(),
                rusqlite::types::Type::Null,
            ));
        }
    };
    Ok(ShareLink {
        id: row.get(0)?,
        project_id: row.get(1)?,
        object_type,
        object_id,
        token: row.get(2)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
    })
}

fn decode_json<T>(column: usize, encoded: &str) -> rusqlite::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

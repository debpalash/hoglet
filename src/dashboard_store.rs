//! Dashboard store — saved insights, dashboards, tiles, share links.
//! All in SQLite behind a mutex. Standard CRUD with cascading deletes.

use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedInsight {
    pub id: String,
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub query_ir: serde_json::Value,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub id: String,
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub tiles: Vec<DashboardTile>,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardTile {
    pub insight_id: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default = "default_w")]
    pub w: i32,
    #[serde(default = "default_h")]
    pub h: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insight: Option<SavedInsight>,
}

fn default_w() -> i32 { 4 }
fn default_h() -> i32 { 3 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareLink {
    pub id: String,
    pub object_type: String,
    pub object_id: String,
    pub token: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedObject {
    pub object_type: String,
    pub dashboard: Option<Dashboard>,
    pub insight: Option<SavedInsight>,
    pub token: String,
}

#[derive(Debug)]
pub enum DashboardError {
    Db(rusqlite::Error),
    NotFound,
}

impl From<rusqlite::Error> for DashboardError {
    fn from(e: rusqlite::Error) -> Self { DashboardError::Db(e) }
}

// ── DashboardStore ─────────────────────────────────────────────────

pub struct DashboardStore {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS saved_insights (
        id TEXT NOT NULL PRIMARY KEY,
        token TEXT NOT NULL,
        name TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        query_ir TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS dashboards (
        id TEXT NOT NULL PRIMARY KEY,
        token TEXT NOT NULL,
        name TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS dashboard_tiles (
        id TEXT NOT NULL PRIMARY KEY,
        dashboard_id TEXT NOT NULL REFERENCES dashboards(id) ON DELETE CASCADE,
        insight_id TEXT NOT NULL REFERENCES saved_insights(id) ON DELETE CASCADE,
        grid_x INTEGER NOT NULL DEFAULT 0,
        grid_y INTEGER NOT NULL DEFAULT 0,
        grid_w INTEGER NOT NULL DEFAULT 4,
        grid_h INTEGER NOT NULL DEFAULT 3
    );
    CREATE TABLE IF NOT EXISTS share_links (
        id TEXT NOT NULL PRIMARY KEY,
        object_type TEXT NOT NULL,
        object_id TEXT NOT NULL,
        token TEXT NOT NULL UNIQUE,
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_insights_token ON saved_insights(token);
    CREATE INDEX IF NOT EXISTS idx_dashboards_token ON dashboards(token);
";

impl DashboardStore {
    pub fn open(path: &Path) -> Result<Self, DashboardError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        // Enable foreign keys for cascade deletes
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(DashboardStore { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self, DashboardError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(DashboardStore { conn: Mutex::new(conn) })
    }

    // ── Insights ──────────────────────────────────────────────

    pub fn list_insights(&self, token: &str) -> Result<Vec<SavedInsight>, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, token, name, description, query_ir, created_by, created_at, updated_at
             FROM saved_insights WHERE token = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([token], row_to_insight)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_insight(&self, id: &str) -> Result<SavedInsight, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, token, name, description, query_ir, created_by, created_at, updated_at
             FROM saved_insights WHERE id = ?1",
        )?;
        stmt.query_row([id], row_to_insight).map_err(|_| DashboardError::NotFound)
    }

    pub fn save_insight(&self, token: &str, insight: &SavedInsight) -> Result<SavedInsight, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        let id = if insight.id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            insight.id.clone()
        };
        let ir_json = serde_json::to_string(&insight.query_ir).unwrap_or_default();
        conn.execute(
            "INSERT INTO saved_insights (id, token, name, description, query_ir, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(id) DO UPDATE SET name=?3, description=?4, query_ir=?5, updated_at=?7",
            rusqlite::params![id, token, insight.name, insight.description, ir_json, insight.created_by, now],
        )?;
        Ok(SavedInsight {
            id, token: token.into(), name: insight.name.clone(),
            description: insight.description.clone(), query_ir: insight.query_ir.clone(),
            created_by: insight.created_by.clone(),
            created_at: if insight.id.is_empty() { now } else { insight.created_at },
            updated_at: now,
        })
    }

    pub fn delete_insight(&self, id: &str) -> Result<(), DashboardError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM saved_insights WHERE id = ?1", [id])?;
        if n == 0 { return Err(DashboardError::NotFound); }
        Ok(())
    }

    // ── Dashboards ────────────────────────────────────────────

    pub fn list_dashboards(&self, token: &str) -> Result<Vec<Dashboard>, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, token, name, created_by, created_at FROM dashboards WHERE token = ?1 ORDER BY updated_at DESC",
        )?;
        let rows: Vec<Dashboard> = stmt.query_map([token], |r| {
            Ok(Dashboard {
                id: r.get(0)?, token: r.get(1)?, name: r.get(2)?,
                created_by: r.get(3)?, created_at: r.get(4)?,
                tiles: vec![],
            })
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_dashboard(&self, id: &str) -> Result<Dashboard, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, token, name, created_by, created_at FROM dashboards WHERE id = ?1",
        )?;
        let mut dash: Dashboard = stmt.query_row([id], |r| {
            Ok(Dashboard {
                id: r.get(0)?, token: r.get(1)?, name: r.get(2)?,
                created_by: r.get(3)?, created_at: r.get(4)?,
                tiles: vec![],
            })
        }).map_err(|_| DashboardError::NotFound)?;

        // Load tiles with embedded insights
        let mut tile_stmt = conn.prepare_cached(
            "SELECT t.insight_id, t.grid_x, t.grid_y, t.grid_w, t.grid_h,
                    i.name, i.description, i.query_ir, i.created_by, i.created_at, i.updated_at
             FROM dashboard_tiles t
             LEFT JOIN saved_insights i ON t.insight_id = i.id
             WHERE t.dashboard_id = ?1",
        )?;
        dash.tiles = tile_stmt.query_map([id], |r| {
            let insight_name: Option<String> = r.get(5)?;
            let insight = insight_name.map(|name| SavedInsight {
                id: r.get::<_, String>(0).unwrap_or_default(),
                token: dash.token.clone(), name,
                description: r.get::<_, String>(6).unwrap_or_default(),
                query_ir: serde_json::from_str(&r.get::<_, String>(7).unwrap_or_default()).unwrap_or(serde_json::Value::Null),
                created_by: r.get::<_, String>(8).unwrap_or_default(),
                created_at: r.get::<_, i64>(9).unwrap_or(0),
                updated_at: r.get::<_, i64>(10).unwrap_or(0),
            });
            Ok(DashboardTile {
                insight_id: r.get(0)?, x: r.get(1)?, y: r.get(2)?,
                w: r.get(3)?, h: r.get(4)?, insight,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(dash)
    }

    pub fn save_dashboard(&self, token: &str, dashboard: &Dashboard) -> Result<Dashboard, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        let id = if dashboard.id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            dashboard.id.clone()
        };
        conn.execute(
            "INSERT INTO dashboards (id, token, name, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(id) DO UPDATE SET name=?3, updated_at=?5",
            rusqlite::params![id, token, dashboard.name, dashboard.created_by, now],
        )?;

        // Replace tiles
        conn.execute("DELETE FROM dashboard_tiles WHERE dashboard_id = ?1", [&id])?;
        let mut tile_stmt = conn.prepare_cached(
            "INSERT INTO dashboard_tiles (id, dashboard_id, insight_id, grid_x, grid_y, grid_w, grid_h)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for tile in &dashboard.tiles {
            let tid = Uuid::new_v4().to_string();
            tile_stmt.execute(rusqlite::params![tid, id, tile.insight_id, tile.x, tile.y, tile.w, tile.h])?;
        }

        Ok(Dashboard {
            id, token: token.into(), name: dashboard.name.clone(),
            created_by: dashboard.created_by.clone(), created_at: dashboard.created_at,
            tiles: dashboard.tiles.clone(),
        })
    }

    pub fn delete_dashboard(&self, id: &str) -> Result<(), DashboardError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM dashboards WHERE id = ?1", [id])?;
        if n == 0 { return Err(DashboardError::NotFound); }
        Ok(())
    }

    // ── Shares ────────────────────────────────────────────────

    pub fn create_share(&self, object_type: &str, object_id: &str, created_by: &str) -> Result<ShareLink, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string().replace('-', "");
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO share_links (id, object_type, object_id, token, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, object_type, object_id, token, created_by, now],
        )?;
        Ok(ShareLink { id, object_type: object_type.into(), object_id: object_id.into(), token, created_at: now, expires_at: None })
    }

    pub fn get_shared(&self, share_token: &str) -> Result<SharedObject, DashboardError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT object_type, object_id, token FROM share_links WHERE token = ?1",
        )?;
        let (obj_type, obj_id, stok): (String, String, String) = stmt
            .query_row([share_token], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|_| DashboardError::NotFound)?;

        match obj_type.as_str() {
            "dashboard" => {
                let dash = self.get_dashboard(&obj_id)?;
                Ok(SharedObject { object_type: obj_type, dashboard: Some(dash), insight: None, token: stok })
            }
            "insight" => {
                let insight = self.get_insight(&obj_id)?;
                Ok(SharedObject { object_type: obj_type, dashboard: None, insight: Some(insight), token: stok })
            }
            _ => Err(DashboardError::NotFound),
        }
    }

    pub fn delete_share(&self, id: &str) -> Result<(), DashboardError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM share_links WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn row_to_insight(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedInsight> {
    let ir_str: String = r.get(4)?;
    Ok(SavedInsight {
        id: r.get(0)?, token: r.get(1)?, name: r.get(2)?,
        description: r.get(3)?,
        query_ir: serde_json::from_str(&ir_str).unwrap_or(serde_json::Value::Null),
        created_by: r.get(5)?, created_at: r.get(6)?, updated_at: r.get(7)?,
    })
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn example_insight(token: &str) -> SavedInsight {
        SavedInsight {
            id: String::new(), token: token.into(), name: "Test insight".into(),
            description: String::new(),
            query_ir: serde_json::json!({"kind": "Trends", "series": []}),
            created_by: "u1".into(), created_at: 0, updated_at: 0,
        }
    }

    #[test]
    fn insight_save_and_list() {
        let store = DashboardStore::open_in_memory().unwrap();
        let saved = store.save_insight("phc_t", &example_insight("phc_t")).unwrap();
        assert!(!saved.id.is_empty());

        let list = store.list_insights("phc_t").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Test insight");

        let got = store.get_insight(&saved.id).unwrap();
        assert_eq!(got.name, "Test insight");

        store.delete_insight(&saved.id).unwrap();
        assert!(store.list_insights("phc_t").unwrap().is_empty());
    }

    #[test]
    fn dashboard_with_tiles() {
        let store = DashboardStore::open_in_memory().unwrap();
        let insight = store.save_insight("phc_t", &example_insight("phc_t")).unwrap();

        let dash = Dashboard {
            id: String::new(), token: "phc_t".into(), name: "My dashboard".into(),
            tiles: vec![DashboardTile { insight_id: insight.id.clone(), x: 0, y: 0, w: 6, h: 4, insight: None }],
            created_by: "u1".into(), created_at: 0,
        };
        let saved = store.save_dashboard("phc_t", &dash).unwrap();
        assert_eq!(saved.tiles.len(), 1);

        let loaded = store.get_dashboard(&saved.id).unwrap();
        assert_eq!(loaded.tiles.len(), 1);
        assert!(loaded.tiles[0].insight.is_some());
        assert_eq!(loaded.tiles[0].insight.as_ref().unwrap().name, "Test insight");
    }

    #[test]
    fn share_link_roundtrip() {
        let store = DashboardStore::open_in_memory().unwrap();
        let insight = store.save_insight("phc_t", &example_insight("phc_t")).unwrap();

        let share = store.create_share("insight", &insight.id, "u1").unwrap();
        assert!(!share.token.is_empty());

        let shared = store.get_shared(&share.token).unwrap();
        assert_eq!(shared.object_type, "insight");
        assert!(shared.insight.is_some());

        store.delete_share(&share.id).unwrap();
    }
}

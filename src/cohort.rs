//! Cohort store — static and behavioral cohorts with membership tracking.
//! Behavioral cohorts are reevaluated on a schedule by the flusher.

use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CohortDef {
    pub id: String,
    pub token: String,
    pub name: String,
    pub kind: CohortKind,
    pub definition: CohortDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CohortKind {
    Static,
    Behavioral,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CohortDefinition {
    #[serde(rename = "static")]
    Static { distinct_ids: Vec<String> },
    #[serde(rename = "behavioral")]
    Behavioral {
        event: String,
        operator: CohortOp,
        count: i64,
        window_days: u32,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CohortOp {
    Gte,
    Lte,
    Eq,
}

#[derive(Debug)]
pub enum CohortError {
    Db(rusqlite::Error),
    NotFound,
    DuckDb(duckdb::Error),
}

impl From<rusqlite::Error> for CohortError {
    fn from(e: rusqlite::Error) -> Self {
        CohortError::Db(e)
    }
}

impl From<duckdb::Error> for CohortError {
    fn from(e: duckdb::Error) -> Self {
        CohortError::DuckDb(e)
    }
}

// ── CohortStore ───────────────────────────────────────────────────

pub struct CohortStore {
    conn: Mutex<Connection>,
}

impl CohortStore {
    pub fn open(path: &Path) -> Result<Self, CohortError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(CohortStore {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, CohortError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(CohortStore {
            conn: Mutex::new(conn),
        })
    }

    pub fn create(
        &self,
        token: &str,
        name: &str,
        kind: CohortKind,
        definition: CohortDefinition,
    ) -> Result<CohortDef, CohortError> {
        let conn = self.conn.lock().unwrap();
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let def_json = serde_json::to_string(&definition).unwrap_or_default();
        conn.execute(
            "INSERT INTO cohorts (id, token, name, kind, definition, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![id, token, name, kind_str(&kind), def_json, now, now],
        )?;
        Ok(CohortDef {
            id,
            token: token.into(),
            name: name.into(),
            kind,
            definition,
        })
    }

    pub fn list(&self, token: &str) -> Result<Vec<CohortDef>, CohortError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, token, name, kind, definition FROM cohorts WHERE token = ?1 ORDER BY name",
        )?;
        let rows: Vec<CohortDef> = stmt
            .query_map([token], |r| {
                let def_str: String = r.get(4)?;
                Ok(CohortDef {
                    id: r.get(0)?,
                    token: r.get(1)?,
                    name: r.get(2)?,
                    kind: parse_kind(&r.get::<_, String>(3)?),
                    definition: serde_json::from_str(&def_str).unwrap_or(
                        CohortDefinition::Static {
                            distinct_ids: vec![],
                        },
                    ),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> Result<CohortDef, CohortError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, token, name, kind, definition FROM cohorts WHERE id = ?1",
        )?;
        stmt.query_row([id], |r| {
            let def_str: String = r.get(4)?;
            Ok(CohortDef {
                id: r.get(0)?,
                token: r.get(1)?,
                name: r.get(2)?,
                kind: parse_kind(&r.get::<_, String>(3)?),
                definition: serde_json::from_str(&def_str).unwrap_or(CohortDefinition::Static {
                    distinct_ids: vec![],
                }),
            })
        })
        .map_err(|_| CohortError::NotFound)
    }

    pub fn delete(&self, id: &str) -> Result<(), CohortError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM cohort_members WHERE cohort_id = ?1", [id])?;
        conn.execute("DELETE FROM cohorts WHERE id = ?1", [id])?;
        Ok(())
    }

    // ── Membership ──────────────────────────────────────────

    pub fn add_members(&self, cohort_id: &str, distinct_ids: &[String]) -> Result<(), CohortError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        let mut stmt = conn.prepare_cached(
            "INSERT OR IGNORE INTO cohort_members (cohort_id, distinct_id, added_at) VALUES (?1, ?2, ?3)",
        )?;
        for did in distinct_ids {
            stmt.execute(rusqlite::params![cohort_id, did, now])?;
        }
        Ok(())
    }

    pub fn remove_members(
        &self,
        cohort_id: &str,
        distinct_ids: &[String],
    ) -> Result<(), CohortError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "DELETE FROM cohort_members WHERE cohort_id = ?1 AND distinct_id = ?2",
        )?;
        for did in distinct_ids {
            stmt.execute(rusqlite::params![cohort_id, did])?;
        }
        Ok(())
    }

    pub fn is_member(&self, cohort_id: &str, distinct_id: &str) -> Result<bool, CohortError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM cohort_members WHERE cohort_id = ?1 AND distinct_id = ?2",
            rusqlite::params![cohort_id, distinct_id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_members(&self, cohort_id: &str) -> Result<Vec<String>, CohortError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT distinct_id FROM cohort_members WHERE cohort_id = ?1 ORDER BY added_at",
        )?;
        let rows: Vec<String> = stmt
            .query_map([cohort_id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Replace all members of a behavioral cohort. Called by the reevaluator.
    pub fn set_members(&self, cohort_id: &str, distinct_ids: &[String]) -> Result<(), CohortError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        conn.execute(
            "DELETE FROM cohort_members WHERE cohort_id = ?1",
            [cohort_id],
        )?;
        let now = Utc::now().timestamp();
        let mut stmt = conn.prepare_cached(
            "INSERT INTO cohort_members (cohort_id, distinct_id, added_at) VALUES (?1, ?2, ?3)",
        )?;
        for did in distinct_ids {
            stmt.execute(rusqlite::params![cohort_id, did, now])?;
        }
        conn.execute(
            "UPDATE cohorts SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, cohort_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reevaluate all behavioral cohorts for a token. Runs DuckDB queries
    /// against the events Parquet store.
    pub fn reevaluate_behavioral(
        &self,
        token: &str,
        parquet_glob: &str,
    ) -> Result<usize, CohortError> {
        let cohorts = self.list(token)?;
        let behavioral: Vec<&CohortDef> = cohorts
            .iter()
            .filter(|c| matches!(c.kind, CohortKind::Behavioral))
            .collect();
        if behavioral.is_empty() {
            return Ok(0);
        }

        let escaped_glob = parquet_glob.replace('\'', "''");
        let conn = duckdb::Connection::open_in_memory()?;
        conn.execute_batch("SET memory_limit='256MB';")?;

        let mut reevaluated = 0;
        for cohort in &behavioral {
            let def = match &cohort.definition {
                CohortDefinition::Behavioral {
                    event,
                    operator,
                    count,
                    window_days,
                } => (event, operator, count, window_days),
                _ => continue,
            };

            let op_sql = match def.1 {
                CohortOp::Gte => ">=",
                CohortOp::Lte => "<=",
                CohortOp::Eq => "=",
            };

            let sql = format!(
                "WITH e AS (SELECT * FROM read_parquet('{escaped_glob}') WHERE token = ?1 QUALIFY row_number() OVER (PARTITION BY uuid ORDER BY timestamp) = 1)
                 SELECT distinct_id FROM e
                 WHERE event = ?2 AND epoch(timestamp) >= epoch(now()) - (?3 * 86400)
                 GROUP BY distinct_id
                 HAVING count(*) {op_sql} ?4",
            );
            let mut stmt = conn.prepare(&sql)?;
            let members: Vec<String> = stmt
                .query_map(
                    &[
                        &token as &dyn duckdb::ToSql,
                        &def.0 as &dyn duckdb::ToSql,
                        &(*def.3 as i64) as &dyn duckdb::ToSql,
                        &(*def.2) as &dyn duckdb::ToSql,
                    ],
                    |r| r.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;

            if !members.is_empty() {
                self.set_members(&cohort.id, &members)?;
                reevaluated += 1;
            }
        }
        Ok(reevaluated)
    }
}

// ── Helpers ───────────────────────────────────────────────────────

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS cohorts (
        id TEXT NOT NULL PRIMARY KEY,
        token TEXT NOT NULL,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        definition TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS cohort_members (
        cohort_id TEXT NOT NULL REFERENCES cohorts(id),
        distinct_id TEXT NOT NULL,
        added_at INTEGER NOT NULL,
        PRIMARY KEY (cohort_id, distinct_id)
    );
    CREATE INDEX IF NOT EXISTS idx_cohorts_token ON cohorts(token);
";

fn kind_str(kind: &CohortKind) -> &'static str {
    match kind {
        CohortKind::Static => "static",
        CohortKind::Behavioral => "behavioral",
    }
}

fn parse_kind(s: &str) -> CohortKind {
    match s {
        "behavioral" => CohortKind::Behavioral,
        _ => CohortKind::Static,
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_cohort_membership() {
        let store = CohortStore::open_in_memory().unwrap();
        let c = store
            .create(
                "phc_t",
                "beta",
                CohortKind::Static,
                CohortDefinition::Static {
                    distinct_ids: vec!["u1".into(), "u2".into()],
                },
            )
            .unwrap();
        store
            .add_members(&c.id, &["u1".into(), "u2".into()])
            .unwrap();
        assert!(store.is_member(&c.id, "u1").unwrap());
        assert!(!store.is_member(&c.id, "u3").unwrap());
        store.remove_members(&c.id, &["u1".into()]).unwrap();
        assert!(!store.is_member(&c.id, "u1").unwrap());
    }

    #[test]
    fn list_and_delete() {
        let store = CohortStore::open_in_memory().unwrap();
        store
            .create(
                "phc_t",
                "a",
                CohortKind::Static,
                CohortDefinition::Static {
                    distinct_ids: vec![],
                },
            )
            .unwrap();
        store
            .create(
                "phc_t",
                "b",
                CohortKind::Static,
                CohortDefinition::Static {
                    distinct_ids: vec![],
                },
            )
            .unwrap();
        assert_eq!(store.list("phc_t").unwrap().len(), 2);
        let c = store.list("phc_t").unwrap();
        store.delete(&c[0].id).unwrap();
        assert_eq!(store.list("phc_t").unwrap().len(), 1);
    }

    #[test]
    fn behavioral_members_set_and_get() {
        let store = CohortStore::open_in_memory().unwrap();
        let c = store
            .create(
                "phc_t",
                "active",
                CohortKind::Behavioral,
                CohortDefinition::Behavioral {
                    event: "pageview".into(),
                    operator: CohortOp::Gte,
                    count: 3,
                    window_days: 7,
                },
            )
            .unwrap();
        store
            .set_members(&c.id, &["u1".into(), "u2".into(), "u3".into()])
            .unwrap();
        assert_eq!(store.get_members(&c.id).unwrap().len(), 3);
        assert!(store.is_member(&c.id, "u1").unwrap());
    }
}

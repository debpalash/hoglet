//! Query layer — DuckDB read-only over Parquet segments (SPEC.md "query
//! lane", stack.md).
//!
//! DuckDB opens the Parquet files as an external table; we never keep a live
//! `.duckdb` database (single-writer wall). Because a replayed WAL segment can
//! produce duplicate events, every query reads through a `deduped` CTE that
//! keeps one row per `uuid` — so counts are honest (claims.md claim 4 spirit:
//! the numbers reconcile).
//!
//! This is the dashboard's engine, not a PostHog-compatible surface —
//! compatibility lives at the ingest edge, analytics behind it.

pub mod oracle;

use std::path::PathBuf;
use std::sync::Arc;

use duckdb::Connection;
use serde::Serialize;
use tokio::sync::Semaphore;
use ts_rs::TS;

/// Hard cap on funnel steps — bounded query cost.
pub const MAX_FUNNEL_STEPS: usize = 12;

/// DuckDB memory ceiling per query. The query lane must never OOM the process
/// the ingest lane lives in (SPEC.md two-lanes invariant); DuckDB spills to
/// disk past this instead of allocating without bound.
pub const QUERY_MEMORY_LIMIT: &str = "256MB";

/// Max concurrent DuckDB queries. Beyond this, callers wait — analytical load
/// is capped so it can't starve ingest.
pub const MAX_CONCURRENT_QUERIES: usize = 4;

pub struct QueryEngine {
    events_dir: PathBuf,
    /// Bounds in-flight queries; the query lane's concurrency cap.
    permits: Arc<Semaphore>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Stats {
    #[ts(type = "number")]
    pub total_events: i64,
    #[ts(type = "number")]
    pub unique_persons: i64,
    #[ts(type = "number")]
    pub events_24h: i64,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct TrendPoint {
    pub day: String,
    #[ts(type = "number")]
    pub count: i64,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct EventCount {
    pub event: String,
    #[ts(type = "number")]
    pub count: i64,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct FunnelStep {
    pub event: String,
    #[ts(type = "number")]
    pub reached: i64,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct RecentEvent {
    pub uuid: String,
    pub event: String,
    pub distinct_id: String,
    pub timestamp: String,
}

#[derive(Debug)]
pub enum QueryError {
    Db(duckdb::Error),
    TooManySteps,
}

impl From<duckdb::Error> for QueryError {
    fn from(e: duckdb::Error) -> Self {
        QueryError::Db(e)
    }
}

impl QueryEngine {
    pub fn new(events_dir: PathBuf) -> Self {
        Self {
            events_dir,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
        }
    }

    /// Acquire a query permit (the concurrency cap). Held for the call's
    /// duration; dropped on return.
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .expect("query semaphore never closed")
    }

    /// Glob of Parquet segments. Empty when nothing has flushed yet.
    fn glob(&self) -> String {
        self.events_dir.join("*.parquet").to_string_lossy().into_owned()
    }

    fn has_data(&self) -> bool {
        std::fs::read_dir(&self.events_dir)
            .map(|mut d| d.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "parquet")).unwrap_or(false)))
            .unwrap_or(false)
    }

    fn conn(&self) -> Result<Connection, QueryError> {
        let conn = Connection::open_in_memory()?;
        // Cap memory so a heavy scan spills to disk rather than OOMing the
        // process the ingest lane shares.
        conn.execute_batch(&format!("SET memory_limit='{QUERY_MEMORY_LIMIT}';"))?;
        Ok(conn)
    }

    /// `read_parquet(glob)` deduped to one row per uuid. Callers wrap this as
    /// a CTE named `e`, filtering by token.
    fn base_cte(&self) -> String {
        format!(
            "WITH e AS (
                SELECT * FROM read_parquet('{}')
                QUALIFY row_number() OVER (PARTITION BY uuid ORDER BY timestamp) = 1
            )",
            self.glob().replace('\'', "''")
        )
    }

    pub fn stats(&self, token: &str) -> Result<Stats, QueryError> {
        if !self.has_data() {
            return Ok(Stats { total_events: 0, unique_persons: 0, events_24h: 0 });
        }
        let conn = self.conn()?;
        // epoch() avoids timestamp/interval arithmetic, which is fussy across
        // TIMESTAMP vs TIMESTAMPTZ; it works on both and is timezone-safe.
        let sql = format!(
            "{} SELECT
                count(*) AS total,
                count(DISTINCT distinct_id) AS persons,
                count(*) FILTER (WHERE epoch(timestamp) >= epoch(now()) - 86400) AS last24
             FROM e WHERE token = ?",
            self.base_cte()
        );
        let row = conn.query_row(&sql, [token], |r| {
            Ok(Stats {
                total_events: r.get(0)?,
                unique_persons: r.get(1)?,
                events_24h: r.get(2)?,
            })
        })?;
        Ok(row)
    }

    pub fn top_events(&self, token: &str, limit: usize) -> Result<Vec<EventCount>, QueryError> {
        if !self.has_data() {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let sql = format!(
            "{} SELECT event, count(*) c FROM e WHERE token = ?
             GROUP BY event ORDER BY c DESC LIMIT {}",
            self.base_cte(),
            limit
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([token], |r| {
                Ok(EventCount { event: r.get(0)?, count: r.get(1)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Daily counts of `event` over the last `days` days.
    pub fn trend(&self, token: &str, event: &str, days: u32) -> Result<Vec<TrendPoint>, QueryError> {
        if !self.has_data() {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let sql = format!(
            "{} SELECT strftime(timestamp, '%Y-%m-%d') d, count(*) c
             FROM e
             WHERE token = ? AND event = ?
               AND epoch(timestamp) >= epoch(now()) - ({} * 86400)
             GROUP BY d ORDER BY d",
            self.base_cte(),
            days
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([token, event], |r| {
                Ok(TrendPoint { day: r.get(0)?, count: r.get(1)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Ordered funnel: for steps [A, B, C], how many distinct_ids did A, then
    /// B at-or-after their A, then C at-or-after their B. Monotonic
    /// non-increasing by construction (the CTE-chain pattern, stack.md).
    pub fn funnel(&self, token: &str, steps: &[String]) -> Result<Vec<FunnelStep>, QueryError> {
        if steps.is_empty() {
            return Ok(vec![]);
        }
        if steps.len() > MAX_FUNNEL_STEPS {
            return Err(QueryError::TooManySteps);
        }
        if !self.has_data() {
            return Ok(steps.iter().map(|s| FunnelStep { event: s.clone(), reached: 0 }).collect());
        }
        let conn = self.conn()?;

        // Build the chained CTEs. Step 0: first time each person did steps[0].
        // Step i: min time they did steps[i] at/after step i-1's time.
        let mut ctes = vec![format!(
            "s0 AS (SELECT distinct_id, min(timestamp) t FROM e \
             WHERE token = ? AND event = ? GROUP BY distinct_id)"
        )];
        for i in 1..steps.len() {
            ctes.push(format!(
                "s{i} AS (SELECT s{prev}.distinct_id, min(e.timestamp) t \
                 FROM s{prev} JOIN e ON e.distinct_id = s{prev}.distinct_id \
                 AND e.token = ? AND e.event = ? AND e.timestamp >= s{prev}.t \
                 GROUP BY s{prev}.distinct_id)",
                i = i,
                prev = i - 1
            ));
        }
        let counts: Vec<String> = (0..steps.len())
            .map(|i| format!("SELECT {i} AS step, count(*) AS reached FROM s{i}"))
            .collect();
        let sql = format!(
            "{base}, {ctes} {union} ORDER BY step",
            base = self.base_cte(),
            ctes = ctes.join(", "),
            union = counts.join(" UNION ALL ")
        );

        // Bind params in CTE order: (token, steps[0]), (token, steps[1]), ...
        let mut params: Vec<&dyn duckdb::ToSql> = Vec::new();
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for step in steps {
            pairs.push((token, step.as_str()));
        }
        for (t, s) in &pairs {
            params.push(t);
            params.push(s);
        }

        let mut stmt = conn.prepare(&sql)?;
        let reached: Vec<i64> = stmt
            .query_map(params.as_slice(), |r| r.get::<_, i64>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(steps
            .iter()
            .zip(reached)
            .map(|(event, reached)| FunnelStep { event: event.clone(), reached })
            .collect())
    }

    pub fn recent_events(&self, token: &str, limit: usize) -> Result<Vec<RecentEvent>, QueryError> {
        if !self.has_data() {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let sql = format!(
            "{} SELECT uuid, event, distinct_id, strftime(timestamp, '%Y-%m-%dT%H:%M:%SZ') ts
             FROM e WHERE token = ? ORDER BY timestamp DESC LIMIT {}",
            self.base_cte(),
            limit
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([token], |r| {
                Ok(RecentEvent {
                    uuid: r.get(0)?,
                    event: r.get(1)?,
                    distinct_id: r.get(2)?,
                    timestamp: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::event::CapturedEvent;
    use crate::store::EventStore;
    use chrono::{Duration, Utc};
    use serde_json::Map;
    use uuid::Uuid;

    fn ev(event: &str, did: &str, ago_hours: i64) -> CapturedEvent {
        CapturedEvent {
            uuid: Uuid::new_v4(),
            event: event.into(),
            distinct_id: did.into(),
            token: "phc_t".into(),
            timestamp: Utc::now() - Duration::hours(ago_hours),
            properties: Map::new(),
        }
    }

    fn engine_with(events: &[CapturedEvent]) -> (tempfile::TempDir, QueryEngine) {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store.write_events(events).unwrap();
        let engine = QueryEngine::new(dir.path().to_path_buf());
        (dir, engine)
    }

    #[test]
    fn stats_count_distinct_uuid_and_persons() {
        let events = vec![
            ev("pageview", "u1", 1),
            ev("pageview", "u1", 2),
            ev("click", "u2", 100),
        ];
        let (_d, e) = engine_with(&events);
        let s = e.stats("phc_t").unwrap();
        assert_eq!(s.total_events, 3);
        assert_eq!(s.unique_persons, 2);
        assert_eq!(s.events_24h, 2); // the 100h-ago click is excluded
    }

    #[test]
    fn empty_store_is_zeros_not_error() {
        let dir = tempfile::tempdir().unwrap();
        EventStore::open(dir.path().to_path_buf()).unwrap();
        let e = QueryEngine::new(dir.path().to_path_buf());
        let s = e.stats("phc_t").unwrap();
        assert_eq!(s.total_events, 0);
        assert!(e.top_events("phc_t", 10).unwrap().is_empty());
    }

    #[test]
    fn duplicate_uuid_counted_once() {
        // Simulate a replayed segment: same event written twice.
        let mut e1 = ev("pageview", "u1", 1);
        e1.uuid = Uuid::from_u128(42);
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store.write_events(std::slice::from_ref(&e1)).unwrap();
        store.write_events(std::slice::from_ref(&e1)).unwrap(); // dup segment
        let engine = QueryEngine::new(dir.path().to_path_buf());
        assert_eq!(engine.stats("phc_t").unwrap().total_events, 1);
    }

    #[test]
    fn top_events_ordered() {
        let events = vec![
            ev("a", "u1", 1),
            ev("a", "u2", 1),
            ev("b", "u1", 1),
        ];
        let (_d, e) = engine_with(&events);
        let top = e.top_events("phc_t", 10).unwrap();
        assert_eq!(top[0].event, "a");
        assert_eq!(top[0].count, 2);
    }

    #[test]
    fn funnel_is_monotonic() {
        // u1 does all three; u2 only first two; u3 only first.
        let events = vec![
            ev("signup", "u1", 5), ev("activate", "u1", 4), ev("pay", "u1", 3),
            ev("signup", "u2", 5), ev("activate", "u2", 4),
            ev("signup", "u3", 5),
        ];
        let (_d, e) = engine_with(&events);
        let steps = vec!["signup".to_string(), "activate".to_string(), "pay".to_string()];
        let f = e.funnel("phc_t", &steps).unwrap();
        assert_eq!(f[0].reached, 3);
        assert_eq!(f[1].reached, 2);
        assert_eq!(f[2].reached, 1);
        // Monotonic non-increasing.
        assert!(f[0].reached >= f[1].reached && f[1].reached >= f[2].reached);
    }

    #[test]
    fn funnel_respects_order() {
        // u1 did pay BEFORE signup — must not count past step 1.
        let events = vec![
            ev("pay", "u1", 10),
            ev("signup", "u1", 5),
        ];
        let (_d, e) = engine_with(&events);
        let steps = vec!["signup".to_string(), "pay".to_string()];
        let f = e.funnel("phc_t", &steps).unwrap();
        assert_eq!(f[0].reached, 1);
        assert_eq!(f[1].reached, 0); // no pay AFTER signup
    }

    #[test]
    fn recent_events_newest_first() {
        let events = vec![ev("old", "u1", 10), ev("new", "u1", 1)];
        let (_d, e) = engine_with(&events);
        let r = e.recent_events("phc_t", 10).unwrap();
        assert_eq!(r[0].event, "new");
    }

    /// Build a varied, deterministic dataset with duplicate uuids sprinkled in.
    fn varied_dataset() -> Vec<CapturedEvent> {
        let names = ["signup", "activate", "purchase", "pageview", "click"];
        let mut events = Vec::new();
        for i in 0..400usize {
            let user = i % 37; // 37 distinct users
            let name = names[(i * 7) % names.len()];
            let mut e = ev(name, &format!("u{user}"), (i % 50) as i64);
            // Deterministic uuid so ~every 13th event duplicates an earlier one.
            e.uuid = uuid::Uuid::from_u128((i % 380) as u128);
            events.push(e);
        }
        events
    }

    /// The query-semantics oracle: DuckDB SQL and the independent Rust
    /// implementation must agree on every question over the same data. A bug
    /// in either is caught here.
    #[test]
    fn duckdb_agrees_with_rust_oracle() {
        let events = varied_dataset();
        let (_d, engine) = engine_with(&events);

        // total + persons
        let stats = engine.stats("phc_t").unwrap();
        let (o_total, o_persons) = oracle::total_and_persons(&events, "phc_t");
        assert_eq!(stats.total_events, o_total, "total_events disagree");
        assert_eq!(stats.unique_persons, o_persons, "unique_persons disagree");

        // top events, compared as a map (tie order is unspecified in SQL)
        let sql_top: std::collections::HashMap<String, i64> = engine
            .top_events("phc_t", 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.event, e.count))
            .collect();
        assert_eq!(sql_top, oracle::top_events(&events, "phc_t"), "top_events disagree");

        // funnels of several shapes
        for steps in [
            vec!["signup".to_string(), "activate".to_string(), "purchase".to_string()],
            vec!["pageview".to_string(), "click".to_string()],
            vec!["click".to_string(), "signup".to_string(), "activate".to_string()],
        ] {
            let sql: Vec<i64> = engine
                .funnel("phc_t", &steps)
                .unwrap()
                .into_iter()
                .map(|s| s.reached)
                .collect();
            let rust = oracle::funnel(&events, "phc_t", &steps);
            assert_eq!(sql, rust, "funnel {steps:?} disagree: sql={sql:?} rust={rust:?}");
        }
    }
}

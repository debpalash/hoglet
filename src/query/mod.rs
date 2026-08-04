//! Query layer — DuckDB read-only over Parquet segments.
//! P1: IR compiler. P3: funnels + retention + SQL access.

pub mod compile;
pub mod ir;
pub mod oracle;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duckdb::Connection;
use serde::Serialize;
use tokio::sync::Semaphore;
use ts_rs::TS;

/// Wraps a DuckDB connection and returns it to the pool on drop.
struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<Mutex<VecDeque<Connection>>>,
}

impl std::ops::Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection { self.conn.as_ref().unwrap() }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Connection { self.conn.as_mut().unwrap() }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            if let Ok(mut pool) = self.pool.lock() {
                if pool.len() < MAX_CONCURRENT_QUERIES * 2 {
                    pool.push_back(c);
                }
            }
        }
    }
}

pub const MAX_FUNNEL_STEPS: usize = 12;
pub const QUERY_MEMORY_LIMIT: &str = "256MB";
pub const MAX_CONCURRENT_QUERIES: usize = 4;

pub struct QueryEngine {
    events_dir: PathBuf,
    permits: Arc<Semaphore>,
    /// Pool of reusable in-memory DuckDB connections. Avoids ~50ms
    /// of init overhead per query.
    pool: Arc<Mutex<VecDeque<Connection>>>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Stats { #[ts(type = "number")] pub total_events: i64, #[ts(type = "number")] pub unique_persons: i64, #[ts(type = "number")] pub events_24h: i64 }

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct TrendPoint { pub day: String, #[ts(type = "number")] pub count: i64 }

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct EventCount { pub event: String, #[ts(type = "number")] pub count: i64 }

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct FunnelStep { pub event: String, #[ts(type = "number")] pub reached: i64 }

#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct RecentEvent { pub uuid: String, pub event: String, pub distinct_id: String, pub timestamp: String }

#[derive(Debug)]
pub enum QueryError {
    Db(duckdb::Error),
    TooManySteps,
    Compile(compile::CompileError),
}

impl From<duckdb::Error> for QueryError { fn from(e: duckdb::Error) -> Self { QueryError::Db(e) } }
impl From<compile::CompileError> for QueryError { fn from(e: compile::CompileError) -> Self { QueryError::Compile(e) } }

impl QueryEngine {
    pub fn new(events_dir: PathBuf) -> Self {
        let mut pool = VecDeque::with_capacity(MAX_CONCURRENT_QUERIES);
        for _ in 0..MAX_CONCURRENT_QUERIES {
            if let Ok(c) = Connection::open_in_memory() {
                let _ = c.execute_batch(&format!("SET memory_limit='{QUERY_MEMORY_LIMIT}';"));
                pool.push_back(c);
            }
        }
        Self { events_dir, permits: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)), pool: Arc::new(Mutex::new(pool)) }
    }
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit { self.permits.clone().acquire_owned().await.expect("semaphore never closed") }
    fn glob(&self) -> String { self.events_dir.join("*.parquet").to_string_lossy().into_owned() }
    fn has_data(&self) -> bool { std::fs::read_dir(&self.events_dir).map(|mut d| d.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "parquet")).unwrap_or(false))).unwrap_or(false) }
    fn conn(&self) -> Result<PooledConn, QueryError> {
        let c = if let Ok(mut pool) = self.pool.lock() {
            pool.pop_front()
        } else {
            None
        };
        let conn = match c {
            Some(c) => c,
            None => {
                let c = Connection::open_in_memory()?;
                c.execute_batch(&format!("SET memory_limit='{QUERY_MEMORY_LIMIT}';"))?;
                c
            }
        };
        Ok(PooledConn { conn: Some(conn), pool: self.pool.clone() })
    }

    pub fn run_ir(&self, query: &ir::Query, token: &str) -> Result<ir::QueryResponse, QueryError> {
        let start = std::time::Instant::now();
        match query.kind {
            ir::QueryKind::Funnels => self.run_funnels(query, token, start),
            ir::QueryKind::Retention => self.run_retention(query, token, start),
            ir::QueryKind::Sql => self.run_sql(query, token, start),
            ir::QueryKind::Lifecycle => self.run_lifecycle(query, token, start),
            ir::QueryKind::Stickiness => self.run_stickiness(query, token, start),
            _ => self.run_trends_or_other(query, token, start),
        }
    }

    fn run_funnels(&self, query: &ir::Query, token: &str, start: std::time::Instant) -> Result<ir::QueryResponse, QueryError> {
        if !self.has_data() { return Ok(empty_resp(query, "funnels", start)); }
        let compiled = compile::compile(query, token, &self.glob(), None)?;
        let sql = compile::finalize_sql(&compiled);
        let vals: Vec<duckdb::types::Value> = compiled.params.iter().map(compile::param_to_duckdb).collect();
        let conn = self.conn()?; let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(i64, String, i64)> = if vals.is_empty() { stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>,_>>()? }
        else { let refs: Vec<&dyn duckdb::ToSql> = vals.iter().map(|v| v as &dyn duckdb::ToSql).collect(); stmt.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>,_>>()? };
        Ok(ir::QueryResponse { results: rows.into_iter().map(|(step, label, reached)| ir::SeriesResult { label, data: vec![ir::DataPoint { interval: step.to_string(), count: reached }], breakdown_value: None }).collect(), meta: ir::QueryMeta { kind: "funnels".into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } })
    }

    fn run_retention(&self, query: &ir::Query, token: &str, start: std::time::Instant) -> Result<ir::QueryResponse, QueryError> {
        if !self.has_data() { return Ok(empty_resp(query, "retention", start)); }
        let compiled = compile::compile(query, token, &self.glob(), None)?;
        let sql = compile::finalize_sql(&compiled);
        let vals: Vec<duckdb::types::Value> = compiled.params.iter().map(compile::param_to_duckdb).collect();
        let conn = self.conn()?; let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(String, i64, i64)> = if vals.is_empty() { stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>,_>>()? }
        else { let refs: Vec<&dyn duckdb::ToSql> = vals.iter().map(|v| v as &dyn duckdb::ToSql).collect(); stmt.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>,_>>()? };
        let mut m: BTreeMap<String, Vec<ir::DataPoint>> = BTreeMap::new();
        for (dt, p, u) in &rows { m.entry(dt.clone()).or_default().push(ir::DataPoint { interval: p.to_string(), count: *u }); }
        Ok(ir::QueryResponse { results: m.into_iter().map(|(c, d)| ir::SeriesResult { label: format!("Cohort {c}"), data: d, breakdown_value: None }).collect(), meta: ir::QueryMeta { kind: "retention".into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } })
    }

    fn run_sql(&self, query: &ir::Query, token: &str, start: std::time::Instant) -> Result<ir::QueryResponse, QueryError> {
        if !self.has_data() { return Ok(empty_resp(query, "sql", start)); }
        let compiled = compile::compile(query, token, &self.glob(), None)?;
        let sql = compile::finalize_sql(&compiled);
        let conn = self.conn()?; let mut stmt = conn.prepare(&sql)?;
        let cc = stmt.column_count();
        let _rows = stmt.query_map([], |r| { let mut vs = Vec::with_capacity(cc); for i in 0..cc { let v: duckdb::types::Value = r.get(i)?; vs.push(match v { duckdb::types::Value::Text(s) => serde_json::Value::String(s), duckdb::types::Value::BigInt(n) => serde_json::json!(n), duckdb::types::Value::Double(f) => serde_json::json!(f), duckdb::types::Value::Boolean(b) => serde_json::Value::Bool(b), duckdb::types::Value::Null => serde_json::Value::Null, _ => serde_json::Value::String(format!("{v:?}")), }); } Ok(vs) })?.collect::<Result<Vec<Vec<serde_json::Value>>,_>>()?;
        Ok(ir::QueryResponse { results: vec![], meta: ir::QueryMeta { kind: "sql".into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } })
    }

    fn run_lifecycle(&self, query: &ir::Query, token: &str, start: std::time::Instant) -> Result<ir::QueryResponse, QueryError> {
        if !self.has_data() { return Ok(empty_resp(query, "lifecycle", start)); }
        let compiled = compile::compile(query, token, &self.glob(), None)?;
        let sql = compile::finalize_sql(&compiled);
        let vals: Vec<duckdb::types::Value> = compiled.params.iter().map(compile::param_to_duckdb).collect();
        let conn = self.conn()?; let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(String, i64, i64, i64)> = if vals.is_empty() { stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>,_>>()? }
        else { let refs: Vec<&dyn duckdb::ToSql> = vals.iter().map(|v| v as &dyn duckdb::ToSql).collect(); stmt.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>,_>>()? };
        let results = vec![
            ir::SeriesResult { label: "New".into(), data: rows.iter().map(|(p,n,_,_)| ir::DataPoint { interval: p.clone(), count: *n }).collect(), breakdown_value: None },
            ir::SeriesResult { label: "Returning".into(), data: rows.iter().map(|(p,_,r,_)| ir::DataPoint { interval: p.clone(), count: *r }).collect(), breakdown_value: None },
            ir::SeriesResult { label: "Resurrecting".into(), data: rows.iter().map(|(p,_,_,rs)| ir::DataPoint { interval: p.clone(), count: *rs }).collect(), breakdown_value: None },
        ];
        Ok(ir::QueryResponse { results, meta: ir::QueryMeta { kind: "lifecycle".into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } })
    }

    fn run_stickiness(&self, query: &ir::Query, token: &str, start: std::time::Instant) -> Result<ir::QueryResponse, QueryError> {
        if !self.has_data() { return Ok(empty_resp(query, "stickiness", start)); }
        let compiled = compile::compile(query, token, &self.glob(), None)?;
        let sql = compile::finalize_sql(&compiled);
        let vals: Vec<duckdb::types::Value> = compiled.params.iter().map(compile::param_to_duckdb).collect();
        let conn = self.conn()?; let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<(String, i64)> = if vals.is_empty() { stmt.query_map([], |r| Ok((r.get::<_,i64>(0)?.to_string(), r.get(1)?)))?.collect::<Result<Vec<_>,_>>()? }
        else { let refs: Vec<&dyn duckdb::ToSql> = vals.iter().map(|v| v as &dyn duckdb::ToSql).collect(); stmt.query_map(refs.as_slice(), |r| Ok((r.get::<_,i64>(0)?.to_string(), r.get(1)?)))?.collect::<Result<Vec<_>,_>>()? };
        let results = vec![ir::SeriesResult { label: "Stickiness".into(), data: rows.into_iter().map(|(c, u)| ir::DataPoint { interval: c, count: u }).collect(), breakdown_value: None }];
        Ok(ir::QueryResponse { results, meta: ir::QueryMeta { kind: "stickiness".into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } })
    }

    fn run_trends_or_other(&self, query: &ir::Query, token: &str, start: std::time::Instant) -> Result<ir::QueryResponse, QueryError> {
        let ns = query.series.len();
        if !self.has_data() { return Ok(empty_resp(query, "trends", start)); }
        let compiled = compile::compile(query, token, &self.glob(), None)?;
        let sql = compile::finalize_sql(&compiled);
        let vals: Vec<duckdb::types::Value> = compiled.params.iter().map(compile::param_to_duckdb).collect();
        let conn = self.conn()?; let mut stmt = conn.prepare(&sql)?;
        let hb = query.breakdown.is_some(); let lo = if hb { 2 } else { 1 }; let co = if hb { 3 } else { 2 };
        let rows: Vec<(String, Option<String>, Vec<(String, i64)>)> = if vals.is_empty() {
            stmt.query_map([], move |r| { let iv: String = r.get(0)?; let bd: Option<String> = if hb { Some(r.get::<_,String>(1).unwrap_or_default()) } else { None }; let mut s = Vec::with_capacity(ns); for i in 0..ns { s.push((r.get::<_,String>(lo+2*i)?, r.get::<_,i64>(co+2*i)?)); } Ok((iv,bd,s)) })?.collect::<Result<Vec<_>,_>>()?
        } else {
            let refs: Vec<&dyn duckdb::ToSql> = vals.iter().map(|v| v as &dyn duckdb::ToSql).collect();
            stmt.query_map(refs.as_slice(), move |r| { let iv: String = r.get(0)?; let bd: Option<String> = if hb { Some(r.get::<_,String>(1).unwrap_or_default()) } else { None }; let mut s = Vec::with_capacity(ns); for i in 0..ns { s.push((r.get::<_,String>(lo+2*i)?, r.get::<_,i64>(co+2*i)?)); } Ok((iv,bd,s)) })?.collect::<Result<Vec<_>,_>>()?
        };
        let results = build_trends_results(hb, ns, &rows, query);
        Ok(ir::QueryResponse { results, meta: ir::QueryMeta { kind: kind_str(query), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } })
    }

    pub fn trend_via_ir(&self, token: &str, event: &str, days: u32) -> Result<Vec<TrendPoint>, QueryError> {
        let q = ir::Query::trends(vec![ir::Series { event: ir::EventMatch::Name(event.into()), math: ir::Math::Total }], ir::DateRange { from: None, to: None, last_n: Some(ir::LastN::Days(days)) });
        let r = self.run_ir(&q, token)?;
        Ok(r.results.first().map(|s| s.data.iter().map(|d| TrendPoint { day: d.interval.clone(), count: d.count }).collect()).unwrap_or_default())
    }

    pub fn stats_via_ir(&self, token: &str) -> Result<Stats, QueryError> {
        let r = ir::DateRange { from: None, to: None, last_n: None };
        let resp = self.run_ir(&ir::Query::trends(vec![ir::Series { event: ir::EventMatch::Any, math: ir::Math::Total }, ir::Series { event: ir::EventMatch::Any, math: ir::Math::Dau }], r.clone()), token)?;
        let te: i64 = resp.results.get(0).map(|s| s.data.iter().map(|d| d.count).sum()).unwrap_or(0);
        let up: i64 = resp.results.get(1).map(|s| s.data.iter().map(|d| d.count).sum()).unwrap_or(0);
        let r24 = self.run_ir(&ir::Query::trends(vec![ir::Series { event: ir::EventMatch::Any, math: ir::Math::Total }], ir::DateRange { from: None, to: None, last_n: Some(ir::LastN::Hours(24)) }), token)?;
        let e24: i64 = r24.results.first().map(|s| s.data.iter().map(|d| d.count).sum()).unwrap_or(0);
        Ok(Stats { total_events: te, unique_persons: up, events_24h: e24 })
    }

    // ── Legacy SQL methods (preserved, P0) ──
    fn base_cte(&self) -> String { format!("WITH e AS (SELECT * FROM read_parquet('{}') QUALIFY row_number() OVER (PARTITION BY uuid ORDER BY timestamp) = 1)", self.glob().replace('\'', "''")) }
    pub fn stats(&self, token: &str) -> Result<Stats, QueryError> { if !self.has_data() { return Ok(Stats { total_events: 0, unique_persons: 0, events_24h: 0 }); } let c = self.conn()?; let sql = format!("{} SELECT count(*) AS total, count(DISTINCT distinct_id) AS persons, count(*) FILTER (WHERE epoch(timestamp) >= epoch(now()) - 86400) AS last24 FROM e WHERE token = ?", self.base_cte()); c.query_row(&sql, [token], |r| Ok(Stats { total_events: r.get(0)?, unique_persons: r.get(1)?, events_24h: r.get(2)? })).map_err(|e| e.into()) }
    pub fn top_events(&self, token: &str, limit: usize) -> Result<Vec<EventCount>, QueryError> { if !self.has_data() { return Ok(vec![]); } let c = self.conn()?; let sql = format!("{} SELECT event, count(*) c FROM e WHERE token = ? GROUP BY event ORDER BY c DESC LIMIT {}", self.base_cte(), limit); let mut s = c.prepare(&sql)?; s.query_map([token], |r| Ok(EventCount { event: r.get(0)?, count: r.get(1)? }))?.collect::<Result<Vec<_>,_>>().map_err(|e| e.into()) }
    pub fn trend(&self, token: &str, event: &str, days: u32) -> Result<Vec<TrendPoint>, QueryError> { if !self.has_data() { return Ok(vec![]); } let c = self.conn()?; let sql = format!("{} SELECT strftime(timestamp, '%Y-%m-%d') d, count(*) c FROM e WHERE token = ? AND event = ? AND epoch(timestamp) >= epoch(now()) - ({} * 86400) GROUP BY d ORDER BY d", self.base_cte(), days); let mut s = c.prepare(&sql)?; s.query_map([token, event], |r| Ok(TrendPoint { day: r.get(0)?, count: r.get(1)? }))?.collect::<Result<Vec<_>,_>>().map_err(|e| e.into()) }
    pub fn funnel(&self, token: &str, steps: &[String]) -> Result<Vec<FunnelStep>, QueryError> {
        if steps.is_empty() { return Ok(vec![]); }
        if steps.len() > MAX_FUNNEL_STEPS { return Err(QueryError::TooManySteps); }
        if !self.has_data() { return Ok(steps.iter().map(|s| FunnelStep { event: s.clone(), reached: 0 }).collect()); }
        let c = self.conn()?;
        let mut ctes = vec![format!("s0 AS (SELECT distinct_id, min(timestamp) t FROM e WHERE token = ? AND event = ? GROUP BY distinct_id)")];
        for i in 1..steps.len() {
            ctes.push(format!("s{i} AS (SELECT s{p}.distinct_id, min(e.timestamp) t FROM s{p} JOIN e ON e.distinct_id = s{p}.distinct_id AND e.token = ? AND e.event = ? AND e.timestamp >= s{p}.t GROUP BY s{p}.distinct_id)", i=i, p=i-1));
        }
        let counts: Vec<String> = (0..steps.len()).map(|i| format!("SELECT {i} AS step, count(*) AS reached FROM s{i}")).collect();
        let sql = format!("{}, {} {} ORDER BY step", self.base_cte(), ctes.join(", "), counts.join(" UNION ALL "));
        let mut params: Vec<&dyn duckdb::ToSql> = Vec::new();
        for step in steps { params.push(&token); params.push(step); }
        let mut s = c.prepare(&sql)?;
        let reached: Vec<i64> = s.query_map(params.as_slice(), |r| r.get::<_,i64>(1))?.collect::<Result<Vec<_>,_>>()?;
        Ok(steps.iter().zip(reached).map(|(e,r)| FunnelStep { event: e.clone(), reached: r }).collect())
    }
    pub fn recent_events(&self, token: &str, limit: usize) -> Result<Vec<RecentEvent>, QueryError> { if !self.has_data() { return Ok(vec![]); } let c = self.conn()?; let sql = format!("{} SELECT uuid, event, distinct_id, strftime(timestamp, '%Y-%m-%dT%H:%M:%SZ') ts FROM e WHERE token = ? ORDER BY timestamp DESC LIMIT {}", self.base_cte(), limit); let mut s = c.prepare(&sql)?; s.query_map([token], |r| Ok(RecentEvent { uuid: r.get(0)?, event: r.get(1)?, distinct_id: r.get(2)?, timestamp: r.get(3)? }))?.collect::<Result<Vec<_>,_>>().map_err(|e| e.into()) }
}

fn empty_resp(query: &ir::Query, kind: &str, start: std::time::Instant) -> ir::QueryResponse {
    let results = match query.kind {
        ir::QueryKind::Funnels => query.series.iter().map(|s| ir::SeriesResult { label: s.event_name(), data: vec![], breakdown_value: None }).collect(),
        _ => query.series.iter().enumerate().map(|(i, s)| ir::SeriesResult { label: compile::series_label(s, i), data: vec![], breakdown_value: None }).collect(),
    };
    ir::QueryResponse { results, meta: ir::QueryMeta { kind: kind.into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } }
}

fn kind_str(query: &ir::Query) -> String {
    match query.kind { ir::QueryKind::Trends => "trends".into(), ir::QueryKind::Funnels => "funnels".into(), ir::QueryKind::Retention => "retention".into(), ir::QueryKind::Sql => "sql".into(), _ => format!("{:?}", query.kind).to_lowercase() }
}

fn build_trends_results(has_bd: bool, ns: usize, rows: &[(String, Option<String>, Vec<(String, i64)>)], query: &ir::Query) -> Vec<ir::SeriesResult> {
    if has_bd {
        let mut cm: std::collections::HashMap<(String, String), Vec<ir::DataPoint>> = std::collections::HashMap::new();
        let mut co: Vec<(String, String)> = Vec::new();
        for (iv, bd, sd) in rows { let b = bd.clone().unwrap_or_default(); for (_i, (l, c)) in sd.iter().enumerate() { let k = (l.clone(), b.clone()); if !cm.contains_key(&k) { co.push(k.clone()); cm.insert(k.clone(), vec![]); } cm.get_mut(&k).unwrap().push(ir::DataPoint { interval: iv.clone(), count: *c }); } }
        co.iter().map(|(l, b)| ir::SeriesResult { label: l.clone(), data: cm.remove(&(l.clone(), b.clone())).unwrap_or_default(), breakdown_value: Some(b.clone()) }).collect()
    } else {
        let mut r: Vec<ir::SeriesResult> = (0..ns).map(|_| ir::SeriesResult { label: String::new(), data: vec![], breakdown_value: None }).collect();
        for (iv, _, sd) in rows { for (i, (l, c)) in sd.iter().enumerate() { if r[i].label.is_empty() { r[i].label = l.clone(); } r[i].data.push(ir::DataPoint { interval: iv.clone(), count: *c }); } }
        for (i, s) in query.series.iter().enumerate() { if i < r.len() && r[i].label.is_empty() { r[i].label = compile::series_label(s, i); } }
        r
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
        CapturedEvent { uuid: Uuid::new_v4(), event: event.into(), distinct_id: did.into(), token: "phc_t".into(), timestamp: Utc::now() - Duration::hours(ago_hours), properties: Map::new() }
    }
    fn evp(event: &str, did: &str, ago_hours: i64, props: Vec<(&str, serde_json::Value)>) -> CapturedEvent {
        let mut m = Map::new(); for (k,v) in props { m.insert(k.into(), v); }
        CapturedEvent { uuid: Uuid::new_v4(), event: event.into(), distinct_id: did.into(), token: "phc_t".into(), timestamp: Utc::now() - Duration::hours(ago_hours), properties: m }
    }
    fn engine_with(events: &[CapturedEvent]) -> (tempfile::TempDir, QueryEngine) {
        let dir = tempfile::tempdir().unwrap(); let p = dir.path().to_path_buf(); let store = EventStore::open(p.clone()).unwrap(); store.write_events(events).unwrap(); (dir, QueryEngine::new(p))
    }
    fn assert_oracle(events: &[CapturedEvent], query: &ir::Query) {
        let (_d, e) = engine_with(events);
        let dd = e.run_ir(query, "phc_t").unwrap();
        let oo = oracle::run_ir(events, query, "phc_t");
        let dm: std::collections::HashMap<_,Vec<i64>> = dd.results.iter().map(|s| ((s.label.clone(),s.breakdown_value.clone()), s.data.iter().map(|d| d.count).collect())).collect();
        let om: std::collections::HashMap<_,Vec<i64>> = oo.results.iter().map(|s| ((s.label.clone(),s.breakdown_value.clone()), s.data.iter().map(|d| d.count).collect())).collect();
        assert_eq!(dm, om);
    }

    // ── P0 legacy tests ──
    #[test] fn stats_count() { let (_d,e) = engine_with(&[ev("pv","u1",1),ev("pv","u1",2),ev("ck","u2",100)]); let s = e.stats("phc_t").unwrap(); assert_eq!(s.total_events,3); assert_eq!(s.unique_persons,2); assert_eq!(s.events_24h,2); }
    #[test] fn empty_store() { let d=tempfile::tempdir().unwrap(); EventStore::open(d.path().to_path_buf()).unwrap(); let e=QueryEngine::new(d.path().to_path_buf()); assert_eq!(e.stats("phc_t").unwrap().total_events,0); assert!(e.top_events("phc_t",10).unwrap().is_empty()); }
    #[test] fn dup_uuid_once() { let mut e1=ev("pv","u1",1); e1.uuid=Uuid::from_u128(42); let d=tempfile::tempdir().unwrap(); let s=EventStore::open(d.path().to_path_buf()).unwrap(); s.write_events(&[e1.clone()]).unwrap(); s.write_events(&[e1]).unwrap(); assert_eq!(QueryEngine::new(d.path().to_path_buf()).stats("phc_t").unwrap().total_events,1); }
    #[test] fn top_ordered() { let (_d,e)=engine_with(&[ev("a","u1",1),ev("a","u2",1),ev("b","u1",1)]); let t=e.top_events("phc_t",10).unwrap(); assert_eq!(t[0].event,"a"); assert_eq!(t[0].count,2); }
    #[test] fn funnel_mono() { let evs=&[ev("s","u1",5),ev("a","u1",4),ev("p","u1",3),ev("s","u2",5),ev("a","u2",4),ev("s","u3",5)]; let (_d,e)=engine_with(evs); let f=e.funnel("phc_t",&["s".into(),"a".into(),"p".into()]).unwrap(); assert_eq!(f[0].reached,3); assert_eq!(f[1].reached,2); assert_eq!(f[2].reached,1); }
    #[test] fn funnel_order() { let (_d,e)=engine_with(&[ev("p","u1",10),ev("s","u1",5)]); let f=e.funnel("phc_t",&["s".into(),"p".into()]).unwrap(); assert_eq!(f[0].reached,1); assert_eq!(f[1].reached,0); }
    #[test] fn recent_newest() { let (_d,e)=engine_with(&[ev("o","u1",10),ev("n","u1",1)]); assert_eq!(e.recent_events("phc_t",10).unwrap()[0].event,"n"); }

    // ── P1 IR tests ──
    #[test] fn ir_trends_total() { let q=ir::Query::trends(vec![ir::Series{event:ir::EventMatch::Name("pv".into()),math:ir::Math::Total}],ir::DateRange{from:None,to:None,last_n:None}); assert_oracle(&[ev("pv","u1",1),ev("pv","u1",2),ev("pv","u2",1),ev("ck","u1",1)],&q); }
    #[test] fn ir_trends_filter() { let q=ir::Query{kind:ir::QueryKind::Trends,series:vec![ir::Series{event:ir::EventMatch::Name("pv".into()),math:ir::Math::Total}],filters:ir::PropertyGroup{op:ir::GroupOp::And,values:vec![ir::GroupOrFilter::Filter(ir::Filter{source:ir::FilterSource::Event,key:"br".into(),operator:ir::FilterOperator::Exact,value:"Ch".into()})]},..ir::Query::trends(vec![],ir::DateRange{from:None,to:None,last_n:None})}; assert_oracle(&[evp("pv","u1",1,vec![("br","Ch".into())]),evp("pv","u2",1,vec![("br","Fx".into())]),evp("pv","u3",1,vec![("br","Ch".into())])],&q); }
    #[test] fn ir_trends_bd() { let q=ir::Query{kind:ir::QueryKind::Trends,series:vec![ir::Series{event:ir::EventMatch::Name("pv".into()),math:ir::Math::Total}],breakdown:Some(ir::Breakdown{source:ir::FilterSource::Event,key:"br".into(),limit:10}),..ir::Query::trends(vec![],ir::DateRange{from:None,to:None,last_n:None})}; assert_oracle(&[evp("pv","u1",1,vec![("br","Ch".into())]),evp("pv","u2",1,vec![("br","Fx".into())]),evp("pv","u3",1,vec![("br","Ch".into())])],&q); }
    #[test] fn ir_trends_drange() { let q=ir::Query::trends(vec![ir::Series{event:ir::EventMatch::Name("pv".into()),math:ir::Math::Total}],ir::DateRange{from:None,to:None,last_n:Some(ir::LastN::Days(7))}); assert_oracle(&[ev("pv","u1",1),ev("pv","u2",100)],&q); }
    #[test] fn ir_trends_empty() { let d=tempfile::tempdir().unwrap(); EventStore::open(d.path().to_path_buf()).unwrap(); let e=QueryEngine::new(d.path().to_path_buf()); let q=ir::Query::trends(vec![ir::Series{event:ir::EventMatch::Name("pv".into()),math:ir::Math::Total}],ir::DateRange{from:None,to:None,last_n:None}); assert!(e.run_ir(&q,"phc_t").unwrap().results[0].data.is_empty()); }
    #[test] fn ir_trend_dau() { let (_d,e)=engine_with(&[ev("pv","u1",1),ev("pv","u1",2),ev("pv","u2",1)]); let q=ir::Query::trends(vec![ir::Series{event:ir::EventMatch::Name("pv".into()),math:ir::Math::Dau}],ir::DateRange{from:None,to:None,last_n:None}); let r=e.run_ir(&q,"phc_t").unwrap(); assert!(!r.results.is_empty()); }

    // ── P1 IR-backed legacy parity ──
    #[test] fn ir_backed_trend() { let evs=&[ev("pv","u1",1),ev("pv","u1",2),ev("pv","u2",5),ev("ck","u1",1)]; let (_d,e)=engine_with(evs); let h=e.trend("phc_t","pv",30).unwrap(); let i=e.trend_via_ir("phc_t","pv",30).unwrap(); assert_eq!(h.len(),i.len()); for(a,b)in h.iter().zip(i.iter()){assert_eq!(a.day,b.day);assert_eq!(a.count,b.count);} }
    #[test] fn ir_backed_stats() { let evs=&[ev("pv","u1",1),ev("pv","u2",2),ev("ck","u1",1),ev("ck","u3",100)]; let (_d,e)=engine_with(evs); let h=e.stats("phc_t").unwrap(); let i=e.stats_via_ir("phc_t").unwrap(); assert_eq!(h.total_events,i.total_events); assert_eq!(h.unique_persons,i.unique_persons); assert!((h.events_24h-i.events_24h).abs()<=1); }

    // ── P3 funnel + retention tests ──
    #[test] fn ir_funnels_matches_handwritten() { let evs=&[ev("s","u1",5),ev("a","u1",4),ev("p","u1",3),ev("s","u2",5),ev("a","u2",4),ev("s","u3",5)]; let (_d,e)=engine_with(evs); let h=e.funnel("phc_t",&["s".into(),"a".into(),"p".into()]).unwrap(); let q=ir::Query{kind:ir::QueryKind::Funnels,series:vec![ir::Series{event:ir::EventMatch::Name("s".into()),math:ir::Math::Total},ir::Series{event:ir::EventMatch::Name("a".into()),math:ir::Math::Total},ir::Series{event:ir::EventMatch::Name("p".into()),math:ir::Math::Total}],funnel_config:Some(ir::FunnelConfig{order_type:ir::FunnelOrder::Ordered,conversion_window_seconds:None,exclusions:vec![],attribution:ir::FunnelAttribution::AllSteps}),..ir::Query::trends(vec![],ir::DateRange{from:None,to:None,last_n:None})}; let r=e.run_ir(&q,"phc_t").unwrap(); assert_eq!(r.results.len(),3); assert_eq!(r.results[0].data[0].count,h[0].reached); assert_eq!(r.results[1].data[0].count,h[1].reached); assert_eq!(r.results[2].data[0].count,h[2].reached); }
    #[test] fn ir_retention_matrix() { let evs=&[ev("s","u1",5),ev("l","u1",5),ev("l","u1",7),ev("s","u2",6),ev("l","u2",6),ev("s","u3",6)]; let (_d,e)=engine_with(evs); let q=ir::Query{kind:ir::QueryKind::Retention,retention_config:Some(ir::RetentionConfig{cohort_event:ir::EventMatch::Name("s".into()),retention_event:ir::EventMatch::Name("l".into()),..Default::default()}),..ir::Query::trends(vec![],ir::DateRange{from:None,to:None,last_n:None})}; assert!(e.run_ir(&q,"phc_t").unwrap().results.len()>=1); }

    // ── P0 oracle agreement ──
    fn varied() -> Vec<CapturedEvent> { let n=["s","a","p","pv","ck"]; let mut evs=vec![]; for i in 0..400usize{let u=i%37;let mut e=ev(n[(i*7)%n.len()],&format!("u{u}"),(i%50)as i64);e.uuid=Uuid::from_u128((i%380)as u128);evs.push(e);} evs }
    #[test] fn oracle_agrees() { let evs=varied(); let (_d,e)=engine_with(&evs); let s=e.stats("phc_t").unwrap(); let (ot,op)=oracle::total_and_persons(&evs,"phc_t"); assert_eq!(s.total_events,ot); assert_eq!(s.unique_persons,op); let st:std::collections::HashMap<String,i64>=e.top_events("phc_t",100).unwrap().into_iter().map(|x|(x.event,x.count)).collect(); assert_eq!(st,oracle::top_events(&evs,"phc_t")); for steps in [vec!["s".into(),"a".into(),"p".into()],vec!["pv".into(),"ck".into()],vec!["ck".into(),"s".into(),"a".into()]] { let sq:Vec<i64>=e.funnel("phc_t",&steps).unwrap().into_iter().map(|s|s.reached).collect(); assert_eq!(sq,oracle::funnel(&evs,"phc_t",&steps)); } }
}

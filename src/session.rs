//! Sessionization — groups events into sessions and maintains a session
//! rollup table. 30-minute idle gap. Respects `$session_id` from SDKs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use uuid::Uuid;

use crate::capture::event::CapturedEvent;

const SESSION_IDLE_TIMEOUT_MINUTES: i64 = 30;

pub struct SessionStore {
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub enum SessionError {
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for SessionError {
    fn from(e: rusqlite::Error) -> Self {
        SessionError::Db(e)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub distinct_id: String,
    pub start_time: String,
    pub end_time: String,
    pub duration_seconds: i64,
    pub event_count: i64,
    pub entry_event: String,
    pub exit_event: String,
    pub is_bounce: bool,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(SessionStore { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self, SessionError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(SessionStore { conn: Mutex::new(conn) })
    }

    pub fn ingest(&self, events: &[CapturedEvent], token: &str) -> Result<Vec<SessionInfo>, SessionError> {
        let sessions = sessionize(events, token);
        let now = Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn.prepare_cached(
            "INSERT INTO sessions (
                session_id, token, distinct_id, start_time, end_time,
                duration_seconds, event_count, entry_event, exit_event, is_bounce,
                last_updated
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(session_id) DO UPDATE SET
                end_time = MAX(sessions.end_time, ?5),
                duration_seconds = MAX(sessions.duration_seconds, ?6),
                event_count = MAX(sessions.event_count, ?7),
                exit_event = CASE WHEN ?5 > sessions.end_time THEN ?9 ELSE sessions.exit_event END,
                is_bounce = CASE WHEN sessions.event_count + ?7 <= 1 THEN 1 ELSE 0 END,
                last_updated = ?11",
        )?;

        let tx = conn.unchecked_transaction()?;
        for session in &sessions {
            stmt.execute(rusqlite::params![
                &session.session_id, token, &session.distinct_id,
                session.start_time, session.end_time, session.duration_seconds,
                session.event_count, &session.entry_event, &session.exit_event,
                session.is_bounce as i64, now,
            ])?;
        }
        tx.commit()?;
        Ok(sessions)
    }

    pub fn sessions_for_person(&self, token: &str, distinct_id: &str, limit: usize) -> Result<Vec<SessionInfo>, SessionError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT session_id, distinct_id,
                    datetime(start_time, 'unixepoch') AS start_ts,
                    datetime(end_time, 'unixepoch') AS end_ts,
                    duration_seconds, event_count, entry_event, exit_event, is_bounce
             FROM sessions WHERE token = ?1 AND distinct_id = ?2
             ORDER BY start_time DESC LIMIT ?3",
        )?;
        let rows: Vec<SessionInfo> = stmt
            .query_map(rusqlite::params![token, distinct_id, limit as i64], |r| {
                Ok(SessionInfo {
                    session_id: r.get(0)?, distinct_id: r.get(1)?,
                    start_time: r.get(2)?, end_time: r.get(3)?,
                    duration_seconds: r.get(4)?, event_count: r.get(5)?,
                    entry_event: r.get(6)?, exit_event: r.get(7)?,
                    is_bounce: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn session_count(&self, token: &str, since: Option<i64>) -> Result<i64, SessionError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = if let Some(s) = since {
            conn.query_row(
                "SELECT count(*) FROM sessions WHERE token = ?1 AND start_time >= ?2",
                rusqlite::params![token, s],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT count(*) FROM sessions WHERE token = ?1",
                [token],
                |r| r.get(0),
            )?
        };
        Ok(count)
    }
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS sessions (
        session_id TEXT NOT NULL PRIMARY KEY,
        token TEXT NOT NULL,
        distinct_id TEXT NOT NULL,
        start_time INTEGER NOT NULL,
        end_time INTEGER NOT NULL,
        duration_seconds INTEGER NOT NULL DEFAULT 0,
        event_count INTEGER NOT NULL DEFAULT 0,
        entry_event TEXT NOT NULL DEFAULT '',
        exit_event TEXT NOT NULL DEFAULT '',
        is_bounce INTEGER NOT NULL DEFAULT 0,
        last_updated INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_sessions_token_start ON sessions(token, start_time);
    CREATE INDEX IF NOT EXISTS idx_sessions_person ON sessions(token, distinct_id);
";

fn sessionize(events: &[CapturedEvent], _token: &str) -> Vec<SessionInfo> {
    let mut by_person: HashMap<&str, Vec<&CapturedEvent>> = HashMap::new();
    for e in events {
        by_person.entry(e.distinct_id.as_str()).or_default().push(e);
    }

    let mut session_events: HashMap<String, Vec<&CapturedEvent>> = HashMap::new();
    let mut session_start: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut session_end: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut session_entry: HashMap<String, String> = HashMap::new();
    let mut session_exit: HashMap<String, String> = HashMap::new();
    let mut session_person: HashMap<String, String> = HashMap::new();

    for (distinct_id, person_events) in &mut by_person {
        person_events.sort_by_key(|e| e.timestamp);
        let mut current_sid: Option<String> = None;
        let mut last_ts: Option<DateTime<Utc>> = None;

        for event in person_events {
            let start_new = match last_ts {
                None => true,
                Some(last) => {
                    let gap = event.timestamp - last;
                    gap > Duration::minutes(SESSION_IDLE_TIMEOUT_MINUTES)
                }
            };
            if start_new {
                current_sid = Some(
                    event.properties.get("$session_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| Uuid::new_v4().to_string()),
                );
            }
            if let Some(ref sid) = current_sid {
                session_events.entry(sid.clone()).or_default().push(event);
                session_start.entry(sid.clone()).and_modify(|e| { if event.timestamp < *e { *e = event.timestamp; } }).or_insert(event.timestamp);
                session_end.entry(sid.clone()).and_modify(|e| { if event.timestamp > *e { *e = event.timestamp; } }).or_insert(event.timestamp);
                session_entry.entry(sid.clone()).or_insert_with(|| event.event.clone());
                session_exit.insert(sid.clone(), event.event.clone());
                session_person.entry(sid.clone()).or_insert_with(|| distinct_id.to_string());
            }
            last_ts = Some(event.timestamp);
        }
    }

    session_events.into_iter().map(|(sid, evts)| {
        let start = session_start.get(&sid).copied().unwrap_or(Utc::now());
        let end = session_end.get(&sid).copied().unwrap_or(start);
        let duration = (end.timestamp() - start.timestamp()).max(0);
        SessionInfo {
            session_id: sid.clone(),
            distinct_id: session_person.get(&sid).cloned().unwrap_or_default(),
            start_time: start.timestamp().to_string(),
            end_time: end.timestamp().to_string(),
            duration_seconds: duration,
            event_count: evts.len() as i64,
            entry_event: session_entry.get(&sid).cloned().unwrap_or_default(),
            exit_event: session_exit.get(&sid).cloned().unwrap_or_default(),
            is_bounce: evts.len() <= 1,
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::{Map, Value};
    use uuid::Uuid;

    fn timestamp(ago_minutes: i64) -> DateTime<Utc> { Utc::now() - Duration::minutes(ago_minutes) }

    fn ev_with_ts(event: &str, did: &str, ts: DateTime<Utc>, sid: Option<&str>) -> CapturedEvent {
        let mut props = Map::new();
        if let Some(s) = sid { props.insert("$session_id".into(), Value::String(s.into())); }
        CapturedEvent { uuid: Uuid::new_v4(), event: event.into(), distinct_id: did.into(), token: "phc_t".into(), timestamp: ts, properties: props }
    }

    #[test] fn single_event_is_bounce() { let s = sessionize(&[ev_with_ts("pv", "u1", timestamp(5), None)], "x"); assert_eq!(s.len(), 1); assert!(s[0].is_bounce); }
    #[test] fn within_30_min_share_session() { let s = sessionize(&[ev_with_ts("a", "u1", timestamp(20), None), ev_with_ts("b", "u1", timestamp(10), None), ev_with_ts("c", "u1", timestamp(5), None)], "x"); assert_eq!(s.len(), 1); assert_eq!(s[0].event_count, 3); }
    #[test] fn gap_over_30_creates_new() { let s = sessionize(&[ev_with_ts("a", "u1", timestamp(60), None), ev_with_ts("b", "u1", timestamp(20), None)], "x"); assert_eq!(s.len(), 2); }
    #[test] fn respects_session_id() { let s = sessionize(&[ev_with_ts("a", "u1", timestamp(60), Some("sid_x")), ev_with_ts("b", "u1", timestamp(20), None)], "x"); assert!(s.iter().any(|si| si.session_id == "sid_x")); }
    #[test] fn different_persons_separate() { let s = sessionize(&[ev_with_ts("a", "u1", timestamp(5), None), ev_with_ts("a", "u2", timestamp(4), None)], "x"); assert_eq!(s.len(), 2); }
    #[test] fn store_ingest_and_query() { let store = SessionStore::open_in_memory().unwrap(); store.ingest(&[ev_with_ts("pv", "u1", timestamp(20), None), ev_with_ts("cl", "u1", timestamp(10), None)], "phc_t").unwrap(); assert_eq!(store.sessions_for_person("phc_t", "u1", 10).unwrap().len(), 1); }
}

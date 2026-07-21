//! Feature flag definitions and evaluation (compat-spec.md "Flags").
//!
//! Bucketing matches PostHog byte-for-byte: `sha1("{key}.{distinct_id}")`,
//! first 60 bits as a fraction in [0,1), enabled when that fraction is below
//! `rollout_percentage/100`. Matching their hash means a flag at 30% rollout
//! selects the *same* users on Hoglet as on PostHog — the compatibility that
//! makes us a true drop-in (`why-hoglet.md`), not just wire-shaped.
//!
//! Definitions live in SQLite, keyed by token. Response *shapes* are owned by
//! `routes/flags.rs`; this module owns *which flags are on for whom*.

use std::sync::Mutex;

use rusqlite::{Connection, params};
use sha1::{Digest, Sha1};

/// 60-bit scale: the max value of the first 15 hex chars of the SHA1 digest.
const LONG_SCALE: f64 = 0xfff_ffff_ffff_ffff_u64 as f64;

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatedFlag {
    pub key: String,
    pub enabled: bool,
}

pub struct FlagStore {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS feature_flags (
    token TEXT NOT NULL,
    key TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    rollout_percentage REAL NOT NULL DEFAULT 100.0,
    PRIMARY KEY (token, key)
);
";

impl FlagStore {
    pub fn open(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::open(Connection::open_in_memory()?)
    }

    pub fn upsert(&self, token: &str, key: &str, active: bool, rollout: f64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO feature_flags (token, key, active, rollout_percentage)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(token, key) DO UPDATE SET active=?3, rollout_percentage=?4",
            params![token, key, active as i64, rollout],
        )?;
        Ok(())
    }

    /// Evaluate every active flag for this token against `distinct_id`.
    pub fn evaluate(&self, token: &str, distinct_id: &str) -> Vec<EvaluatedFlag> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT key, rollout_percentage FROM feature_flags WHERE token=?1 AND active=1")
        {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map(params![token], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        });
        let Ok(rows) = rows else { return vec![] };
        rows.flatten()
            .map(|(key, rollout)| EvaluatedFlag {
                enabled: is_enabled(&key, distinct_id, rollout),
                key,
            })
            .collect()
    }
}

/// PostHog's consistent-hash bucketing. A user's fraction is stable across
/// calls, so raising the rollout only ever adds users, never reshuffles them.
fn hash_fraction(key: &str, distinct_id: &str) -> f64 {
    let mut hasher = Sha1::new();
    hasher.update(format!("{key}.{distinct_id}").as_bytes());
    let digest = hasher.finalize();
    let hex = hex::encode(digest);
    let first15 = &hex[..15];
    let val = u64::from_str_radix(first15, 16).unwrap_or(0);
    val as f64 / LONG_SCALE
}

fn is_enabled(key: &str, distinct_id: &str, rollout_percentage: f64) -> bool {
    if rollout_percentage >= 100.0 {
        return true;
    }
    if rollout_percentage <= 0.0 {
        return false;
    }
    hash_fraction(key, distinct_id) <= rollout_percentage / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_rollout_always_on() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "new-ui", true, 100.0).unwrap();
        let flags = s.evaluate("phc_t", "anyone");
        assert_eq!(flags, vec![EvaluatedFlag { key: "new-ui".into(), enabled: true }]);
    }

    #[test]
    fn zero_rollout_always_off() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "off", true, 0.0).unwrap();
        assert!(!s.evaluate("phc_t", "anyone")[0].enabled);
    }

    #[test]
    fn inactive_flags_excluded() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "dead", false, 100.0).unwrap();
        assert!(s.evaluate("phc_t", "u").is_empty());
    }

    #[test]
    fn bucketing_is_consistent_per_user() {
        // Same user, same flag → same answer every time.
        let a = is_enabled("flag", "user-42", 50.0);
        let b = is_enabled("flag", "user-42", 50.0);
        assert_eq!(a, b);
    }

    #[test]
    fn raising_rollout_only_adds_users() {
        // A user enabled at 30% must still be enabled at 60% (monotonic).
        let users: Vec<String> = (0..500).map(|i| format!("u{i}")).collect();
        for u in &users {
            if is_enabled("f", u, 30.0) {
                assert!(is_enabled("f", u, 60.0), "user {u} dropped when rollout rose");
            }
        }
    }

    #[test]
    fn rollout_fraction_is_roughly_accurate() {
        // ~50% of many users enabled at 50% rollout (statistical, wide bound).
        let n = 2000;
        let on = (0..n).filter(|i| is_enabled("f", &format!("u{i}"), 50.0)).count();
        let frac = on as f64 / n as f64;
        assert!((0.42..0.58).contains(&frac), "got {frac}");
    }
}

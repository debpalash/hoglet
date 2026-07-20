//! Identity — persons, distinct_ids, and PostHog's merge semantics
//! (compat-spec.md "Identity", claims.md claim 4).
//!
//! Invariants:
//! - A (token, distinct_id) maps to exactly one person at any time.
//! - Merges are deterministic: the person keyed by the event's own
//!   distinct_id wins property conflicts (`{...loser, ...winner}`), the
//!   surviving `created_at` is the older, self-merge is a no-op.
//! - `$identify`/`$create_alias` refuse to merge an already-identified other
//!   person — warn and accept the event, never error. Only
//!   `$merge_dangerously` overrides.
//!
//! All writes go through one connection behind a mutex: SQLite is our single
//! writer by design (stack.md).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

use crate::capture::event::CapturedEvent;

pub struct IdentityStore {
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub enum IdentityError {
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for IdentityError {
    fn from(e: rusqlite::Error) -> Self {
        IdentityError::Db(e)
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS persons (
    id INTEGER PRIMARY KEY,
    token TEXT NOT NULL,
    created_at TEXT NOT NULL,
    is_identified INTEGER NOT NULL DEFAULT 0,
    properties TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS distinct_ids (
    token TEXT NOT NULL,
    distinct_id TEXT NOT NULL,
    person_id INTEGER NOT NULL REFERENCES persons(id),
    PRIMARY KEY (token, distinct_id)
);
CREATE INDEX IF NOT EXISTS idx_distinct_person ON distinct_ids(person_id);
";

impl IdentityStore {
    pub fn open(path: &Path) -> Result<Self, IdentityError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self, IdentityError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Process one captured event's identity effects. Regular events create
    /// a person for unseen distinct_ids; `$identify`/`$create_alias`/
    /// `$merge_dangerously` merge per the rules above.
    pub fn process(&self, event: &CapturedEvent) -> Result<(), IdentityError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        match event.event.as_str() {
            "$identify" => {
                let person = ensure_person(&tx, &event.token, &event.distinct_id, event)?;
                mark_identified(&tx, person)?;
                if let Some(anon) = event
                    .properties
                    .get("$anon_distinct_id")
                    .and_then(Value::as_str)
                {
                    merge(&tx, &event.token, anon, &event.distinct_id, false, event)?;
                }
                apply_set_props(&tx, &event.token, &event.distinct_id, event)?;
            }
            "$create_alias" => {
                ensure_person(&tx, &event.token, &event.distinct_id, event)?;
                if let Some(alias) = event.properties.get("alias").and_then(Value::as_str) {
                    merge(&tx, &event.token, alias, &event.distinct_id, false, event)?;
                }
            }
            "$merge_dangerously" => {
                ensure_person(&tx, &event.token, &event.distinct_id, event)?;
                if let Some(alias) = event.properties.get("alias").and_then(Value::as_str) {
                    merge(&tx, &event.token, alias, &event.distinct_id, true, event)?;
                }
            }
            _ => {
                ensure_person(&tx, &event.token, &event.distinct_id, event)?;
                apply_set_props(&tx, &event.token, &event.distinct_id, event)?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Person id for a distinct_id, if any (tests + future query layer).
    pub fn person_of(&self, token: &str, distinct_id: &str) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT person_id FROM distinct_ids WHERE token=?1 AND distinct_id=?2",
            params![token, distinct_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn person_count(&self, token: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM persons WHERE token=?1",
            params![token],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    pub fn person_properties(&self, token: &str, distinct_id: &str) -> Option<Map<String, Value>> {
        let conn = self.conn.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT p.properties FROM persons p
                 JOIN distinct_ids d ON d.person_id = p.id
                 WHERE d.token=?1 AND d.distinct_id=?2",
                params![token, distinct_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()?;
        serde_json::from_str::<Value>(&json)
            .ok()?
            .as_object()
            .cloned()
    }
}

fn ensure_person(
    tx: &rusqlite::Transaction,
    token: &str,
    distinct_id: &str,
    event: &CapturedEvent,
) -> Result<i64, IdentityError> {
    if let Some(id) = tx
        .query_row(
            "SELECT person_id FROM distinct_ids WHERE token=?1 AND distinct_id=?2",
            params![token, distinct_id],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    tx.execute(
        "INSERT INTO persons (token, created_at, is_identified) VALUES (?1, ?2, 0)",
        params![token, event.timestamp.to_rfc3339()],
    )?;
    let person_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO distinct_ids (token, distinct_id, person_id) VALUES (?1, ?2, ?3)",
        params![token, distinct_id, person_id],
    )?;
    Ok(person_id)
}

fn mark_identified(tx: &rusqlite::Transaction, person_id: i64) -> Result<(), IdentityError> {
    tx.execute(
        "UPDATE persons SET is_identified=1 WHERE id=?1",
        params![person_id],
    )?;
    Ok(())
}

/// Merge `loser_did`'s person into `winner_did`'s person. Winner is the
/// person keyed by the event's own distinct_id.
fn merge(
    tx: &rusqlite::Transaction,
    token: &str,
    loser_did: &str,
    winner_did: &str,
    dangerous: bool,
    event: &CapturedEvent,
) -> Result<(), IdentityError> {
    let winner = ensure_person(tx, token, winner_did, event)?;
    let loser = ensure_person(tx, token, loser_did, event)?;

    // Self-merge is a silent no-op.
    if winner == loser {
        return Ok(());
    }

    // Refusal rule: don't merge an already-identified person unless
    // $merge_dangerously. Accept the event, log, no error.
    if !dangerous {
        let loser_identified: i64 = tx.query_row(
            "SELECT is_identified FROM persons WHERE id=?1",
            params![loser],
            |r| r.get(0),
        )?;
        if loser_identified != 0 {
            tracing::warn!(
                token,
                loser_did,
                winner_did,
                "refusing to merge already-identified person"
            );
            return Ok(());
        }
    }

    let (winner_props, winner_created): (String, String) = tx.query_row(
        "SELECT properties, created_at FROM persons WHERE id=?1",
        params![winner],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (loser_props, loser_created): (String, String) = tx.query_row(
        "SELECT properties, created_at FROM persons WHERE id=?1",
        params![loser],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // {...loser, ...winner} — winner wins conflicts.
    let mut merged: Map<String, Value> = serde_json::from_str(&loser_props).unwrap_or_default();
    let winner_map: Map<String, Value> = serde_json::from_str(&winner_props).unwrap_or_default();
    for (k, v) in winner_map {
        merged.insert(k, v);
    }
    let created_at = if loser_created < winner_created {
        loser_created
    } else {
        winner_created
    };

    tx.execute(
        "UPDATE distinct_ids SET person_id=?1 WHERE person_id=?2",
        params![winner, loser],
    )?;
    tx.execute(
        "UPDATE persons SET properties=?1, created_at=?2 WHERE id=?3",
        params![
            serde_json::Value::Object(merged).to_string(),
            created_at,
            winner
        ],
    )?;
    tx.execute("DELETE FROM persons WHERE id=?1", params![loser])?;
    Ok(())
}

/// Fold `$set` / `$set_once` into the person keyed by this distinct_id.
fn apply_set_props(
    tx: &rusqlite::Transaction,
    token: &str,
    distinct_id: &str,
    event: &CapturedEvent,
) -> Result<(), IdentityError> {
    let set = event.properties.get("$set").and_then(Value::as_object);
    let set_once = event.properties.get("$set_once").and_then(Value::as_object);
    if set.is_none() && set_once.is_none() {
        return Ok(());
    }
    let person = ensure_person(tx, token, distinct_id, event)?;
    let current: String = tx.query_row(
        "SELECT properties FROM persons WHERE id=?1",
        params![person],
        |r| r.get(0),
    )?;
    let mut props: Map<String, Value> = serde_json::from_str(&current).unwrap_or_default();
    if let Some(once) = set_once {
        for (k, v) in once {
            props.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    if let Some(set) = set {
        for (k, v) in set {
            props.insert(k.clone(), v.clone());
        }
    }
    tx.execute(
        "UPDATE persons SET properties=?1 WHERE id=?2",
        params![serde_json::Value::Object(props).to_string(), person],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn event(name: &str, distinct_id: &str, props: Value) -> CapturedEvent {
        CapturedEvent {
            uuid: Uuid::new_v4(),
            event: name.into(),
            distinct_id: distinct_id.into(),
            token: "phc_t".into(),
            timestamp: Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap(),
            properties: props.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn anonymous_event_creates_person() {
        let store = IdentityStore::in_memory().unwrap();
        store.process(&event("click", "anon1", Value::Null)).unwrap();
        assert!(store.person_of("phc_t", "anon1").is_some());
        assert_eq!(store.person_count("phc_t"), 1);
    }

    #[test]
    fn identify_stitches_anon_history_once() {
        let store = IdentityStore::in_memory().unwrap();
        store.process(&event("click", "anon1", Value::Null)).unwrap();
        store
            .process(&event(
                "$identify",
                "user@x.com",
                serde_json::json!({"$anon_distinct_id": "anon1"}),
            ))
            .unwrap();
        // Same person behind both ids, exactly one person.
        assert_eq!(
            store.person_of("phc_t", "anon1"),
            store.person_of("phc_t", "user@x.com")
        );
        assert_eq!(store.person_count("phc_t"), 1);
    }

    #[test]
    fn identify_refuses_merging_identified_person() {
        let store = IdentityStore::in_memory().unwrap();
        // Two users identify separately.
        store
            .process(&event(
                "$identify",
                "a@x.com",
                serde_json::json!({"$anon_distinct_id": "anon_a"}),
            ))
            .unwrap();
        store
            .process(&event(
                "$identify",
                "b@x.com",
                serde_json::json!({"$anon_distinct_id": "anon_b"}),
            ))
            .unwrap();
        // Attempt to fold identified b into a via identify — refused.
        store
            .process(&event(
                "$identify",
                "a@x.com",
                serde_json::json!({"$anon_distinct_id": "b@x.com"}),
            ))
            .unwrap();
        assert_ne!(
            store.person_of("phc_t", "a@x.com"),
            store.person_of("phc_t", "b@x.com")
        );
        assert_eq!(store.person_count("phc_t"), 2);
    }

    #[test]
    fn merge_dangerously_overrides_refusal() {
        let store = IdentityStore::in_memory().unwrap();
        store
            .process(&event(
                "$identify",
                "a@x.com",
                serde_json::json!({"$anon_distinct_id": "anon_a"}),
            ))
            .unwrap();
        store
            .process(&event(
                "$identify",
                "b@x.com",
                serde_json::json!({"$anon_distinct_id": "anon_b"}),
            ))
            .unwrap();
        store
            .process(&event(
                "$merge_dangerously",
                "a@x.com",
                serde_json::json!({"alias": "b@x.com"}),
            ))
            .unwrap();
        assert_eq!(
            store.person_of("phc_t", "a@x.com"),
            store.person_of("phc_t", "b@x.com")
        );
        assert_eq!(store.person_count("phc_t"), 1);
    }

    #[test]
    fn winner_properties_beat_loser() {
        let store = IdentityStore::in_memory().unwrap();
        store
            .process(&event(
                "click",
                "anon1",
                serde_json::json!({"$set": {"plan": "free", "source": "ad"}}),
            ))
            .unwrap();
        store
            .process(&event(
                "click",
                "user@x.com",
                serde_json::json!({"$set": {"plan": "pro"}}),
            ))
            .unwrap();
        store
            .process(&event(
                "$identify",
                "user@x.com",
                serde_json::json!({"$anon_distinct_id": "anon1"}),
            ))
            .unwrap();
        let props = store.person_properties("phc_t", "user@x.com").unwrap();
        // Winner (user@x.com) keeps plan=pro; loser-only key survives.
        assert_eq!(props["plan"], "pro");
        assert_eq!(props["source"], "ad");
    }

    #[test]
    fn self_merge_is_noop() {
        let store = IdentityStore::in_memory().unwrap();
        store
            .process(&event(
                "$identify",
                "u",
                serde_json::json!({"$anon_distinct_id": "u"}),
            ))
            .unwrap();
        assert_eq!(store.person_count("phc_t"), 1);
    }

    #[test]
    fn set_once_does_not_overwrite() {
        let store = IdentityStore::in_memory().unwrap();
        store
            .process(&event(
                "e",
                "u",
                serde_json::json!({"$set_once": {"first_seen": "jan"}}),
            ))
            .unwrap();
        store
            .process(&event(
                "e",
                "u",
                serde_json::json!({"$set_once": {"first_seen": "feb"}}),
            ))
            .unwrap();
        let props = store.person_properties("phc_t", "u").unwrap();
        assert_eq!(props["first_seen"], "jan");
    }

    /// Claim 4's evidence shape: merge order does not change the final
    /// person graph. Permute a set of identify events; every permutation
    /// must converge to the same mapping.
    #[test]
    fn merge_order_permutations_converge() {
        let anon_ids = ["anon_a", "anon_b", "anon_c"];
        let build_ops = || {
            anon_ids
                .iter()
                .map(|anon| {
                    event(
                        "$identify",
                        "user@x.com",
                        serde_json::json!({"$anon_distinct_id": anon}),
                    )
                })
                .collect::<Vec<_>>()
        };

        let permutations: Vec<Vec<usize>> = vec![
            vec![0, 1, 2],
            vec![0, 2, 1],
            vec![1, 0, 2],
            vec![1, 2, 0],
            vec![2, 0, 1],
            vec![2, 1, 0],
        ];

        for perm in permutations {
            let store = IdentityStore::in_memory().unwrap();
            // Anonymous browsing first, in permuted order too.
            for &i in perm.iter().rev() {
                store
                    .process(&event("click", anon_ids[i], Value::Null))
                    .unwrap();
            }
            let ops = build_ops();
            for &i in &perm {
                store.process(&ops[i]).unwrap();
            }
            // Convergence: one person, all ids attached.
            assert_eq!(store.person_count("phc_t"), 1, "perm {perm:?} diverged");
            let user = store.person_of("phc_t", "user@x.com");
            for anon in &anon_ids {
                assert_eq!(store.person_of("phc_t", anon), user, "perm {perm:?}");
            }
        }
    }
}

//! Independent reference implementation of the analytics, in plain Rust
//! (spec/README.md "query lane" oracle; decisions.md build method).
//!
//! This computes the same numbers as the DuckDB SQL — stats, top events,
//! funnel — but from a `&[CapturedEvent]` slice with HashMaps and sorts,
//! sharing no code with the SQL path. Tests run both over the same data and
//! assert they agree, so a bug in one is caught by the other. It is also the
//! "slow obvious" funnel the fast SQL path is checked against.
//!
//! The real-PostHog oracle (diff insight results against a live PostHog) needs
//! a PostHog deployment and is out of reach here; this catches the class of
//! bug that matters most — our SQL disagreeing with a straightforward
//! computation of the same question.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::capture::event::CapturedEvent;

/// Dedup by uuid keeping the earliest timestamp — matches the SQL
/// `QUALIFY row_number() OVER (PARTITION BY uuid ORDER BY timestamp) = 1`.
fn deduped<'a>(events: &'a [CapturedEvent], token: &str) -> Vec<&'a CapturedEvent> {
    let mut by_uuid: HashMap<uuid::Uuid, &CapturedEvent> = HashMap::new();
    for e in events {
        by_uuid
            .entry(e.uuid)
            .and_modify(|kept| {
                if e.timestamp < kept.timestamp {
                    *kept = e;
                }
            })
            .or_insert(e);
    }
    let mut v: Vec<&CapturedEvent> = by_uuid.into_values().filter(|e| e.token == token).collect();
    v.sort_by_key(|e| e.timestamp);
    v
}

pub fn total_and_persons(events: &[CapturedEvent], token: &str) -> (i64, i64) {
    let d = deduped(events, token);
    let persons: std::collections::HashSet<&str> =
        d.iter().map(|e| e.distinct_id.as_str()).collect();
    (d.len() as i64, persons.len() as i64)
}

pub fn events_since(events: &[CapturedEvent], token: &str, cutoff: DateTime<Utc>) -> i64 {
    deduped(events, token)
        .iter()
        .filter(|e| e.timestamp >= cutoff)
        .count() as i64
}

/// (event, count) pairs, sorted by count desc. Ties are left unordered —
/// callers comparing against SQL should compare as a map to avoid tie-order
/// mismatch.
pub fn top_events(events: &[CapturedEvent], token: &str) -> HashMap<String, i64> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    for e in deduped(events, token) {
        *counts.entry(e.event.clone()).or_insert(0) += 1;
    }
    counts
}

/// Ordered funnel: reached count per step. Step 0 is the first time each
/// person did `steps[0]`; step i is the first time at/after step i-1.
pub fn funnel(events: &[CapturedEvent], token: &str, steps: &[String]) -> Vec<i64> {
    if steps.is_empty() {
        return vec![];
    }
    let d = deduped(events, token);

    // Per person, the sorted (timestamp, event) list.
    let mut by_person: HashMap<&str, Vec<(&DateTime<Utc>, &str)>> = HashMap::new();
    for e in &d {
        by_person
            .entry(e.distinct_id.as_str())
            .or_default()
            .push((&e.timestamp, e.event.as_str()));
    }
    for v in by_person.values_mut() {
        v.sort_by_key(|(t, _)| **t);
    }

    let mut reached = vec![0i64; steps.len()];
    for events in by_person.values() {
        // Walk steps in order; advance a cursor through the person's timeline.
        let mut step = 0usize;
        for (_, ev) in events {
            if *ev == steps[step] {
                reached[step] += 1;
                step += 1;
                if step == steps.len() {
                    break;
                }
            }
        }
    }
    reached
}

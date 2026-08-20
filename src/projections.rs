//! Ordered, rebuildable identity and catalog projections.
//!
//! The publication coordinator owns the surrounding SQLite transaction.  It
//! calls [`apply_captured_event`] in WAL order, publishes the event-file
//! generation and advances its checkpoint in that same transaction.  This
//! module intentionally does not own either generations or checkpoints.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use crate::capture::event::CapturedEvent;
use crate::pipeline::wal::WalCursor;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projected_events (
    project_id TEXT NOT NULL,
    uuid TEXT NOT NULL,
    wal_segment INTEGER NOT NULL CHECK(wal_segment >= 0),
    wal_offset INTEGER NOT NULL CHECK(wal_offset >= 0),
    PRIMARY KEY (project_id, uuid)
);
CREATE INDEX IF NOT EXISTS projected_events_wal_order
    ON projected_events(wal_segment, wal_offset);

CREATE TABLE IF NOT EXISTS persons (
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    is_identified INTEGER NOT NULL DEFAULT 0 CHECK(is_identified IN (0, 1)),
    properties TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(properties)),
    first_seen_key TEXT NOT NULL,
    PRIMARY KEY (project_id, id)
);

CREATE TABLE IF NOT EXISTS distinct_ids (
    project_id TEXT NOT NULL,
    distinct_id TEXT NOT NULL,
    person_id TEXT NOT NULL,
    PRIMARY KEY (project_id, distinct_id),
    FOREIGN KEY (project_id, person_id)
        REFERENCES persons(project_id, id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS distinct_ids_person
    ON distinct_ids(project_id, person_id);

CREATE TABLE IF NOT EXISTS event_names (
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    count INTEGER NOT NULL CHECK(count > 0),
    PRIMARY KEY (project_id, name)
);

CREATE TABLE IF NOT EXISTS property_keys (
    project_id TEXT NOT NULL,
    source TEXT NOT NULL,
    key TEXT NOT NULL,
    type_guess TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    count INTEGER NOT NULL CHECK(count > 0),
    PRIMARY KEY (project_id, source, key)
);

CREATE TABLE IF NOT EXISTS property_values (
    project_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    count INTEGER NOT NULL CHECK(count > 0),
    PRIMARY KEY (project_id, key, value)
);
"#;

/// Whether the event changed projections or was already applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

#[derive(Debug)]
pub enum ProjectionError {
    Database(rusqlite::Error),
    InvalidJson(serde_json::Error),
    InvalidProjectId,
    CursorOutOfRange(WalCursor),
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "projection database error: {error}"),
            Self::InvalidJson(error) => write!(formatter, "invalid projected JSON: {error}"),
            Self::InvalidProjectId => formatter.write_str("project id is empty"),
            Self::CursorOutOfRange(cursor) => write!(
                formatter,
                "WAL cursor {}:{} exceeds SQLite's integer range",
                cursor.segment, cursor.byte_offset
            ),
        }
    }
}

impl std::error::Error for ProjectionError {}

impl From<rusqlite::Error> for ProjectionError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<serde_json::Error> for ProjectionError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidJson(error)
    }
}

/// Install projection tables into the shared `projections.db` connection.
///
/// The schema has no generation/checkpoint DDL, which lets the event-lake
/// owner evolve those tables independently.
pub fn initialize_schema(connection: &Connection) -> Result<(), ProjectionError> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

/// Apply one event's rebuildable effects inside the caller's publication
/// transaction.
///
/// The `(project_id, uuid)` guard is inserted first.  A duplicate therefore
/// returns before identity or catalog state can be counted twice.  Callers
/// must invoke this function in WAL order; `$set`, `$set_once`, and identify
/// merges consequently have deterministic last-writer behavior.
pub fn apply_captured_event(
    transaction: &Transaction<'_>,
    project_id: &str,
    event: &CapturedEvent,
    cursor: WalCursor,
) -> Result<ApplyOutcome, ProjectionError> {
    if project_id.trim().is_empty() {
        return Err(ProjectionError::InvalidProjectId);
    }
    let segment =
        i64::try_from(cursor.segment).map_err(|_| ProjectionError::CursorOutOfRange(cursor))?;
    let offset =
        i64::try_from(cursor.byte_offset).map_err(|_| ProjectionError::CursorOutOfRange(cursor))?;

    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO projected_events(
             project_id, uuid, wal_segment, wal_offset
         ) VALUES (?1, ?2, ?3, ?4)",
        params![project_id, event.uuid.to_string(), segment, offset],
    )?;
    if inserted == 0 {
        return Ok(ApplyOutcome::Duplicate);
    }

    apply_identity(transaction, project_id, event)?;
    apply_catalog(transaction, project_id, event)?;
    Ok(ApplyOutcome::Applied)
}

fn apply_identity(
    transaction: &Transaction<'_>,
    project_id: &str,
    event: &CapturedEvent,
) -> Result<(), ProjectionError> {
    let winner = ensure_person(transaction, project_id, &event.distinct_id, event)?;

    if event.event == "$identify" {
        transaction.execute(
            "UPDATE persons SET is_identified=1 WHERE project_id=?1 AND id=?2",
            params![project_id, winner],
        )?;

        if let Some(anonymous_id) = event
            .properties
            .get("$anon_distinct_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            merge_persons(
                transaction,
                project_id,
                anonymous_id,
                &event.distinct_id,
                event,
            )?;
        }
    }

    apply_person_properties(transaction, project_id, &event.distinct_id, event)
}

fn ensure_person(
    transaction: &Transaction<'_>,
    project_id: &str,
    distinct_id: &str,
    event: &CapturedEvent,
) -> Result<String, ProjectionError> {
    if let Some(person_id) = transaction
        .query_row(
            "SELECT person_id FROM distinct_ids
             WHERE project_id=?1 AND distinct_id=?2",
            params![project_id, distinct_id],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(person_id);
    }

    // Capture validation bounds distinct ids, making the first identity key a
    // compact and deterministic person id during every rebuild.
    let person_id = distinct_id.to_owned();
    transaction.execute(
        "INSERT INTO persons(
             project_id, id, created_at, is_identified, properties, first_seen_key
         ) VALUES (?1, ?2, ?3, 0, '{}', ?2)",
        params![project_id, person_id, event.timestamp.to_rfc3339()],
    )?;
    transaction.execute(
        "INSERT INTO distinct_ids(project_id, distinct_id, person_id)
         VALUES (?1, ?2, ?3)",
        params![project_id, distinct_id, person_id],
    )?;
    Ok(person_id)
}

fn merge_persons(
    transaction: &Transaction<'_>,
    project_id: &str,
    losing_distinct_id: &str,
    winning_distinct_id: &str,
    event: &CapturedEvent,
) -> Result<(), ProjectionError> {
    let winner = ensure_person(transaction, project_id, winning_distinct_id, event)?;
    let loser = ensure_person(transaction, project_id, losing_distinct_id, event)?;
    if winner == loser {
        return Ok(());
    }

    // Match the safe PostHog identify behavior: a normal identify does not
    // silently collapse two already identified people.
    let loser_is_identified: bool = transaction.query_row(
        "SELECT is_identified FROM persons WHERE project_id=?1 AND id=?2",
        params![project_id, loser],
        |row| row.get(0),
    )?;
    if loser_is_identified {
        return Ok(());
    }

    let (winner_json, winner_created, winner_first_seen): (String, String, String) = transaction
        .query_row(
            "SELECT properties, created_at, first_seen_key
             FROM persons WHERE project_id=?1 AND id=?2",
            params![project_id, winner],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let (loser_json, loser_created): (String, String) = transaction.query_row(
        "SELECT properties, created_at
         FROM persons WHERE project_id=?1 AND id=?2",
        params![project_id, loser],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let mut merged = decode_properties(&loser_json)?;
    for (key, value) in decode_properties(&winner_json)? {
        merged.insert(key, value);
    }
    let created_at = std::cmp::min(winner_created, loser_created);

    transaction.execute(
        "UPDATE distinct_ids SET person_id=?1
         WHERE project_id=?2 AND person_id=?3",
        params![winner, project_id, loser],
    )?;
    transaction.execute(
        "UPDATE persons
         SET properties=?1, created_at=?2, first_seen_key=?3
         WHERE project_id=?4 AND id=?5",
        params![
            Value::Object(merged).to_string(),
            created_at,
            winner_first_seen,
            project_id,
            winner,
        ],
    )?;
    transaction.execute(
        "DELETE FROM persons WHERE project_id=?1 AND id=?2",
        params![project_id, loser],
    )?;
    Ok(())
}

fn apply_person_properties(
    transaction: &Transaction<'_>,
    project_id: &str,
    distinct_id: &str,
    event: &CapturedEvent,
) -> Result<(), ProjectionError> {
    let set_once = event.properties.get("$set_once").and_then(Value::as_object);
    let set = event.properties.get("$set").and_then(Value::as_object);
    if set_once.is_none() && set.is_none() {
        return Ok(());
    }

    let person_id = ensure_person(transaction, project_id, distinct_id, event)?;
    let encoded: String = transaction.query_row(
        "SELECT properties FROM persons WHERE project_id=?1 AND id=?2",
        params![project_id, person_id],
        |row| row.get(0),
    )?;
    let mut properties = decode_properties(&encoded)?;

    if let Some(values) = set_once {
        for (key, value) in values {
            properties
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    if let Some(values) = set {
        for (key, value) in values {
            properties.insert(key.clone(), value.clone());
        }
    }

    transaction.execute(
        "UPDATE persons SET properties=?1 WHERE project_id=?2 AND id=?3",
        params![Value::Object(properties).to_string(), project_id, person_id],
    )?;
    Ok(())
}

fn decode_properties(encoded: &str) -> Result<Map<String, Value>, ProjectionError> {
    let decoded: Value = serde_json::from_str(encoded)?;
    decoded
        .as_object()
        .cloned()
        .ok_or_else(|| ProjectionError::InvalidJson(json_object_expected_error()))
}

fn json_object_expected_error() -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom("person properties must be a JSON object")
}

fn apply_catalog(
    transaction: &Transaction<'_>,
    project_id: &str,
    event: &CapturedEvent,
) -> Result<(), ProjectionError> {
    let last_seen = event.timestamp.timestamp_millis();
    transaction.execute(
        "INSERT INTO event_names(project_id, name, last_seen, count)
         VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(project_id, name) DO UPDATE SET
             last_seen=max(event_names.last_seen, excluded.last_seen),
             count=event_names.count + 1",
        params![project_id, event.event, last_seen],
    )?;

    for (key, value) in &event.properties {
        transaction.execute(
            "INSERT INTO property_keys(
                 project_id, source, key, type_guess, last_seen, count
             ) VALUES (?1, 'event', ?2, ?3, ?4, 1)
             ON CONFLICT(project_id, source, key) DO UPDATE SET
                 type_guess=excluded.type_guess,
                 last_seen=max(property_keys.last_seen, excluded.last_seen),
                 count=property_keys.count + 1",
            params![project_id, key, value_type(value), last_seen],
        )?;

        if !value.is_null() {
            transaction.execute(
                "INSERT INTO property_values(project_id, key, value, last_seen, count)
                 VALUES (?1, ?2, ?3, ?4, 1)
                 ON CONFLICT(project_id, key, value) DO UPDATE SET
                     last_seen=max(property_values.last_seen, excluded.last_seen),
                     count=property_values.count + 1",
                params![project_id, key, value_text(value), last_seen],
            )?;
        }
    }
    Ok(())
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

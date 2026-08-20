use chrono::{TimeZone, Utc};
use hoglet::pipeline::wal::WalCursor;
use hoglet::projections::{ApplyOutcome, apply_captured_event, initialize_schema};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn event(
    project_token: &str,
    uuid: u128,
    name: &str,
    distinct_id: &str,
    properties: Value,
) -> hoglet::capture::event::CapturedEvent {
    hoglet::capture::event::CapturedEvent {
        uuid: Uuid::from_u128(uuid),
        event: name.to_owned(),
        distinct_id: distinct_id.to_owned(),
        token: project_token.to_owned(),
        timestamp: Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap(),
        properties: properties.as_object().cloned().unwrap_or_default(),
    }
}

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    initialize_schema(&connection).unwrap();
    connection
}

fn scalar(connection: &Connection, sql: &str, project_id: &str) -> i64 {
    connection
        .query_row(sql, [project_id], |row| row.get(0))
        .unwrap()
}

fn person_properties(
    connection: &Connection,
    project_id: &str,
    distinct_id: &str,
) -> Map<String, Value> {
    let encoded: String = connection
        .query_row(
            "SELECT p.properties
             FROM persons p
             JOIN distinct_ids d
               ON d.project_id = p.project_id AND d.person_id = p.id
             WHERE d.project_id = ?1 AND d.distinct_id = ?2",
            params![project_id, distinct_id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str::<Value>(&encoded)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

#[test]
fn duplicate_in_one_project_has_no_second_identity_or_catalog_effect() {
    let mut connection = connection();
    let captured = event(
        "phc_edge_token",
        1,
        "signup",
        "person-1",
        json!({"plan": "free", "$set": {"name": "Ada"}}),
    );

    let transaction = connection.transaction().unwrap();
    assert_eq!(
        apply_captured_event(&transaction, "project-a", &captured, WalCursor::new(4, 128),)
            .unwrap(),
        ApplyOutcome::Applied
    );
    assert_eq!(
        apply_captured_event(&transaction, "project-a", &captured, WalCursor::new(4, 256),)
            .unwrap(),
        ApplyOutcome::Duplicate
    );
    transaction.commit().unwrap();

    assert_eq!(
        scalar(
            &connection,
            "SELECT count FROM event_names WHERE project_id=?1 AND name='signup'",
            "project-a",
        ),
        1
    );
    assert_eq!(
        scalar(
            &connection,
            "SELECT count FROM property_keys WHERE project_id=?1 AND source='event' AND key='plan'",
            "project-a",
        ),
        1
    );
    assert_eq!(
        scalar(
            &connection,
            "SELECT count FROM property_values WHERE project_id=?1 AND key='plan' AND value='free'",
            "project-a",
        ),
        1
    );
    assert_eq!(
        person_properties(&connection, "project-a", "person-1")["name"],
        "Ada"
    );
}

#[test]
fn an_uuid_collision_in_two_projects_is_two_events() {
    let mut connection = connection();
    let captured = event("phc_edge_token", 7, "clicked", "same-person", json!({}));

    let transaction = connection.transaction().unwrap();
    assert_eq!(
        apply_captured_event(&transaction, "project-a", &captured, WalCursor::new(1, 10),).unwrap(),
        ApplyOutcome::Applied
    );
    assert_eq!(
        apply_captured_event(&transaction, "project-b", &captured, WalCursor::new(1, 20),).unwrap(),
        ApplyOutcome::Applied
    );
    transaction.commit().unwrap();

    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM projected_events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        scalar(
            &connection,
            "SELECT count FROM event_names WHERE project_id=?1 AND name='clicked'",
            "project-a",
        ),
        1
    );
    assert_eq!(
        scalar(
            &connection,
            "SELECT count FROM event_names WHERE project_id=?1 AND name='clicked'",
            "project-b",
        ),
        1
    );
}

#[test]
fn transaction_call_order_controls_latest_identity_values() {
    let mut connection = connection();
    let anonymous = event(
        "phc_token",
        20,
        "pageview",
        "anon-1",
        json!({"$set": {"plan": "free"}, "$set_once": {"origin": "landing"}}),
    );
    let identify = event(
        "phc_token",
        21,
        "$identify",
        "ada@example.com",
        json!({
            "$anon_distinct_id": "anon-1",
            "$set": {"plan": "pro", "name": "Ada"},
            "$set_once": {"origin": "identify", "first_campaign": "spring"}
        }),
    );
    let latest = event(
        "phc_token",
        22,
        "$identify",
        "ada@example.com",
        json!({
            "$set": {"plan": "enterprise"},
            "$set_once": {"first_campaign": "summer"}
        }),
    );

    let transaction = connection.transaction().unwrap();
    for (offset, captured) in [(10, &anonymous), (20, &identify), (30, &latest)] {
        assert_eq!(
            apply_captured_event(
                &transaction,
                "project-a",
                captured,
                WalCursor::new(9, offset),
            )
            .unwrap(),
            ApplyOutcome::Applied
        );
    }
    transaction.commit().unwrap();

    let anonymous_person: String = connection
        .query_row(
            "SELECT person_id FROM distinct_ids WHERE project_id=?1 AND distinct_id=?2",
            params!["project-a", "anon-1"],
            |row| row.get(0),
        )
        .unwrap();
    let identified_person: String = connection
        .query_row(
            "SELECT person_id FROM distinct_ids WHERE project_id=?1 AND distinct_id=?2",
            params!["project-a", "ada@example.com"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(anonymous_person, identified_person);

    let properties = person_properties(&connection, "project-a", "ada@example.com");
    assert_eq!(properties["plan"], "enterprise");
    assert_eq!(properties["name"], "Ada");
    assert_eq!(properties["origin"], "landing");
    assert_eq!(properties["first_campaign"], "spring");
}

#[test]
fn rolling_back_the_publication_transaction_removes_every_projection_effect() {
    let mut connection = connection();
    let captured = event(
        "phc_token",
        31,
        "purchase",
        "buyer-1",
        json!({"amount": 42, "$set": {"customer": true}}),
    );

    let transaction = connection.transaction().unwrap();
    apply_captured_event(
        &transaction,
        "project-a",
        &captured,
        WalCursor::new(12, 512),
    )
    .unwrap();
    transaction.rollback().unwrap();

    for table in [
        "projected_events",
        "persons",
        "distinct_ids",
        "event_names",
        "property_keys",
        "property_values",
    ] {
        let count = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} escaped the rolled-back transaction");
    }

    let missing: Option<String> = connection
        .query_row(
            "SELECT uuid FROM projected_events WHERE project_id=?1",
            ["project-a"],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(missing.is_none());
}

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{NaiveDate, TimeZone, Utc};
use hoglet::capture::event::CapturedEvent;
use hoglet::event_lake::{EventScope, GenerationId};
use hoglet::pipeline::publication::{PublicationCoordinator, PublicationError};
use hoglet::pipeline::wal::{CapturedBatch, WalConfig, WalCursor, WriteAheadLog};
use rusqlite::Connection;
use serde_json::{Map, Value};
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    projections_database: PathBuf,
    event_root: PathBuf,
    wal_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create test directory");
        let control_database = directory.path().join("control.db");
        let projections_database = directory.path().join("projections.db");
        let event_root = directory.path().join("events");
        let wal_root = directory.path().join("wal");
        std::fs::create_dir(&event_root).expect("create event root");
        hoglet::storage_bootstrap::bootstrap_storage(&control_database, &projections_database)
            .expect("bootstrap database pair");
        Self {
            _directory: directory,
            projections_database,
            event_root,
            wal_root,
        }
    }

    fn coordinator(&self) -> PublicationCoordinator {
        PublicationCoordinator::open(&self.projections_database, &self.event_root)
            .expect("open publication coordinator")
    }

    fn wal(&self, segment_target_bytes: u64) -> WriteAheadLog {
        WriteAheadLog::open(
            &self.wal_root,
            WalConfig::new(segment_target_bytes, 64 * 1024).expect("valid WAL config"),
        )
        .expect("open WAL")
        .0
    }

    fn state(&self) -> (i64, i64, i64, i64) {
        Connection::open(&self.projections_database)
            .expect("open projection database")
            .query_row(
                "SELECT current_generation_id, applied_wal_segment,
                        applied_wal_offset, data_epoch
                 FROM projection_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read projection state")
    }

    fn scalar(&self, sql: &str) -> i64 {
        Connection::open(&self.projections_database)
            .expect("open projection database")
            .query_row(sql, [], |row| row.get(0))
            .expect("read scalar")
    }
}

fn event(uuid: u128) -> CapturedEvent {
    let mut properties = Map::new();
    properties.insert("plan".to_owned(), Value::String("pro".to_owned()));
    CapturedEvent {
        uuid: Uuid::from_u128(uuid),
        event: "signed_up".to_owned(),
        distinct_id: "person-1".to_owned(),
        token: "phc_project_a".to_owned(),
        timestamp: Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap(),
        properties,
    }
}

fn authorized_batch(event: CapturedEvent) -> CapturedBatch {
    CapturedBatch::authorized(
        vec![event],
        BTreeMap::from([("phc_project_a".to_owned(), "project-a".to_owned())]),
        false,
    )
    .expect("valid durable project binding")
}

#[test]
fn projection_error_rolls_back_generation_checkpoint_and_every_projection_effect() {
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();
    let mut wal = fixture.wal(64 * 1024);
    wal.append(authorized_batch(event(1)))
        .expect("append durable event");
    wal.seal().expect("seal publication input");

    Connection::open(&fixture.projections_database)
        .expect("open projections database")
        .execute_batch(
            "CREATE TRIGGER fail_projection
             BEFORE INSERT ON event_names
             BEGIN
                 SELECT RAISE(ABORT, 'simulated projection failure');
             END;",
        )
        .expect("install deterministic projection failure");

    let error = coordinator
        .publish_pending(&wal, 32 * 1024 * 1024)
        .expect_err("projection failure aborts publication");
    assert!(matches!(error, PublicationError::Projection(_)));
    assert_eq!(fixture.state(), (0, 1, 0, 0));
    assert_eq!(fixture.scalar("SELECT count(*) FROM event_generations"), 1);
    assert_eq!(fixture.scalar("SELECT count(*) FROM event_files"), 0);
    assert_eq!(fixture.scalar("SELECT count(*) FROM projected_events"), 0);
    assert!(
        fixture.wal_root.join("0000000000000001.wal").exists(),
        "every pre-commit failure must leave Event Truth replayable"
    );

    let scope = EventScope::new(
        "project-a",
        NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
        NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
    )
    .unwrap();
    assert_eq!(
        coordinator.event_lake().acquire(&scope).generation_id(),
        GenerationId::EMPTY
    );
}

#[test]
fn replay_is_idempotent_and_reclamation_waits_for_a_normalized_eof_checkpoint() {
    let fixture = Fixture::new();
    let coordinator = fixture.coordinator();
    let mut wal = fixture.wal(64 * 1024);
    let first = wal
        .append(authorized_batch(event(7)))
        .expect("append first durable event");
    wal.append(authorized_batch(event(7)))
        .expect("append replay of the same captured event");
    wal.seal().expect("seal publication input");

    let first_publication = coordinator
        .publish_pending(&wal, first.span.framed_bytes)
        .expect("publish the first record only")
        .expect("one record is pending");
    assert_eq!(first_publication.checkpoint, first.span.end);
    assert_eq!(first_publication.data_epoch, 1);
    assert_eq!(first_publication.applied_events, 1);
    assert_eq!(first_publication.duplicate_events, 0);
    assert!(first_publication.reclaimed_segments.is_empty());
    assert!(
        fixture.wal_root.join("0000000000000001.wal").exists(),
        "a valid mid-segment checkpoint must not reclaim its segment"
    );
    assert_eq!(
        fixture.state(),
        (
            first_publication.generation_id.get() as i64,
            first.span.end.segment as i64,
            first.span.end.byte_offset as i64,
            1,
        )
    );

    let scope = EventScope::new(
        "project-a",
        NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
        NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
    )
    .unwrap();
    assert_eq!(
        coordinator.event_lake().acquire(&scope).generation_id(),
        first_publication.generation_id,
        "the in-memory lease must refresh immediately after commit"
    );

    let second_publication = coordinator
        .publish_pending(&wal, 32 * 1024 * 1024)
        .expect("publish replay through normalized EOF")
        .expect("one replay record is pending");
    assert_eq!(second_publication.checkpoint, WalCursor::new(2, 0));
    assert_eq!(second_publication.data_epoch, 2);
    assert_eq!(second_publication.applied_events, 0);
    assert_eq!(second_publication.duplicate_events, 1);
    assert_eq!(second_publication.reclaimed_segments, vec![1]);
    assert!(
        !fixture.wal_root.join("0000000000000001.wal").exists(),
        "a normalized checkpoint past sealed EOF may reclaim the segment"
    );
    assert_eq!(fixture.scalar("SELECT count(*) FROM projected_events"), 1);
    assert_eq!(
        fixture.scalar("SELECT count FROM event_names WHERE name = 'signed_up'"),
        1,
        "replay must not double-count catalog projections"
    );
    assert_eq!(
        fixture.state(),
        (second_publication.generation_id.get() as i64, 2, 0, 2,)
    );
}

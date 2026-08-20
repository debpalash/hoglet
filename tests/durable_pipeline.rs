use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use hoglet::capture::event::CapturedEvent;
use hoglet::pipeline::publication::PublicationCoordinator;
use hoglet::pipeline::wal::{CapturedBatch, WalConfig, WriteAheadLog};
use hoglet::sink::{AuthorizedEventBatch, DurableWalSink, EventSink};
use hoglet::storage_bootstrap::{StoragePaths, bootstrap_storage};
use serde_json::Map;
use uuid::Uuid;

#[tokio::test]
async fn durable_ack_is_published_before_clean_shutdown_completes() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let paths = StoragePaths::new(directory.path());
    bootstrap_storage(paths.control(), paths.projections()).expect("paired storage");
    let event_root = directory.path().join("events");
    std::fs::create_dir_all(&event_root).expect("event root");
    let coordinator = PublicationCoordinator::open(paths.projections(), &event_root)
        .expect("publication coordinator");
    let event_lake = coordinator.event_lake();
    let (sink, runtime, _) = DurableWalSink::open(directory.path().join("wal-v2"), coordinator)
        .expect("durable pipeline");
    let project_id = Uuid::new_v4().to_string();
    let event = CapturedEvent {
        uuid: Uuid::new_v4(),
        event: "signed_up".to_owned(),
        distinct_id: "person-1".to_owned(),
        token: "phc_pipeline_test".to_owned(),
        timestamp: Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap(),
        properties: Map::new(),
    };
    let mut bindings = BTreeMap::new();
    bindings.insert(event.token.clone(), project_id.clone());

    sink.append(AuthorizedEventBatch {
        events: vec![event.clone()],
        project_ids_by_token: bindings,
        historical_migration: false,
    })
    .await
    .expect("fsynced durable receipt");
    drop(sink);
    runtime.shutdown().await.expect("publish and join");

    let visible = event_lake.visible_files();
    assert_eq!(visible.len(), 1);
    assert!(matches!(
        &visible[0].scope,
        hoglet::event_lake::FileScope::ProjectDate { project_id: id, event_date }
            if id == &project_id && *event_date == event.timestamp.date_naive()
    ));
    let stored = hoglet::store::parquet::read_file(&visible[0].path).expect("published parquet");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].uuid, event.uuid);
}

#[tokio::test]
async fn startup_reclaims_wal_covered_by_an_already_committed_checkpoint() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let paths = StoragePaths::new(directory.path());
    bootstrap_storage(paths.control(), paths.projections()).expect("paired storage");
    let event_root = directory.path().join("events");
    std::fs::create_dir_all(&event_root).expect("event root");
    let coordinator = PublicationCoordinator::open(paths.projections(), &event_root)
        .expect("publication coordinator");
    let wal_root = directory.path().join("wal-v2");
    let (mut wal, _) = WriteAheadLog::open(&wal_root, WalConfig::default()).expect("WAL");

    let project_id = Uuid::new_v4().to_string();
    let event = CapturedEvent {
        uuid: Uuid::new_v4(),
        event: "signed_up".to_owned(),
        distinct_id: "person-1".to_owned(),
        token: "phc_pipeline_test".to_owned(),
        timestamp: Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap(),
        properties: Map::new(),
    };
    wal.append(
        CapturedBatch::authorized(
            vec![event.clone()],
            BTreeMap::from([(event.token.clone(), project_id.clone())]),
            false,
        )
        .expect("authorized batch"),
    )
    .expect("durable append");
    wal.seal().expect("sealed WAL");
    let sealed = wal_root.join("0000000000000001.wal");
    let committed_wal = std::fs::read(&sealed).expect("backup committed WAL segment");

    coordinator
        .publish_pending(&wal, 32 * 1024 * 1024)
        .expect("committed publication")
        .expect("one record is pending");
    assert!(!sealed.exists(), "normal publication reclaimed the segment");
    drop(wal);

    // Recreate the exact covered segment to model commit-before-unlink crash
    // recovery. There are no pending records after the committed checkpoint.
    std::fs::write(&sealed, committed_wal).expect("restore retained committed WAL");
    let (sink, runtime, _) =
        DurableWalSink::open(&wal_root, coordinator).expect("pipeline restart");
    assert!(
        !sealed.exists(),
        "startup must reclaim WAL even when no records remain to publish"
    );
    drop(sink);
    runtime.shutdown().await.expect("clean shutdown");
}

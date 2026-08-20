use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use hoglet::capture::event::CapturedEvent;
use hoglet::migration::{MigrationError, completion_marker_path, migrate_legacy_storage};
use hoglet::storage_bootstrap::{
    StorageDisposition, StoragePaths, bootstrap_storage, inspect_storage,
};
use rusqlite::Connection;
use serde_json::Map;
use uuid::Uuid;

fn event(token: &str, name: &str) -> CapturedEvent {
    CapturedEvent {
        uuid: Uuid::new_v4(),
        event: name.into(),
        distinct_id: "person-1".into(),
        token: token.into(),
        timestamp: Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap(),
        properties: Map::new(),
    }
}

fn seed_legacy_sources(data_dir: &Path) -> Vec<PathBuf> {
    let projects = data_dir.join("projects.db");
    Connection::open(&projects)
        .unwrap()
        .execute_batch(
            "CREATE TABLE projects (token TEXT PRIMARY KEY, name TEXT, created_at TEXT);
             INSERT INTO projects VALUES ('phc_sqlite','SQLite Project','2026-08-20T00:00:00Z');",
        )
        .unwrap();

    let events_dir = data_dir.join("events/legacy");
    fs::create_dir_all(&events_dir).unwrap();
    let parquet = events_dir.join("events.parquet");
    hoglet::store::parquet::write_file(&[event("phc_parquet", "from parquet")], &parquet).unwrap();

    let wal_dir = data_dir.join("wal");
    fs::create_dir_all(&wal_dir).unwrap();
    let wal = hoglet::wal::segment::segment_path(&wal_dir, 7);
    let payload = serde_json::to_vec(&vec![event("phc_wal", "from wal")]).unwrap();
    let mut record = Vec::new();
    hoglet::wal::segment::encode_record(&payload, &mut record);
    fs::write(&wal, record).unwrap();

    vec![projects, parquet, wal]
}

fn snapshot(paths: &[PathBuf]) -> BTreeMap<PathBuf, Vec<u8>> {
    paths
        .iter()
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect()
}

fn seed_legacy_resources(data_dir: &Path) -> Vec<PathBuf> {
    let flags = data_dir.join("flags.db");
    Connection::open(&flags)
        .unwrap()
        .execute_batch(
            r#"CREATE TABLE feature_flags (
                 token TEXT NOT NULL, key TEXT NOT NULL, active INTEGER NOT NULL,
                 rollout_percentage REAL NOT NULL, variants TEXT, conditions TEXT,
                 PRIMARY KEY(token,key)
             );
             INSERT INTO feature_flags VALUES (
                 'phc_resource','new-ui',1,25.0,
                 '[{"key":"control","rollout":100.0}]',
                 '{"properties":[],"cohort_ids":[],"payload":"welcome"}'
             );"#,
        )
        .unwrap();

    let dashboards = data_dir.join("dashboards.db");
    Connection::open(&dashboards)
        .unwrap()
        .execute_batch(
            r#"CREATE TABLE saved_insights (
                 id TEXT PRIMARY KEY, token TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL, query_ir TEXT NOT NULL, created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE TABLE dashboards (
                 id TEXT PRIMARY KEY, token TEXT NOT NULL, name TEXT NOT NULL,
                 created_by TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE TABLE dashboard_tiles (
                 id TEXT PRIMARY KEY, dashboard_id TEXT NOT NULL, insight_id TEXT NOT NULL,
                 grid_x INTEGER NOT NULL, grid_y INTEGER NOT NULL,
                 grid_w INTEGER NOT NULL, grid_h INTEGER NOT NULL
             );
             CREATE TABLE share_links (
                 id TEXT PRIMARY KEY, object_type TEXT NOT NULL, object_id TEXT NOT NULL,
                 token TEXT NOT NULL UNIQUE, created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL, expires_at INTEGER
             );
             INSERT INTO saved_insights VALUES (
                 'insight-legacy','phc_resource','Funnel','Broad query',
                 '{"kind":"Funnels","series":[{"event":{"type":"name","value":"signup"},"math":{"type":"total"}}],"filters":{"op":"AND","values":[]},"range":{"from":"2026-01-01T00:00:00Z","to":"2026-02-01T00:00:00Z"},"interval":"Day"}',
                 'legacy-user',100,200
             );
             INSERT INTO dashboards VALUES (
                 'dashboard-legacy','phc_resource','Legacy dashboard','legacy-user',300,400
             );
             INSERT INTO dashboard_tiles VALUES (
                 'tile-legacy','dashboard-legacy','insight-legacy',1,2,6,4
             );
             INSERT INTO share_links VALUES (
                 'share-legacy','dashboard','dashboard-legacy','legacy-share-token',
                 'legacy-user',500,900
             );"#,
        )
        .unwrap();
    vec![flags, dashboards]
}

#[test]
fn migration_discovers_every_event_token_and_never_changes_legacy_files() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    let legacy_paths = seed_legacy_sources(directory.path());
    let before = snapshot(&legacy_paths);

    assert_eq!(
        inspect_storage(&paths).unwrap(),
        StorageDisposition::LegacyOnly
    );
    let report = migrate_legacy_storage(&paths).expect("offline migration should complete");
    assert_eq!(report.discovered_tokens, 3);
    assert!(!report.resumed);
    assert!(!report.already_complete);
    assert!(completion_marker_path(&paths).is_file());
    assert!(matches!(
        inspect_storage(&paths).unwrap(),
        StorageDisposition::ReadyV2(_)
    ));

    let projections = Connection::open(paths.projections()).unwrap();
    let generation: i64 = projections
        .query_row(
            "SELECT current_generation_id FROM projection_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let published_files: i64 = projections
        .query_row(
            "SELECT count(*) FROM generation_files WHERE generation_id=?1",
            [generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(generation, 1);
    assert_eq!(published_files, 1);
    drop(projections);

    let control =
        Connection::open_with_flags(paths.control(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = control
        .prepare("SELECT capture_token FROM projects ORDER BY capture_token")
        .unwrap();
    let tokens = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(tokens, vec!["phc_parquet", "phc_sqlite", "phc_wal"]);
    drop(statement);
    drop(control);

    assert_eq!(snapshot(&legacy_paths), before);

    let second = migrate_legacy_storage(&paths).expect("completed migration is idempotent");
    assert!(second.already_complete);
    assert_eq!(second.pair_id, report.pair_id);
    assert_eq!(snapshot(&legacy_paths), before);
}

#[test]
fn completed_rerun_detects_legacy_source_drift() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    let legacy_paths = seed_legacy_sources(directory.path());
    migrate_legacy_storage(&paths).unwrap();

    let wal = legacy_paths.last().unwrap();
    let mut bytes = fs::read(wal).unwrap();
    bytes.push(0);
    fs::write(wal, bytes).unwrap();

    let error = migrate_legacy_storage(&paths)
        .expect_err("completed migration must remain bound to its legacy source snapshot");
    assert!(matches!(error, MigrationError::InvalidMarker(_)));
}

#[test]
fn legacy_resources_follow_an_orphan_capture_token_into_its_synthesized_project() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    seed_legacy_sources(directory.path());
    let resource_paths = seed_legacy_resources(directory.path());
    let before = snapshot(&resource_paths);

    migrate_legacy_storage(&paths).expect("legacy resources should import in shadows");
    let control = Connection::open(paths.control()).unwrap();
    let project_id: String = control
        .query_row(
            "SELECT id FROM projects WHERE capture_token='phc_resource'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let flag: (f64, String) = control
        .query_row(
            "SELECT rollout_percentage,payload FROM feature_flags
             WHERE project_id=?1 AND key='new-ui'",
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(flag, (25.0, "welcome".into()));
    let insight: (i64, i64, i64, String) = control
        .query_row(
            "SELECT query_supported,created_at,updated_at,query_ir
             FROM saved_insights WHERE project_id=?1 AND id='insight-legacy'",
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((insight.0, insight.1, insight.2), (0, 100, 200));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&insight.3).unwrap()["kind"],
        "Funnels"
    );
    let dashboard: (i64, i64) = control
        .query_row(
            "SELECT created_at,updated_at FROM dashboards
             WHERE project_id=?1 AND id='dashboard-legacy'",
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(dashboard, (300, 400));
    let tile_count: i64 = control
        .query_row(
            "SELECT count(*) FROM dashboard_tiles
             WHERE project_id=?1 AND dashboard_id='dashboard-legacy'",
            [&project_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tile_count, 1);
    let share: (String, i64, Option<i64>) = control
        .query_row(
            "SELECT token,created_at,expires_at FROM share_links
             WHERE project_id=?1 AND id='share-legacy'",
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(share, ("legacy-share-token".into(), 500, Some(900)));
    drop(control);
    assert_eq!(snapshot(&resource_paths), before);
}

#[test]
fn a_complete_shadow_pair_is_the_supported_crash_resume_state() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    seed_legacy_sources(directory.path());
    bootstrap_storage(paths.control_migrating(), paths.projections_migrating()).unwrap();
    assert_eq!(
        inspect_storage(&paths).unwrap(),
        StorageDisposition::MigrationIncomplete
    );

    let report = migrate_legacy_storage(&paths).expect("complete shadows should resume");
    assert!(report.resumed);
    assert!(matches!(
        inspect_storage(&paths).unwrap(),
        StorageDisposition::ReadyV2(_)
    ));
}

#[test]
fn an_unknown_incomplete_layout_is_refused_without_overwriting_it() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    let legacy_paths = seed_legacy_sources(directory.path());
    let before = snapshot(&legacy_paths);
    let unsupported = paths.projections_migrating();
    fs::write(&unsupported, b"not a complete shadow pair").unwrap();

    let error = migrate_legacy_storage(&paths).expect_err("unknown crash state must be refused");
    assert!(matches!(error, MigrationError::UnsupportedIncomplete(_)));
    assert_eq!(
        fs::read(&unsupported).unwrap(),
        b"not a complete shadow pair"
    );
    assert!(!paths.control().exists());
    assert!(!paths.projections().exists());
    assert_eq!(snapshot(&legacy_paths), before);
}

#[test]
fn a_torn_legacy_wal_is_refused_without_repairing_it() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    let legacy_paths = seed_legacy_sources(directory.path());
    let wal = legacy_paths.last().unwrap();
    let mut torn = fs::read(wal).unwrap();
    torn.pop();
    fs::write(wal, &torn).unwrap();

    let error = migrate_legacy_storage(&paths).expect_err("torn WAL must block migration");
    assert!(matches!(error, MigrationError::InvalidLegacyData { .. }));
    assert_eq!(fs::read(wal).unwrap(), torn);
    assert!(!paths.control().exists());
    assert!(!paths.projections().exists());
}

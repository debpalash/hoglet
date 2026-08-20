#[path = "../src/storage_bootstrap.rs"]
mod storage_bootstrap;

use std::path::Path;

use rusqlite::Connection;
use storage_bootstrap::{
    CONTROL_APPLICATION_ID, CONTROL_SCHEMA_VERSION, DatabaseRole, PROJECTIONS_APPLICATION_ID,
    PROJECTIONS_SCHEMA_VERSION, StorageBootstrapError, StorageDisposition, StoragePaths,
    bootstrap_storage, discover_legacy_tokens, inspect_storage,
};

fn pragma_i64(path: &Path, pragma: &str) -> i64 {
    let connection = Connection::open(path).unwrap();
    connection
        .pragma_query_value(None, pragma, |row| row.get(0))
        .unwrap()
}

#[test]
fn fresh_bootstrap_creates_a_durable_paired_generation_zero() {
    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.db");
    let projections = directory.path().join("projections.db");

    let first = bootstrap_storage(&control, &projections).unwrap();
    let second = bootstrap_storage(&control, &projections).unwrap();

    assert_eq!(first, second, "validating an existing pair is idempotent");
    assert!(!first.pair_id.is_empty());
    assert_eq!(first.current_generation_id, 0);
    assert_eq!(
        pragma_i64(&control, "application_id"),
        CONTROL_APPLICATION_ID
    );
    assert_eq!(pragma_i64(&control, "user_version"), CONTROL_SCHEMA_VERSION);
    assert_eq!(pragma_i64(&control, "synchronous"), 2, "FULL");
    assert_eq!(
        pragma_i64(&projections, "application_id"),
        PROJECTIONS_APPLICATION_ID
    );
    assert_eq!(
        pragma_i64(&projections, "user_version"),
        PROJECTIONS_SCHEMA_VERSION
    );
    assert_eq!(pragma_i64(&projections, "synchronous"), 2, "FULL");

    let control_connection = Connection::open(&control).unwrap();
    let control_meta: (String, String) = control_connection
        .query_row(
            "SELECT pair_id, database_role FROM database_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(control_meta, (first.pair_id.clone(), "control".into()));

    let projections_connection = Connection::open(&projections).unwrap();
    let projections_meta: (String, String) = projections_connection
        .query_row(
            "SELECT pair_id, database_role FROM database_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        projections_meta,
        (first.pair_id.clone(), "projections".into())
    );
    assert_eq!(
        projections_connection
            .query_row(
                "SELECT COUNT(*) FROM event_generations WHERE id = 0",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );
    let state: (i64, i64, i64) = projections_connection
        .query_row(
            "SELECT current_generation_id, applied_wal_segment, applied_wal_offset
             FROM projection_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, (0, 1, 0));
}

#[cfg(unix)]
#[test]
fn fresh_bootstrap_restricts_database_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.db");
    let projections = directory.path().join("projections.db");
    bootstrap_storage(&control, &projections).unwrap();

    assert_eq!(
        std::fs::metadata(control).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(projections).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn inspection_of_a_missing_data_directory_is_fresh_and_creates_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().join("not-created").join("hoglet-data");
    let paths = StoragePaths::new(&data_dir);

    assert_eq!(inspect_storage(&paths).unwrap(), StorageDisposition::Fresh);
    assert_eq!(paths.data_dir(), data_dir);
    assert!(!data_dir.exists(), "inspection must not create data_dir");
}

#[test]
fn inspection_recognizes_each_kind_of_legacy_event_truth() {
    for artifact in ["auth.db", "wal", "events"] {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("hoglet-data");
        std::fs::create_dir(&data_dir).unwrap();
        let artifact_path = data_dir.join(artifact);
        if artifact.ends_with(".db") {
            std::fs::File::create(&artifact_path).unwrap();
        } else {
            std::fs::create_dir(&artifact_path).unwrap();
        }

        assert_eq!(
            inspect_storage(&StoragePaths::new(&data_dir)).unwrap(),
            StorageDisposition::LegacyOnly,
            "{artifact} is legacy state that requires explicit migration"
        );
        assert!(!data_dir.join("control.db").exists());
        assert!(!data_dir.join("projections.db").exists());
    }
}

#[test]
fn inspection_validates_a_ready_pair_without_creating_sqlite_sidecars() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    let expected = bootstrap_storage(paths.control(), paths.projections()).unwrap();
    let entries_before = directory_entries(directory.path());

    let disposition = inspect_storage(&paths).unwrap();

    assert_eq!(disposition, StorageDisposition::ReadyV2(expected));
    assert_eq!(directory_entries(directory.path()), entries_before);
}

#[test]
fn inspection_reports_incomplete_pairs_and_staging_files_without_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StoragePaths::new(directory.path());
    std::fs::File::create(paths.control()).unwrap();

    assert_eq!(
        inspect_storage(&paths).unwrap(),
        StorageDisposition::MigrationIncomplete
    );
    assert!(!paths.projections().exists());

    std::fs::remove_file(paths.control()).unwrap();
    std::fs::File::create(directory.path().join("control.db.migrating")).unwrap();
    assert_eq!(
        inspect_storage(&paths).unwrap(),
        StorageDisposition::MigrationIncomplete
    );
    assert!(
        !paths.control().exists(),
        "inspection must not recover staging"
    );
}

#[test]
fn bootstrap_refuses_an_incomplete_pair() {
    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.db");
    let projections = directory.path().join("projections.db");
    Connection::open(&control).unwrap();

    let error = bootstrap_storage(&control, &projections).unwrap_err();
    assert!(matches!(
        error,
        StorageBootstrapError::IncompletePair {
            present: DatabaseRole::Control
        }
    ));
    assert!(
        !projections.exists(),
        "validation must not fill in half a pair"
    );
}

#[test]
fn bootstrap_finishes_when_projection_was_committed_before_control() {
    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.db");
    let projections = directory.path().join("projections.db");
    let expected = bootstrap_storage(&control, &projections).unwrap();
    let migrating_control = directory.path().join("control.db.migrating");
    std::fs::rename(&control, &migrating_control).unwrap();

    let recovered = bootstrap_storage(&control, &projections).unwrap();

    assert_eq!(recovered, expected);
    assert!(control.exists());
    assert!(!migrating_control.exists());
}

#[test]
fn bootstrap_refuses_pair_id_mismatch() {
    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.db");
    let projections = directory.path().join("projections.db");
    bootstrap_storage(&control, &projections).unwrap();
    Connection::open(&projections)
        .unwrap()
        .execute(
            "UPDATE database_meta SET pair_id = 'another-installation' WHERE singleton = 1",
            [],
        )
        .unwrap();

    let error = bootstrap_storage(&control, &projections).unwrap_err();
    assert!(matches!(
        error,
        StorageBootstrapError::PairIdMismatch { .. }
    ));
}

#[test]
fn bootstrap_refuses_a_newer_schema_without_rewriting_it() {
    let directory = tempfile::tempdir().unwrap();
    let control = directory.path().join("control.db");
    let projections = directory.path().join("projections.db");
    bootstrap_storage(&control, &projections).unwrap();
    let newer = CONTROL_SCHEMA_VERSION + 1;
    Connection::open(&control)
        .unwrap()
        .pragma_update(None, "user_version", newer)
        .unwrap();

    let error = bootstrap_storage(&control, &projections).unwrap_err();
    assert!(matches!(
        error,
        StorageBootstrapError::SchemaTooNew {
            role: DatabaseRole::Control,
            found,
            supported: CONTROL_SCHEMA_VERSION,
        } if found == newer
    ));
    assert_eq!(pragma_i64(&control, "user_version"), newer);
}

#[test]
fn legacy_token_discovery_reads_known_sqlite_token_columns_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let registry_path = directory.path().join("projects.db");
    let identity_path = directory.path().join("identity.db");
    let absent_path = directory.path().join("absent.db");

    let registry = Connection::open(&registry_path).unwrap();
    registry
        .execute_batch(
            "CREATE TABLE projects (token TEXT, name TEXT);
             INSERT INTO projects VALUES ('phc_registry', 'Registry');
             INSERT INTO projects VALUES ('', 'Blank');",
        )
        .unwrap();
    drop(registry);

    let identity = Connection::open(&identity_path).unwrap();
    identity
        .execute_batch(
            "CREATE TABLE persons (token TEXT, id INTEGER);
             CREATE TABLE event_names (token TEXT, name TEXT);
             CREATE TABLE share_links (token TEXT, object_id TEXT);
             CREATE TABLE unrelated (secret TEXT);
             INSERT INTO persons VALUES ('phc_identity', 1);
             INSERT INTO event_names VALUES ('phc_registry', '$pageview');
             INSERT INTO share_links VALUES ('phc_share_credential', 'dashboard-id');
             INSERT INTO unrelated VALUES ('phc_must_not_be_inventoried');",
        )
        .unwrap();
    drop(identity);

    let before_registry = std::fs::metadata(&registry_path).unwrap().len();
    let tokens = discover_legacy_tokens([&registry_path, &identity_path, &absent_path]).unwrap();

    assert_eq!(
        tokens.into_iter().collect::<Vec<_>>(),
        vec!["phc_identity".to_string(), "phc_registry".to_string()]
    );
    assert!(
        !absent_path.exists(),
        "read-only discovery must not create databases"
    );
    assert_eq!(
        std::fs::metadata(&registry_path).unwrap().len(),
        before_registry
    );
}

fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
    let mut entries = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

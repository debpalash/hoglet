#[path = "../src/event_lake/mod.rs"]
mod event_lake;
#[path = "../src/storage_bootstrap.rs"]
mod storage_bootstrap;

use std::path::PathBuf;

use chrono::NaiveDate;
use event_lake::{EventScope, FileScope, GenerationId, PublishedFile, VersionedEventLake};

struct Fixture {
    _directory: tempfile::TempDir,
    control_database: PathBuf,
    projections_database: PathBuf,
    event_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create test directory");
        let control_database = directory.path().join("control.db");
        let projections_database = directory.path().join("projections.db");
        let event_root = directory.path().join("events");
        std::fs::create_dir(&event_root).expect("create event root");
        storage_bootstrap::bootstrap_storage(&control_database, &projections_database)
            .expect("bootstrap database pair");
        Self {
            _directory: directory,
            control_database,
            projections_database,
            event_root,
        }
    }

    fn open(&self) -> VersionedEventLake {
        VersionedEventLake::open(&self.projections_database, &self.event_root)
            .expect("open event lake")
    }

    fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.event_root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create event partition");
        }
        std::fs::write(&path, contents).expect("write event file");
        path.canonicalize()
            .expect("canonicalize the event file path")
    }
}

fn date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("valid test date")
}

fn file(path: impl Into<PathBuf>, project_id: &str, event_date: &str) -> PublishedFile {
    PublishedFile::new(
        path,
        FileScope::ProjectDate {
            project_id: project_id.to_owned(),
            event_date: date(event_date),
        },
    )
}

fn error_message<T>(result: Result<T, event_lake::EventLakeError>) -> String {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn reader_lease_is_stable_while_a_new_generation_is_published() {
    let fixture = Fixture::new();
    let a = fixture.write("project-a/2026-08-19/a.parquet", b"a");
    let b = fixture.write("project-a/2026-08-20/b.parquet", b"b");
    let c = fixture.write("project-b/2026-08-20/c.parquet", b"c");
    let legacy = fixture.write("legacy.parquet", b"legacy");
    let d = fixture.write("project-a/2026-08-20/d.parquet", b"d");
    let lake = fixture.open();

    let scope =
        EventScope::new("project-a", date("2026-08-20"), date("2026-08-21")).expect("valid scope");
    let empty = lake.acquire(&scope);
    assert_eq!(empty.generation_id(), GenerationId::EMPTY);
    assert!(empty.is_empty());

    let first_generation = lake
        .publish_generation(vec![
            file("project-a/2026-08-19/a.parquet", "project-a", "2026-08-19"),
            file(&b, "project-a", "2026-08-20"),
            file(&c, "project-b", "2026-08-20"),
            PublishedFile::new("legacy.parquet", FileScope::LegacyUnknown),
        ])
        .expect("publish first generation");
    assert!(a.exists());

    let old_lease = lake.acquire(&scope);
    assert_eq!(old_lease.generation_id(), first_generation);
    assert_eq!(old_lease.paths(), vec![b.clone(), legacy.clone()]);

    let second_generation = lake
        .publish_generation(vec![
            file(&d, "project-a", "2026-08-20"),
            PublishedFile::new("legacy.parquet", FileScope::LegacyUnknown),
        ])
        .expect("publish second generation");
    assert!(second_generation > first_generation);

    let new_lease = lake.acquire(&scope);
    assert_eq!(new_lease.generation_id(), second_generation);
    assert_eq!(new_lease.paths(), vec![d, legacy.clone()]);
    assert_eq!(old_lease.generation_id(), first_generation);
    assert_eq!(
        old_lease.paths(),
        vec![b, legacy],
        "an acquired generation must not change underneath a reader"
    );
}

#[test]
fn publication_never_deletes_files_retired_from_the_next_manifest() {
    let fixture = Fixture::new();
    let old_path = fixture.write("old.parquet", b"reader still owns this generation");
    let lake = fixture.open();
    let scope =
        EventScope::new("project-a", date("2026-08-20"), date("2026-08-21")).expect("valid scope");

    lake.publish_generation(vec![file(&old_path, "project-a", "2026-08-20")])
        .expect("publish old file");
    let old_lease = lake.acquire(&scope);
    lake.publish_generation(Vec::new())
        .expect("publish empty generation");

    assert!(old_path.exists());
    assert_eq!(old_lease.paths(), vec![old_path]);
    assert!(lake.acquire(&scope).is_empty());
}

#[test]
fn visible_generation_is_reloaded_from_full_synchronous_sqlite_metadata() {
    let fixture = Fixture::new();
    let current = fixture.write("project-a/2026-08-20/a.parquet", b"current");
    let legacy = fixture.write("legacy.parquet", b"legacy");
    let generation;

    {
        let lake = fixture.open();
        generation = lake
            .publish_generation(vec![
                file(&current, "project-a", "2026-08-20"),
                PublishedFile::new("legacy.parquet", FileScope::LegacyUnknown),
            ])
            .expect("publish generation");
    }

    let reopened = fixture.open();
    let scope =
        EventScope::new("project-a", date("2026-08-20"), date("2026-08-21")).expect("valid scope");
    let lease = reopened.acquire(&scope);
    assert_eq!(lease.generation_id(), generation);
    assert_eq!(lease.paths(), vec![current, legacy]);

    let connection = rusqlite::Connection::open(&fixture.projections_database)
        .expect("open projections database");
    let synchronous: i64 = connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("read synchronous pragma");
    assert_eq!(
        synchronous, 2,
        "projection metadata must use FULL durability"
    );
}

#[test]
fn publication_records_content_identity_size_relative_path_and_unknown_row_count() {
    let fixture = Fixture::new();
    fixture.write("project-a/2026-08-20/known.parquet", b"abc");
    let lake = fixture.open();
    lake.publish_generation(vec![file(
        "project-a/2026-08-20/known.parquet",
        "project-a",
        "2026-08-20",
    )])
    .expect("publish known file");

    let connection = rusqlite::Connection::open(&fixture.projections_database)
        .expect("open projections database");
    let stored = connection
        .query_row(
            "SELECT id, relative_path, sha256, project_id, size_bytes, row_count
             FROM event_files",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .expect("read file metadata");
    let known_sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(stored.0, known_sha256, "file id must derive from content");
    assert_eq!(stored.1, "project-a/2026-08-20/known.parquet");
    assert_eq!(stored.2, known_sha256);
    assert_eq!(stored.3, "project-a");
    assert_eq!(stored.4, 3);
    assert_eq!(stored.5, None, "unknown row count must be stored as NULL");
}

#[test]
fn opening_requires_a_prebootstrapped_projections_database() {
    let directory = tempfile::tempdir().expect("create test directory");
    let event_root = directory.path().join("events");
    std::fs::create_dir(&event_root).expect("create event root");
    let missing_database = directory.path().join("missing.db");

    let message = error_message(VersionedEventLake::open(&missing_database, &event_root));
    assert!(!message.is_empty());
    assert!(
        !missing_database.exists(),
        "open must not create a projections database"
    );

    let blank_database = directory.path().join("blank.db");
    drop(rusqlite::Connection::open(&blank_database).expect("create blank database"));
    let message = error_message(VersionedEventLake::open(&blank_database, &event_root));
    assert!(message.contains("application id"));
    let connection = rusqlite::Connection::open(blank_database).expect("reopen blank database");
    let event_table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'event_files'",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema");
    assert_eq!(event_table_count, 0, "identity is checked before mutation");
}

#[test]
fn opening_rejects_a_database_with_the_wrong_metadata_role_before_mutation() {
    let fixture = Fixture::new();
    let connection =
        rusqlite::Connection::open(&fixture.control_database).expect("open control database");
    connection
        .pragma_update(
            None,
            "application_id",
            storage_bootstrap::PROJECTIONS_APPLICATION_ID,
        )
        .expect("make application id reach the role check");
    drop(connection);

    let message = error_message(VersionedEventLake::open(
        &fixture.control_database,
        &fixture.event_root,
    ));
    assert!(message.contains("expected \"projections\""), "{message}");
    let connection =
        rusqlite::Connection::open(&fixture.control_database).expect("reopen control database");
    let event_table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'event_files'",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema");
    assert_eq!(event_table_count, 0, "role is checked before mutation");
}

#[test]
fn opening_migrates_only_empty_legacy_event_file_metadata() {
    let fixture = Fixture::new();
    let connection = rusqlite::Connection::open(&fixture.projections_database)
        .expect("open projections database");
    connection
        .execute_batch(
            "CREATE TABLE event_files (
                id TEXT PRIMARY KEY NOT NULL,
                path TEXT NOT NULL UNIQUE,
                checksum TEXT NOT NULL,
                project_token TEXT,
                partition_day TEXT,
                size_bytes INTEGER NOT NULL,
                row_count INTEGER NOT NULL
             );
             CREATE TABLE generation_files (
                generation_id INTEGER NOT NULL REFERENCES event_generations(id),
                file_id TEXT NOT NULL REFERENCES event_files(id),
                ordinal INTEGER NOT NULL,
                PRIMARY KEY (generation_id, file_id)
             );",
        )
        .expect("install empty legacy schema");
    drop(connection);

    drop(fixture.open());

    let connection = rusqlite::Connection::open(&fixture.projections_database)
        .expect("reopen projections database");
    let mut statement = connection
        .prepare("PRAGMA table_info(event_files)")
        .expect("inspect event_files schema");
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("read columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect columns");
    assert!(columns.iter().any(|column| column == "project_id"));
    assert!(columns.iter().any(|column| column == "relative_path"));
    assert!(!columns.iter().any(|column| column == "project_token"));
}

#[test]
fn publication_rejects_files_outside_the_event_root() {
    let fixture = Fixture::new();
    let outside = fixture
        .event_root
        .parent()
        .expect("event root has parent")
        .join("outside.parquet");
    std::fs::write(&outside, b"outside").expect("write outside file");
    let lake = fixture.open();

    let message =
        error_message(lake.publish_generation(vec![file(outside, "project-a", "2026-08-20")]));
    assert!(
        message.contains("outside the configured event root"),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn publication_rejects_symbolic_links_even_when_the_target_is_inside_the_event_root() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let target = fixture.write("target.parquet", b"inside");
    let link = fixture.event_root.join("link.parquet");
    symlink(&target, &link).expect("create symlink");
    let lake = fixture.open();

    let message = error_message(lake.publish_generation(vec![file(
        "link.parquet",
        "project-a",
        "2026-08-20",
    )]));
    assert!(
        message.contains("symbolic links cannot be published"),
        "{message}"
    );
}

#[test]
fn immutable_path_cannot_be_reused_for_different_content() {
    let fixture = Fixture::new();
    let path = fixture.write("reused.parquet", b"first");
    let lake = fixture.open();
    let first_generation = lake
        .publish_generation(vec![file(&path, "project-a", "2026-08-20")])
        .expect("publish first contents");

    std::fs::write(&path, b"second").expect("replace event file contents");
    let message =
        error_message(lake.publish_generation(vec![file(&path, "project-a", "2026-08-20")]));
    assert!(
        message.contains("was already assigned to content"),
        "{message}"
    );
    let scope =
        EventScope::new("project-a", date("2026-08-20"), date("2026-08-21")).expect("valid scope");
    assert_eq!(
        lake.acquire(&scope).generation_id(),
        first_generation,
        "failed publication must not advance visible metadata"
    );
}

#[test]
fn existing_content_cannot_be_reattributed_to_another_project() {
    let fixture = Fixture::new();
    let path = fixture.write("immutable.parquet", b"immutable");
    let lake = fixture.open();
    lake.publish_generation(vec![file(&path, "project-a", "2026-08-20")])
        .expect("publish project-a metadata");

    let message =
        error_message(lake.publish_generation(vec![file(&path, "project-b", "2026-08-20")]));
    assert!(message.contains("immutable metadata changed"), "{message}");
}

#[test]
fn invalid_date_range_is_rejected() {
    assert!(EventScope::new("project-a", date("2026-08-20"), date("2026-08-20")).is_err());
    assert!(EventScope::new("project-a", date("2026-08-21"), date("2026-08-20")).is_err());
}

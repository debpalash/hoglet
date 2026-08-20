use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{NaiveDate, TimeZone, Utc};
use hoglet::capture::event::CapturedEvent;
use hoglet::event_lake::{EventScope, FileScope, PublishedFile, VersionedEventLake};
use hoglet::query::ir::{DateRange, EventMatch, Math, Query, Series};
use hoglet::query::supported::SupportedQuery;
use hoglet::query::{QueryEngine, QueryError};
use hoglet::storage_bootstrap::bootstrap_storage;
use hoglet::store::EventStore;
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    projections_database: PathBuf,
    event_root: PathBuf,
    store: EventStore,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let control_database = directory.path().join("control.db");
        let projections_database = directory.path().join("projections.db");
        bootstrap_storage(&control_database, &projections_database)
            .expect("storage pair should bootstrap");
        let event_root = directory.path().join("events");
        let store = EventStore::open(event_root.clone()).expect("event store should open");
        Self {
            _directory: directory,
            projections_database,
            event_root,
            store,
        }
    }

    fn lake(&self) -> Arc<VersionedEventLake> {
        Arc::new(
            VersionedEventLake::open(&self.projections_database, &self.event_root)
                .expect("event lake should open"),
        )
    }

    fn write(&self, events: &[CapturedEvent]) -> PathBuf {
        self.store
            .write_events(events)
            .expect("events should be written")
            .into_iter()
            .next()
            .expect("one partition should produce one file")
    }
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("test date should be valid")
}

fn event(token: &str, distinct_id: &str, year: i32, month: u32, day: u32) -> CapturedEvent {
    CapturedEvent {
        uuid: Uuid::new_v4(),
        event: "pageview".into(),
        distinct_id: distinct_id.into(),
        token: token.into(),
        timestamp: Utc
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .expect("test timestamp should be valid"),
        properties: serde_json::Map::new(),
    }
}

fn published(path: &Path, project_id: &str, event_date: NaiveDate) -> PublishedFile {
    PublishedFile::new(
        path,
        FileScope::ProjectDate {
            project_id: project_id.into(),
            event_date,
        },
    )
}

fn trends() -> SupportedQuery {
    SupportedQuery::try_from(Query::trends(
        vec![Series {
            event: EventMatch::Name("pageview".into()),
            math: Math::Total,
        }],
        DateRange {
            from: Some("2026-01-01T00:00:00Z".into()),
            to: Some("2026-02-01T00:00:00Z".into()),
            last_n: None,
        },
    ))
    .expect("query should be in the supported Trends subset")
}

fn total(response: &hoglet::query::ir::QueryResponse) -> i64 {
    response.results[0]
        .data
        .iter()
        .map(|point| point.count)
        .sum()
}

#[test]
fn project_query_uses_one_leased_generation_and_generation_scoped_cache_identity() {
    let fixture = Fixture::new();
    let lake = fixture.lake();
    let project_id = Uuid::new_v4().to_string();
    let other_project_id = Uuid::new_v4().to_string();
    let capture_token = "phc_authorized";

    let first = fixture.write(&[event(capture_token, "first", 2026, 1, 15)]);
    let other_project = fixture.write(&[event(capture_token, "other-project", 2026, 1, 15)]);
    let outside_range = fixture.write(&[event(capture_token, "outside-range", 2025, 12, 15)]);
    let first_generation = lake
        .publish_generation(vec![
            published(&first, &project_id, date(2026, 1, 15)),
            published(&other_project, &other_project_id, date(2026, 1, 15)),
            published(&outside_range, &project_id, date(2025, 12, 15)),
        ])
        .expect("first generation should publish");
    let engine =
        QueryEngine::try_new_versioned(lake.clone()).expect("versioned query engine should open");

    let first_response = engine
        .run_supported_for_project(&trends(), &project_id, capture_token, false)
        .expect("first project query should run");
    assert_eq!(first_response.meta.generation_id, first_generation.get());
    assert_eq!(total(&first_response), 1);
    assert!(!first_response.meta.cached);

    let scope = EventScope::new(project_id.clone(), date(2026, 1, 1), date(2026, 2, 2))
        .expect("scope should be valid");
    let in_flight = lake.acquire(&scope);
    let second = fixture.write(&[
        event(capture_token, "second-a", 2026, 1, 16),
        event(capture_token, "second-b", 2026, 1, 16),
    ]);
    let second_generation = lake
        .publish_generation(vec![published(&second, &project_id, date(2026, 1, 16))])
        .expect("second generation should publish");

    assert_eq!(in_flight.generation_id(), first_generation);
    assert_eq!(
        in_flight.paths(),
        vec![std::fs::canonicalize(first).expect("published file is canonical")]
    );
    let second_response = engine
        .run_supported_for_project(&trends(), &project_id, capture_token, false)
        .expect("new project query should use the new generation");
    assert_eq!(second_response.meta.generation_id, second_generation.get());
    assert_eq!(total(&second_response), 2);
    assert!(!second_response.meta.cached);

    let cached = engine
        .run_supported_for_project(&trends(), &project_id, capture_token, false)
        .expect("same-generation query should use its cache entry");
    assert_eq!(cached.meta.generation_id, second_generation.get());
    assert_eq!(total(&cached), 2);
    assert!(cached.meta.cached);
}

#[test]
fn empty_scoped_generation_returns_empty_trends_without_discovering_files() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.event_root.join("unpublished.parquet"),
        b"not parquet",
    )
    .expect("unpublished sentinel should be written");
    let lake = fixture.lake();
    let engine = QueryEngine::try_new_versioned(lake).expect("versioned query engine should open");

    let response = engine
        .run_supported_for_project(&trends(), &Uuid::new_v4().to_string(), "phc_unused", false)
        .expect("an empty lease should not ask DuckDB to read a glob");

    assert_eq!(response.meta.generation_id, 0);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].label, "pageview");
    assert!(response.results[0].data.is_empty());
}

#[test]
fn legacy_engine_cannot_back_the_authenticated_project_query_route() {
    let fixture = Fixture::new();
    let engine = QueryEngine::try_new(fixture.event_root.clone())
        .expect("legacy compatibility engine should open");

    assert!(matches!(
        engine.run_supported_for_project(
            &trends(),
            &Uuid::new_v4().to_string(),
            "phc_unused",
            false,
        ),
        Err(QueryError::VersionedSourceRequired)
    ));
}

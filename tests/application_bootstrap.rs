use std::fs;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hoglet::application::{Application, ApplicationConfig, ApplicationError};
use hoglet::storage_bootstrap::{
    StorageDisposition, StoragePaths, bootstrap_storage, inspect_storage,
};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn fresh_storage_bootstraps_a_safe_production_router() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let config = ApplicationConfig::new(directory.path());

    let application = Application::prepare(config)
        .await
        .expect("fresh application starts");
    assert!(matches!(
        inspect_storage(&StoragePaths::new(directory.path())).expect("storage inspection"),
        StorageDisposition::ReadyV2(_)
    ));

    let router = application.router();
    assert_eq!(
        router
            .clone()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        router
            .clone()
            .oneshot(
                Request::get("/api/stats?token=phc_should_not_be_accepted")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
        "the legacy raw-token dashboard interface is not mounted"
    );
    assert_eq!(
        router
            .clone()
            .oneshot(
                Request::get("/api/admin/projects")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
        "the legacy admin interface is not mounted"
    );

    let wire_preflight = router
        .clone()
        .oneshot(
            Request::options("/e/")
                .header(header::ORIGIN, "https://sdk.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        wire_preflight
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        "wire capture is cross-origin by contract"
    );

    let dashboard_preflight = router
        .oneshot(
            Request::options("/api/workspace")
                .header(header::ORIGIN, "https://untrusted.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !dashboard_preflight
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        "dashboard and control routes do not inherit wire CORS"
    );

    application.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn legacy_storage_is_rejected_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let legacy = directory.path().join("auth.db");
    let original = b"legacy bytes that normal startup must not touch";
    fs::write(&legacy, original).expect("legacy fixture");

    let error = Application::prepare(ApplicationConfig::new(directory.path()))
        .await
        .expect_err("legacy storage requires explicit migration");

    assert!(matches!(error, ApplicationError::LegacyOnly { .. }));
    assert_eq!(fs::read(&legacy).expect("legacy fixture remains"), original);
    assert!(!directory.path().join("control.db").exists());
    assert!(!directory.path().join("projections.db").exists());
    assert!(!directory.path().join("events").exists());
    assert!(!directory.path().join("wal-v2").exists());
}

#[tokio::test]
async fn incomplete_storage_is_rejected_without_recovery_writes() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let partial = directory.path().join("control.db.migrating");
    fs::write(&partial, b"partial migration").expect("partial fixture");

    let error = Application::prepare(ApplicationConfig::new(directory.path()))
        .await
        .expect_err("incomplete migration cannot be guessed at startup");

    assert!(matches!(
        error,
        ApplicationError::MigrationIncomplete { .. }
    ));
    assert_eq!(
        fs::read(&partial).expect("partial fixture remains"),
        b"partial migration"
    );
    assert!(!directory.path().join("control.db").exists());
    assert!(!directory.path().join("projections.db").exists());
}

#[tokio::test]
async fn exact_fresh_pair_publish_crash_is_recovered_after_validation() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let paths = StoragePaths::new(directory.path());
    let expected = bootstrap_storage(paths.control(), paths.projections())
        .expect("fresh pair should bootstrap");
    fs::rename(paths.control(), paths.control_migrating())
        .expect("simulate crash after projections rename");

    let application = Application::prepare(ApplicationConfig::new(directory.path()))
        .await
        .expect("the exact fresh bootstrap crash state should recover");
    let recovered = match inspect_storage(&paths).expect("recovered pair should inspect") {
        StorageDisposition::ReadyV2(metadata) => metadata,
        disposition => panic!("expected ready storage, got {disposition:?}"),
    };
    assert_eq!(recovered.pair_id, expected.pair_id);
    assert!(!paths.control_migrating().exists());
    application.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn authorized_capture_reaches_a_leased_generation_query() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let application = Application::prepare(ApplicationConfig::new(directory.path()))
        .await
        .expect("application starts");
    let router = application.router();
    let setup = router
        .clone()
        .oneshot(
            Request::post("/api/auth/setup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "email": "owner@example.com",
                        "password": "correct horse battery staple",
                        "organization_name": "Acme",
                        "project_name": "Website",
                        "existing_project_token": "phc_application_e2e"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("setup response");
    assert_eq!(setup.status(), StatusCode::OK);
    let cookie = setup
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .expect("session cookie")
        .to_owned();
    let setup_body: serde_json::Value = serde_json::from_slice(
        &setup
            .into_body()
            .collect()
            .await
            .expect("setup body")
            .to_bytes(),
    )
    .expect("workspace JSON");
    let project_id = setup_body["organizations"][0]["projects"][0]["id"]
        .as_str()
        .expect("project id")
        .to_owned();

    let capture = router
        .clone()
        .oneshot(
            Request::post("/e/")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "event": "signed_up",
                        "distinct_id": "person-1",
                        "token": "phc_application_e2e",
                        "timestamp": "2026-08-20T12:00:00Z",
                        "properties": {"plan": "pro"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("capture response");
    assert_eq!(capture.status(), StatusCode::OK);

    let query_body = json!({
        "query": {
            "kind": "Trends",
            "series": [{
                "event": { "type": "name", "value": "signed_up" },
                "math": { "type": "total" }
            }],
            "filters": { "op": "AND", "values": [] },
            "range": {
                "from": "2026-08-20T00:00:00Z",
                "to": "2026-08-21T00:00:00Z"
            },
            "interval": "Day"
        },
        "refresh": true
    });
    let mut observed = None;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let response = router
            .clone()
            .oneshot(
                Request::post(format!("/api/projects/{project_id}/query"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(query_body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("query response");
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("query body")
                .to_bytes(),
        )
        .expect("query JSON");
        if body["results"][0]["data"][0]["count"] == 1 {
            observed = Some(body);
            break;
        }
    }
    let observed = observed.expect("publisher makes the durable receipt queryable");
    assert_eq!(observed["meta"]["generation_id"], 1);

    let catalog = router
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/projects/{project_id}/catalog/events?prefix=signed&limit=20"
            ))
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .expect("catalog response");
    assert_eq!(catalog.status(), StatusCode::OK);
    let names: Vec<String> = serde_json::from_slice(
        &catalog
            .into_body()
            .collect()
            .await
            .expect("catalog body")
            .to_bytes(),
    )
    .expect("catalog JSON");
    assert_eq!(names, vec!["signed_up"]);

    let values = router
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/projects/{project_id}/catalog/values?key=plan&limit=20"
            ))
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .expect("catalog values response");
    assert_eq!(values.status(), StatusCode::OK);
    let values: Vec<String> = serde_json::from_slice(
        &values
            .into_body()
            .collect()
            .await
            .expect("catalog values body")
            .to_bytes(),
    )
    .expect("catalog values JSON");
    assert_eq!(values, vec!["pro"]);

    drop(router);
    application.shutdown().await.expect("clean shutdown");
}

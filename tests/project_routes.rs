use std::{path::Path, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::{TimeZone, Utc};
use hoglet::{
    capture::event::CapturedEvent,
    control::{ProjectAccess, SetupRequest},
    event_lake::{FileScope, PublishedFile, VersionedEventLake},
    query::QueryEngine,
    routes::project,
    storage_bootstrap::bootstrap_storage,
    store::EventStore,
};
use http_body_util::BodyExt;
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn json_response(
    app: Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = app.oneshot(request).await.expect("router should respond");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should be readable")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).expect("response should be JSON");
    (status, headers, body)
}

fn query_request(uri: &str, credential: Option<(&str, &str)>, body: Value) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((name, value)) = credential {
        request = request.header(name, value);
    }
    request
        .body(Body::from(body.to_string()))
        .expect("request should be valid")
}

fn trends_query() -> Value {
    json!({
        "kind": "Trends",
        "series": [{
            "event": { "type": "name", "value": "pageview" },
            "math": { "type": "total" }
        }],
        "filters": { "op": "AND", "values": [] },
        "range": {
            "from": "2025-01-01T00:00:00Z",
            "to": "2027-01-01T00:00:00Z"
        },
        "interval": "Day"
    })
}

fn bootstrap_control(directory: &Path) -> std::path::PathBuf {
    let control = directory.join("control.db");
    bootstrap_storage(&control, directory.join("projections.db"))
        .expect("storage pair should bootstrap");
    control
}

fn seed_second_user(control_path: &Path) -> (String, String, String) {
    let user_id = Uuid::new_v4().to_string();
    let organization_id = Uuid::new_v4().to_string();
    let project_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let mut connection = Connection::open(control_path).expect("control database should open");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys should enable");
    let transaction = connection
        .transaction()
        .expect("fixture transaction should start");
    transaction
        .execute(
            "INSERT INTO users(id,email,password_hash,name,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                user_id,
                "second@example.com",
                "unused-in-router-test",
                "Second User",
                now
            ],
        )
        .expect("second user should be inserted");
    transaction
        .execute(
            "INSERT INTO organizations(id,name,created_at) VALUES (?1,?2,?3)",
            rusqlite::params![organization_id, "Second Organization", now],
        )
        .expect("second organization should be inserted");
    transaction
        .execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES (?1,?2,'owner')",
            rusqlite::params![organization_id, user_id],
        )
        .expect("second membership should be inserted");
    transaction
        .execute(
            "INSERT INTO projects(id,organization_id,name,capture_token,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![project_id, organization_id, "Second Project", "phc_second", now],
        )
        .expect("second project should be inserted");
    transaction
        .execute(
            "INSERT INTO auth_sessions(id,user_id,created_at,expires_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![session_id, user_id, now, now + 3600],
        )
        .expect("second session should be inserted");
    transaction
        .commit()
        .expect("fixture transaction should commit");
    (user_id, project_id, session_id)
}

#[tokio::test]
async fn authorized_query_derives_capture_token_for_session_and_personal_key() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let control_path = bootstrap_control(directory.path());
    let events_path = directory.path().join("events");
    let (access, runtime) = ProjectAccess::open(control_path).expect("project access should open");
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Website".into(),
            existing_project_token: Some("phc_authorized".into()),
        })
        .await
        .expect("setup should succeed");
    let project_id = setup.workspace.organizations[0].projects[0].id.clone();
    let principal = access
        .validate_session(&setup.session_id)
        .await
        .expect("setup session should authenticate");
    let personal_key = access
        .create_personal_key(&principal, "Test client")
        .await
        .expect("personal key should be created");

    let store = EventStore::open(events_path.clone()).expect("event store should open");
    let written = store
        .write_events(&[CapturedEvent {
            uuid: Uuid::new_v4(),
            event: "pageview".into(),
            distinct_id: "person-1".into(),
            token: "phc_authorized".into(),
            timestamp: Utc
                .with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
                .single()
                .expect("timestamp should be valid"),
            properties: serde_json::Map::new(),
        }])
        .expect("event should be written");
    let lake = Arc::new(
        VersionedEventLake::open(directory.path().join("projections.db"), &events_path)
            .expect("versioned event lake should open"),
    );
    lake.publish_generation(vec![PublishedFile::new(
        written[0].clone(),
        FileScope::ProjectDate {
            project_id: project_id.clone(),
            event_date: chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                .expect("test date should be valid"),
        },
    )])
    .expect("event generation should publish");
    let app = project::router(
        Arc::new(access.clone()),
        Arc::new(QueryEngine::try_new_versioned(lake).expect("versioned query engine should open")),
    );
    let uri = format!("/api/projects/{project_id}/query");
    let cookie = format!("hoglet_sid={}", setup.session_id);
    let bearer = format!("Bearer {}", personal_key.secret);

    for credential in [
        (header::COOKIE.as_str(), cookie.as_str()),
        (header::AUTHORIZATION.as_str(), bearer.as_str()),
    ] {
        let (status, headers, body) = json_response(
            app.clone(),
            query_request(&uri, Some(credential), json!({"query": trends_query()})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["meta"]["kind"], "trends");
        assert_eq!(body["meta"]["generation_id"], 1);
        assert_eq!(body["results"][0]["data"][0]["count"], 1);
        assert!(headers.contains_key("x-request-id"));
    }

    let (status, _, body) = json_response(
        app.clone(),
        query_request(
            &uri,
            Some((header::COOKIE.as_str(), &cookie)),
            json!({"query": trends_query(), "token": "phc_someone_else"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");

    drop(app);
    drop(store);
    drop(principal);
    drop(access);
    runtime.close();
}

#[tokio::test]
async fn cross_project_authorization_precedes_query_validation() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let control_path = bootstrap_control(directory.path());
    let (access, runtime) =
        ProjectAccess::open(control_path.clone()).expect("project access should open");
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Website".into(),
            existing_project_token: Some("phc_first".into()),
        })
        .await
        .expect("setup should succeed");
    let first_project_id = setup.workspace.organizations[0].projects[0].id.clone();
    let (_, _second_project_id, second_session_id) = seed_second_user(&control_path);
    let events_path = directory.path().join("events");
    EventStore::open(events_path.clone()).expect("empty event root should open");
    let lake = Arc::new(
        VersionedEventLake::open(directory.path().join("projections.db"), events_path)
            .expect("versioned event lake should open"),
    );
    let app = project::router(
        Arc::new(access.clone()),
        Arc::new(QueryEngine::try_new_versioned(lake).expect("versioned query engine should open")),
    );
    let unsupported = json!({
        "query": {
            "kind": "Funnels",
            "series": [{
                "event": { "type": "name", "value": "pageview" },
                "math": { "type": "total" }
            }],
            "filters": { "op": "AND", "values": [] },
            "range": {
                "from": "2025-01-01T00:00:00Z",
                "to": "2027-01-01T00:00:00Z"
            },
            "interval": "Day"
        }
    });
    let uri = format!("/api/projects/{first_project_id}/query");

    let second_cookie = format!("hoglet_sid={second_session_id}");
    let (status, headers, body) = json_response(
        app.clone(),
        query_request(
            &uri,
            Some((header::COOKIE.as_str(), &second_cookie)),
            unsupported.clone(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
    assert_eq!(
        body["error"]["request_id"],
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("response should expose its request ID")
    );

    let first_cookie = format!("hoglet_sid={}", setup.session_id);
    let (status, _, body) = json_response(
        app.clone(),
        query_request(
            &uri,
            Some((header::COOKIE.as_str(), &first_cookie)),
            unsupported,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "unsupported_query");
    assert_eq!(body["error"]["field"], "query.kind");

    let (status, _, body) = json_response(
        app.clone(),
        query_request(&uri, None, json!({"not": "valid query JSON"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");

    let (status, _, body) = json_response(
        app.clone(),
        query_request(
            "/api/projects/not-a-project/query",
            Some((header::COOKIE.as_str(), &first_cookie)),
            json!({"query": trends_query()}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    drop(app);
    drop(access);
    runtime.close();
}

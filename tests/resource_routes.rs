use std::{path::Path, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::Utc;
use hoglet::{
    control::{ProjectAccess, SetupRequest},
    control_resources::ControlResources,
    routes::resources,
    storage_bootstrap::bootstrap_storage,
};
use http_body_util::BodyExt;
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn response(
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response should be JSON")
    };
    (status, headers, body)
}

fn request(method: &str, uri: &str, credential: Option<(&str, &str)>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((name, value)) = credential {
        request = request.header(name, value);
    }
    request
        .body(Body::from(body.to_owned()))
        .expect("request should be valid")
}

fn seed_unrelated_owner(control_path: &Path) -> (String, String) {
    let user_id = Uuid::now_v7().to_string();
    let organization_id = Uuid::now_v7().to_string();
    let project_id = Uuid::now_v7().to_string();
    let session_id = Uuid::now_v7().to_string();
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
                "unrelated@example.com",
                "unused-in-route-test",
                "Unrelated Owner",
                now
            ],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO organizations(id,name,created_at) VALUES (?1,?2,?3)",
            rusqlite::params![organization_id, "Unrelated", now],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO organization_members(organization_id,user_id,role) VALUES (?1,?2,'owner')",
            rusqlite::params![organization_id, user_id],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO projects(id,organization_id,name,capture_token,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![project_id, organization_id, "Other", "phc_other", now],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO auth_sessions(id,user_id,created_at,expires_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![session_id, user_id, now, now + 3600],
        )
        .unwrap();
    transaction.commit().unwrap();
    (project_id, session_id)
}

fn trends_query() -> Value {
    json!({
        "kind": "Trends",
        "series": [{
            "event": {"type": "name", "value": "$pageview"},
            "math": {"type": "total"}
        }],
        "filters": {"op": "AND", "values": []},
        "range": {
            "from": "2026-08-01T00:00:00Z",
            "to": "2026-08-08T00:00:00Z"
        },
        "interval": "Day"
    })
}

#[tokio::test]
async fn project_resources_are_scoped_and_mutations_require_privileged_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let control_path = directory.path().join("control.db");
    bootstrap_storage(&control_path, directory.path().join("projections.db")).unwrap();
    let (access, runtime) = ProjectAccess::open(control_path.clone()).unwrap();
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Website".into(),
            existing_project_token: Some("phc_website".into()),
        })
        .await
        .unwrap();
    let project_id = setup.workspace.organizations[0].projects[0].id.clone();
    let principal = access.validate_session(&setup.session_id).await.unwrap();
    let personal_key = access
        .create_personal_key(&principal, "Read-only route test")
        .await
        .unwrap();
    let (other_project_id, other_session_id) = seed_unrelated_owner(&control_path);
    let app = resources::router(
        Arc::new(access.clone()),
        Arc::new(ControlResources::open(&control_path).unwrap()),
    );
    let owner_cookie = format!("hoglet_sid={}", setup.session_id);
    let other_cookie = format!("hoglet_sid={other_session_id}");
    let personal_bearer = format!("Bearer {}", personal_key.secret);

    let flag = json!({
        "key": "checkout",
        "active": true,
        "rollout_percentage": 100.0,
        "variants": [],
        "payload": null
    });
    let flags_uri = format!("/api/projects/{project_id}/flags");
    let (status, _, _) = response(
        app.clone(),
        request(
            "POST",
            &flags_uri,
            Some((header::COOKIE.as_str(), &owner_cookie)),
            &flag.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, flags) = response(
        app.clone(),
        request(
            "GET",
            &flags_uri,
            Some((header::AUTHORIZATION.as_str(), &personal_bearer)),
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(flags[0]["key"], "checkout");

    let (status, _, error) = response(
        app.clone(),
        request(
            "POST",
            &flags_uri,
            Some((header::AUTHORIZATION.as_str(), &personal_bearer)),
            &flag.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "forbidden");

    let insights_uri = format!("/api/projects/{project_id}/insights");
    let insight_draft = json!({
        "name": "Traffic",
        "description": "Page views",
        "query_ir": trends_query()
    });
    let (status, _, insight) = response(
        app.clone(),
        request(
            "POST",
            &insights_uri,
            Some((header::COOKIE.as_str(), &owner_cookie)),
            &insight_draft.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let insight_id = insight["id"].as_str().unwrap();

    let other_insight_uri = format!("/api/projects/{other_project_id}/insights/{insight_id}");
    let (status, _, error) = response(
        app.clone(),
        request(
            "PUT",
            &other_insight_uri,
            Some((header::COOKIE.as_str(), &other_cookie)),
            &insight_draft.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error"]["code"], "not_found");

    let dashboards_uri = format!("/api/projects/{project_id}/dashboards");
    let (status, _, dashboard) = response(
        app.clone(),
        request(
            "POST",
            &dashboards_uri,
            Some((header::COOKIE.as_str(), &owner_cookie)),
            r#"{"name":"Overview"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let dashboard_id = dashboard["id"].as_str().unwrap();

    let tiles_uri = format!("/api/projects/{project_id}/dashboards/{dashboard_id}/tiles");
    let tiles = json!([{"insight_id": insight_id, "x": 0, "y": 0, "w": 6, "h": 4}]);
    let (status, _, dashboard) = response(
        app.clone(),
        request(
            "PUT",
            &tiles_uri,
            Some((header::COOKIE.as_str(), &owner_cookie)),
            &tiles.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dashboard["tiles"][0]["insight_id"], insight_id);

    let shares_uri = format!("/api/projects/{project_id}/shares");
    let share_draft = json!({"object_type": "dashboard", "object_id": dashboard_id});
    let (status, _, share) = response(
        app.clone(),
        request(
            "POST",
            &shares_uri,
            Some((header::COOKIE.as_str(), &owner_cookie)),
            &share_draft.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let share_token = share["token"].as_str().unwrap();
    let (status, _, public) = response(
        app.clone(),
        request("GET", &format!("/shared/{share_token}"), None, ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(public["dashboard"]["id"], dashboard_id);

    drop(app);
    drop(principal);
    drop(access);
    runtime.close();
}

#[tokio::test]
async fn authentication_and_project_authorization_precede_body_parsing() {
    let directory = tempfile::tempdir().unwrap();
    let control_path = directory.path().join("control.db");
    bootstrap_storage(&control_path, directory.path().join("projections.db")).unwrap();
    let (access, runtime) = ProjectAccess::open(control_path.clone()).unwrap();
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Website".into(),
            existing_project_token: None,
        })
        .await
        .unwrap();
    let project_id = setup.workspace.organizations[0].projects[0].id.clone();
    let (_, other_session_id) = seed_unrelated_owner(&control_path);
    let app = resources::router(
        Arc::new(access.clone()),
        Arc::new(ControlResources::open(&control_path).unwrap()),
    );
    let uri = format!("/api/projects/{project_id}/insights");

    let (status, headers, error) = response(app.clone(), request("POST", &uri, None, "{")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"]["code"], "unauthorized");
    assert_eq!(
        error["error"]["request_id"].as_str(),
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
    );

    let other_cookie = format!("hoglet_sid={other_session_id}");
    let (status, _, error) = response(
        app.clone(),
        request(
            "POST",
            &uri,
            Some((header::COOKIE.as_str(), &other_cookie)),
            "{",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "forbidden");

    let owner_cookie = format!("hoglet_sid={}", setup.session_id);
    let (status, _, error) = response(
        app.clone(),
        request(
            "POST",
            &uri,
            Some((header::COOKIE.as_str(), &owner_cookie)),
            "{",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["code"], "invalid_request");

    drop(app);
    drop(access);
    runtime.close();
}

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use hoglet::control::{ProjectAccess, ProjectAccessRuntime};
use hoglet::storage_bootstrap::bootstrap_storage;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub mod control {
    pub use hoglet::control::*;
}

#[path = "../src/routes/workspace.rs"]
mod workspace;

fn open_access(directory: &std::path::Path) -> (ProjectAccess, ProjectAccessRuntime) {
    let control = directory.join("control.db");
    bootstrap_storage(&control, directory.join("projections.db"))
        .expect("storage pair should bootstrap");
    ProjectAccess::open(control).expect("project access should open")
}

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

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should be valid")
}

#[tokio::test]
async fn first_run_bootstrap_and_setup_return_an_authenticated_workspace() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let (access, runtime) = open_access(directory.path());
    let app = workspace::router(std::sync::Arc::new(access.clone()));

    let (status, headers, body) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/auth/bootstrap")
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"setup_required": true}));
    assert!(headers.contains_key("x-request-id"));

    let (status, headers, body) = json_response(
        app.clone(),
        json_request(
            "POST",
            "/api/auth/setup",
            json!({
                "email": "owner@example.com",
                "password": "correct horse battery staple",
                "organization_name": "Acme",
                "project_name": "Website",
                "existing_project_token": "phc_existing"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["email"], "owner@example.com");
    assert_eq!(body["organizations"][0]["name"], "Acme");
    assert_eq!(body["organizations"][0]["projects"][0]["name"], "Website");
    assert_eq!(
        body["organizations"][0]["projects"][0]["token"],
        "phc_existing"
    );
    let cookie = headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("setup should establish a session");
    assert!(cookie.starts_with("hoglet_sid="));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));

    let (status, _, body) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/auth/bootstrap")
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"setup_required": false}));

    drop(app);
    drop(access);
    runtime.close();
}

#[tokio::test]
async fn session_can_create_workspace_resources_while_anonymous_requests_are_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let (access, runtime) = open_access(directory.path());
    let app = workspace::router(std::sync::Arc::new(access.clone()));
    let (_, setup_headers, _) = json_response(
        app.clone(),
        json_request(
            "POST",
            "/api/auth/setup",
            json!({
                "email": "owner@example.com",
                "password": "correct horse battery staple",
                "organization_name": "Acme"
            }),
        ),
    )
    .await;
    let cookie = setup_headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .expect("setup should establish a session")
        .to_owned();

    let (status, _, organization) = json_response(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/organizations")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, &cookie)
            .body(Body::from(json!({"name": "Labs"}).to_string()))
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(organization["name"], "Labs");
    let organization_id = organization["id"]
        .as_str()
        .expect("organization should have an id");

    let (status, _, project) = json_response(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!("/api/organizations/{organization_id}/projects"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, &cookie)
            .body(Body::from(json!({"name": "Mobile"}).to_string()))
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(project["name"], "Mobile");
    assert!(
        project["token"]
            .as_str()
            .expect("project should have a token")
            .starts_with("phc_")
    );

    let (status, _, organizations) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/organizations")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(organizations.as_array().map(Vec::len), Some(2));

    let (status, headers, error) = json_response(
        app.clone(),
        json_request("POST", "/api/organizations", json!({"name": "Intruder"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"]["code"], "unauthorized");
    assert_eq!(
        error["error"]["request_id"].as_str(),
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
    );

    drop(app);
    drop(access);
    runtime.close();
}

#[tokio::test]
async fn sessions_and_personal_keys_share_one_workspace_principal_lifecycle() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let (access, runtime) = open_access(directory.path());
    let app = workspace::router(std::sync::Arc::new(access.clone()));
    let (_, setup_headers, _) = json_response(
        app.clone(),
        json_request(
            "POST",
            "/api/auth/setup",
            json!({
                "email": "owner@example.com",
                "password": "correct horse battery staple",
                "organization_name": "Acme"
            }),
        ),
    )
    .await;
    let setup_cookie = setup_headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .expect("setup should establish a session")
        .to_owned();

    let (status, _, created) = json_response(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/auth/keys")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, &setup_cookie)
            .body(Body::from(json!({"name": "CLI"}).to_string()))
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["key"]["name"], "CLI");
    let key_id = created["key"]["id"]
        .as_str()
        .expect("personal key should have an id")
        .to_owned();
    let secret = created["secret"]
        .as_str()
        .expect("secret should be returned once")
        .to_owned();
    assert!(secret.starts_with("phx_"));

    let (status, _, workspace) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/auth/me")
            .header(header::AUTHORIZATION, format!("Bearer {secret}"))
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(workspace["user"]["email"], "owner@example.com");

    let (status, _, error) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/auth/keys")
            .header(header::AUTHORIZATION, format!("Bearer {secret}"))
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "forbidden");

    let forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/auth/keys/{key_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .expect("request should be valid"),
        )
        .await
        .expect("router should respond");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/auth/keys/{key_id}"))
                .header(header::COOKIE, &setup_cookie)
                .body(Body::empty())
                .expect("request should be valid"),
        )
        .await
        .expect("router should respond");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key("x-request-id"));

    let (status, _, error) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/auth/me")
            .header(header::AUTHORIZATION, format!("Bearer {secret}"))
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"]["code"], "unauthorized");

    let (status, login_headers, workspace) = json_response(
        app.clone(),
        json_request(
            "POST",
            "/api/auth/login",
            json!({
                "email": "OWNER@example.com",
                "password": "correct horse battery staple"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(workspace["organizations"][0]["name"], "Acme");
    let login_cookie = login_headers
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .expect("login should establish a session")
        .to_owned();

    let (status, logout_headers, body) = json_response(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .header(header::COOKIE, &login_cookie)
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"status": "ok"}));
    assert!(
        logout_headers
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|cookie| cookie.contains("Max-Age=0"))
    );

    let (status, _, error) = json_response(
        app.clone(),
        Request::builder()
            .uri("/api/auth/me")
            .header(header::COOKIE, &login_cookie)
            .body(Body::empty())
            .expect("request should be valid"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"]["code"], "unauthorized");

    drop(app);
    drop(access);
    runtime.close();
}

//! Minimal admin API (SPEC.md "Security and tenancy" — project/flag
//! management). Creates projects and feature flags so the registry and flag
//! evaluator are reachable, not just internal.
//!
//! Guarded by a bearer token from `HOGLET_ADMIN_TOKEN`. **Disabled by
//! default**: with no admin token set, every admin route returns 404, so a
//! hobby install exposes no management surface it didn't ask for. Authority is
//! a separate contract from capability (CLAUDE.md engineering standard).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;

use crate::flags::FlagStore;
use crate::registry::Registry;

#[derive(Clone)]
pub struct AdminState {
    pub registry: Arc<Registry>,
    pub flags: Arc<FlagStore>,
    /// None ⇒ admin API disabled.
    pub admin_token: Option<Arc<String>>,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/api/admin/projects", post(create_project))
        .route("/api/admin/flags", post(upsert_flag))
        .with_state(state)
}

/// Returns Some(response) when the request must be rejected, None when
/// authorized. 404 (not 401) when disabled, so a disabled install looks like
/// it has no such endpoint at all.
fn authorize(state: &AdminState, headers: &HeaderMap) -> Option<Response> {
    let Some(expected) = state.admin_token.as_ref() else {
        return Some(StatusCode::NOT_FOUND.into_response());
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if presented == Some(expected.as_str()) {
        None
    } else {
        Some(StatusCode::UNAUTHORIZED.into_response())
    }
}

#[derive(Deserialize)]
struct CreateProject {
    token: String,
    name: String,
}

async fn create_project(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<CreateProject>,
) -> Response {
    if let Some(rejection) = authorize(&state, &headers) {
        return rejection;
    }
    if crate::token::validate(&body.token).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // No clock in the registry; stamp here.
    let now = chrono::Utc::now().to_rfc3339();
    match state.registry.create_project(&body.token, &body.name, &now) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct UpsertFlag {
    token: String,
    key: String,
    #[serde(default = "yes")]
    active: bool,
    #[serde(default = "full")]
    rollout_percentage: f64,
    #[serde(default)]
    variants: Vec<crate::flags::Variant>,
    #[serde(default)]
    conditions: Option<crate::flags::Conditions>,
}
fn yes() -> bool {
    true
}
fn full() -> f64 {
    100.0
}

async fn upsert_flag(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<UpsertFlag>,
) -> Response {
    if let Some(rejection) = authorize(&state, &headers) {
        return rejection;
    }
    let rollout = body.rollout_percentage.clamp(0.0, 100.0);
    match state.flags.upsert_full(
        &body.token,
        &body.key,
        body.active,
        rollout,
        &body.variants,
        body.conditions.as_ref(),
    ) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state(admin_token: Option<&str>) -> AdminState {
        AdminState {
            registry: Arc::new(Registry::in_memory().unwrap()),
            flags: Arc::new(FlagStore::in_memory().unwrap()),
            admin_token: admin_token.map(|s| Arc::new(s.to_string())),
        }
    }

    async fn post(app: Router, uri: &str, auth: Option<&str>, body: &str) -> StatusCode {
        let mut req = Request::post(uri).header("content-type", "application/json");
        if let Some(a) = auth {
            req = req.header("authorization", format!("Bearer {a}"));
        }
        app.oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn disabled_by_default_returns_404() {
        let s = state(None);
        let status = post(
            router(s),
            "/api/admin/flags",
            Some("whatever"),
            r#"{"token":"phc_t","key":"f"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_token_rejected() {
        let s = state(Some("secret"));
        let status = post(
            router(s),
            "/api/admin/flags",
            Some("wrong"),
            r#"{"token":"phc_t","key":"f"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authorized_flag_upsert_and_eval() {
        let s = state(Some("secret"));
        let flags = s.flags.clone();
        let status = post(
            router(s),
            "/api/admin/flags",
            Some("secret"),
            r#"{"token":"phc_t","key":"new-ui","active":true,"rollout_percentage":100}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The flag is now evaluable.
        assert!(flags.evaluate("phc_t", "u1", &serde_json::Map::new())[0].enabled);
    }

    #[tokio::test]
    async fn authorized_project_creation_flips_registry() {
        let s = state(Some("secret"));
        let registry = s.registry.clone();
        let status = post(
            router(s),
            "/api/admin/projects",
            Some("secret"),
            r#"{"token":"phc_acme","name":"Acme"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(registry.project_count(), 1);
    }
}

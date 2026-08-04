//! Auth middleware — gates API routes behind session cookie or personal API key.
//! Capture edge stays token-authed and is never gated.
//! If no auth store is configured, all requests pass through.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::auth::AuthStore;

const COOKIE_NAME: &str = "hoglet_sid";

#[derive(Clone)]
pub struct AuthLayerState {
    pub store: Option<Arc<AuthStore>>,
}

fn extract_session(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix(&format!("{COOKIE_NAME}=")).map(|v| v.to_string())
            })
        })
}

pub async fn require_auth(
    State(state): State<AuthLayerState>,
    headers: axum::http::HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let store = match &state.store {
        Some(s) => s,
        None => return next.run(request).await,
    };

    let path = request.uri().path();

    // Never gate capture endpoints.
    let capture_prefixes = ["/e", "/batch", "/capture", "/track", "/engage", "/i/v0/e"];
    if capture_prefixes.iter().any(|p| path == *p || path.starts_with(&format!("{p}/"))) {
        return next.run(request).await;
    }

    // Never gate login, setup, logout.
    if path == "/api/auth/setup" || path == "/api/auth/login" || path == "/api/auth/logout" {
        return next.run(request).await;
    }

    // Never gate shared pages — share token is the auth.
    if path.starts_with("/shared/") {
        return next.run(request).await;
    }

    // Only gate /api/*. The dashboard shell itself is public: it holds no data,
    // and the SPA renders its own setup/login screen off a 401 from /api/auth/me.
    // Redirecting the page here would strand the browser — there is no static
    // login page to redirect to, the login form lives inside the bundle.
    if !path.starts_with("/api/") {
        return next.run(request).await;
    }

    // Session cookie.
    if let Some(sid) = extract_session(&headers) {
        if store.validate_session(&sid).is_ok() {
            return next.run(request).await;
        }
    }

    // Personal API key.
    if let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        if let Ok(user) = store.validate_api_key(key) {
            let mut request = request;
            request.extensions_mut().insert(user);
            return next.run(request).await;
        }
    }

    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::get};
    use tower::ServiceExt;

    /// A router shaped like the real one: the dashboard shell, one gated API
    /// route, and the capture edge, all behind the middleware.
    fn app(store: Arc<AuthStore>) -> Router {
        Router::new()
            .route("/", get(|| async { "dashboard shell" }))
            .route("/dashboard", get(|| async { "dashboard shell" }))
            .route("/api/stats", get(|| async { "stats" }))
            .route("/api/auth/me", get(|| async { "me" }))
            .route("/e/", get(|| async { "captured" }))
            .layer(axum::middleware::from_fn_with_state(
                AuthLayerState { store: Some(store) },
                require_auth,
            ))
    }

    async fn status(store: Arc<AuthStore>, uri: &str) -> StatusCode {
        app(store)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    fn empty_store() -> Arc<AuthStore> {
        Arc::new(AuthStore::open_in_memory().expect("in-memory auth store"))
    }

    fn store_with_user() -> Arc<AuthStore> {
        let store = empty_store();
        store.setup("a@b.c", "correct horse battery", "Org").expect("setup");
        store
    }

    /// The login and setup forms live inside the JS bundle, so the shell must
    /// load for a visitor with no session — there is no static page to redirect
    /// to, and a redirect here strands every first-time browser on a 404.
    #[tokio::test]
    async fn dashboard_shell_is_public() {
        for store in [empty_store(), store_with_user()] {
            assert_eq!(status(store.clone(), "/").await, StatusCode::OK);
            assert_eq!(status(store, "/dashboard").await, StatusCode::OK);
        }
    }

    /// The shell being public buys nothing if the data behind it leaks: the
    /// SPA decides between its setup and login screens off exactly this 401.
    #[tokio::test]
    async fn api_is_gated_without_a_session() {
        let store = store_with_user();
        assert_eq!(status(store.clone(), "/api/stats").await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(store, "/api/auth/me").await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_cookie_opens_the_api() {
        let store = store_with_user();
        let (_, sid) = store.login("a@b.c", "correct horse battery").expect("login");
        let code = app(store)
            .oneshot(
                Request::get("/api/stats")
                    .header(header::COOKIE, format!("{COOKIE_NAME}={sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(code, StatusCode::OK);
    }

    /// Ingest must never depend on dashboard auth — SDKs carry a project token,
    /// not a session.
    #[tokio::test]
    async fn capture_edge_is_never_gated() {
        assert_eq!(status(store_with_user(), "/e/").await, StatusCode::OK);
    }
}


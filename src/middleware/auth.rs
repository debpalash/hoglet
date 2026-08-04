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

    // Only gate /api/* and /dashboard.
    if !path.starts_with("/api/") && path != "/dashboard" && !path.starts_with("/dashboard") && path != "/" {
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

    // For page requests, redirect to setup or login.
    if path == "/" || path.starts_with("/dashboard") {
        if store.is_empty().unwrap_or(false) {
            return axum::http::Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, "/setup.html")
                .body(axum::body::Body::empty())
                .unwrap();
        }
        return axum::http::Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/login.html")
            .body(axum::body::Body::empty())
            .unwrap();
    }

    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}


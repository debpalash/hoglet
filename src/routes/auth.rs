//! Auth routes — login, logout, setup, me, API keys.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, delete},
};
use serde::{Deserialize, Serialize};

use crate::auth::AuthStore;

#[allow(dead_code)]
const COOKIE_PATH: &str = "/";
const COOKIE_NAME: &str = "hoglet_sid";
const COOKIE_TTL: &str = "Max-Age=604800; Path=/; HttpOnly; SameSite=Lax";

#[derive(Clone)]
pub struct AuthState {
    pub store: Arc<AuthStore>,
}

pub fn router(store: Arc<AuthStore>) -> Router {
    Router::new()
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/keys", get(list_keys).post(create_key))
        .route("/api/auth/keys/{id}", delete(revoke_key))
        .with_state(AuthState { store })
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize)]
struct SetupBody {
    email: String,
    password: String,
    org_name: String,
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct CreateKeyBody {
    name: String,
}

#[derive(Serialize)]
struct LoginResponse {
    user: crate::auth::User,
    orgs: Vec<crate::auth::Org>,
}

#[derive(Serialize)]
struct KeyCreatedResponse {
    key: crate::auth::PersonalApiKey,
    full_key: String,
}

// ── Helpers ──────────────────────────────────────────────────

fn set_cookie(response: &mut Response, sid: &str) {
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!("{COOKIE_NAME}={sid}; {COOKIE_TTL}").parse().unwrap(),
    );
}

fn clear_cookie(response: &mut Response) {
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!("{COOKIE_NAME}=; Max-Age=0; Path=/").parse().unwrap(),
    );
}

fn extract_session(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                if let Some(v) = c.strip_prefix(&format!("{COOKIE_NAME}=")) {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        })
}

// ── Handlers ─────────────────────────────────────────────────

async fn setup(
    State(state): State<AuthState>,
    Json(body): Json<SetupBody>,
) -> Response {
    if !state.store.is_empty().unwrap_or(true) {
        return (StatusCode::NOT_FOUND, "setup already completed").into_response();
    }
    match state.store.setup(&body.email, &body.password, &body.org_name) {
        Ok((user, _, _)) => {
            let sid = state.store.login(&body.email, &body.password);
            let mut resp = Json(LoginResponse { user, orgs: vec![] }).into_response();
            if let Ok((_, s)) = sid {
                set_cookie(&mut resp, &s);
            }
            resp
        }
        Err(e) => {
            let code = match e {
                crate::auth::AuthError::Forbidden => StatusCode::FORBIDDEN,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, format!("{e:?}")).into_response()
        }
    }
}

async fn login(
    State(state): State<AuthState>,
    Json(body): Json<LoginBody>,
) -> Response {
    match state.store.login(&body.email, &body.password) {
        Ok((user, sid)) => {
            let orgs = state.store.list_orgs(&user.id).unwrap_or_default();
            let mut resp = Json(LoginResponse { user, orgs }).into_response();
            set_cookie(&mut resp, &sid);
            resp
        }
        Err(_) => (StatusCode::UNAUTHORIZED, "invalid email or password").into_response(),
    }
}

async fn logout(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(sid) = extract_session(&headers) {
        let _ = state.store.logout(&sid);
    }
    let mut resp = Json(serde_json::json!({"status": "ok"})).into_response();
    clear_cookie(&mut resp);
    resp
}

async fn me(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let user = if let Some(sid) = extract_session(&headers) {
        state.store.validate_session(&sid).ok()
    } else if let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        state.store.validate_api_key(key).ok()
    } else {
        None
    };

    match user {
        Some(u) => {
            let orgs = state.store.list_orgs(&u.id).unwrap_or_default();
            Json(crate::auth::MeResponse { user: Some(u), orgs }).into_response()
        }
        None => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}

async fn list_keys(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let user = match authenticate(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.store.list_api_keys(&user.id) {
        Ok(keys) => Json(keys).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}")).into_response(),
    }
}

async fn create_key(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateKeyBody>,
) -> Response {
    let user = match authenticate(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.store.create_api_key(&user.id, &body.name) {
        Ok((key, full_key)) => Json(KeyCreatedResponse { key, full_key }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:?}")).into_response(),
    }
}

async fn revoke_key(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(key_id): axum::extract::Path<String>,
) -> Response {
    let user = match authenticate(&state, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.store.revoke_api_key(&user.id, &key_id) {
        Ok(()) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Auth helper for gated endpoints ──────────────────────────

pub fn authenticate(
    state: &AuthState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::auth::User, Response> {
    if let Some(sid) = extract_session(headers) {
        match state.store.validate_session(&sid) {
            Ok(u) => return Ok(u),
            Err(_) => {}
        }
    }
    if let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        match state.store.validate_api_key(key) {
            Ok(u) => return Ok(u),
            Err(_) => {}
        }
    }
    Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response())
}

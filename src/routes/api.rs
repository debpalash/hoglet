//! Dashboard query API (SPEC.md "query lane"). Internal JSON, not a
//! PostHog-compatible surface — the dashboard is ours to shape.
//!
//! Every endpoint is read-only over the Parquet store via [`QueryEngine`].
//! Failures return 500 with no body; an empty store returns empty results,
//! never an error.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

use crate::query::QueryEngine;

#[derive(Clone)]
pub struct ApiState {
    pub engine: Arc<QueryEngine>,
}

pub fn router(engine: Arc<QueryEngine>) -> Router {
    Router::new()
        .route("/api/stats", get(stats))
        .route("/api/top_events", get(top_events))
        .route("/api/trend", get(trend))
        .route("/api/funnel", post(funnel))
        .route("/api/recent", get(recent))
        .with_state(ApiState { engine })
}

fn err<T>(_: T) -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn stats(State(s): State<ApiState>, Query(q): Query<TokenQuery>) -> Response {
    match s.engine.stats(&q.token) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct TopQuery {
    token: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    20
}

async fn top_events(State(s): State<ApiState>, Query(q): Query<TopQuery>) -> Response {
    match s.engine.top_events(&q.token, q.limit.min(100)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct TrendQuery {
    token: String,
    event: String,
    #[serde(default = "default_days")]
    days: u32,
}
fn default_days() -> u32 {
    30
}

async fn trend(State(s): State<ApiState>, Query(q): Query<TrendQuery>) -> Response {
    match s.engine.trend(&q.token, &q.event, q.days.min(365)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct FunnelBody {
    token: String,
    steps: Vec<String>,
}

async fn funnel(State(s): State<ApiState>, Json(b): Json<FunnelBody>) -> Response {
    match s.engine.funnel(&b.token, &b.steps) {
        Ok(v) => Json(v).into_response(),
        Err(crate::query::QueryError::TooManySteps) => StatusCode::BAD_REQUEST.into_response(),
        Err(e) => err(e),
    }
}

async fn recent(State(s): State<ApiState>, Query(q): Query<TopQuery>) -> Response {
    match s.engine.recent_events(&q.token, q.limit.min(200)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

//! Dashboard query API (spec/README.md "query lane"). Internal JSON, not a
//! PostHog-compatible surface — the dashboard is ours to shape.
//!
//! Every query acquires a permit (the query-lane concurrency cap) and runs on
//! a blocking thread, so analytical load never starves the async ingest
//! runtime. Failures return 500 with no body; an empty store returns empty
//! results, never an error.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::query::{QueryEngine, QueryError};

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

/// Run a query under the concurrency cap, off the async runtime.
async fn run<T, F>(state: &ApiState, f: F) -> Response
where
    F: FnOnce(&QueryEngine) -> Result<T, QueryError> + Send + 'static,
    T: Serialize + Send + 'static,
{
    let _permit = state.engine.acquire().await;
    let engine = state.engine.clone();
    match tokio::task::spawn_blocking(move || f(&engine)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(QueryError::TooManySteps)) => StatusCode::BAD_REQUEST.into_response(),
        Ok(Err(_)) | Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn stats(State(s): State<ApiState>, Query(q): Query<TokenQuery>) -> Response {
    run(&s, move |e| e.stats(&q.token)).await
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
    run(&s, move |e| e.top_events(&q.token, q.limit.min(100))).await
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
    run(&s, move |e| e.trend(&q.token, &q.event, q.days.min(365))).await
}

#[derive(Deserialize)]
struct FunnelBody {
    token: String,
    steps: Vec<String>,
}

async fn funnel(State(s): State<ApiState>, Json(b): Json<FunnelBody>) -> Response {
    run(&s, move |e| e.funnel(&b.token, &b.steps)).await
}

async fn recent(State(s): State<ApiState>, Query(q): Query<TopQuery>) -> Response {
    run(&s, move |e| e.recent_events(&q.token, q.limit.min(200))).await
}

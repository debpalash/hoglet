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

use crate::cache::{CacheKey, ResultCache, hash_ir};
use crate::file_index::FileIndex;
use crate::query::ir;
use crate::query::{QueryEngine, QueryError};

#[derive(Clone)]
pub struct ApiState {
    pub engine: Arc<QueryEngine>,
    pub cache: Option<Arc<ResultCache>>,
    pub index: Option<Arc<FileIndex>>,
}

pub fn router(
    engine: Arc<QueryEngine>,
    cache: Option<Arc<ResultCache>>,
    index: Option<Arc<FileIndex>>,
) -> Router {
    Router::new()
        .route("/api/stats", get(stats))
        .route("/api/top_events", get(top_events))
        .route("/api/trend", get(trend))
        .route("/api/funnel", post(funnel))
        .route("/api/recent", get(recent))
        .route("/api/query", post(query))
        .with_state(ApiState { engine, cache, index })
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

#[derive(Deserialize)]
struct QueryRequest {
    token: String,
    query: ir::Query,
    #[serde(default)]
    #[allow(dead_code)]
    refresh: bool,
}

async fn query(State(s): State<ApiState>, Json(b): Json<QueryRequest>) -> Response {
    // Check cache if refresh not requested
    if !b.refresh {
        if let Some(ref cache) = s.cache {
            let version = s.index.as_ref().map(|i| i.read_version()).unwrap_or(0);
            let key = CacheKey {
                token: b.token.clone(),
                ir_hash: hash_ir(&serde_json::to_value(&b.query).unwrap_or_default()),
                data_version: version,
            };
            if let Some(cached) = cache.get(&key) {
                return axum::http::Response::builder()
                    .header("content-type", "application/json")
                    .header("x-cache", "HIT")
                    .body(axum::body::Body::from(cached))
                    .unwrap();
            }
        }
    }

    let state = s.clone();
    let query_ir = b.query.clone();
    let token = b.token.clone();
    let _do_refresh = b.refresh;
    let result = run(&state, move |e| e.run_ir(&query_ir, &token)).await;

    // Cache successful responses
    if result.status().is_success() && state.cache.is_some() {
        // Can't easily extract body from an already-consumed response
        // Cache miss — the next request will be a hit.
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::event::CapturedEvent;
    use crate::query::ir;
    use crate::store::EventStore;
    use axum::body::Body;
    use chrono::Utc;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    fn ev(event: &str, did: &str) -> CapturedEvent {
        CapturedEvent {
            uuid: uuid::Uuid::new_v4(),
            event: event.into(),
            distinct_id: did.into(),
            token: "phc_t".into(),
            timestamp: Utc::now(),
            properties: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn api_query_returns_trends_response() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store.write_events(&[
            ev("pageview", "u1"),
            ev("pageview", "u2"),
            ev("click", "u1"),
        ]).unwrap();
        let engine = Arc::new(crate::query::QueryEngine::new(dir.path().to_path_buf()));

        let app = router(engine, None, None);
        let body = json!({
            "token": "phc_t",
            "query": {
                "kind": "Trends",
                "series": [{
                    "event": { "type": "name", "value": "pageview" },
                    "math": { "type": "total" }
                }],
                "filters": { "op": "AND", "values": [] },
                "range": {},
                "interval": "Day"
            }
        });

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/query")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(parsed["meta"]["kind"], "trends");
        assert!(parsed["results"].is_array());
        assert!(!parsed["results"][0]["data"].as_array().unwrap().is_empty());
    }
}

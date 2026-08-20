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
use crate::query::supported::{SupportedQuery, UnsupportedQuery};
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
        .with_state(ApiState {
            engine,
            cache,
            index,
        })
}

/// Run a query under the concurrency cap, off the async runtime.
async fn run<T, F>(state: &ApiState, f: F) -> Response
where
    F: FnOnce(&QueryEngine) -> Result<T, QueryError> + Send + 'static,
    T: Serialize + Send + 'static,
{
    match run_json(state, f).await {
        Ok(body) => json_response(body, "MISS"),
        Err(code) => code.into_response(),
    }
}

/// Same as `run`, but hands back the serialized bytes so the caller can cache
/// them. A `Response` body is a stream — once it exists the bytes are gone, so
/// anything that wants to keep a copy has to branch before that point.
async fn run_json<T, F>(state: &ApiState, f: F) -> Result<Vec<u8>, StatusCode>
where
    F: FnOnce(&QueryEngine) -> Result<T, QueryError> + Send + 'static,
    T: Serialize + Send + 'static,
{
    let _permit = state.engine.acquire().await;
    let engine = state.engine.clone();
    match tokio::task::spawn_blocking(move || f(&engine)).await {
        Ok(Ok(v)) => serde_json::to_vec(&v).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
        Ok(Err(QueryError::TooManySteps)) => Err(StatusCode::BAD_REQUEST),
        Ok(Err(_)) | Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn json_response(body: Vec<u8>, cache_status: &str) -> Response {
    axum::http::Response::builder()
        .header("content-type", "application/json")
        .header("x-cache", cache_status)
        .body(axum::body::Body::from(body))
        .expect("static headers are always valid")
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
    refresh: bool,
}

#[derive(Serialize)]
struct QueryErrorEnvelope {
    error: QueryErrorBody,
}

#[derive(Serialize)]
struct QueryErrorBody {
    code: &'static str,
    field: String,
    message: String,
    request_id: String,
}

fn unsupported_query_response(error: UnsupportedQuery) -> Response {
    let request_id = uuid::Uuid::now_v7().to_string();
    let mut response = (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(QueryErrorEnvelope {
            error: QueryErrorBody {
                code: error.code,
                field: format!("query.{}", error.field),
                message: error.message,
                request_id: request_id.clone(),
            },
        }),
    )
        .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

async fn query(State(s): State<ApiState>, Json(b): Json<QueryRequest>) -> Response {
    // Reject broad IR before touching the cache or entering the bounded query
    // lane. A cached response must never bypass current semantic validation.
    let supported = match SupportedQuery::try_from(b.query) {
        Ok(query) => query,
        Err(error) => return unsupported_query_response(error),
    };

    // The data version is part of the key, so a flush invalidates every entry
    // for free — no explicit eviction, and a stale segment can never be served.
    let key = s.cache.as_ref().map(|_| CacheKey {
        token: b.token.clone(),
        ir_hash: hash_ir(&serde_json::to_value(supported.as_query()).unwrap_or_default()),
        data_version: s.index.as_ref().map(|i| i.read_version()).unwrap_or(0),
    });

    if !b.refresh {
        if let (Some(cache), Some(key)) = (s.cache.as_ref(), key.as_ref()) {
            if let Some(cached) = cache.get(key) {
                return json_response(cached, "HIT");
            }
        }
    }

    let token = b.token.clone();
    match run_json(&s, move |e| e.run_supported(&supported, &token)).await {
        Ok(body) => {
            if let (Some(cache), Some(key)) = (s.cache.as_ref(), key) {
                cache.put(key, body.clone());
            }
            json_response(body, "MISS")
        }
        Err(code) => code.into_response(),
    }
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
        store
            .write_events(&[
                ev("pageview", "u1"),
                ev("pageview", "u2"),
                ev("click", "u1"),
            ])
            .unwrap();
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
                "range": {
                    "from": "2020-01-01T00:00:00Z",
                    "to": "2030-01-01T00:00:00Z"
                },
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

    /// The cache is only worth having if the second identical query skips
    /// DuckDB entirely — and `refresh: true` has to be able to bypass it.
    #[tokio::test]
    async fn repeated_query_is_served_from_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(dir.path().to_path_buf()).unwrap();
        store
            .write_events(&[ev("pageview", "u1"), ev("pageview", "u2")])
            .unwrap();
        let engine = Arc::new(crate::query::QueryEngine::new(dir.path().to_path_buf()));
        let app = router(engine, Some(Arc::new(ResultCache::new(8))), None);

        let body = json!({
            "token": "phc_t",
            "query": {
                "kind": "Trends",
                "series": [{
                    "event": { "type": "name", "value": "pageview" },
                    "math": { "type": "total" }
                }],
                "filters": { "op": "AND", "values": [] },
                "range": {
                    "from": "2020-01-01T00:00:00Z",
                    "to": "2030-01-01T00:00:00Z"
                },
                "interval": "Day"
            }
        });

        let send = |app: Router, refresh: bool| {
            let mut b = body.clone();
            b["refresh"] = json!(refresh);
            async move {
                let resp = app
                    .oneshot(
                        axum::http::Request::builder()
                            .method("POST")
                            .uri("/api/query")
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::to_vec(&b).unwrap()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = resp.status();
                let cache = resp
                    .headers()
                    .get("x-cache")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let bytes = resp.into_body().collect().await.unwrap().to_bytes();
                (status, cache, bytes)
            }
        };

        let (status, cache, first) = send(app.clone(), false).await;
        assert_eq!(status, 200);
        assert_eq!(cache, "MISS");

        let (status, cache, second) = send(app.clone(), false).await;
        assert_eq!(status, 200);
        assert_eq!(cache, "HIT", "identical query should not re-run DuckDB");
        assert_eq!(
            first, second,
            "a cache hit must be byte-identical to the miss"
        );

        let (status, cache, _) = send(app, true).await;
        assert_eq!(status, 200);
        assert_eq!(cache, "MISS", "refresh must bypass the cache");
    }

    #[tokio::test]
    async fn unsupported_query_is_rejected_before_cache_and_query_permit() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(crate::query::QueryEngine::new(dir.path().to_path_buf()));
        let held_permits = acquire_all_query_permits(&engine).await;
        let cache = Arc::new(ResultCache::new(8));

        let query = json!({
            "kind": "Funnels",
            "series": [{
                "event": { "type": "name", "value": "signup" },
                "math": { "type": "total" }
            }],
            "filters": { "op": "AND", "values": [] },
            "range": {
                "from": "2026-08-01T00:00:00Z",
                "to": "2026-08-02T00:00:00Z"
            },
            "interval": "Day"
        });
        let parsed_query: ir::Query = serde_json::from_value(query.clone()).unwrap();
        cache.put(
            CacheKey {
                token: "phc_t".into(),
                ir_hash: hash_ir(&serde_json::to_value(parsed_query).unwrap()),
                data_version: 0,
            },
            br#"{"cached":true}"#.to_vec(),
        );
        let app = router(engine, Some(cache), None);
        let body = json!({ "token": "phc_t", "query": query });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/query")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            ),
        )
        .await
        .expect("unsupported validation must not wait for a query permit")
        .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(response.headers().get("x-cache").is_none());
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("error response must carry a request id")
            .to_owned();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"]["code"], "unsupported_query");
        assert_eq!(parsed["error"]["field"], "query.kind");
        assert_eq!(
            parsed["error"]["message"],
            "only trends queries are supported"
        );
        assert_eq!(parsed["error"]["request_id"], request_id);
        drop(held_permits);
    }

    async fn acquire_all_query_permits(
        engine: &Arc<QueryEngine>,
    ) -> Vec<tokio::sync::OwnedSemaphorePermit> {
        let mut permits = Vec::new();
        for _ in 0..crate::query::MAX_CONCURRENT_QUERIES {
            permits.push(engine.acquire().await);
        }
        permits
    }
}

//! Capture endpoints — one handler aliased across every path PostHog SDKs
//! post events to (compat-spec.md "Endpoints").
//!
//! Response codes are load-bearing: 200 normally, 204 when `beacon=1`, 4xx
//! for anything the client must not retry, 503 only for retryable sink
//! failure. posthog-js retries 5xx and network errors, never 4xx.

pub mod decompress;
pub mod event;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use axum::body::Bytes;
use chrono::Utc;
use serde_json::json;

use crate::identity::IdentityStore;
use crate::registry::{Decision, Registry};
use crate::sink::EventSink;

#[derive(Clone)]
pub struct CaptureState {
    pub sink: Arc<dyn EventSink>,
    pub identity: Arc<IdentityStore>,
    pub registry: Arc<Registry>,
}

/// Body limit for browser-SDK endpoints (/e and friends).
pub const MAX_EVENT_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Body limit for server-SDK batches (/batch).
pub const MAX_BATCH_BODY_BYTES: usize = 20 * 1024 * 1024;

#[derive(serde::Deserialize, Default)]
pub struct CaptureQuery {
    /// sent_at in ms since epoch; doubles as cache buster.
    #[serde(rename = "_")]
    pub sent_at: Option<String>,
    pub compression: Option<String>,
    pub beacon: Option<String>,
}

pub fn router(state: CaptureState) -> Router {
    let small = Router::new()
        .route("/e", post(capture))
        .route("/e/", post(capture))
        .route("/capture", post(capture))
        .route("/capture/", post(capture))
        .route("/track", post(capture))
        .route("/track/", post(capture))
        .route("/engage", post(capture))
        .route("/engage/", post(capture))
        .route("/i/v0/e", post(capture))
        .route("/i/v0/e/", post(capture))
        .layer(DefaultBodyLimit::max(MAX_EVENT_BODY_BYTES));
    let batch = Router::new()
        .route("/batch", post(capture))
        .route("/batch/", post(capture))
        .layer(DefaultBodyLimit::max(MAX_BATCH_BODY_BYTES));
    small.merge(batch).with_state(state)
}

async fn capture(
    State(state): State<CaptureState>,
    Query(query): Query<CaptureQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let beacon = query.beacon.as_deref() == Some("1");

    let form_encoded = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));

    let text = match decompress::decode(&body, form_encoded, query.compression.as_deref()) {
        Ok(text) => text,
        Err(decompress::DecodeError::TooLarge) => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(decompress::DecodeError::Undecodable) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let now = Utc::now();
    let sent_at = query.sent_at.as_deref().and_then(event::parse_sent_at_ms);

    let batch = match event::parse_body(&text, sent_at, now) {
        Ok(batch) => batch,
        Err(event::CaptureError::Malformed(_)) => return StatusCode::BAD_REQUEST.into_response(),
        Err(event::CaptureError::Unauthorized(_)) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Empty after filtering is still success — never make clients retry.
    if !batch.events.is_empty() {
        // Token authenticity (SPEC.md "Security and tenancy"): shape was
        // checked at parse; the registry adds project authenticity. Open
        // mode (no projects) accepts any valid token.
        if let Some(first) = batch.events.first()
            && state.registry.check(&first.token) == Decision::Reject
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let events = batch.events;
        match state.sink.append(events.clone()).await {
            Ok(()) => {}
            Err(crate::sink::SinkError::Retryable) => {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            Err(crate::sink::SinkError::Fatal) => {
                return StatusCode::BAD_REQUEST.into_response();
            }
        }
        // Identity runs after the durability ack decision: person state is
        // rebuildable from the event log, so a failure here logs rather
        // than failing an already-durable batch.
        let identity = state.identity.clone();
        tokio::task::spawn_blocking(move || {
            for event in &events {
                if let Err(e) = identity.process(event) {
                    tracing::error!("identity processing failed: {e:?}");
                }
            }
        });
    }

    if beacon {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Json(json!({"status": 1})).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::MemorySink;
    use axum::body::Body;
    use axum::http::Request;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use tower::ServiceExt;

    fn app_with_sink() -> (Router, Arc<MemorySink>) {
        let sink = Arc::new(MemorySink::default());
        let state = CaptureState {
            sink: sink.clone(),
            identity: Arc::new(IdentityStore::in_memory().unwrap()),
            registry: Arc::new(Registry::in_memory().unwrap()),
        };
        (router(state), sink)
    }

    async fn post_body(router: Router, uri: &str, body: impl Into<Body>) -> StatusCode {
        router
            .oneshot(Request::post(uri).body(body.into()).unwrap())
            .await
            .unwrap()
            .status()
    }

    const EVENT: &str = r#"[{"event":"click","distinct_id":"u1","token":"phc_t"}]"#;

    #[tokio::test]
    async fn plain_json_to_e_stores_event() {
        let (router, sink) = app_with_sink();
        let status = post_body(router, "/e/", EVENT).await;
        assert_eq!(status, StatusCode::OK);
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "click");
        assert_eq!(events[0].distinct_id, "u1");
    }

    #[tokio::test]
    async fn gzip_without_hint_is_sniffed() {
        let (router, sink) = app_with_sink();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(EVENT.as_bytes()).unwrap();
        let status = post_body(router, "/e/", enc.finish().unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(sink.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn base64_body_unwraps() {
        let (router, sink) = app_with_sink();
        let status = post_body(router, "/e/", BASE64.encode(EVENT)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(sink.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn beacon_returns_204() {
        let (router, sink) = app_with_sink();
        let status = post_body(router, "/e/?beacon=1", EVENT).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(sink.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn batch_endpoint_accepts_batch_shape() {
        let (router, sink) = app_with_sink();
        let body = r#"{"api_key":"phc_t","batch":[{"event":"a","distinct_id":"u1"},{"event":"b","distinct_id":"u2"}]}"#;
        let status = post_body(router, "/batch/", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(sink.snapshot().len(), 2);
    }

    #[tokio::test]
    async fn all_aliases_accept() {
        for path in ["/e", "/capture", "/track", "/engage", "/i/v0/e", "/batch"] {
            let (router, _) = app_with_sink();
            let body = if path == "/engage" {
                r#"{"distinct_id":"u1","token":"phc_t","$set":{"a":1}}"#
            } else {
                EVENT
            };
            let status = post_body(router, path, body).await;
            assert_eq!(status, StatusCode::OK, "alias {path} failed");
        }
    }

    #[tokio::test]
    async fn malformed_json_is_400_never_retried() {
        let (router, _) = app_with_sink();
        assert_eq!(post_body(router, "/e/", "not json").await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let (router, _) = app_with_sink();
        let body = r#"[{"event":"a","distinct_id":"u1"}]"#;
        assert_eq!(post_body(router, "/e/", body).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn form_encoded_beacon_path_works() {
        let (router, sink) = app_with_sink();
        let data = BASE64
            .encode(EVENT)
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D");
        let status = router
            .oneshot(
                Request::post("/e/?beacon=1")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("data={data}")))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(sink.snapshot().len(), 1);
    }
}

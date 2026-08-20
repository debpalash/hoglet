//! Capture endpoints — one handler aliased across every path PostHog SDKs
//! post events to (spec/wire-compat.md "Endpoints").
//!
//! Response codes are load-bearing: 200 normally, 204 when `beacon=1`, 4xx
//! for anything the client must not retry, 503 only for retryable sink
//! failure. posthog-js retries 5xx and network errors, never 4xx.

pub mod decompress;
pub mod event;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::Utc;
use serde_json::json;

use crate::identity::IdentityStore;
use crate::metrics::Metrics;
use crate::ratelimit::RateLimiter;
use crate::registry::{Decision, Registry};
use crate::sink::{AuthorizedEventBatch, EventSink};

/// A project approved for capture.
///
/// New control-plane authorization always supplies `project_id`. The legacy
/// registry adapter cannot, which is why the field is optional until the
/// compatibility ingest path is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedCapture {
    pub project_id: Option<String>,
}

/// Capture authorization is deliberately fail-closed at the HTTP edge.
///
/// Both variants become a 401 so SDKs retain the established no-retry
/// behavior. `Unavailable` exists so adapters can preserve useful diagnostics
/// without leaking control-plane failures into the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureAuthorizationError {
    Rejected,
    Unavailable,
}

/// The only project-authentication dependency visible to capture handlers.
#[async_trait::async_trait]
pub trait CaptureAuthorizer: Send + Sync {
    async fn authorize(&self, token: &str) -> Result<AuthorizedCapture, CaptureAuthorizationError>;
}

/// Fail-closed adapter for authoritative `control.db` project access.
pub struct ProjectAccessCaptureAuthorizer {
    access: crate::control::ProjectAccess,
}

impl ProjectAccessCaptureAuthorizer {
    pub fn new(access: crate::control::ProjectAccess) -> Self {
        Self { access }
    }
}

#[async_trait::async_trait]
impl CaptureAuthorizer for ProjectAccessCaptureAuthorizer {
    async fn authorize(&self, token: &str) -> Result<AuthorizedCapture, CaptureAuthorizationError> {
        match self.access.authorize_capture(token).await {
            Ok(project) => Ok(AuthorizedCapture {
                project_id: project.project_id,
            }),
            Err(crate::control::AccessError::InvalidToken)
            | Err(crate::control::AccessError::Unauthorized) => {
                Err(CaptureAuthorizationError::Rejected)
            }
            Err(error) => {
                tracing::error!(?error, "capture authorization unavailable");
                Err(CaptureAuthorizationError::Unavailable)
            }
        }
    }
}

/// Compatibility adapter for legacy stores and isolated capture tests.
///
/// This preserves the registry's historical open mode. Production callers
/// must opt into it explicitly; the authoritative adapter above never opens.
pub struct LegacyRegistryCaptureAuthorizer {
    registry: Arc<Registry>,
}

impl LegacyRegistryCaptureAuthorizer {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl CaptureAuthorizer for LegacyRegistryCaptureAuthorizer {
    async fn authorize(&self, token: &str) -> Result<AuthorizedCapture, CaptureAuthorizationError> {
        match self.registry.check(token) {
            Decision::Accept => Ok(AuthorizedCapture { project_id: None }),
            Decision::Reject => Err(CaptureAuthorizationError::Rejected),
        }
    }
}

#[derive(Clone)]
pub struct CaptureState {
    pub sink: Arc<dyn EventSink>,
    pub identity: Arc<IdentityStore>,
    pub authorizer: Arc<dyn CaptureAuthorizer>,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
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
        Err(event::CaptureError::Malformed(_)) => {
            state.metrics.inc_rejected();
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(event::CaptureError::Unauthorized(_)) => {
            state.metrics.inc_rejected();
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Empty after filtering is still success — never make clients retry.
    if !batch.events.is_empty() {
        // Shape was checked during parsing. Authenticate every distinct token
        // before applying rate limits or making any durable write; a batch may
        // never inherit the first event's project authorization.
        let mut token_counts = BTreeMap::<&str, u32>::new();
        for event in &batch.events {
            let count = token_counts.entry(event.token.as_str()).or_default();
            *count = count.saturating_add(1);
        }
        let mut project_ids_by_token = BTreeMap::new();
        for token in token_counts.keys() {
            match state.authorizer.authorize(token).await {
                Ok(authorized) => {
                    if let Some(project_id) = authorized.project_id {
                        project_ids_by_token.insert((*token).to_owned(), project_id);
                    }
                }
                Err(_) => {
                    state.metrics.inc_rejected();
                    return StatusCode::UNAUTHORIZED.into_response();
                }
            }
        }
        // Rate limit each authorized project independently; 429 is retry-safe
        // on the SDK's backoff.
        for (token, count) in token_counts {
            if !state.limiter.allow(token, count, now.timestamp()) {
                state.metrics.inc_rejected();
                return StatusCode::TOO_MANY_REQUESTS.into_response();
            }
        }
        // `historical_migration` is an offline-import authority, never a wire
        // property. A client cannot elevate an ordinary capture batch by
        // placing this implementation detail in its JSON body.
        let historical_migration = false;
        let events = batch.events;
        let n = events.len() as u64;
        state.metrics.inc_captured(n);
        match state
            .sink
            .append(AuthorizedEventBatch {
                events: events.clone(),
                project_ids_by_token,
                historical_migration,
            })
            .await
        {
            Ok(()) => state.metrics.inc_acked(n),
            Err(crate::sink::SinkError::Retryable) => {
                state.metrics.inc_sink_errors();
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            Err(crate::sink::SinkError::Fatal) => {
                state.metrics.inc_rejected();
                return StatusCode::BAD_REQUEST.into_response();
            }
        }
        // Identity runs after the durability ack decision: person state is
        // rebuildable from the event log, so a failure here logs rather
        // than failing an already-durable batch.
        let identity = state.identity.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || {
            for event in &events {
                if let Err(e) = identity.process(event) {
                    tracing::error!("identity processing failed: {e:?}");
                }
            }
        })
        .await
        {
            tracing::error!(?error, "identity projection task failed");
        }
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

    #[derive(Default)]
    struct BatchSink {
        batches: std::sync::Mutex<Vec<AuthorizedEventBatch>>,
    }

    #[async_trait::async_trait]
    impl EventSink for BatchSink {
        async fn append(&self, batch: AuthorizedEventBatch) -> Result<(), crate::sink::SinkError> {
            self.batches.lock().unwrap().push(batch);
            Ok(())
        }
    }

    fn app_with_sink() -> (Router, Arc<MemorySink>) {
        let sink = Arc::new(MemorySink::default());
        let registry = Arc::new(Registry::in_memory().unwrap());
        let state = CaptureState {
            sink: sink.clone(),
            identity: Arc::new(IdentityStore::in_memory().unwrap()),
            authorizer: Arc::new(LegacyRegistryCaptureAuthorizer::new(registry)),
            limiter: Arc::new(RateLimiter::new(crate::ratelimit::DEFAULT_MAX_PER_SEC)),
            metrics: Arc::new(Metrics::default()),
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
    async fn one_unknown_token_rejects_entire_batch_before_append() {
        let sink = Arc::new(MemorySink::default());
        let registry = Arc::new(Registry::in_memory().unwrap());
        registry
            .create_project("phc_known", "Known", "2026-08-20T00:00:00Z")
            .unwrap();
        let state = CaptureState {
            sink: sink.clone(),
            identity: Arc::new(IdentityStore::in_memory().unwrap()),
            authorizer: Arc::new(LegacyRegistryCaptureAuthorizer::new(registry)),
            limiter: Arc::new(RateLimiter::new(crate::ratelimit::DEFAULT_MAX_PER_SEC)),
            metrics: Arc::new(Metrics::default()),
        };
        let body = r#"[{"event":"known","distinct_id":"u1","token":"phc_known"},{"event":"unknown","distinct_id":"u2","token":"phc_unknown"}]"#;

        let status = post_body(router(state), "/batch/", body).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn capture_clients_cannot_mark_batches_as_historical_migrations() {
        let sink = Arc::new(BatchSink::default());
        let registry = Arc::new(Registry::in_memory().unwrap());
        let state = CaptureState {
            sink: sink.clone(),
            identity: Arc::new(IdentityStore::in_memory().unwrap()),
            authorizer: Arc::new(LegacyRegistryCaptureAuthorizer::new(registry)),
            limiter: Arc::new(RateLimiter::new(crate::ratelimit::DEFAULT_MAX_PER_SEC)),
            metrics: Arc::new(Metrics::default()),
        };
        let body = r#"{"api_key":"phc_t","historical_migration":true,"batch":[{"event":"a","distinct_id":"u1"}]}"#;

        let status = post_body(router(state), "/batch/", body).await;

        assert_eq!(status, StatusCode::OK);
        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(!batches[0].historical_migration);
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
        assert_eq!(
            post_body(router, "/e/", "not json").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let (router, _) = app_with_sink();
        let body = r#"[{"event":"a","distinct_id":"u1"}]"#;
        assert_eq!(
            post_body(router, "/e/", body).await,
            StatusCode::UNAUTHORIZED
        );
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

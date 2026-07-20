//! Hoglet — PostHog-compatible product analytics. One binary.
//!
//! The wire contract lives in `compat-spec.md` at the repo root; every handler
//! cites the section it implements. PostHog wire semantics at the edge,
//! byte-for-byte — anything custom stays behind it.

pub mod capture;
pub mod routes;
pub mod sink;
pub mod token;

use std::sync::Arc;

use axum::Router;
use tower_http::cors::CorsLayer;

use sink::EventSink;

/// Build the full application router.
///
/// CORS is maximally permissive by contract (compat-spec.md "Responses"):
/// old SDKs and reverse proxies send funky headers, and analytics endpoints
/// are public by nature.
pub fn app_with_sink(sink: Arc<dyn EventSink>) -> Router {
    Router::new()
        .merge(routes::config::router())
        .merge(capture::router(sink))
        .layer(CorsLayer::very_permissive())
}

/// Default app: logs events until the WAL lands.
pub fn app() -> Router {
    app_with_sink(Arc::new(sink::LogSink))
}

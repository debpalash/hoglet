//! Hoglet — PostHog-compatible product analytics. One binary.
//!
//! The wire contract lives in `compat-spec.md` at the repo root; every handler
//! cites the section it implements. PostHog wire semantics at the edge,
//! byte-for-byte — anything custom stays behind it.

pub mod capture;
pub mod flush;
pub mod identity;
pub mod routes;
pub mod sink;
pub mod store;
pub mod token;
pub mod wal;

use std::sync::Arc;

use axum::Router;
use tower_http::cors::CorsLayer;

use capture::CaptureState;

/// Build the full application router.
///
/// CORS is maximally permissive by contract (compat-spec.md "Responses"):
/// old SDKs and reverse proxies send funky headers, and analytics endpoints
/// are public by nature.
pub fn app_with_state(state: CaptureState, readiness: routes::health::Readiness) -> Router {
    Router::new()
        .merge(routes::config::router())
        .merge(routes::flags::router())
        .merge(routes::health::router(readiness))
        .merge(capture::router(state))
        .layer(CorsLayer::very_permissive())
}

/// Default app: log sink, in-memory identity, immediately ready. For tests
/// and dry runs.
pub fn app() -> Router {
    let readiness = routes::health::Readiness::new();
    readiness.mark_ready();
    app_with_state(
        CaptureState {
            sink: Arc::new(sink::LogSink),
            identity: Arc::new(identity::IdentityStore::in_memory().expect("in-memory sqlite")),
        },
        readiness,
    )
}

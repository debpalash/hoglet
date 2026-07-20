//! Hoglet — PostHog-compatible product analytics. One binary.
//!
//! The wire contract lives in `compat-spec.md` at the repo root; every handler
//! cites the section it implements. PostHog wire semantics at the edge,
//! byte-for-byte — anything custom stays behind it.

pub mod routes;
pub mod token;

use axum::Router;
use tower_http::cors::CorsLayer;

/// Build the full application router.
///
/// CORS is maximally permissive by contract (compat-spec.md "Responses"):
/// old SDKs and reverse proxies send funky headers, and analytics endpoints
/// are public by nature.
pub fn app() -> Router {
    Router::new()
        .merge(routes::config::router())
        .layer(CorsLayer::very_permissive())
}

//! `/metrics` — Prometheus text exposition of Hoglet's own counters
//! (SPEC.md "self-observability").

use std::sync::Arc;

use axum::{Router, extract::State, response::IntoResponse, routing::get};

use crate::metrics::Metrics;

pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(metrics)
}

async fn scrape(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    (
        [("content-type", "text/plain; version=0.0.4")],
        metrics.render(now),
    )
}

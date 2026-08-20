//! Liveness and readiness (spec/README.md "Operational contract").
//!
//! `/health` is liveness: the process is up. `/ready` gates traffic — it is
//! not ready until WAL recovery and the stores are open, closing the
//! cold-start window where a load balancer could route to a not-yet-serving
//! process.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};

/// Flips to true once startup is complete. Cheap to clone and share.
#[derive(Clone, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn mark_ready(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn mark_not_ready(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub fn router(readiness: Readiness) -> Router {
    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .route(
            "/ready",
            get(move || {
                let readiness = readiness.clone();
                async move {
                    if readiness.is_ready() {
                        StatusCode::OK.into_response()
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    }
                }
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn status(router: Router, uri: &str) -> StatusCode {
        router
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn health_always_ok() {
        let r = Readiness::new();
        assert_eq!(status(router(r), "/health").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_gates_until_marked() {
        let r = Readiness::new();
        assert_eq!(
            status(router(r.clone()), "/ready").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        r.mark_ready();
        assert_eq!(status(router(r.clone()), "/ready").await, StatusCode::OK);
        r.mark_not_ready();
        assert_eq!(
            status(router(r), "/ready").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

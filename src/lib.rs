//! Hoglet — PostHog-compatible product analytics. One binary.
//!
//! The wire contract lives in `spec/wire-compat.md` at the repo root; every handler
//! cites the section it implements. PostHog wire semantics at the edge,
//! byte-for-byte — anything custom stays behind it.

pub mod auth;
pub mod cache;
pub mod capture;
pub mod catalog;
pub mod cohort;
pub mod dashboard_store;
pub mod demo;
pub mod enrichment;
pub mod file_index;
pub mod flags;
pub mod flush;
pub mod identity;
pub mod metrics;
pub mod middleware;
pub mod query;
pub mod ratelimit;
pub mod registry;
pub mod routes;
pub mod session;
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
/// CORS is maximally permissive by contract (spec/wire-compat.md "Responses"):
/// old SDKs and reverse proxies send funky headers, and analytics endpoints
/// are public by nature.
#[allow(clippy::too_many_arguments)]
pub fn app_with_state(
    state: CaptureState,
    readiness: routes::health::Readiness,
    engine: Arc<query::QueryEngine>,
    flag_store: Arc<flags::FlagStore>,
    store: Arc<store::EventStore>,
    admin_token: Option<Arc<String>>,
    catalog: Option<Arc<catalog::CatalogStore>>,
    auth_store: Option<Arc<auth::AuthStore>>,
    dash_store: Option<Arc<dashboard_store::DashboardStore>>,
    cache: Option<Arc<cache::ResultCache>>,
    index: Option<Arc<file_index::FileIndex>>,
) -> Router {
    let admin = routes::admin::AdminState {
        registry: state.registry.clone(),
        flags: flag_store.clone(),
        identity: state.identity.clone(),
        store: store.clone(),
        admin_token,
    };
    let metrics = state.metrics.clone();
    let mut router = Router::new()
        .merge(routes::dashboard::router())
        .merge(routes::config::router())
        .merge(routes::flags::router(flag_store, state.identity.clone()))
        .merge(routes::health::router(readiness))
        .merge(routes::metrics::router(metrics))
        .merge(routes::docs::router())
        .merge(routes::api::router(engine, cache, index))
        .merge(routes::admin::router(admin))
        .merge(capture::router(state));
    if let Some(cat) = catalog {
        router = router.merge(routes::catalog::router(cat));
    }
    let auth_for_middleware = auth_store.clone();
    if let Some(ref auth) = auth_store {
        router = router.merge(routes::auth::router(auth.clone()));
    }
    if let Some(dash) = dash_store {
        router = router.merge(routes::dashboards::router(dash));
    }
    let auth_layer_state = middleware::auth::AuthLayerState { store: auth_for_middleware };
    router = router.layer(axum::middleware::from_fn_with_state(auth_layer_state, middleware::auth::require_auth));
    router.layer(CorsLayer::very_permissive())
}

/// Default app: log sink, in-memory identity, empty query engine, immediately
/// ready. For tests and dry runs.
pub fn app() -> Router {
    let readiness = routes::health::Readiness::new();
    readiness.mark_ready();
    app_with_state(
        CaptureState {
            sink: Arc::new(sink::LogSink),
            identity: Arc::new(identity::IdentityStore::in_memory().expect("in-memory sqlite")),
            registry: Arc::new(registry::Registry::in_memory().expect("in-memory sqlite")),
            limiter: Arc::new(ratelimit::RateLimiter::new(ratelimit::DEFAULT_MAX_PER_SEC)),
            metrics: Arc::new(metrics::Metrics::default()),
        },
        readiness,
        Arc::new(query::QueryEngine::new(std::path::PathBuf::from(
            "/nonexistent-hoglet-events",
        ))),
        Arc::new(flags::FlagStore::in_memory().expect("in-memory sqlite")),
        Arc::new(
            store::EventStore::open(std::env::temp_dir().join("hoglet-default-store"))
                .expect("temp event store"),
        ),
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

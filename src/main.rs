use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hoglet=info".into()),
        )
        .init();

    let addr: SocketAddr = std::env::var("HOGLET_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8000".into())
        .parse()
        .expect("HOGLET_ADDR must be a valid socket address like 0.0.0.0:8000");

    let data_dir = std::path::PathBuf::from(
        std::env::var("HOGLET_DATA").unwrap_or_else(|_| "hoglet-data".into()),
    );
    let (wal, _wal_runtime, recovered) =
        hoglet::wal::Wal::open(data_dir.join("wal")).expect("cannot open WAL");
    if !recovered.events.is_empty() {
        tracing::info!(
            count = recovered.events.len(),
            truncated_tail = recovered.truncated,
            "recovered unflushed events from WAL; flusher will store them"
        );
    }
    let store = std::sync::Arc::new(
        hoglet::store::EventStore::open(data_dir.join("events")).expect("cannot open event store"),
    );
    let retention_days = std::env::var("HOGLET_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok());
    let catalog = hoglet::catalog::CatalogStore::open(&data_dir.join("catalog.db"))
        .ok()
        .map(std::sync::Arc::new);

    let session = hoglet::session::SessionStore::open(&data_dir.join("sessions.db"))
        .ok()
        .map(std::sync::Arc::new);

    let session_for_demo = session.clone();

    hoglet::flush::spawn(
        wal.clone(),
        store.clone(),
        catalog.clone(),
        session,
        retention_days,
    );

    let auth = hoglet::auth::AuthStore::open(&data_dir.join("auth.db"))
        .ok()
        .map(std::sync::Arc::new);

    let is_first_run = auth
        .as_ref()
        .map(|a| a.is_empty().unwrap_or(false))
        .unwrap_or(true);
    if is_first_run {
        tracing::info!("first run detected — visit /dashboard to set up your account");
    }

    let dash = hoglet::dashboard_store::DashboardStore::open(&data_dir.join("dashboards.db"))
        .ok()
        .map(std::sync::Arc::new);
    let identity = std::sync::Arc::new(
        hoglet::identity::IdentityStore::open(&data_dir.join("identity.db"))
            .expect("cannot open identity store"),
    );
    let registry = std::sync::Arc::new(
        hoglet::registry::Registry::open(
            rusqlite::Connection::open(data_dir.join("projects.db"))
                .expect("cannot open projects db"),
        )
        .expect("cannot init registry"),
    );
    let max_per_sec = std::env::var("HOGLET_MAX_EVENTS_PER_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(hoglet::ratelimit::DEFAULT_MAX_PER_SEC);
    let now_epoch = chrono::Utc::now().timestamp().max(0) as u64;
    let state = hoglet::capture::CaptureState {
        sink: std::sync::Arc::new(hoglet::wal::WalSink(wal)),
        identity,
        registry,
        limiter: std::sync::Arc::new(hoglet::ratelimit::RateLimiter::new(max_per_sec)),
        metrics: std::sync::Arc::new(hoglet::metrics::Metrics::new(now_epoch)),
    };
    let engine = std::sync::Arc::new(hoglet::query::QueryEngine::new(data_dir.join("events")));
    let flag_store = std::sync::Arc::new(
        hoglet::flags::FlagStore::open(
            rusqlite::Connection::open(data_dir.join("flags.db")).expect("cannot open flags db"),
        )
        .expect("cannot init flag store"),
    );

    let readiness = hoglet::routes::health::Readiness::new();

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    // Seed demo data if HOGLET_DEMO=1
    hoglet::demo::seed_if_demo(
        &store,
        &flag_store,
        dash.as_deref(),
        catalog.as_ref(),
        session_for_demo.as_ref(),
    );

    // Stores are open and the flusher is running: declare ready so /ready
    // starts answering 200.
    readiness.mark_ready();
    tracing::info!("hoglet listening on {addr}");

    let admin_token = std::env::var("HOGLET_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::sync::Arc::new);

    let catalog = hoglet::catalog::CatalogStore::open(&data_dir.join("catalog.db"))
        .ok()
        .map(std::sync::Arc::new);

    let _session = hoglet::session::SessionStore::open(&data_dir.join("sessions.db"))
        .ok()
        .map(std::sync::Arc::new);

    let file_index = std::sync::Arc::new(hoglet::file_index::FileIndex::new());
    file_index.rebuild_from_dir(&data_dir.join("events"));

    let result_cache = std::sync::Arc::new(hoglet::cache::ResultCache::new(200));

    axum::serve(
        listener,
        hoglet::app_with_state(
            state,
            readiness,
            engine,
            flag_store,
            store,
            admin_token,
            catalog,
            auth,
            dash,
            Some(result_cache),
            Some(file_index),
        ),
    )
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
        .expect("server error");
}

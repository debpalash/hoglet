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
    hoglet::flush::spawn(wal.clone(), store);
    let identity = std::sync::Arc::new(
        hoglet::identity::IdentityStore::open(&data_dir.join("identity.db"))
            .expect("cannot open identity store"),
    );
    let state = hoglet::capture::CaptureState {
        sink: std::sync::Arc::new(hoglet::wal::WalSink(wal)),
        identity,
    };

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    tracing::info!("hoglet listening on {addr}");

    axum::serve(listener, hoglet::app_with_state(state))
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
        .expect("server error");
}

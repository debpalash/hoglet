use std::error::Error;
use std::net::SocketAddr;

use hoglet::application::{Application, ApplicationConfig};
use hoglet::storage_bootstrap::StoragePaths;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hoglet=info".into()),
        )
        .init();

    let data_dir = std::path::PathBuf::from(
        std::env::var("HOGLET_DATA").unwrap_or_else(|_| "hoglet-data".into()),
    );
    match std::env::args().nth(1).as_deref() {
        Some("migrate") => {
            let report = hoglet::migration::migrate_legacy_storage(&StoragePaths::new(&data_dir))?;
            tracing::info!(
                pair_id = %report.pair_id,
                discovered_tokens = report.discovered_tokens,
                resumed = report.resumed,
                already_complete = report.already_complete,
                "offline migration complete"
            );
            return Ok(());
        }
        Some(command) => {
            return Err(format!("unknown command {command:?}; expected `migrate`").into());
        }
        None => {}
    }

    let addr: SocketAddr = std::env::var("HOGLET_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8000".into())
        .parse()
        .map_err(|error| format!("HOGLET_ADDR must be a socket address: {error}"))?;
    let mut config = ApplicationConfig::new(data_dir);
    if let Some(limit) = std::env::var("HOGLET_MAX_EVENTS_PER_SEC")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        config.max_events_per_second = limit;
    }

    let application = Application::prepare(config).await?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    application.mark_ready();
    tracing::info!(%addr, "hoglet listening");
    let shutdown_readiness = application.readiness();
    let server_result = axum::serve(listener, application.router())
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_readiness.mark_not_ready();
        })
        .await;
    let shutdown_result = application.shutdown().await;
    server_result?;
    shutdown_result?;
    Ok(())
}

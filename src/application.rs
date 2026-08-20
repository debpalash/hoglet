//! Safe production application composition.
//!
//! `Application::prepare` is the startup-policy seam: inspection always comes
//! before mutation, every authoritative dependency is required, and only the
//! production trust classes are mounted.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use tower_http::cors::CorsLayer;

use crate::capture::{CaptureAuthorizer, CaptureState, ProjectAccessCaptureAuthorizer};
use crate::control::{AccessError, ProjectAccess, ProjectAccessRuntime};
use crate::control_resources::{ControlResourceError, ControlResources};
use crate::event_lake::EventLakeError;
use crate::pipeline::publication::PublicationCoordinator;
use crate::projection_catalog::{ProjectionCatalog, ProjectionCatalogError};
use crate::query::QueryEngine;
use crate::routes::health::Readiness;
use crate::sink::{DurablePipelineError, DurableWalRuntime, DurableWalSink};
use crate::storage_bootstrap::{
    StorageBootstrapError, StorageDisposition, StoragePaths, bootstrap_storage, inspect_storage,
};

#[derive(Debug, Clone)]
pub struct ApplicationConfig {
    pub data_dir: PathBuf,
    pub max_events_per_second: u32,
}

impl ApplicationConfig {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            max_events_per_second: crate::ratelimit::DEFAULT_MAX_PER_SEC,
        }
    }
}

#[derive(Debug)]
pub enum ApplicationError {
    LegacyOnly {
        data_dir: PathBuf,
    },
    MigrationIncomplete {
        data_dir: PathBuf,
    },
    Storage(StorageBootstrapError),
    Access(AccessError),
    Resources(ControlResourceError),
    ProjectionCatalog(ProjectionCatalogError),
    EventLake(EventLakeError),
    Query(String),
    Wal(crate::pipeline::wal::WalError),
    Pipeline(DurablePipelineError),
    LocalStore(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyOnly { data_dir } => write!(
                formatter,
                "legacy Hoglet storage found in {}; stop the server and run `hoglet migrate`",
                data_dir.display()
            ),
            Self::MigrationIncomplete { data_dir } => write!(
                formatter,
                "incomplete storage migration found in {}; inspect it and rerun `hoglet migrate`",
                data_dir.display()
            ),
            Self::Storage(error) => write!(formatter, "storage bootstrap failed: {error}"),
            Self::Access(error) => write!(formatter, "project access failed: {error}"),
            Self::Resources(error) => write!(formatter, "control resources failed: {error}"),
            Self::ProjectionCatalog(error) => {
                write!(formatter, "projection catalog failed: {error}")
            }
            Self::EventLake(error) => write!(formatter, "event lake failed: {error}"),
            Self::Query(error) => write!(formatter, "query engine failed: {error}"),
            Self::Wal(error) => write!(formatter, "durable capture WAL failed: {error}"),
            Self::Pipeline(error) => write!(formatter, "durable pipeline failed: {error}"),
            Self::LocalStore(error) => write!(formatter, "ephemeral wire state failed: {error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ApplicationError {}

impl From<StorageBootstrapError> for ApplicationError {
    fn from(error: StorageBootstrapError) -> Self {
        Self::Storage(error)
    }
}
impl From<AccessError> for ApplicationError {
    fn from(error: AccessError) -> Self {
        Self::Access(error)
    }
}
impl From<ControlResourceError> for ApplicationError {
    fn from(error: ControlResourceError) -> Self {
        Self::Resources(error)
    }
}
impl From<ProjectionCatalogError> for ApplicationError {
    fn from(error: ProjectionCatalogError) -> Self {
        Self::ProjectionCatalog(error)
    }
}
impl From<EventLakeError> for ApplicationError {
    fn from(error: EventLakeError) -> Self {
        Self::EventLake(error)
    }
}
impl From<crate::pipeline::wal::WalError> for ApplicationError {
    fn from(error: crate::pipeline::wal::WalError) -> Self {
        Self::Wal(error)
    }
}
impl From<DurablePipelineError> for ApplicationError {
    fn from(error: DurablePipelineError) -> Self {
        Self::Pipeline(error)
    }
}

pub struct Application {
    router: Router,
    readiness: Readiness,
    wal_runtime: DurableWalRuntime,
    control_runtime: ProjectAccessRuntime,
}

impl fmt::Debug for Application {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Application")
            .finish_non_exhaustive()
    }
}

impl Application {
    pub async fn prepare(config: ApplicationConfig) -> Result<Self, ApplicationError> {
        let paths = StoragePaths::new(&config.data_dir);
        match inspect_storage(&paths)? {
            StorageDisposition::Fresh | StorageDisposition::ReadyV2(_) => {
                bootstrap_storage(paths.control(), paths.projections())?;
            }
            StorageDisposition::LegacyOnly => {
                return Err(ApplicationError::LegacyOnly {
                    data_dir: config.data_dir,
                });
            }
            StorageDisposition::MigrationIncomplete
                if recoverable_fresh_bootstrap(&paths) && !paths.has_legacy_artifacts() =>
            {
                // Fresh-pair publication renames projections first. If the
                // process died before the final control rename, the staged
                // control database can be validated against that projection
                // database and completed safely. Legacy migrations use the
                // same staging suffix, so their artifacts deliberately keep
                // this recovery path closed and must resume through `migrate`.
                bootstrap_storage(paths.control(), paths.projections())?;
            }
            StorageDisposition::MigrationIncomplete => {
                return Err(ApplicationError::MigrationIncomplete {
                    data_dir: config.data_dir,
                });
            }
        }

        let event_root = canonical_directory(config.data_dir.join("events"))?;
        let wal_root = config.data_dir.join("wal-v2");
        let (access, control_runtime) = ProjectAccess::open(paths.control())?;
        let access = Arc::new(access);
        let resources = Arc::new(ControlResources::open(&paths.control())?);
        let coordinator = PublicationCoordinator::open(paths.projections(), &event_root)
            .map_err(DurablePipelineError::Publication)?;
        let engine = Arc::new(
            QueryEngine::try_new_versioned(coordinator.event_lake())
                .map_err(|error| ApplicationError::Query(format!("{error:?}")))?,
        );
        let projection_catalog = Arc::new(ProjectionCatalog::open(&paths.projections())?);
        let (durable_sink, wal_runtime, recovery) = DurableWalSink::open(wal_root, coordinator)?;
        if recovery.truncated_tail {
            tracing::warn!("recovered a torn tail from the v2 capture WAL");
        }

        let identity = Arc::new(
            crate::identity::IdentityStore::in_memory().map_err(|error| {
                ApplicationError::LocalStore(format!("identity initialization: {error:?}"))
            })?,
        );
        let metrics = Arc::new(crate::metrics::Metrics::new(
            chrono::Utc::now().timestamp().max(0) as u64,
        ));
        let authorizer: Arc<dyn CaptureAuthorizer> =
            Arc::new(ProjectAccessCaptureAuthorizer::new(access.as_ref().clone()));
        let capture = CaptureState {
            sink: durable_sink,
            identity: identity.clone(),
            authorizer: authorizer.clone(),
            limiter: Arc::new(crate::ratelimit::RateLimiter::new(
                config.max_events_per_second,
            )),
            metrics: metrics.clone(),
        };
        let readiness = Readiness::new();
        let wire = crate::capture::router(capture)
            .merge(crate::routes::config::wire_router(authorizer.clone()))
            .merge(crate::routes::flags::control_wire_router(
                resources.clone(),
                identity,
                authorizer,
            ))
            .layer(CorsLayer::very_permissive());
        let public = crate::routes::dashboard::router()
            .merge(crate::routes::health::router(readiness.clone()))
            .merge(crate::routes::metrics::router(metrics))
            .merge(crate::routes::docs::router());
        let dashboard = crate::routes::workspace::router(access.clone())
            .merge(crate::routes::project::router(access.clone(), engine))
            .merge(crate::routes::catalog_v2::router(
                access.clone(),
                projection_catalog,
            ))
            .merge(crate::routes::resources::router(access, resources));

        Ok(Self {
            router: wire.merge(public).merge(dashboard),
            readiness,
            wal_runtime,
            control_runtime,
        })
    }

    pub fn router(&self) -> Router {
        self.router.clone()
    }
    pub fn mark_ready(&self) {
        self.readiness.mark_ready();
    }
    pub fn mark_not_ready(&self) {
        self.readiness.mark_not_ready();
    }
    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    pub async fn shutdown(self) -> Result<(), ApplicationError> {
        self.readiness.mark_not_ready();
        drop(self.router);
        let wal_result = self.wal_runtime.shutdown().await;
        self.control_runtime.close();
        wal_result?;
        Ok(())
    }
}

fn recoverable_fresh_bootstrap(paths: &StoragePaths) -> bool {
    !paths.control().exists()
        && paths.projections().is_file()
        && paths.control_migrating().is_file()
        && !paths.projections_migrating().exists()
}

fn canonical_directory(path: PathBuf) -> Result<PathBuf, ApplicationError> {
    std::fs::create_dir_all(&path).map_err(|source| ApplicationError::Io {
        path: path.clone(),
        source,
    })?;
    std::fs::canonicalize(&path).map_err(|source| ApplicationError::Io { path, source })
}

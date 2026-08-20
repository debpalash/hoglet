//! Read-only import of legacy project resources into canonical control state.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::control_resources::{
    ControlResourceError, ControlResources, DashboardTileInput, FeatureFlag, ImportedDashboard,
    ImportedFeatureFlag, ImportedInsight, ImportedShareLink, ShareTarget,
};
use crate::flags::{Conditions, Variant};
use rusqlite::{Connection, OpenFlags};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyResourceImportReport {
    pub flags: usize,
    pub insights: usize,
    pub dashboards: usize,
    pub tiles: usize,
    pub shares: usize,
}

#[derive(Debug)]
pub enum LegacyResourceImportError {
    Database {
        path: PathBuf,
        source: rusqlite::Error,
    },
    Resource(ControlResourceError),
    Invalid {
        path: PathBuf,
        detail: String,
    },
}

impl fmt::Display for LegacyResourceImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Resource(error) => write!(formatter, "control resource import failed: {error}"),
            Self::Invalid { path, detail } => {
                write!(
                    formatter,
                    "invalid legacy resources in {}: {detail}",
                    path.display()
                )
            }
        }
    }
}

impl Error for LegacyResourceImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database { source, .. } => Some(source),
            Self::Resource(source) => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

impl From<ControlResourceError> for LegacyResourceImportError {
    fn from(value: ControlResourceError) -> Self {
        Self::Resource(value)
    }
}

pub fn import_legacy_resources(
    data_dir: &Path,
    control_path: &Path,
) -> Result<LegacyResourceImportReport, LegacyResourceImportError> {
    let project_by_token = load_project_map(control_path)?;
    let flags_path = data_dir.join("flags.db");
    let dashboards_path = data_dir.join("dashboards.db");
    let flags = load_flags(&flags_path)?;
    let dashboard_data = load_dashboards(&dashboards_path)?;

    let resources = ControlResources::open(control_path)?;
    for flag in &flags {
        let project_id = project_for_token(&project_by_token, &flags_path, &flag.token)?;
        resources.import_flag(
            project_id,
            ImportedFeatureFlag {
                flag: flag.flag.clone(),
                created_at: 0,
                updated_at: 0,
            },
        )?;
    }
    for insight in &dashboard_data.insights {
        let project_id = project_for_token(&project_by_token, &dashboards_path, &insight.token)?;
        resources.import_insight(project_id, insight.insight.clone())?;
    }
    for dashboard in &dashboard_data.dashboards {
        let project_id = project_for_token(&project_by_token, &dashboards_path, &dashboard.token)?;
        resources.import_dashboard(project_id, dashboard.dashboard.clone())?;
    }
    for share in &dashboard_data.shares {
        let project_id =
            project_for_token(&project_by_token, &dashboards_path, &share.project_token)?;
        resources.import_share(project_id, share.share.clone())?;
    }

    Ok(LegacyResourceImportReport {
        flags: flags.len(),
        insights: dashboard_data.insights.len(),
        dashboards: dashboard_data.dashboards.len(),
        tiles: dashboard_data.tile_count,
        shares: dashboard_data.shares.len(),
    })
}

#[derive(Clone)]
struct LegacyFlag {
    token: String,
    flag: FeatureFlag,
}

fn load_flags(path: &Path) -> Result<Vec<LegacyFlag>, LegacyResourceImportError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = open_legacy(path)?;
    require_table(&connection, path, "feature_flags")?;
    let mut statement = connection
        .prepare(
            "SELECT token,key,active,rollout_percentage,variants,conditions
             FROM feature_flags ORDER BY token,key",
        )
        .map_err(|source| database_error(path, source))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|source| database_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| database_error(path, source))?;

    rows.into_iter()
        .map(|(token, key, active, rollout_percentage, variants, conditions)| {
            let variants = decode_optional_json::<Vec<Variant>>(path, variants)?.unwrap_or_default();
            let conditions = decode_optional_json::<Conditions>(path, conditions)?;
            if conditions.as_ref().is_some_and(|conditions| {
                !conditions.properties.is_empty() || !conditions.cohort_ids.is_empty()
            }) {
                return Err(LegacyResourceImportError::Invalid {
                    path: path.to_path_buf(),
                    detail: "targeted feature-flag conditions are not representable in control state"
                        .into(),
                });
            }
            Ok(LegacyFlag {
                token,
                flag: FeatureFlag {
                    key,
                    active,
                    rollout_percentage,
                    variants,
                    payload: conditions.and_then(|conditions| conditions.payload),
                },
            })
        })
        .collect()
}

#[derive(Clone)]
struct LegacyInsight {
    token: String,
    insight: ImportedInsight,
}

#[derive(Clone)]
struct LegacyDashboard {
    token: String,
    dashboard: ImportedDashboard,
}

#[derive(Clone)]
struct LegacyShare {
    project_token: String,
    share: ImportedShareLink,
}

#[derive(Default)]
struct LegacyDashboardData {
    insights: Vec<LegacyInsight>,
    dashboards: Vec<LegacyDashboard>,
    shares: Vec<LegacyShare>,
    tile_count: usize,
}

fn load_dashboards(path: &Path) -> Result<LegacyDashboardData, LegacyResourceImportError> {
    if !path.exists() {
        return Ok(LegacyDashboardData::default());
    }
    let connection = open_legacy(path)?;
    for table in [
        "saved_insights",
        "dashboards",
        "dashboard_tiles",
        "share_links",
    ] {
        require_table(&connection, path, table)?;
    }

    let insights = collect_rows(
        &connection,
        path,
        "SELECT id,token,name,description,query_ir,created_by,created_at,updated_at
         FROM saved_insights ORDER BY token,id",
        |row| {
            let query_ir: String = row.get(4)?;
            Ok((
                row.get::<_, String>(1)?,
                ImportedInsight {
                    id: row.get(0)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    query_ir: serde_json::from_str(&query_ir).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    created_by: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                },
            ))
        },
    )?
    .into_iter()
    .map(|(token, insight)| LegacyInsight { token, insight })
    .collect::<Vec<_>>();

    let raw_dashboards = collect_rows(
        &connection,
        path,
        "SELECT id,token,name,created_by,created_at,updated_at
         FROM dashboards ORDER BY token,id",
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    )?;
    let tiles = collect_rows(
        &connection,
        path,
        "SELECT id,dashboard_id,insight_id,grid_x,grid_y,grid_w,grid_h
         FROM dashboard_tiles ORDER BY dashboard_id,id",
        |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                DashboardTileInput {
                    insight_id: row.get(2)?,
                    x: row.get(3)?,
                    y: row.get(4)?,
                    w: row.get(5)?,
                    h: row.get(6)?,
                },
            ))
        },
    )?;

    let insight_tokens = insights
        .iter()
        .map(|insight| (insight.insight.id.clone(), insight.token.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut tiles_by_dashboard: BTreeMap<String, Vec<DashboardTileInput>> = BTreeMap::new();
    for (dashboard_id, insight_id, tile) in tiles {
        if !insight_tokens.contains_key(&insight_id) {
            return Err(LegacyResourceImportError::Invalid {
                path: path.to_path_buf(),
                detail: "dashboard tile references a missing insight".into(),
            });
        }
        tiles_by_dashboard
            .entry(dashboard_id)
            .or_default()
            .push(tile);
    }

    let mut dashboard_tokens = BTreeMap::new();
    let mut dashboards = Vec::new();
    for (id, token, name, created_by, created_at, updated_at) in raw_dashboards {
        let dashboard_tiles = tiles_by_dashboard.remove(&id).unwrap_or_default();
        if dashboard_tiles.iter().any(|tile| {
            insight_tokens
                .get(&tile.insight_id)
                .is_some_and(|insight_token| insight_token != &token)
        }) {
            return Err(LegacyResourceImportError::Invalid {
                path: path.to_path_buf(),
                detail: "dashboard tile crosses project-token boundaries".into(),
            });
        }
        dashboard_tokens.insert(id.clone(), token.clone());
        dashboards.push(LegacyDashboard {
            token,
            dashboard: ImportedDashboard {
                id,
                name,
                tiles: dashboard_tiles,
                created_by,
                created_at,
                updated_at,
            },
        });
    }
    if !tiles_by_dashboard.is_empty() {
        return Err(LegacyResourceImportError::Invalid {
            path: path.to_path_buf(),
            detail: "dashboard tile references a missing dashboard".into(),
        });
    }

    let raw_shares = collect_rows(
        &connection,
        path,
        "SELECT id,object_type,object_id,token,created_by,created_at,expires_at
         FROM share_links ORDER BY id",
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        },
    )?;
    let mut shares = Vec::new();
    for (id, object_type, object_id, token, created_by, created_at, expires_at) in raw_shares {
        let (target, project_token) = match object_type.as_str() {
            "insight" => (
                ShareTarget::Insight,
                insight_tokens.get(&object_id).ok_or_else(|| {
                    LegacyResourceImportError::Invalid {
                        path: path.to_path_buf(),
                        detail: "share link references a missing insight".into(),
                    }
                })?,
            ),
            "dashboard" => (
                ShareTarget::Dashboard,
                dashboard_tokens.get(&object_id).ok_or_else(|| {
                    LegacyResourceImportError::Invalid {
                        path: path.to_path_buf(),
                        detail: "share link references a missing dashboard".into(),
                    }
                })?,
            ),
            _ => {
                return Err(LegacyResourceImportError::Invalid {
                    path: path.to_path_buf(),
                    detail: "share link has an unsupported object type".into(),
                });
            }
        };
        shares.push((
            project_token.clone(),
            ImportedShareLink {
                id,
                token,
                target,
                object_id,
                created_by,
                created_at,
                expires_at,
            },
        ));
    }

    let tile_count = dashboards
        .iter()
        .map(|dashboard| dashboard.dashboard.tiles.len())
        .sum();
    Ok(LegacyDashboardData {
        tile_count,
        insights,
        dashboards,
        shares: shares
            .into_iter()
            .map(|(project_token, share)| LegacyShare {
                project_token,
                share,
            })
            .collect(),
    })
}

fn load_project_map(
    control_path: &Path,
) -> Result<BTreeMap<String, String>, LegacyResourceImportError> {
    let connection = Connection::open_with_flags(
        control_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| database_error(control_path, source))?;
    collect_rows(
        &connection,
        control_path,
        "SELECT capture_token,id FROM projects ORDER BY capture_token",
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map(|rows| rows.into_iter().collect())
}

fn project_for_token<'a>(
    projects: &'a BTreeMap<String, String>,
    path: &Path,
    token: &str,
) -> Result<&'a str, LegacyResourceImportError> {
    projects
        .get(token)
        .map(String::as_str)
        .ok_or_else(|| LegacyResourceImportError::Invalid {
            path: path.to_path_buf(),
            detail: "resource capture token has no imported project".into(),
        })
}

fn open_legacy(path: &Path) -> Result<Connection, LegacyResourceImportError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| database_error(path, source))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|source| database_error(path, source))?;
    Ok(connection)
}

fn require_table(
    connection: &Connection,
    path: &Path,
    table: &str,
) -> Result<(), LegacyResourceImportError> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|source| database_error(path, source))?;
    if exists {
        Ok(())
    } else {
        Err(LegacyResourceImportError::Invalid {
            path: path.to_path_buf(),
            detail: format!("required table {table:?} is absent"),
        })
    }
}

fn decode_optional_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    encoded: Option<String>,
) -> Result<Option<T>, LegacyResourceImportError> {
    encoded
        .map(|encoded| {
            serde_json::from_str(&encoded).map_err(|error| LegacyResourceImportError::Invalid {
                path: path.to_path_buf(),
                detail: format!("invalid resource JSON: {error}"),
            })
        })
        .transpose()
}

fn collect_rows<T, F>(
    connection: &Connection,
    path: &Path,
    sql: &str,
    map: F,
) -> Result<Vec<T>, LegacyResourceImportError>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut statement = connection
        .prepare(sql)
        .map_err(|source| database_error(path, source))?;
    statement
        .query_map([], map)
        .map_err(|source| database_error(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| database_error(path, source))
}

fn database_error(path: &Path, source: rusqlite::Error) -> LegacyResourceImportError {
    LegacyResourceImportError::Database {
        path: path.to_path_buf(),
        source,
    }
}

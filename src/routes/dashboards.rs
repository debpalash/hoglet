//! Saved insights + dashboards + share links API.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;

use crate::dashboard_store::{Dashboard, DashboardStore, SavedInsight};

#[derive(Clone)]
pub struct DashState {
    pub store: Arc<DashboardStore>,
}

pub fn router(store: Arc<DashboardStore>) -> Router {
    Router::new()
        .route("/api/insights", get(list_insights).post(save_insight))
        .route(
            "/api/insights/{id}",
            get(get_insight).delete(delete_insight),
        )
        .route("/api/dashboards", get(list_dashboards).post(save_dashboard))
        .route(
            "/api/dashboards/{id}",
            get(get_dashboard).delete(delete_dashboard),
        )
        .route("/api/dashboards/{id}/tiles", put(update_tiles))
        .route("/api/share", post(create_share))
        .route("/api/share/{id}", delete(delete_share))
        .route("/shared/{token}", get(get_shared))
        .with_state(DashState { store })
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

#[derive(Deserialize)]
struct SaveInsightBody {
    token: String,
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    query_ir: serde_json::Value,
}

#[derive(Deserialize)]
struct SaveDashboardBody {
    token: String,
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default)]
    tiles: Vec<crate::dashboard_store::DashboardTile>,
}

#[derive(Deserialize)]
struct CreateShareBody {
    object_type: String,
    object_id: String,
}

async fn list_insights(State(s): State<DashState>, Query(q): Query<TokenQuery>) -> Response {
    match s.store.list_insights(&q.token) {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_insight(State(s): State<DashState>, Path(id): Path<String>) -> Response {
    match s.store.get_insight(&id) {
        Ok(i) => Json(i).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn save_insight(State(s): State<DashState>, Json(body): Json<SaveInsightBody>) -> Response {
    let insight = SavedInsight {
        id: body.id,
        token: body.token.clone(),
        name: body.name,
        description: body.description,
        query_ir: body.query_ir,
        created_by: "user".into(),
        created_at: 0,
        updated_at: 0,
    };
    match s.store.save_insight(&body.token, &insight) {
        Ok(saved) => Json(saved).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn delete_insight(State(s): State<DashState>, Path(id): Path<String>) -> Response {
    match s.store.delete_insight(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_dashboards(State(s): State<DashState>, Query(q): Query<TokenQuery>) -> Response {
    match s.store.list_dashboards(&q.token) {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_dashboard(State(s): State<DashState>, Path(id): Path<String>) -> Response {
    match s.store.get_dashboard(&id) {
        Ok(d) => Json(d).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn save_dashboard(
    State(s): State<DashState>,
    Json(body): Json<SaveDashboardBody>,
) -> Response {
    let dash = Dashboard {
        id: body.id,
        token: body.token.clone(),
        name: body.name,
        tiles: body.tiles,
        created_by: "user".into(),
        created_at: 0,
    };
    match s.store.save_dashboard(&body.token, &dash) {
        Ok(saved) => Json(saved).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn delete_dashboard(State(s): State<DashState>, Path(id): Path<String>) -> Response {
    match s.store.delete_dashboard(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn update_tiles(
    State(s): State<DashState>,
    Path(id): Path<String>,
    Json(tiles): Json<Vec<crate::dashboard_store::DashboardTile>>,
) -> Response {
    let mut dash = match s.store.get_dashboard(&id) {
        Ok(d) => d,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    dash.tiles = tiles;
    match s.store.save_dashboard(&dash.token.clone(), &dash) {
        Ok(saved) => Json(saved).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn create_share(State(s): State<DashState>, Json(body): Json<CreateShareBody>) -> Response {
    match s
        .store
        .create_share(&body.object_type, &body.object_id, "user")
    {
        Ok(link) => Json(serde_json::json!({
            "id": link.id,
            "url": format!("/shared/{}", link.token),
        }))
        .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn delete_share(State(s): State<DashState>, Path(id): Path<String>) -> Response {
    let _ = s.store.delete_share(&id);
    StatusCode::NO_CONTENT.into_response()
}

async fn get_shared(State(s): State<DashState>, Path(token): Path<String>) -> Response {
    match s.store.get_shared(&token) {
        Ok(shared) => {
            let mut html = String::from(
                "<!DOCTYPE html><html><head><title>Hoglet — Shared</title><meta charset=utf-8><style>body{font-family:system-ui,sans-serif;max-width:960px;margin:40px auto;padding:0 20px;background:#0f0f12;color:#e0e0e0}h1{color:#f47e3e}.tile{background:#1a1a22;border-radius:8px;padding:16px;margin:12px 0}.tile h3{margin:0 0 8px}.tile .val{font-size:24px;font-weight:700}</style></head><body><h1>Hoglet</h1>",
            );
            if let Some(dash) = &shared.dashboard {
                html.push_str(&format!("<h2>{}</h2>", dash.name));
                for tile in &dash.tiles {
                    if let Some(insight) = &tile.insight {
                        html.push_str(&format!(
                            "<div class=tile><h3>{}</h3><div class=val>—</div></div>",
                            insight.name
                        ));
                    }
                }
            } else if let Some(insight) = &shared.insight {
                html.push_str(&format!("<h2>{}</h2><div class=val>—</div>", insight.name));
            }
            html.push_str("</body></html>");
            axum::http::Response::builder()
                .header("content-type", "text/html; charset=utf-8")
                .body(axum::body::Body::from(html))
                .unwrap()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

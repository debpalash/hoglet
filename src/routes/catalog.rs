//! Catalog autocomplete API — event names, property keys, property values.
//! Serves the dashboard's event-picker and filter-builder dropdowns.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;

use crate::catalog::CatalogStore;

#[derive(Clone)]
pub struct CatalogState {
    pub catalog: Arc<CatalogStore>,
}

pub fn router(catalog: Arc<CatalogStore>) -> Router {
    Router::new()
        .route("/api/catalog/events", get(events))
        .route("/api/catalog/properties", get(properties))
        .route("/api/catalog/values", get(values))
        .with_state(CatalogState { catalog })
}

#[derive(Deserialize)]
struct CatalogQuery {
    token: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Deserialize)]
struct PropertiesQuery {
    token: String,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "event".to_string()
}

#[derive(Deserialize)]
struct ValuesQuery {
    token: String,
    key: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn events(
    State(s): State<CatalogState>,
    Query(q): Query<CatalogQuery>,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        if q.prefix.is_empty() {
            s.catalog.all_event_names(&q.token)
        } else {
            s.catalog.event_names(&q.token, &q.prefix, q.limit.min(100))
        }
    })
    .await
    {
        Ok(Ok(names)) => Json(names).into_response(),
        Ok(Err(_)) | Err(_) => {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn properties(
    State(s): State<CatalogState>,
    Query(q): Query<PropertiesQuery>,
) -> Response {
    match tokio::task::spawn_blocking(move || s.catalog.property_keys(&q.token, &q.source)).await {
        Ok(Ok(keys)) => Json(keys).into_response(),
        Ok(Err(_)) | Err(_) => {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn values(
    State(s): State<CatalogState>,
    Query(q): Query<ValuesQuery>,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        s.catalog
            .property_values(&q.token, &q.key, &q.prefix, q.limit.min(200))
    })
    .await
    {
        Ok(Ok(vals)) => Json(vals).into_response(),
        Ok(Err(_)) | Err(_) => {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn catalog_events_endpoint() {
        let catalog = CatalogStore::open_in_memory().unwrap();
        use crate::capture::event::CapturedEvent;
        use chrono::Utc;
        use uuid::Uuid;

        catalog
            .ingest(
                &[CapturedEvent {
                    uuid: Uuid::new_v4(),
                    event: "pageview".into(),
                    distinct_id: "u1".into(),
                    token: "phc_t".into(),
                    timestamp: Utc::now(),
                    properties: serde_json::Map::new(),
                }],
                "phc_t",
            )
            .unwrap();

        let app = router(Arc::new(catalog));

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/catalog/events?token=phc_t&prefix=page")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let names: Vec<String> = serde_json::from_slice(&body_bytes).unwrap();
        assert!(names.contains(&"pageview".to_string()));
    }

    #[tokio::test]
    async fn catalog_properties_endpoint() {
        let catalog = CatalogStore::open_in_memory().unwrap();
        use crate::capture::event::CapturedEvent;
        use chrono::Utc;
        use uuid::Uuid;

        catalog
            .ingest(
                &[CapturedEvent {
                    uuid: Uuid::new_v4(),
                    event: "pageview".into(),
                    distinct_id: "u1".into(),
                    token: "phc_t".into(),
                    timestamp: Utc::now(),
                    properties: {
                        let mut m = serde_json::Map::new();
                        m.insert("browser".into(), json!("Chrome"));
                        m
                    },
                }],
                "phc_t",
            )
            .unwrap();

        let app = router(Arc::new(catalog));

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/catalog/properties?token=phc_t&source=event")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let keys: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes).unwrap();
        let browser = keys.iter().find(|k| k["key"] == "browser").unwrap();
        assert_eq!(browser["type_guess"], "string");
    }
}

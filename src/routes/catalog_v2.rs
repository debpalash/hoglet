//! Authenticated catalog autocomplete over ordered projection state.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::control::{AccessError, ProjectAccess};
use crate::projection_catalog::{ProjectionCatalog, ProjectionCatalogError};

#[derive(Clone)]
struct CatalogState {
    access: Arc<ProjectAccess>,
    catalog: Arc<ProjectionCatalog>,
}

#[derive(Clone)]
struct RequestId(String);

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    prefix: String,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct PropertiesQuery {
    #[serde(default = "default_source")]
    source: String,
}

#[derive(Debug, Deserialize)]
struct ValuesQuery {
    key: String,
    #[serde(default)]
    prefix: String,
    #[serde(default = "default_value_limit")]
    limit: usize,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'static str>,
    request_id: String,
}

pub fn router(access: Arc<ProjectAccess>, catalog: Arc<ProjectionCatalog>) -> Router {
    Router::new()
        .route("/api/projects/{project_id}/catalog/events", get(events))
        .route(
            "/api/projects/{project_id}/catalog/properties",
            get(properties),
        )
        .route("/api/projects/{project_id}/catalog/values", get(values))
        .with_state(CatalogState { access, catalog })
        .layer(middleware::from_fn(assign_request_id))
}

async fn assign_request_id(mut request: Request, next: Next) -> Response {
    let request_id = RequestId(Uuid::now_v7().to_string());
    request.extensions_mut().insert(request_id.clone());
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

async fn authorize(
    state: &CatalogState,
    headers: &HeaderMap,
    project_id: &str,
    request_id: &RequestId,
) -> Result<String, Response> {
    let principal = crate::routes::workspace::authenticate(&state.access, headers)
        .await
        .map_err(|error| access_error(error, request_id))?;
    if Uuid::parse_str(project_id).is_err() {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
            None,
            request_id,
        ));
    }
    state
        .access
        .authorize_project(&principal, project_id)
        .await
        .map(|project| project.project_id)
        .map_err(|error| access_error(error, request_id))
}

async fn events(
    State(state): State<CatalogState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let project_id = match authorize(&state, &headers, &project_id, &request_id).await {
        Ok(project_id) => project_id,
        Err(response) => return response,
    };
    let Query(query) = match Query::<EventsQuery>::try_from_uri(&uri) {
        Ok(query) => query,
        Err(_) => return invalid_request(None, &request_id),
    };
    let catalog = state.catalog;
    catalog_result(
        tokio::task::spawn_blocking(move || {
            catalog.event_names(&project_id, &query.prefix, query.limit)
        })
        .await,
        &request_id,
    )
}

async fn properties(
    State(state): State<CatalogState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let project_id = match authorize(&state, &headers, &project_id, &request_id).await {
        Ok(project_id) => project_id,
        Err(response) => return response,
    };
    let Query(query) = match Query::<PropertiesQuery>::try_from_uri(&uri) {
        Ok(query) => query,
        Err(_) => return invalid_request(None, &request_id),
    };
    let catalog = state.catalog;
    catalog_result(
        tokio::task::spawn_blocking(move || catalog.property_keys(&project_id, &query.source))
            .await,
        &request_id,
    )
}

async fn values(
    State(state): State<CatalogState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let project_id = match authorize(&state, &headers, &project_id, &request_id).await {
        Ok(project_id) => project_id,
        Err(response) => return response,
    };
    let Query(query) = match Query::<ValuesQuery>::try_from_uri(&uri) {
        Ok(query) if !query.key.trim().is_empty() => query,
        Ok(_) => return invalid_request(Some("key"), &request_id),
        Err(_) => return invalid_request(None, &request_id),
    };
    let catalog = state.catalog;
    catalog_result(
        tokio::task::spawn_blocking(move || {
            catalog.property_values(&project_id, &query.key, &query.prefix, query.limit)
        })
        .await,
        &request_id,
    )
}

fn catalog_result<T: Serialize>(
    result: Result<Result<T, ProjectionCatalogError>, tokio::task::JoinError>,
    request_id: &RequestId,
) -> Response {
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(ProjectionCatalogError::InvalidSource)) => {
            invalid_request(Some("source"), request_id)
        }
        Ok(Err(_)) | Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The catalog could not be loaded.",
            None,
            request_id,
        ),
    }
}

fn default_event_limit() -> usize {
    200
}

fn default_value_limit() -> usize {
    50
}

fn default_source() -> String {
    "event".into()
}

fn invalid_request(field: Option<&'static str>, request_id: &RequestId) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "The request is invalid.",
        field,
        request_id,
    )
}

fn access_error(error: AccessError, request_id: &RequestId) -> Response {
    let (status, code, message) = match error {
        AccessError::InvalidCredentials | AccessError::InvalidToken | AccessError::Unauthorized => {
            (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication is required.",
            )
        }
        AccessError::Forbidden => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "You do not have access to this project.",
        ),
        AccessError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
        ),
        AccessError::InvalidRequest => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "The request is invalid.",
        ),
        AccessError::SetupComplete => (
            StatusCode::CONFLICT,
            "conflict",
            "The request conflicts with the current state.",
        ),
        AccessError::Unavailable
        | AccessError::InvalidStorage
        | AccessError::Incompatible { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "The service is temporarily unavailable.",
        ),
        AccessError::Database(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An internal error occurred.",
        ),
    };
    error_response(status, code, message, None, request_id)
}

fn error_response(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    field: Option<&'static str>,
    request_id: &RequestId,
) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody {
                code,
                message,
                field,
                request_id: request_id.0.clone(),
            },
        }),
    )
        .into_response()
}

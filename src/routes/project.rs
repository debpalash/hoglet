//! Authenticated, project-scoped dashboard analytics routes.
//!
//! The path project is authorized before the request body is read. Handlers
//! receive the project's capture token from [`ProjectAccess`]; dashboard
//! clients never select Event Truth by supplying a token themselves.
//!
//! Catalog, flag-definition, insight, and dashboard routes remain absent here:
//! their legacy stores are token-keyed and some object lookups are not scoped
//! by project. They must gain project-aware store interfaces before they can be
//! exposed through this authenticated surface.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    control::{AccessError, ProjectAccess},
    query::{
        QueryEngine, QueryError, ir,
        supported::{SupportedQuery, UnsupportedQuery},
    },
};

const MAX_QUERY_BODY_BYTES: usize = 1024 * 1024;
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone)]
struct ProjectState {
    access: Arc<ProjectAccess>,
    engine: Arc<QueryEngine>,
}

#[derive(Clone)]
struct RequestId(String);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    query: ir::Query,
    #[serde(default)]
    refresh: bool,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    request_id: String,
}

/// Builds the authenticated dashboard surface for project analytics.
pub fn router(access: Arc<ProjectAccess>, engine: Arc<QueryEngine>) -> Router {
    Router::new()
        .route("/api/projects/{project_id}/query", post(query))
        .with_state(ProjectState { access, engine })
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

async fn query(
    State(state): State<ProjectState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    // Authentication and project authorization intentionally precede reading
    // the body. An unauthenticated or cross-project caller cannot use parser or
    // semantic-validation responses as an oracle.
    let principal =
        match crate::routes::workspace::authenticate(&state.access, request.headers()).await {
            Ok(principal) => principal,
            Err(error) => return access_error(error, &request_id),
        };
    if Uuid::parse_str(&project_id).is_err() {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
            None,
            &request_id,
        );
    }
    let project = match state
        .access
        .authorize_project(&principal, &project_id)
        .await
    {
        Ok(project) => project,
        Err(error) => return access_error(error, &request_id),
    };

    let body = match to_bytes(request.into_body(), MAX_QUERY_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request body is invalid or too large.",
                None,
                &request_id,
            );
        }
    };
    let body: QueryRequest = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request body is invalid.",
                None,
                &request_id,
            );
        }
    };
    let supported = match SupportedQuery::try_from(body.query) {
        Ok(query) => query,
        Err(error) => return unsupported_query(error, &request_id),
    };

    let admission = match state.engine.admit().await {
        Ok(admission) => admission,
        Err(_) => return query_busy(&request_id),
    };
    let engine = state.engine.clone();
    let project_id = project.project_id;
    let capture_token = project.capture_token;
    let refresh = body.refresh;
    let mut task = tokio::task::spawn_blocking(move || {
        // A timed-out query keeps the only execution permit until DuckDB
        // actually returns, so another query cannot overlap it invisibly.
        let _admission = admission;
        engine.run_supported_for_project(&supported, &project_id, &capture_token, refresh)
    });
    match tokio::time::timeout(QUERY_TIMEOUT, &mut task).await {
        Err(_) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "query_timeout",
            "The query exceeded its execution deadline.",
            None,
            &request_id,
        ),
        Ok(result) => match result {
            Ok(Ok(response)) => Json(response).into_response(),
            Ok(Err(QueryError::TooManySteps | QueryError::Compile(_))) => error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_query",
                "The query could not be executed with the supported semantics.",
                Some("query".into()),
                &request_id,
            ),
            Ok(Err(
                QueryError::Db(_)
                | QueryError::EventLake(_)
                | QueryError::VersionedSourceRequired
                | QueryError::InvalidProjectRange,
            ))
            | Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The query could not be completed.",
                None,
                &request_id,
            ),
        },
    }
}

fn query_busy(request_id: &RequestId) -> Response {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "query_busy",
        "The bounded query queue is full.",
        None,
        request_id,
    );
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("1"),
    );
    response
}

fn unsupported_query(error: UnsupportedQuery, request_id: &RequestId) -> Response {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        error.code,
        &error.message,
        Some(format!("query.{}", error.field)),
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
    message: &str,
    field: Option<String>,
    request_id: &RequestId,
) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody {
                code,
                message: message.into(),
                field,
                request_id: request_id.0.clone(),
            },
        }),
    )
        .into_response()
}

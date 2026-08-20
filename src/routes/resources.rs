//! Authenticated, project-scoped control-resource routes.
//!
//! Every handler authenticates and authorizes the path project before reading a
//! request body or looking up an object id.  Capture tokens never participate in
//! dashboard resource selection.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
};
use chrono::Utc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{
    control::{AccessError, Authentication, AuthorizedProject, ProjectAccess, Role},
    control_resources::{
        ControlResourceError, ControlResources, Dashboard, DashboardDraft, DashboardTileInput,
        FeatureFlag, InsightDraft, SavedInsight, ShareLink, ShareTarget,
    },
};

const MAX_RESOURCE_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct ResourceState {
    access: Arc<ProjectAccess>,
    resources: Arc<ControlResources>,
}

#[derive(Clone)]
struct RequestId(String);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareDraft {
    object_type: ShareTarget,
    object_id: String,
    #[serde(default)]
    expires_at: Option<i64>,
}

#[derive(Serialize)]
struct PublicShare {
    share: ShareLink,
    #[serde(skip_serializing_if = "Option::is_none")]
    insight: Option<SavedInsight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dashboard: Option<Dashboard>,
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

/// Builds the authenticated control-resource API and the capability-token
/// public share resolver.
pub fn router(access: Arc<ProjectAccess>, resources: Arc<ControlResources>) -> Router {
    Router::new()
        .route(
            "/api/projects/{project_id}/flags",
            get(list_flags).post(create_flag),
        )
        .route(
            "/api/projects/{project_id}/flags/{key}",
            put(update_flag).delete(delete_flag),
        )
        .route(
            "/api/projects/{project_id}/insights",
            get(list_insights).post(create_insight),
        )
        .route(
            "/api/projects/{project_id}/insights/{insight_id}",
            get(get_insight).put(update_insight).delete(delete_insight),
        )
        .route(
            "/api/projects/{project_id}/dashboards",
            get(list_dashboards).post(create_dashboard),
        )
        .route(
            "/api/projects/{project_id}/dashboards/{dashboard_id}",
            get(get_dashboard)
                .put(update_dashboard)
                .delete(delete_dashboard),
        )
        .route(
            "/api/projects/{project_id}/dashboards/{dashboard_id}/tiles",
            put(replace_dashboard_tiles),
        )
        .route(
            "/api/projects/{project_id}/shares",
            get(list_shares).post(create_share),
        )
        .route(
            "/api/projects/{project_id}/shares/{share_id}",
            delete(delete_share),
        )
        .route("/shared/{token}", get(resolve_public_share))
        .route("/api/shares/{token}", get(resolve_public_share))
        .with_state(ResourceState { access, resources })
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
    state: &ResourceState,
    headers: &HeaderMap,
    project_id: &str,
    mutation: bool,
    request_id: &RequestId,
) -> Result<AuthorizedProject, Response> {
    // Do not disclose whether a path project exists to an anonymous caller.
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
    let project = state
        .access
        .authorize_project(&principal, project_id)
        .await
        .map_err(|error| access_error(error, request_id))?;
    if mutation
        && (project.principal.authentication != Authentication::Session
            || !matches!(project.role, Role::Owner | Role::Admin))
    {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "forbidden",
            "A project owner or administrator session is required.",
            None,
            request_id,
        ));
    }
    Ok(project)
}

async fn parse_body<T: DeserializeOwned>(
    request: Request,
    request_id: &RequestId,
) -> Result<T, Response> {
    let bytes = to_bytes(request.into_body(), MAX_RESOURCE_BODY_BYTES)
        .await
        .map_err(|_| invalid_request(request_id))?;
    serde_json::from_slice(&bytes).map_err(|_| invalid_request(request_id))
}

async fn invoke<T, F>(operation: F) -> Result<T, ControlResourceError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ControlResourceError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| ControlResourceError::Unavailable)?
}

async fn read_authorized<T, F>(
    state: ResourceState,
    request_id: RequestId,
    project_id: String,
    headers: HeaderMap,
    operation: F,
) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce(Arc<ControlResources>, String) -> Result<T, ControlResourceError> + Send + 'static,
{
    if let Err(response) = authorize(&state, &headers, &project_id, false, &request_id).await {
        return response;
    }
    let resources = state.resources;
    match invoke(move || operation(resources, project_id)).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn list_flags(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_authorized(
        state,
        request_id,
        project_id,
        headers,
        |resources, project| resources.list_flags(&project),
    )
    .await
}

async fn create_flag(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let flag: FeatureFlag = match parse_body(request, &request_id).await {
        Ok(flag) => flag,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.create_flag(&project.project_id, &flag)).await {
        Ok(flag) => (StatusCode::CREATED, Json(flag)).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn update_flag(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, key)): Path<(String, String)>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let flag: FeatureFlag = match parse_body(request, &request_id).await {
        Ok(flag) => flag,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.update_flag(&project.project_id, &key, &flag)).await {
        Ok(flag) => Json(flag).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn delete_flag(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let project = match authorize(&state, &headers, &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.delete_flag(&project.project_id, &key)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn list_insights(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_authorized(
        state,
        request_id,
        project_id,
        headers,
        |resources, project| resources.list_insights(&project),
    )
    .await
}

async fn get_insight(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, insight_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &headers, &project_id, false, &request_id).await {
        return response;
    }
    let resources = state.resources;
    match invoke(move || resources.get_insight(&project_id, &insight_id)).await {
        Ok(insight) => Json(insight).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn create_insight(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let draft: InsightDraft = match parse_body(request, &request_id).await {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let created_by = project.principal.user_id;
    let resources = state.resources;
    match invoke(move || resources.create_insight(&project.project_id, &created_by, &draft)).await {
        Ok(insight) => (StatusCode::CREATED, Json(insight)).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn update_insight(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, insight_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let draft: InsightDraft = match parse_body(request, &request_id).await {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.update_insight(&project.project_id, &insight_id, &draft)).await {
        Ok(insight) => Json(insight).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn delete_insight(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, insight_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let project = match authorize(&state, &headers, &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.delete_insight(&project.project_id, &insight_id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn list_dashboards(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_authorized(
        state,
        request_id,
        project_id,
        headers,
        |resources, project| resources.list_dashboards(&project),
    )
    .await
}

async fn get_dashboard(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, dashboard_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &headers, &project_id, false, &request_id).await {
        return response;
    }
    let resources = state.resources;
    match invoke(move || resources.get_dashboard(&project_id, &dashboard_id)).await {
        Ok(dashboard) => Json(dashboard).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn create_dashboard(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let draft: DashboardDraft = match parse_body(request, &request_id).await {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let created_by = project.principal.user_id;
    let resources = state.resources;
    match invoke(move || resources.create_dashboard(&project.project_id, &created_by, &draft)).await
    {
        Ok(dashboard) => (StatusCode::CREATED, Json(dashboard)).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn update_dashboard(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, dashboard_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let draft: DashboardDraft = match parse_body(request, &request_id).await {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.update_dashboard(&project.project_id, &dashboard_id, &draft))
        .await
    {
        Ok(dashboard) => Json(dashboard).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn replace_dashboard_tiles(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, dashboard_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let tiles: Vec<DashboardTileInput> = match parse_body(request, &request_id).await {
        Ok(tiles) => tiles,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || {
        resources.replace_dashboard_tiles(&project.project_id, &dashboard_id, &tiles)
    })
    .await
    {
        Ok(dashboard) => Json(dashboard).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn delete_dashboard(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, dashboard_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let project = match authorize(&state, &headers, &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.delete_dashboard(&project.project_id, &dashboard_id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn list_shares(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_authorized(
        state,
        request_id,
        project_id,
        headers,
        |resources, project| resources.list_shares(&project),
    )
    .await
}

async fn create_share(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let project = match authorize(&state, request.headers(), &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let draft: ShareDraft = match parse_body(request, &request_id).await {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    let created_by = project.principal.user_id;
    let resources = state.resources;
    match invoke(move || {
        resources.create_share(
            &project.project_id,
            draft.object_type,
            &draft.object_id,
            &created_by,
            draft.expires_at,
        )
    })
    .await
    {
        Ok(share) => (StatusCode::CREATED, Json(share)).into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn delete_share(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path((project_id, share_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let project = match authorize(&state, &headers, &project_id, true, &request_id).await {
        Ok(project) => project,
        Err(response) => return response,
    };
    let resources = state.resources;
    match invoke(move || resources.delete_share(&project.project_id, &share_id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => resource_error(error, &request_id),
    }
}

async fn resolve_public_share(
    State(state): State<ResourceState>,
    Extension(request_id): Extension<RequestId>,
    Path(token): Path<String>,
) -> Response {
    // The random share token is the capability.  Resolution returns only the
    // object already bound to that token and loads it through scoped methods.
    let resources = state.resources;
    match invoke(move || {
        let share = resources.authorize_share_token(&token, Utc::now().timestamp())?;
        let (insight, dashboard) = match share.object_type {
            ShareTarget::Insight => (
                Some(resources.get_insight(&share.project_id, &share.object_id)?),
                None,
            ),
            ShareTarget::Dashboard => (
                None,
                Some(resources.get_dashboard(&share.project_id, &share.object_id)?),
            ),
        };
        Ok(PublicShare {
            share,
            insight,
            dashboard,
        })
    })
    .await
    {
        Ok(shared) => Json(shared).into_response(),
        Err(ControlResourceError::NotFound) => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "The shared resource was not found.",
            None,
            &request_id,
        ),
        Err(error) => resource_error(error, &request_id),
    }
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

fn resource_error(error: ControlResourceError, request_id: &RequestId) -> Response {
    match error {
        ControlResourceError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found.",
            None,
            request_id,
        ),
        ControlResourceError::Conflict => error_response(
            StatusCode::CONFLICT,
            "conflict",
            "The request conflicts with an existing resource.",
            None,
            request_id,
        ),
        ControlResourceError::InvalidResource { field, message } => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_resource",
            &message,
            Some(field.into()),
            request_id,
        ),
        ControlResourceError::InvalidQuery { field, message } => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_query",
            &message,
            Some(format!("query_ir.{field}")),
            request_id,
        ),
        ControlResourceError::Unavailable | ControlResourceError::InvalidStorage => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "The service is temporarily unavailable.",
            None,
            request_id,
        ),
        ControlResourceError::Database(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "An internal error occurred.",
            None,
            request_id,
        ),
    }
}

fn invalid_request(request_id: &RequestId) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "The request body is invalid or too large.",
        None,
        request_id,
    )
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

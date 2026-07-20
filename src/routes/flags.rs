//! `/flags` and `/decide` — one handler, `?v=` selects the response shape
//! (compat-spec.md "Flags response shapes").
//!
//! Flag *definitions* don't exist yet; every shape resolves to an empty flag
//! set, which the SDK handles gracefully. The wire shapes are the contract
//! being implemented here — real evaluation slots in behind them.

use axum::{
    Json, Router,
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};

use crate::capture::decompress;
use crate::token;

#[derive(serde::Deserialize, Default)]
pub struct FlagsQuery {
    pub v: Option<String>,
    pub compression: Option<String>,
}

pub fn router() -> Router {
    Router::new()
        .route("/flags", post(flags))
        .route("/flags/", post(flags))
        .route("/decide", post(flags))
        .route("/decide/", post(flags))
}

async fn flags(
    Query(query): Query<FlagsQuery>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let form_encoded = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));

    let Ok(text) = decompress::decode(&body, form_encoded, query.compression.as_deref()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Flags bodies are json5 in the wild: NaN/Infinity appear (mapped to
    // null by the json5 parser) and Android clients send lossy UTF-8 —
    // already replaced during decode.
    let Ok(request): Result<Value, _> = json5::from_str(&text) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let Some(raw_token) = request
        .get("token")
        .or_else(|| request.get("api_key"))
        .and_then(Value::as_str)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if token::validate(raw_token).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let request_id = uuid::Uuid::new_v4().to_string();

    // Shape table from compat-spec.md. Current posthog-js: /flags/?v=2 for
    // the value map, plain /flags for FlagDetails.
    let body = match query.v.as_deref() {
        Some("1") => json!({
            "feature_flags": [],
            "errors_while_computing_flags": false,
            "request_id": request_id,
        }),
        Some("2") => json!({
            "feature_flags": {},
            "feature_flag_payloads": {},
            "errors_while_computing_flags": false,
            "request_id": request_id,
        }),
        Some(_) | None => json!({
            "flags": {},
            "errors_while_computing_flags": false,
            "request_id": request_id,
        }),
    };
    Json(body).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn post(uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let res = super::router()
            .oneshot(Request::post(uri).body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    const BODY: &str = r#"{"token":"phc_t","distinct_id":"u1"}"#;

    #[tokio::test]
    async fn v2_shape_is_value_map() {
        let (status, body) = post("/flags/?v=2", BODY).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["feature_flags"].is_object());
        assert!(body["feature_flag_payloads"].is_object());
        assert_eq!(body["errors_while_computing_flags"], false);
        assert!(body["request_id"].is_string());
    }

    #[tokio::test]
    async fn v1_shape_is_key_array() {
        let (_, body) = post("/decide/?v=1", BODY).await;
        assert!(body["feature_flags"].is_array());
    }

    #[tokio::test]
    async fn default_shape_is_flag_details() {
        let (_, body) = post("/flags/", BODY).await;
        assert!(body["flags"].is_object());
    }

    #[tokio::test]
    async fn decide_alias_serves_same_handler() {
        let (status, body) = post("/decide/?v=2", BODY).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["feature_flags"].is_object());
    }

    #[tokio::test]
    async fn json5_nan_tolerated() {
        let (status, _) = post("/flags/?v=2", r#"{"token":"phc_t","distinct_id":"u1","x":NaN}"#).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let (status, _) = post("/flags/?v=2", r#"{"distinct_id":"u1"}"#).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

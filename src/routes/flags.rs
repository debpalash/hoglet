//! `/flags` and `/decide` — one handler, `?v=` selects the response shape
//! (spec/wire-compat.md "Flags response shapes").
//!
//! This module owns the wire *shapes*; `crate::flags` owns *which flags are on
//! for whom*. Evaluations come from the flag store, keyed by token, bucketed
//! per `distinct_id` with PostHog's exact hash.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Map, Value, json};

use crate::flags::FlagStore;
use crate::identity::IdentityStore;
use crate::token;

#[derive(serde::Deserialize, Default)]
pub struct FlagsQuery {
    pub v: Option<String>,
    pub compression: Option<String>,
}

#[derive(Clone)]
pub struct FlagsState {
    pub store: Arc<FlagStore>,
    pub identity: Option<Arc<IdentityStore>>,
}

pub fn router(store: Arc<FlagStore>, identity: Arc<IdentityStore>) -> Router {
    Router::new()
        .route("/flags", post(flags))
        .route("/flags/", post(flags))
        .route("/decide", post(flags))
        .route("/decide/", post(flags))
        .route("/api/flags", get(list_flags))
        .route("/flags/definitions", get(local_eval_definitions))
        .with_state(FlagsState { store, identity: Some(identity) })
    }

#[derive(serde::Deserialize)]
struct ListQuery {
    token: String,
}

async fn list_flags(
    State(state): State<FlagsState>,
    Query(q): Query<ListQuery>,
) -> Response {
    Json(state.store.list(&q.token)).into_response()
}

async fn flags(
    State(state): State<FlagsState>,
    Query(query): Query<FlagsQuery>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let form_encoded = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));

    let Ok(text) = crate::capture::decompress::decode(&body, form_encoded, query.compression.as_deref())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Flags bodies are json5 in the wild (NaN/Infinity; lossy UTF-8 already
    // replaced during decode).
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

    let distinct_id = request
        .get("distinct_id")
        .and_then(Value::as_str)
        .unwrap_or("");

    // PostHog local-eval model: conditions match against person_properties the
    // SDK passes on the request.
    let person_properties = request
        .get("person_properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let first_seen_key = state.identity.as_ref()
        .and_then(|id| id.first_seen_key_for(raw_token, distinct_id));
    let evaluated = state.store.evaluate(
        raw_token, distinct_id, &person_properties, &|_, _| true, first_seen_key.as_deref()
    );
    let request_id = uuid::Uuid::new_v4().to_string();

    // A flag's value is its variant string when multivariate, else its bool.
    let flag_value = |f: &crate::flags::EvaluatedFlag| -> Value {
        match &f.variant {
            Some(v) if f.enabled => Value::String(v.clone()),
            _ => Value::Bool(f.enabled),
        }
    };

    let body = match query.v.as_deref() {
        // v1: array of enabled flag keys.
        Some("1") => {
            let keys: Vec<&str> = evaluated
                .iter()
                .filter(|f| f.enabled)
                .map(|f| f.key.as_str())
                .collect();
            json!({
                "feature_flags": keys,
                "errors_while_computing_flags": false,
                "request_id": request_id,
            })
        }
        // v2: {key: bool|variant} map + payloads.
        Some("2") => {
            let mut map = Map::new();
            for f in &evaluated {
                map.insert(f.key.clone(), flag_value(f));
            }
            json!({
                "feature_flags": map,
                "feature_flag_payloads": {},
                "errors_while_computing_flags": false,
                "request_id": request_id,
            })
        }
        // default /flags: {key: FlagDetails}.
        Some(_) | None => {
            let mut map = Map::new();
            for f in &evaluated {
                map.insert(
                    f.key.clone(),
                    json!({
                        "key": f.key,
                        "enabled": f.enabled,
                        "variant": f.variant.clone().map(Value::String).unwrap_or(Value::Null),
                        "reason": {
                            "code": if f.enabled { "condition_match" } else { "no_condition_match" },
                            "condition_index": 0,
                            "description": "rollout percentage",
                        },
                        "metadata": {
                            "id": 0,
                            "version": 1,
                            "description": Value::Null,
                            "payload": Value::Null,
                        },
                    }),
                );
            }
            json!({
                "flags": map,
                "errors_while_computing_flags": false,
                "request_id": request_id,
            })
        }
    };
    Json(body).into_response()
}

async fn local_eval_definitions(
    State(state): State<FlagsState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let defs = state.store.list(&q.token);
    Json(serde_json::json!({
        "flags": defs.into_iter().map(|d: crate::flags::FlagDef| serde_json::json!({
            "key": d.key,
            "enabled": d.active,
            "variants": d.variants.iter().map(|v| serde_json::json!({
                "key": v.key,
                "rollout_percentage": v.rollout,
            })).collect::<Vec<_>>(),
            "filters": { "groups": serde_json::json!([]) },
            "rollout_percentage": d.rollout_percentage,
            "payload": d.payload,
        })).collect::<Vec<_>>(),
    })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        let store = Arc::new(FlagStore::in_memory().unwrap());
        store.upsert("phc_t", "new-ui", true, 100.0).unwrap();
        store.upsert("phc_t", "beta", true, 0.0).unwrap();
        router(store, std::sync::Arc::new(crate::identity::IdentityStore::in_memory().unwrap()))
    }

    async fn post(app: Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let res = app
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
    async fn v2_evaluates_flags() {
        let (status, body) = post(app(), "/flags/?v=2", BODY).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["feature_flags"]["new-ui"], true);
        assert_eq!(body["feature_flags"]["beta"], false);
    }

    #[tokio::test]
    async fn v1_lists_only_enabled() {
        let (_, body) = post(app(), "/decide/?v=1", BODY).await;
        let arr = body["feature_flags"].as_array().unwrap();
        assert!(arr.iter().any(|v| v == "new-ui"));
        assert!(!arr.iter().any(|v| v == "beta"));
    }

    #[tokio::test]
    async fn default_shape_has_flag_details() {
        let (_, body) = post(app(), "/flags/", BODY).await;
        assert_eq!(body["flags"]["new-ui"]["enabled"], true);
        assert_eq!(body["flags"]["new-ui"]["variant"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn empty_store_returns_empty_not_error() {
        let store = Arc::new(FlagStore::in_memory().unwrap());
        let (status, body) = post(router(store, std::sync::Arc::new(crate::identity::IdentityStore::in_memory().unwrap())), "/flags/?v=2", BODY).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["feature_flags"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_token_is_401() {
        let (status, _) = post(app(), "/flags/?v=2", r#"{"distinct_id":"u1"}"#).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

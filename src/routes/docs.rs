//! API documentation: an OpenAPI 3.1 spec at `/openapi.json` and the Scalar
//! reference UI at `/docs`.
//!
//! The Scalar bundle is vendored (`web/vendor/scalar.standalone.js`) and
//! embedded, so the docs page works offline with no CDN — consistent with the
//! single-binary promise. The spec is built in Rust so it stays close to the
//! routes it documents.

use axum::{
    Router,
    http::header,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
};
use serde_json::{Map, Value, json};

const SCALAR_JS: &[u8] = include_bytes!("../../web/vendor/scalar.standalone.js");

pub fn router() -> Router {
    Router::new()
        .route("/openapi.json", get(|| async { Json(spec()) }))
        .route("/docs", get(docs_page))
        .route("/docs/scalar.js", get(scalar_js))
}

async fn scalar_js() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], SCALAR_JS).into_response()
}

async fn docs_page() -> Html<&'static str> {
    // Hoglet theme: the dashboard's palette (web/src/styles.css) mapped onto
    // Scalar's CSS variables. theme:'none' so ours is the only skin.
    Html(
        r##"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Hoglet API</title>
  <link rel="icon" href="data:image/svg+xml,<svg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 100 100%22><text y=%22.9em%22 font-size=%2290%22>🦔</text></svg>" />
  <style>
    :root, .dark-mode, .light-mode {
      /* surfaces */
      --scalar-background-1: #0b0d10;
      --scalar-background-2: #14181d;
      --scalar-background-3: #1a1f26;
      --scalar-background-accent: #ffb45414;
      /* text */
      --scalar-color-1: #e6edf3;
      --scalar-color-2: #8b98a5;
      --scalar-color-3: #6b7681;
      --scalar-color-accent: #ffb454;
      /* lines & controls */
      --scalar-border-color: #232a31;
      --scalar-button-1: #ffb454;
      --scalar-button-1-color: #0b0d10;
      --scalar-button-1-hover: #ffc678;
      /* semantic */
      --scalar-color-green: #3fb950;
      --scalar-color-red: #f47067;
      --scalar-color-yellow: #ffb454;
      --scalar-color-blue: #58a6ff;
      --scalar-color-orange: #ffb454;
      --scalar-color-purple: #bc8cff;
      /* sidebar */
      --scalar-sidebar-background-1: #0b0d10;
      --scalar-sidebar-color-1: #e6edf3;
      --scalar-sidebar-color-2: #8b98a5;
      --scalar-sidebar-color-active: #ffb454;
      --scalar-sidebar-item-active-background: #ffb45414;
      --scalar-sidebar-item-hover-background: #14181d;
      --scalar-sidebar-border-color: #232a31;
      --scalar-sidebar-search-background: #14181d;
      --scalar-sidebar-search-color: #8b98a5;
      --scalar-sidebar-search-border-color: #232a31;
      /* type — the dashboard is mono; docs get mono headings, readable body */
      --scalar-font: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
      --scalar-font-code: ui-monospace, SFMono-Regular, Menlo, monospace;
    }
    /* mono, uppercase section headings — the dashboard's panel-title look */
    .section-header, .sidebar-heading, h2.t-editor__heading {
      font-family: var(--scalar-font-code) !important;
      letter-spacing: .03em;
    }
    /* amber method badges pop on the dark ground */
    .sidebar-heading-type, .http-verb { font-family: var(--scalar-font-code) !important; }
  </style>
</head>
<body>
  <div id="app"></div>
  <script src="/docs/scalar.js"></script>
  <script>
    Scalar.createApiReference('#app', {
      url: '/openapi.json',
      theme: 'none',
      darkMode: true,
      hideDarkModeToggle: true,
      metaData: { title: 'Hoglet API — one binary, PostHog-compatible' },
    })
  </script>
</body>
</html>"##,
    )
}

fn tag(name: &str, desc: &str) -> Value {
    json!({ "name": name, "description": desc })
}

/// The OpenAPI 3.1 document describing Hoglet's HTTP surface.
pub fn spec() -> Value {
    let mut document = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Hoglet API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "PostHog-compatible product analytics in one binary. \
                The ingest edge (capture, config, flags) is byte-for-byte PostHog \
                wire-compatible — point a PostHog SDK at it. The dashboard and \
                admin APIs are Hoglet's own.",
            "license": { "name": "AGPL-3.0-only" }
        },
        "servers": [{ "url": "/", "description": "This Hoglet instance" }],
        "tags": [
            tag("Capture", "PostHog-compatible event ingestion. SDKs post here."),
            tag("Config", "The config posthog-js fetches first on init."),
            tag("Flags", "Feature flag evaluation, PostHog-compatible."),
            tag("Query", "Hoglet's dashboard query API (not PostHog-shaped)."),
            tag("Admin", "Project/flag management and GDPR erasure. Requires HOGLET_ADMIN_TOKEN."),
            tag("Ops", "Health, readiness, metrics."),
        ],
        "paths": {
            "/e/": {
                "post": {
                    "tags": ["Capture"],
                    "summary": "Capture — /e",
                    "description": "Ingest events from browser SDKs. The same handler is served at `/capture`, `/track`, `/engage`, and `/i/v0/e`.\n\nThe body may be a bare JSON array of events or a single event object. Accepted encodings: gzip (detected by magic bytes), base64, form-encoded (`data=`), or plain JSON — the `compression` hint is never trusted.\n\nResponses follow the SDK retry contract: `200` on success, `204` when `beacon=1`, `4xx` for requests the client must not retry, `503` only for retryable failures.",
                    "parameters": [
                        { "name": "beacon", "in": "query", "schema": {"type":"string"}, "description": "Set to `1` by `sendBeacon` on page unload; the response is then `204 No Content`." },
                        { "name": "compression", "in": "query", "schema": {"type":"string"}, "description": "Client hint only — gzip is detected by content sniffing regardless." },
                        { "name": "_", "in": "query", "schema": {"type":"string"}, "description": "Send timestamp in milliseconds; doubles as a cache buster." }
                    ],
                    "requestBody": {
                        "content": { "application/json": {
                            "schema": { "$ref": "#/components/schemas/EventArray" },
                            "example": [{ "event": "pageview", "distinct_id": "u1", "token": "phc_demo", "properties": { "page": "/" } }]
                        }}
                    },
                    "responses": {
                        "200": { "description": "Accepted", "content": {"application/json": {"example": {"status": 1}}}},
                        "204": { "description": "Accepted (beacon)" },
                        "400": { "description": "Malformed — do not retry" },
                        "401": { "description": "Bad/missing token — do not retry" },
                        "429": { "description": "Rate limited" },
                        "503": { "description": "Retryable sink failure" }
                    }
                }
            },
            "/batch/": {
                "post": {
                    "tags": ["Capture"],
                    "summary": "Batch capture — /batch",
                    "description": "Ingest event batches from server SDKs.\n\nThe batch-level `api_key` authenticates every event in the batch and takes precedence over per-event tokens. When `sent_at` is present, Hoglet uses it to correct client clock skew before storing timestamps.\n\nCompression handling and the response contract are identical to `/e`.",
                    "requestBody": { "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/Batch" },
                        "example": { "api_key": "phc_demo", "batch": [
                            { "event": "signup", "distinct_id": "u1" },
                            { "event": "$identify", "distinct_id": "u1@x.com", "properties": { "$anon_distinct_id": "u1", "$set": { "plan": "pro" } } }
                        ] }
                    }}},
                    "responses": { "200": { "description": "Accepted" }, "401": { "description": "Bad token" } }
                }
            },
            "/array/{token}/config": {
                "get": {
                    "tags": ["Config"],
                    "summary": "SDK config",
                    "description": "The bootstrap configuration `posthog-js` fetches before anything else.\n\nThe response advertises supported compression and which product features are enabled. Features Hoglet does not implement are declared off — `sessionRecording`, `surveys`, `heatmaps` — so the SDK never calls endpoints that do not exist.",
                    "parameters": [{ "name": "token", "in": "path", "required": true, "schema": {"type":"string"} }],
                    "responses": { "200": { "description": "Config JSON" }, "401": { "description": "Invalid token" } }
                }
            },
            "/flags/": {
                "post": {
                    "tags": ["Flags"],
                    "summary": "Evaluate flags",
                    "description": "Evaluate every active feature flag for a user. Also served at `/decide` for older SDKs.\n\nThe `v` query parameter selects the response shape:\n- `v=1` — array of enabled flag keys\n- `v=2` — map of flag key to `true`/`false` or a variant name\n- default — detailed per-flag objects with evaluation metadata\n\nRollouts bucket deterministically per `distinct_id`: results are stable across calls, and raising a rollout only ever adds users. Property conditions are matched against `person_properties` from the request body.",
                    "parameters": [{ "name": "v", "in": "query", "schema": {"type":"string","enum":["1","2"]} }],
                    "requestBody": { "content": { "application/json": {
                        "example": { "token": "phc_demo", "distinct_id": "u1", "person_properties": { "plan": "pro" } }
                    }}},
                    "responses": { "200": { "description": "Evaluated flags" }, "401": { "description": "Bad token" } }
                }
            },
            "/api/projects/{project_id}/query": {
                "post": {
                    "tags": ["Query"],
                    "summary": "Run a bounded Trends query",
                    "description": "Runs the supported Trends subset for an authorized path project. The server derives the project's capture token; request bodies cannot select another tenant. Responses report the immutable EventLake generation used by the query.",
                    "security": [{"sessionCookie": []}, {"personalBearer": []}],
                    "parameters": [{
                        "name": "project_id", "in": "path", "required": true,
                        "schema": {"type": "string", "format": "uuid"}
                    }],
                    "requestBody": {
                        "required": true,
                        "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ProjectQueryRequest"}}}
                    },
                    "responses": {
                        "200": {"description": "Trends result from one immutable generation"},
                        "401": {"description": "Authentication required"},
                        "403": {"description": "Project access denied"},
                        "422": {"description": "Query shape is outside the supported subset"},
                        "503": {"description": "Bounded query queue is full"},
                        "504": {"description": "Query execution deadline exceeded"}
                    }
                }
            },
            "/api/stats": {
                "get": {
                    "tags": ["Query"], "summary": "Stats",
                    "description": "Headline numbers for a project: total events, unique persons, and events in the last 24 hours.\n\nCounts are deduplicated by event `uuid`, so a replayed write-ahead-log segment can never inflate them.",
                    "parameters": [{ "name": "token", "in": "query", "required": true, "schema": {"type":"string"} }],
                    "responses": { "200": { "description": "OK", "content": {"application/json": {"schema": {"$ref":"#/components/schemas/Stats"}}}}}
                }
            },
            "/api/top_events": {
                "get": {
                    "tags": ["Query"], "summary": "Top events",
                    "description": "Event names ranked by occurrence count, highest first.\n\nPowers the *Top events* panel on the dashboard.",
                    "parameters": [
                        { "name": "token", "in": "query", "required": true, "schema": {"type":"string"} },
                        { "name": "limit", "in": "query", "schema": {"type":"integer","default":20} }
                    ],
                    "responses": { "200": { "description": "OK", "content": {"application/json": {"schema": {"type":"array","items":{"$ref":"#/components/schemas/EventCount"}}}}}}
                }
            },
            "/api/trend": {
                "get": {
                    "tags": ["Query"], "summary": "Trend",
                    "description": "Daily occurrence counts for one event over the last `days` days (default 30, maximum 365).\n\nDays with no occurrences are omitted from the result.",
                    "parameters": [
                        { "name": "token", "in": "query", "required": true, "schema": {"type":"string"} },
                        { "name": "event", "in": "query", "required": true, "schema": {"type":"string"} },
                        { "name": "days", "in": "query", "schema": {"type":"integer","default":30} }
                    ],
                    "responses": { "200": { "description": "OK", "content": {"application/json": {"schema": {"type":"array","items":{"$ref":"#/components/schemas/TrendPoint"}}}}}}
                }
            },
            "/api/funnel": {
                "post": {
                    "tags": ["Query"], "summary": "Funnel",
                    "description": "Ordered conversion funnel.\n\nFor steps `[A, B, C]`: how many distinct persons did `A`, then `B` at or after their `A`, then `C` at or after their `B`. Step counts are monotonically non-increasing by construction. Up to 12 steps.",
                    "requestBody": { "content": { "application/json": {
                        "example": { "token": "phc_demo", "steps": ["signup", "activate", "purchase"] }
                    }}},
                    "responses": { "200": { "description": "OK", "content": {"application/json": {"schema": {"type":"array","items":{"$ref":"#/components/schemas/FunnelStep"}}}}}, "400": { "description": "Too many steps" } }
                }
            },
            "/api/recent": {
                "get": {
                    "tags": ["Query"], "summary": "Recent events",
                    "description": "The newest events for a project, most recent first — the dashboard's live stream.\n\nCapped at 200 events per request.",
                    "parameters": [
                        { "name": "token", "in": "query", "required": true, "schema": {"type":"string"} },
                        { "name": "limit", "in": "query", "schema": {"type":"integer","default":20} }
                    ],
                    "responses": { "200": { "description": "OK", "content": {"application/json": {"schema": {"type":"array","items":{"$ref":"#/components/schemas/RecentEvent"}}}}}}
                }
            },
            "/api/flags": {
                "get": {
                    "tags": ["Query"], "summary": "Flags list",
                    "description": "Every flag definition for a project: key, active state, rollout percentage, and variants.\n\nRead-only — definitions are created and updated through the Admin API.",
                    "parameters": [{ "name": "token", "in": "query", "required": true, "schema": {"type":"string"} }],
                    "responses": { "200": { "description": "OK", "content": {"application/json": {"schema": {"type":"array","items":{"$ref":"#/components/schemas/FlagDef"}}}}}}
                }
            },
            "/api/admin/projects": {
                "post": {
                    "tags": ["Admin"], "summary": "Create project",
                    "description": "Register a project and its ingest token.\n\nWith no projects registered, Hoglet runs in **open mode**: any well-formed token is accepted, so a single-tenant install needs no setup. Creating the first project switches to **closed mode**, where only registered tokens may ingest.",
                    "security": [{ "adminBearer": [] }],
                    "requestBody": { "content": { "application/json": { "example": { "token": "phc_acme", "name": "Acme" } }}},
                    "responses": { "201": { "description": "Created" }, "401": { "description": "Bad admin token" }, "404": { "description": "Admin API disabled" } }
                }
            },
            "/api/admin/flags": {
                "post": {
                    "tags": ["Admin"], "summary": "Upsert flag",
                    "description": "Create a feature flag, or update it in place.\n\nA flag has an active state, a rollout percentage, optional weighted **variants** for multivariate tests, and optional **conditions** — property filters matched against `person_properties` at evaluation time.",
                    "security": [{ "adminBearer": [] }],
                    "requestBody": { "content": { "application/json": { "example": {
                        "token": "phc_demo", "key": "new-checkout", "rollout_percentage": 60,
                        "variants": [{ "key": "control", "rollout": 50 }, { "key": "test", "rollout": 50 }],
                        "conditions": { "properties": [{ "key": "plan", "operator": "exact", "value": "pro" }] }
                    }}}},
                    "responses": { "200": { "description": "Upserted" }, "401": { "description": "Bad admin token" }, "404": { "description": "Admin API disabled" } }
                }
            },
            "/api/admin/forget": {
                "post": {
                    "tags": ["Admin"], "summary": "Erase person (GDPR)",
                    "description": "Erase one person, physically and synchronously.\n\nRemoves the person's events from columnar storage and deletes their identity mappings. Built for GDPR right-to-erasure requests: the data is gone when the request returns.",
                    "security": [{ "adminBearer": [] }],
                    "requestBody": { "content": { "application/json": { "example": { "token": "phc_demo", "distinct_id": "forget-me" } }}},
                    "responses": { "200": { "description": "Erased", "content": {"application/json": {"example": {"events_removed": 3}}}}}
                }
            },
            "/health": { "get": { "tags": ["Ops"], "summary": "Liveness",
                    "description": "Liveness probe. Returns `200` for as long as the process is running — it says nothing about readiness to serve traffic.", "responses": { "200": { "description": "Alive" } } } },
            "/ready": { "get": { "tags": ["Ops"], "summary": "Readiness",
                    "description": "Readiness gate. Returns `200` only once write-ahead-log recovery has finished and every store is open; `503` before that.\n\nPoint load balancers and orchestration health checks here.", "responses": { "200": { "description": "Ready" }, "503": { "description": "Not ready" } } } },
            "/metrics": { "get": { "tags": ["Ops"], "summary": "Metrics",
                    "description": "Hoglet's own operational counters in Prometheus text format: events captured and acknowledged, rejected requests, sink errors, and uptime.\n\nOperator telemetry for the Hoglet process itself — not part of the analytics product.", "responses": { "200": { "description": "text/plain exposition" } } } }
        },
        "components": {
            "securitySchemes": {
                "adminBearer": { "type": "http", "scheme": "bearer", "description": "Legacy operator token" },
                "sessionCookie": { "type": "apiKey", "in": "cookie", "name": "hoglet_sid" },
                "personalBearer": { "type": "http", "scheme": "bearer", "bearerFormat": "phx_*" }
            },
            "schemas": {
                "ProjectQueryRequest": { "type": "object", "required": ["query"], "additionalProperties": false, "properties": {
                    "query": { "type": "object", "description": "Validated Trends query IR" },
                    "refresh": { "type": "boolean", "default": false }
                } },
                "EventArray": { "type": "array", "items": { "$ref": "#/components/schemas/Event" } },
                "Event": {
                    "type": "object",
                    "required": ["event", "distinct_id"],
                    "properties": {
                        "event": { "type": "string" },
                        "distinct_id": { "type": "string" },
                        "token": { "type": "string" },
                        "timestamp": { "type": "string", "format": "date-time" },
                        "properties": { "type": "object", "additionalProperties": true }
                    }
                },
                "Batch": {
                    "type": "object",
                    "required": ["batch"],
                    "properties": {
                        "api_key": { "type": "string" },
                        "sent_at": { "type": "string", "format": "date-time" },
                        "batch": { "type": "array", "items": { "$ref": "#/components/schemas/Event" } }
                    }
                },
                "Stats": { "type": "object", "properties": {
                    "total_events": { "type": "integer" }, "unique_persons": { "type": "integer" }, "events_24h": { "type": "integer" } } },
                "EventCount": { "type": "object", "properties": { "event": { "type": "string" }, "count": { "type": "integer" } } },
                "TrendPoint": { "type": "object", "properties": { "day": { "type": "string" }, "count": { "type": "integer" } } },
                "FunnelStep": { "type": "object", "properties": { "event": { "type": "string" }, "reached": { "type": "integer" } } },
                "RecentEvent": { "type": "object", "properties": {
                    "uuid": { "type": "string" }, "event": { "type": "string" }, "distinct_id": { "type": "string" }, "timestamp": { "type": "string" } } },
                "Variant": { "type": "object", "properties": { "key": { "type": "string" }, "rollout": { "type": "number" } } },
                "FlagDef": { "type": "object", "properties": {
                    "key": { "type": "string" }, "active": { "type": "boolean" }, "rollout_percentage": { "type": "number" },
                    "variants": { "type": "array", "items": { "$ref": "#/components/schemas/Variant" } } } }
            }
        }
    });

    document["info"]["description"] = Value::String(
        "A documented subset of PostHog's capture/config/decide wire contract, plus Hoglet's authenticated workspace and project APIs. Only the endpoints listed here are mounted by the production application."
            .into(),
    );
    document["tags"] = json!([
        tag(
            "Capture",
            "Supported PostHog-compatible event ingestion aliases."
        ),
        tag(
            "Config",
            "The supported posthog-js bootstrap configuration."
        ),
        tag("Flags", "Supported PostHog-compatible flag evaluation."),
        tag("Workspace", "Session and personal-key workspace APIs."),
        tag("Query", "Bounded project-scoped Trends queries."),
        tag(
            "Resources",
            "Project-scoped flags, insights, dashboards, and shares."
        ),
        tag("Ops", "Health, readiness, and metrics."),
    ]);

    let paths = document["paths"]
        .as_object_mut()
        .expect("OpenAPI paths are an object");
    const DETAILED_PRODUCTION_PATHS: &[&str] = &[
        "/e/",
        "/batch/",
        "/array/{token}/config",
        "/flags/",
        "/api/projects/{project_id}/query",
        "/health",
        "/ready",
        "/metrics",
    ];
    paths.retain(|path, _| DETAILED_PRODUCTION_PATHS.contains(&path.as_str()));

    for (path, methods, tag_name, protected) in [
        ("/api/auth/bootstrap", &["get"][..], "Workspace", false),
        ("/api/auth/setup", &["post"][..], "Workspace", false),
        ("/api/auth/login", &["post"][..], "Workspace", false),
        ("/api/auth/logout", &["post"][..], "Workspace", true),
        ("/api/auth/me", &["get"][..], "Workspace", true),
        ("/api/auth/keys", &["get", "post"][..], "Workspace", true),
        (
            "/api/auth/keys/{key_id}",
            &["delete"][..],
            "Workspace",
            true,
        ),
        (
            "/api/organizations",
            &["get", "post"][..],
            "Workspace",
            true,
        ),
        (
            "/api/organizations/{organization_id}/projects",
            &["post"][..],
            "Workspace",
            true,
        ),
        (
            "/api/projects/{project_id}/flags",
            &["get", "post"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/flags/{key}",
            &["put", "delete"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/catalog/events",
            &["get"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/catalog/properties",
            &["get"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/catalog/values",
            &["get"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/insights",
            &["get", "post"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/insights/{insight_id}",
            &["get", "put", "delete"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/dashboards",
            &["get", "post"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/dashboards/{dashboard_id}",
            &["get", "put", "delete"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/dashboards/{dashboard_id}/tiles",
            &["put"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/shares",
            &["get", "post"][..],
            "Resources",
            true,
        ),
        (
            "/api/projects/{project_id}/shares/{share_id}",
            &["delete"][..],
            "Resources",
            true,
        ),
        ("/shared/{token}", &["get"][..], "Resources", false),
    ] {
        paths.insert(
            path.into(),
            documented_operations(methods, tag_name, protected),
        );
    }

    if let Some(schemes) = document["components"]["securitySchemes"].as_object_mut() {
        schemes.remove("adminBearer");
    }
    document
}

fn documented_operations(methods: &[&str], tag_name: &str, protected: bool) -> Value {
    let mut operations = Map::new();
    for method in methods {
        let mut operation = json!({
            "tags": [tag_name],
            "summary": "Production endpoint",
            "responses": {
                "200": {"description": "Success"},
                "400": {"description": "Invalid request"},
                "401": {"description": "Authentication required"},
                "403": {"description": "Access denied"},
                "404": {"description": "Resource not found"}
            }
        });
        if protected {
            operation["security"] = json!([{"sessionCookie": []}, {"personalBearer": []}]);
        }
        operations.insert((*method).into(), operation);
    }
    Value::Object(operations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_valid_openapi_json() {
        let s = spec();
        assert_eq!(s["openapi"], "3.1.0");
        assert!(s["paths"]["/e/"]["post"].is_object());
        assert!(s["paths"]["/api/projects/{project_id}/query"]["post"].is_object());
        assert!(s["paths"]["/api/auth/bootstrap"]["get"].is_object());
        assert!(s["paths"]["/api/projects/{project_id}/dashboards"]["post"].is_object());
        assert!(s["paths"].get("/api/stats").is_none());
        assert!(s["paths"].get("/api/admin/projects").is_none());
        assert!(
            s["components"]["securitySchemes"]
                .get("adminBearer")
                .is_none()
        );
        assert!(s["components"]["schemas"]["Stats"].is_object());
    }
}

//! The dashboard, embedded in the binary (spec/README.md "Dashboard").
//!
//! A real React + TypeScript app (source in `web/`, types generated from the
//! Rust API structs by ts-rs) built by Vite into `web/dist/` and compiled into
//! the binary with `rust-embed`. No Node process, no CDN, no separate deploy —
//! the moment the dashboard is anything but part of this binary, the
//! single-binary claim is dead (`claims.md`).
//!
//! `web/dist/` is committed so `cargo build` needs no Node; rebuild it with
//! `cd web && npm run build` after changing the frontend.

use axum::{
    Router,
    extract::Path,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/dashboard", get(index))
        // Hashed JS/CSS bundles live under /assets/.
        .route("/assets/{*path}", get(asset))
}

async fn index() -> Response {
    match Assets::get("index.html") {
        Some(f) => Html(f.data.into_owned()).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "dashboard not built").into_response(),
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    let full = format!("assets/{path}");
    match Assets::get(&full) {
        Some(f) => {
            let mime = mime_guess::from_path(&full).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                // Hashed filenames are immutable — cache hard.
                f.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

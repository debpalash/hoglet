//! The dashboard, embedded in the binary (SPEC.md "Dashboard").
//!
//! A single self-contained page compiled in with `include_str!` — no Node
//! process, no CDN, no separate deploy. The moment the dashboard is anything
//! but part of this binary, the single-binary claim is dead (`claims.md`).
//! When the UI grows into a real React build, this becomes a `rust-embed`
//! asset dir; the serving contract stays the same.

use axum::{Router, response::Html, routing::get};

const INDEX: &str = include_str!("../../dashboard/index.html");

pub fn router() -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/dashboard", get(|| async { Html(INDEX) }))
}

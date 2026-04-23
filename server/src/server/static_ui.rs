//! Serve the embedded Vite-built SPA as the HTTP router's fallback.
//!
//! The React UI ships inside the `djinn-server` binary via `rust-embed`
//! (folder `../ui/dist/`, populated by `pnpm --dir ui build`). This lets us
//! run a single image that hosts both the API and the client — no sidecar
//! nginx, no separate Ingress.
//!
//! Routing model: the router registers every backend path explicitly
//! (`/api/*`, `/health`, `/events`, merged sub-routers for auth, agents,
//! etc.). Anything unmatched falls through to `serve_static`, which either
//! serves the requested asset (e.g. `/assets/index-abc.js`) or replies
//! with `index.html` so the client-side router can pick it up.

use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../ui/dist/"]
struct UiAssets;

pub(super) async fn serve_static(uri: Uri) -> Response<Body> {
    let requested = uri.path().trim_start_matches('/');
    let (path, status) = match UiAssets::get(requested) {
        Some(_) if !requested.is_empty() => (requested.to_string(), StatusCode::OK),
        _ => ("index.html".to_string(), StatusCode::OK),
    };

    let Some(file) = UiAssets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // index.html must never be cached — a stale cache would pin the client
    // to old hashed asset filenames that the new build no longer contains.
    // Vite content-hashes every other asset filename, so `immutable` is
    // safe and lets browsers skip revalidation entirely.
    let cache_control = if path == "index.html" {
        "no-cache, must-revalidate"
    } else {
        "public, max-age=31536000, immutable"
    };

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, file.metadata.mimetype())
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(file.data.into_owned()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

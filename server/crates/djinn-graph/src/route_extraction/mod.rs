//! HTTP route extraction passes.
//!
//! Framework-specific extractors materialize synthetic `Route` nodes and
//! typed route edges without changing the SCIP-derived symbol/file graph.

pub mod axum;

pub use axum::{AxumRouteHit, RouteExtractionReport, detect_axum_routes};

/// Environment flag that disables route extraction when set to `0` / `false`.
/// Default = on.
pub const ROUTE_DETECTION_FLAG: &str = "DJINN_ROUTE_DETECTION";

/// Returns `true` when route extraction should run.
pub fn route_detection_enabled() -> bool {
    match std::env::var(ROUTE_DETECTION_FLAG) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

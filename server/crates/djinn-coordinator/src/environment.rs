//! Compatibility shim: environment detection.
//!
//! Stub for `djinn-agent::environment::project_has_indexable_code`.

use djinn_db::Database;

/// Determine whether a project likely has indexable source code.
///
/// Returns `true` when the project has language config or workspaces
/// configured; `false` when there is no image config at all. Errors
/// resolve to `true` (conservative: attempt the warm).
pub async fn project_has_indexable_code(db: &Database, project_id: &str) -> bool {
    match djinn_db::ImageRepository::new(db.clone())
        .resolve_for_project(project_id)
        .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "environment: failed to resolve catalog image; assuming project has code"
            );
            true
        }
    }
}

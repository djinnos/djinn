//! Runtime-derived filesystem locations.
//!
//! Paths are NOT persisted in the DB — each container mounts the
//! projects volume at its own location, so a single canonical path
//! can't be correct for all consumers. Every process derives its
//! local location from `$DJINN_HOME/projects/{owner}/{repo}`, using
//! this module as the single source of truth.

use std::path::PathBuf;

/// Root directory containing all project clones.
///
/// Resolution order:
/// 1. `$DJINN_HOME/projects` — Helm sets `DJINN_HOME=/var/lib/djinn`
///    so non-root containers can write to `/var/lib/djinn/projects`.
/// 2. `~/.djinn/projects` — docker-compose / local-dev fallback
///    where `$HOME` points at the invoking user.
/// 3. `/tmp/.djinn/projects` — last-ditch fallback when `$HOME`
///    isn't set (rare; mostly paranoia for init-container scenarios).
pub fn projects_root() -> PathBuf {
    if let Ok(djinn_home) = std::env::var("DJINN_HOME")
        && !djinn_home.is_empty()
    {
        return PathBuf::from(djinn_home).join("projects");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("projects")
}

/// Per-project clone directory: `{projects_root}/{owner}/{repo}`.
///
/// Every consumer of a project's filesystem location — git fetch,
/// devcontainer builder, worker CWD, memory note writer — calls this
/// with the project's `(github_owner, github_repo)` coords. The path
/// is a derivation, not persisted state.
pub fn project_dir(owner: &str, repo: &str) -> PathBuf {
    projects_root().join(owner).join(repo)
}

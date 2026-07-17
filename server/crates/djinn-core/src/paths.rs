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
    projects_root_from(
        std::env::var_os("DJINN_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn projects_root_from(djinn_home: Option<PathBuf>, home_dir: Option<PathBuf>) -> PathBuf {
    if let Some(djinn_home) = djinn_home
        && !djinn_home.as_os_str().is_empty()
    {
        return djinn_home.join("projects");
    }
    home_dir
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("projects")
}

/// Root directory holding the shared build/tool cache.
///
/// The same cache PVC is mounted at *different* paths by different pods: Job
/// pods (warm/verify/task-run) mount it at `/cache`, while the long-lived
/// server/coordinator pod mounts it at `$DJINN_HOME/cache`. Any host-side
/// reaping of cache contents MUST derive its path from the host's own mount,
/// not from the Job-pod convention, or it silently operates on a nonexistent
/// directory (the bug that let `cargo-target-runs` grow unbounded on disk).
///
/// Resolution order mirrors [`projects_root`]:
/// 1. `$DJINN_HOME/cache` — Helm sets `DJINN_HOME=/var/lib/djinn`.
/// 2. `~/.djinn/cache` — docker-compose / local-dev fallback.
/// 3. `/tmp/.djinn/cache` — last-ditch fallback when `$HOME` isn't set.
pub fn cache_root() -> PathBuf {
    if let Ok(djinn_home) = std::env::var("DJINN_HOME")
        && !djinn_home.is_empty()
    {
        return PathBuf::from(djinn_home).join("cache");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("cache")
}

/// Host-side directory holding per-task-run private Cargo target dirs.
///
/// This is the server/coordinator pod's view of the same directory the Job
/// pods know as `/cache/cargo-target-runs`. The periodic sweep and the host
/// teardown backstop both resolve their root here so they operate on the
/// directory that is actually mounted in the server pod.
pub fn cargo_target_runs_root() -> PathBuf {
    cache_root().join("cargo-target-runs")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_djinn_home_keeps_project_clone_under_projects_namespace() {
        let home = PathBuf::from("/isolated/djinn-home");
        let projects = projects_root_from(Some(home.clone()), None);
        assert_eq!(
            projects.join("octo").join("repo"),
            home.join("projects/octo/repo")
        );
        assert_eq!(projects, home.join("projects"));
    }

    #[test]
    fn projects_root_keeps_home_djinn_projects_fallback() {
        let home = PathBuf::from("/isolated/home");
        assert_eq!(
            projects_root_from(None, Some(home.clone())),
            home.join(".djinn/projects")
        );
    }
}

//! Runtime-derived filesystem locations.
//!
//! Paths are NOT persisted in the DB — each container mounts the
//! projects volume at its own location, so a single canonical path
//! can't be correct for all consumers. Every process derives its
//! local location from `$DJINN_HOME/projects/{owner}/{repo}`, using
//! this module as the single source of truth.

use std::path::{Path, PathBuf};

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
    cache_root_from(
        std::env::var_os("DJINN_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Pure derivation behind [`cache_root`].
///
/// Deliberately takes **only** `$DJINN_HOME` and the home directory: there is
/// no `XDG_CACHE_HOME` parameter, because `XDG_CACHE_HOME` is a Job-pod-only
/// variable and consulting it on the host is precisely the bug this module
/// exists to prevent. A regression that reintroduces an XDG leg has to change
/// this signature.
fn cache_root_from(djinn_home: Option<PathBuf>, home_dir: Option<PathBuf>) -> PathBuf {
    if let Some(djinn_home) = djinn_home
        && !djinn_home.as_os_str().is_empty()
    {
        return djinn_home.join("cache");
    }
    home_dir
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

/// Host-side directory holding the warm, per-project Cargo target bases.
///
/// The server/coordinator pod's view of the directory Job pods know as
/// `/cache/cargo-target` (`djinn_agent_worker::cargo_target_seed::WARM_BASE_ROOT`).
/// Host-side inspection MUST resolve through here for the same reason
/// [`cargo_target_runs_root`] does: the Job-pod path does not exist in the
/// server pod, so a hardcoded `/cache/cargo-target` silently reports nothing.
pub fn cargo_target_root() -> PathBuf {
    cache_root().join("cargo-target")
}

/// Host-side directory holding the shared sccache store.
///
/// The server/coordinator pod's view of the directory Job pods know as
/// `/cache/sccache` (the `SCCACHE_DIR` compatibility fallback rendered into
/// Job specs). The coordinator's sccache guard resolves here for the same
/// reason [`cargo_target_root`] does: `/cache/sccache` does not exist in the
/// server pod, so a hardcoded literal makes the guard a permanent
/// `path does not exist` no-op.
pub fn sccache_root() -> PathBuf {
    cache_root().join("sccache")
}

/// Relative location of the durable output stash inside *any* XDG cache tree.
///
/// Kept as a shared constant so the writer (`$XDG_CACHE_HOME/djinn/output_stash`
/// in Job pods) and the host-side GC cannot drift apart.
const OUTPUT_STASH_SUFFIX: [&str; 2] = ["djinn", "output_stash"];

/// Relative location of the SCIP indexer cache inside *any* XDG cache tree.
const SCIP_INDEXER_SUFFIX: [&str; 2] = ["djinn", "scip-indexer"];

/// Host-side root of the per-project XDG cache trees that Job pods write into.
///
/// `XDG_CACHE_HOME` is rendered **only** into Job pods, as
/// `/cache/xdg/{project_id}` (`djinn_k8s::job`). It is set nowhere in the
/// server/coordinator pod's Helm templates. So every `$XDG_CACHE_HOME`-relative
/// store — the durable output stash, the SCIP indexer cache — lands on the
/// shared cache PVC under `<pvc>/xdg/{project_id}/…` when a Job writes it, but
/// the *same* XDG→`$HOME/.cache` chain evaluated in the server pod resolves to
/// `/home/djinn/.cache`, a path that does not even exist there (verified in
/// production). Host-side sweeps that want the bytes Job pods actually wrote
/// MUST enumerate from here, exactly as [`cargo_target_runs_root`] does for the
/// non-XDG half of the same PVC.
pub fn xdg_cache_root() -> PathBuf {
    xdg_cache_root_under(&cache_root())
}

/// [`xdg_cache_root`] relative to an explicit cache mount.
///
/// Callers that already hold a resolved cache root (a coordinator context, a
/// test tempdir) use this so the `xdg` path component lives in exactly one
/// place.
pub fn xdg_cache_root_under(cache_root: &Path) -> PathBuf {
    cache_root.join("xdg")
}

/// Host-side view of one project's Job-pod `$XDG_CACHE_HOME`.
pub fn project_xdg_cache_dir(project_id: &str) -> PathBuf {
    xdg_cache_root().join(project_id)
}

/// Host-side view of one project's Job-pod `$XDG_CACHE_HOME`, relative to an
/// explicit cache mount.
pub fn project_xdg_cache_dir_under(cache_root: &Path, project_id: &str) -> PathBuf {
    xdg_cache_root_under(cache_root).join(project_id)
}

/// Durable output-stash root beneath an arbitrary XDG cache directory.
///
/// Pass a [`project_xdg_cache_dir`] to get the host's view of a Job pod's stash.
pub fn output_stash_dir_under(xdg_cache_dir: &Path) -> PathBuf {
    let mut path = xdg_cache_dir.to_path_buf();
    path.extend(OUTPUT_STASH_SUFFIX);
    path
}

/// SCIP indexer cache root beneath an arbitrary XDG cache directory.
pub fn scip_indexer_cache_dir_under(xdg_cache_dir: &Path) -> PathBuf {
    let mut path = xdg_cache_dir.to_path_buf();
    path.extend(SCIP_INDEXER_SUFFIX);
    path
}

/// Host-side durable output-stash root for a process with no `$XDG_CACHE_HOME`.
///
/// This is where a server-pod-hosted session's durable stash *should* live: on
/// the cache PVC, not on the pod's ephemeral container layer.
pub fn output_stash_root() -> PathBuf {
    output_stash_dir_under(&cache_root())
}

/// Host-side SCIP indexer cache root for a process with no `$XDG_CACHE_HOME`.
///
/// The last-resort fallback for `djinn_graph`'s cache-root resolution. The
/// alternative it replaced was `$HOME/.cache/djinn/scip-indexer` (ephemeral,
/// wrong tree) and — when `$HOME` was unset too — a **cwd-relative**
/// `.cache/djinn/scip-indexer`, which silently scatters cache state into
/// whatever directory the process happened to start in.
pub fn scip_indexer_cache_root() -> PathBuf {
    scip_indexer_cache_dir_under(&cache_root())
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

    /// Every host-side cache accessor must hang off [`cache_root`] and must
    /// never equal the Job-pod literal. The server pod has no `/cache` at all
    /// (verified: `ls /cache` → No such file or directory), so any accessor
    /// that collapses onto the Job-pod path silently operates on nothing.
    ///
    /// `ends_with` alone is not sufficient to catch a regression here —
    /// `/cache/cargo-target` also ends with `cache/cargo-target`. The
    /// `assert_ne!` against the literal is the load-bearing assertion.
    #[test]
    fn host_cache_accessors_never_collapse_onto_the_job_pod_literals() {
        for (host, job_pod_literal) in [
            (cargo_target_root(), "/cache/cargo-target"),
            (cargo_target_runs_root(), "/cache/cargo-target-runs"),
            (sccache_root(), "/cache/sccache"),
        ] {
            assert_ne!(
                host,
                PathBuf::from(job_pod_literal),
                "host accessor must not resolve to the Job-pod path {job_pod_literal}"
            );
            assert!(
                host.starts_with(cache_root()),
                "host accessor {} must live under the host cache root {}",
                host.display(),
                cache_root().display()
            );
        }
    }

    #[test]
    fn host_cache_accessors_are_named_children_of_the_cache_root() {
        assert_eq!(cargo_target_root(), cache_root().join("cargo-target"));
        assert_eq!(
            cargo_target_runs_root(),
            cache_root().join("cargo-target-runs")
        );
        assert_eq!(sccache_root(), cache_root().join("sccache"));
    }

    /// The XDG accessors exist precisely because `XDG_CACHE_HOME` is a Job-pod
    /// -only variable. If any of them ever resolved through the ambient
    /// `XDG_CACHE_HOME`/`$HOME` chain, the host sweep would walk the server
    /// pod's ephemeral container layer instead of the PVC — the leak these
    /// accessors were added to close.
    ///
    /// Neutralization check: `cache_root_from` is the *only* input path, and it
    /// takes no `XDG_CACHE_HOME`. Every XDG accessor must hang off it, so a
    /// server pod (`DJINN_HOME=/var/lib/djinn`, `HOME=/home/djinn`,
    /// `XDG_CACHE_HOME` unset) resolves the PVC mount and never
    /// `/home/djinn/.cache`, whatever the ambient environment says.
    #[test]
    fn xdg_host_accessors_ignore_the_ambient_xdg_and_home_chain() {
        let server_pod_cache = cache_root_from(
            Some(PathBuf::from("/var/lib/djinn")),
            Some(PathBuf::from("/home/djinn")),
        );
        assert_eq!(server_pod_cache, PathBuf::from("/var/lib/djinn/cache"));
        assert!(
            !server_pod_cache.starts_with("/home/djinn/.cache"),
            "the server pod's cache root must never be the ephemeral $HOME/.cache tree"
        );

        for path in [
            xdg_cache_root(),
            output_stash_root(),
            scip_indexer_cache_root(),
        ] {
            assert!(
                path.starts_with(cache_root()),
                "{} must live under the host cache root {}",
                path.display(),
                cache_root().display()
            );
            if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
                assert!(
                    !path.starts_with(PathBuf::from(xdg)),
                    "{} must not resolve through the ambient XDG_CACHE_HOME",
                    path.display()
                );
            }
        }
        assert_eq!(xdg_cache_root(), cache_root().join("xdg"));
    }

    /// The Job pod writes `$XDG_CACHE_HOME/djinn/{output_stash,scip-indexer}`
    /// where `$XDG_CACHE_HOME = /cache/xdg/<project_id>`. The host's view of
    /// that exact tree must be `cache_root()/xdg/<project_id>/djinn/…` — and
    /// must never collapse onto the Job-pod `/cache` literal.
    #[test]
    fn project_xdg_accessors_mirror_the_job_pod_layout_under_the_host_mount() {
        let project = "019ea3bd-a305-73e3-806c-4edcc96ebfe2";
        let project_xdg = project_xdg_cache_dir(project);
        assert_eq!(project_xdg, cache_root().join("xdg").join(project));

        assert_eq!(
            output_stash_dir_under(&project_xdg),
            project_xdg.join("djinn").join("output_stash")
        );
        assert_eq!(
            scip_indexer_cache_dir_under(&project_xdg),
            project_xdg.join("djinn").join("scip-indexer")
        );

        for (host, job_pod_literal) in [
            (
                output_stash_dir_under(&project_xdg),
                format!("/cache/xdg/{project}/djinn/output_stash"),
            ),
            (
                scip_indexer_cache_dir_under(&project_xdg),
                format!("/cache/xdg/{project}/djinn/scip-indexer"),
            ),
        ] {
            assert_ne!(
                host,
                PathBuf::from(&job_pod_literal),
                "host accessor must not resolve to the Job-pod path {job_pod_literal}"
            );
        }
    }
}

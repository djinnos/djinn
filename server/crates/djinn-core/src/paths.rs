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

// ---------------------------------------------------------------------------
// Cache-root manifest
// ---------------------------------------------------------------------------

/// Mount path of the shared cache PVC inside **Job** pods (warm / verify /
/// task-run). The server/coordinator pod mounts the same claim at
/// [`cache_root`] instead; see that function for why the two must never be
/// conflated.
pub const JOB_POD_CACHE_MOUNT: &str = "/cache";

/// How the immediate children of a cache root are keyed.
///
/// This drives how far a host-side inspector may descend before it stops being
/// able to say anything meaningful about what it sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheRootNamespacing {
    /// Contents are keyed by content, not by tenant (a Cargo registry, a pnpm
    /// store, a Go module cache). Shared deliberately across every project on
    /// the deployment, so nothing under it can be attributed to a tenant and
    /// nothing under it can be declared orphaned by a tenant reconciliation.
    Shared,
    /// Immediate children are project ids. These ARE reconcilable against the
    /// live project set: a child whose name is not a live project id belongs to
    /// a project that no longer exists.
    PerProject,
    /// Immediate children are per-task-run ids with a lifetime of one task run.
    /// They are created and destroyed constantly by normal operation, so their
    /// presence, absence, age and count carry no staleness signal at all — a
    /// dedicated sweep (`djinn_coordinator::health::sweep_orphaned_cargo_target_run_dirs`)
    /// owns them. A cache-root inspector must never descend into these.
    PerRun,
}

/// Static description of one cache root djinn actually uses.
#[derive(Debug, Clone, Copy)]
pub struct CacheRootSpec {
    /// Directory name directly under the cache mount. Never a nested path:
    /// the manifest describes cache *roots*, not their contents.
    pub dir_name: &'static str,
    /// How the root's immediate children are keyed.
    pub namespacing: CacheRootNamespacing,
    /// Child names owned by djinn's own machinery rather than by a tenant.
    /// A reconciler must not classify these as tenant namespaces.
    pub reserved_children: &'static [&'static str],
    /// Where the platform renders this root. Kept as prose so a reader can go
    /// straight to the code that would have to change to retire the root.
    pub declared_by: &'static str,
    /// What the root holds, and why it is on the shared PVC.
    pub purpose: &'static str,
}

/// Declare the cache-root manifest.
///
/// One list generates the [`CacheRootId`] enum, its `ALL` slice, and the
/// per-variant [`CacheRootSpec`]. Because all three come from the same
/// expansion it is *structurally impossible* to add a variant that is missing
/// from `ALL` or from the spec table — the failure mode of a hand-synced
/// manifest, which is the very bug this manifest exists to prevent.
macro_rules! declare_cache_roots {
    ($(
        $(#[$meta:meta])*
        $variant:ident {
            dir: $dir:literal,
            namespacing: $ns:expr,
            reserved_children: [$($reserved:literal),* $(,)?],
            declared_by: $declared:literal,
            purpose: $purpose:literal,
        }
    )+) => {
        /// Every cache root djinn is known to use, as a closed set.
        ///
        /// Adding a variant is a compile-time obligation to describe it: the
        /// generated `spec()` match is exhaustive over this enum.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum CacheRootId {
            $($(#[$meta])* $variant,)+
        }

        impl CacheRootId {
            /// The manifest: every declared cache root, in declaration order.
            pub const ALL: &'static [CacheRootId] = &[$(CacheRootId::$variant,)+];

            /// Static description of this root.
            pub const fn spec(self) -> &'static CacheRootSpec {
                match self {
                    $(CacheRootId::$variant => &CacheRootSpec {
                        dir_name: $dir,
                        namespacing: $ns,
                        reserved_children: &[$($reserved,)*],
                        declared_by: $declared,
                        purpose: $purpose,
                    },)+
                }
            }

            /// Absolute path of this root **inside a Job pod**.
            ///
            /// Renderers that bake a cache path into a Pod spec or an image ENV
            /// must resolve through here rather than interpolating `/cache/...`,
            /// so a newly-introduced root cannot exist without a manifest entry.
            pub const fn job_pod_path(self) -> &'static str {
                match self {
                    $(CacheRootId::$variant => concat!("/cache/", $dir),)+
                }
            }
        }
    };
}

declare_cache_roots! {
    /// Warm, per-project Cargo target bases.
    CargoTarget {
        dir: "cargo-target",
        namespacing: CacheRootNamespacing::PerProject,
        // The shared warm-base flock directory, keyed by project id but owned
        // by djinn's warm machinery rather than being a cache namespace.
        reserved_children: [".warm-locks"],
        declared_by: "djinn_k8s::job::cache_env_vars (CARGO_TARGET_DIR) / djinn_agent_worker::cargo_target_seed::WARM_BASE_ROOT",
        purpose: "per-project warm Cargo target base seeded into task-run target dirs",
    }
    /// Private per-task-run Cargo target dirs seeded from the warm base.
    CargoTargetRuns {
        dir: "cargo-target-runs",
        namespacing: CacheRootNamespacing::PerRun,
        reserved_children: [],
        declared_by: "djinn_supervisor::CARGO_TARGET_RUNS_ROOT (task-run CARGO_TARGET_DIR)",
        purpose: "private per-task-run Cargo target dir; churns constantly by design",
    }
    /// Compatibility-only sccache store.
    Sccache {
        dir: "sccache",
        namespacing: CacheRootNamespacing::PerProject,
        reserved_children: [],
        declared_by: "djinn_k8s::job::common_cache_env_vars (SCCACHE_DIR)",
        purpose: "writable, Landlock-allowed fallback for a repo tool that invokes sccache itself; djinn build pods clear RUSTC_WRAPPER and never populate it",
    }
    /// Per-project XDG cache home (durable output stash, SCIP indexer cache).
    Xdg {
        dir: "xdg",
        namespacing: CacheRootNamespacing::PerProject,
        reserved_children: [],
        declared_by: "djinn_k8s::job::common_cache_env_vars (XDG_CACHE_HOME)",
        purpose: "per-project XDG cache home: durable output stash and SCIP indexer cache",
    }
    /// Host-process cache namespace used when XDG_CACHE_HOME is unset.
    Djinn {
        dir: "djinn",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_core::paths::{output_stash_root,scip_indexer_cache_root}",
        purpose: "host-process durable output stash and SCIP indexer cache fallback",
    }
    /// Shared Cargo registry index and crate sources.
    Cargo {
        dir: "cargo",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_k8s::job::common_cache_env_vars (CARGO_HOME)",
        purpose: "content-addressed Cargo registry index and crate sources, shared across projects",
    }
    /// Shared Go module and build caches.
    Go {
        dir: "go",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_image_builder::dockerfile (GOMODCACHE / GOCACHE / GOBIN image ENV)",
        purpose: "content-addressed Go module cache, build cache and GOBIN",
    }
    /// Shared pnpm home and store.
    Pnpm {
        dir: "pnpm",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_image_builder::dockerfile (PNPM_HOME image ENV)",
        purpose: "content-addressed pnpm global dir and store",
    }
    /// Shared npm cache.
    Npm {
        dir: "npm",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_image_builder::dockerfile (npm_config_cache image ENV)",
        purpose: "content-addressed npm package cache",
    }
    /// Shared yarn cache.
    Yarn {
        dir: "yarn",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_image_builder::dockerfile (YARN_CACHE_FOLDER image ENV)",
        purpose: "content-addressed yarn package cache",
    }
    /// Shared pip cache.
    Pip {
        dir: "pip",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_image_builder::dockerfile (PIP_CACHE_DIR image ENV)",
        purpose: "content-addressed pip wheel/http cache",
    }
    /// Shared uv cache.
    Uv {
        dir: "uv",
        namespacing: CacheRootNamespacing::Shared,
        reserved_children: [],
        declared_by: "djinn_image_builder::dockerfile (UV_CACHE_DIR image ENV)",
        purpose: "content-addressed uv package cache",
    }
}

impl CacheRootId {
    /// Directory name of this root under the cache mount.
    pub const fn dir_name(self) -> &'static str {
        self.spec().dir_name
    }

    /// How this root's immediate children are keyed.
    pub const fn namespacing(self) -> CacheRootNamespacing {
        self.spec().namespacing
    }

    /// Host-side (server/coordinator pod) absolute path of this root.
    ///
    /// Always derived from [`cache_root`], never from [`JOB_POD_CACHE_MOUNT`]:
    /// the server pod has no `/cache`, so a Job-pod literal here silently
    /// operates on a nonexistent directory.
    pub fn host_path(self) -> PathBuf {
        cache_root().join(self.dir_name())
    }

    /// Resolve a directory name observed directly under the cache root to its
    /// manifest entry, if any. `None` means "not a cache root djinn declares".
    pub fn from_dir_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|root| root.dir_name() == name)
    }
}

/// Host-side directory holding per-task-run private Cargo target dirs.
///
/// This is the server/coordinator pod's view of the same directory the Job
/// pods know as `/cache/cargo-target-runs`. The periodic sweep and the host
/// teardown backstop both resolve their root here so they operate on the
/// directory that is actually mounted in the server pod.
pub fn cargo_target_runs_root() -> PathBuf {
    CacheRootId::CargoTargetRuns.host_path()
}

/// Host-side directory holding the warm, per-project Cargo target bases.
///
/// The server/coordinator pod's view of the directory Job pods know as
/// `/cache/cargo-target` (`djinn_agent_worker::cargo_target_seed::WARM_BASE_ROOT`).
/// Host-side inspection MUST resolve through here for the same reason
/// [`cargo_target_runs_root`] does: the Job-pod path does not exist in the
/// server pod, so a hardcoded `/cache/cargo-target` silently reports nothing.
pub fn cargo_target_root() -> PathBuf {
    CacheRootId::CargoTarget.host_path()
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
    CacheRootId::Sccache.host_path()
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
    CacheRootId::Xdg.host_path()
}

/// [`xdg_cache_root`] relative to an explicit cache mount.
///
/// Callers that already hold a resolved cache root (a coordinator context, a
/// test tempdir) use this so the `xdg` path component lives in exactly one
/// place.
pub fn xdg_cache_root_under(cache_root: &Path) -> PathBuf {
    cache_root.join(CacheRootId::Xdg.dir_name())
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
    CacheRootId::Djinn.host_path().join("output_stash")
}

/// Host-side SCIP indexer cache root for a process with no `$XDG_CACHE_HOME`.
///
/// The last-resort fallback for `djinn_graph`'s cache-root resolution. The
/// alternative it replaced was `$HOME/.cache/djinn/scip-indexer` (ephemeral,
/// wrong tree) and — when `$HOME` was unset too — a **cwd-relative**
/// `.cache/djinn/scip-indexer`, which silently scatters cache state into
/// whatever directory the process happened to start in.
pub fn scip_indexer_cache_root() -> PathBuf {
    CacheRootId::Djinn.host_path().join("scip-indexer")
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

    /// The named accessors must be views onto manifest entries, not a second,
    /// parallel list. If a root gains an accessor without a manifest entry, the
    /// cache-root inspector would report the live root as unrecognised.
    #[test]
    fn named_accessors_resolve_to_manifest_entries() {
        for (path, id) in [
            (cargo_target_root(), CacheRootId::CargoTarget),
            (cargo_target_runs_root(), CacheRootId::CargoTargetRuns),
            (sccache_root(), CacheRootId::Sccache),
            (xdg_cache_root(), CacheRootId::Xdg),
        ] {
            assert_eq!(path, id.host_path());
            assert_eq!(
                CacheRootId::from_dir_name(id.dir_name()),
                Some(id),
                "{} must be resolvable from its directory name",
                id.dir_name()
            );
        }
        assert_eq!(
            output_stash_root().parent(),
            Some(CacheRootId::Djinn.host_path().as_path())
        );
        assert_eq!(
            scip_indexer_cache_root().parent(),
            Some(CacheRootId::Djinn.host_path().as_path())
        );
    }

    #[test]
    fn manifest_entries_are_unique_single_segment_names() {
        let mut seen = std::collections::BTreeSet::new();
        for root in CacheRootId::ALL.iter().copied() {
            let name = root.dir_name();
            assert!(
                seen.insert(name),
                "duplicate cache root directory name {name}"
            );
            assert!(!name.is_empty(), "cache root name must not be empty");
            assert!(
                !name.contains('/'),
                "{name} is a nested path; the manifest describes cache ROOTS only"
            );
            assert_eq!(
                root.job_pod_path(),
                format!("{JOB_POD_CACHE_MOUNT}/{name}"),
                "job-pod path must be the cache mount joined with the root name"
            );
            assert_eq!(
                root.host_path(),
                cache_root().join(name),
                "host path must hang off the host cache root"
            );
        }
    }

    /// Every manifest entry must be reachable from `ALL`. The macro makes this
    /// structurally true; the assertion pins it so a future hand-written
    /// `impl` cannot quietly reintroduce a hand-synced list.
    #[test]
    fn manifest_covers_every_declared_variant() {
        for root in CacheRootId::ALL.iter().copied() {
            let spec = root.spec();
            assert!(
                !spec.declared_by.is_empty(),
                "{} must name where the platform renders it",
                spec.dir_name
            );
            assert!(
                !spec.purpose.is_empty(),
                "{} must state what it holds",
                spec.dir_name
            );
        }
        // The set of roots that carry tenant namespaces is what the orphan
        // reconciler descends into. Pinning it makes adding a per-project root
        // a visible, reviewed change rather than an incidental one.
        let per_project: Vec<&str> = CacheRootId::ALL
            .iter()
            .copied()
            .filter(|root| root.namespacing() == CacheRootNamespacing::PerProject)
            .map(CacheRootId::dir_name)
            .collect();
        assert_eq!(per_project, vec!["cargo-target", "sccache", "xdg"]);
        let per_run: Vec<&str> = CacheRootId::ALL
            .iter()
            .copied()
            .filter(|root| root.namespacing() == CacheRootNamespacing::PerRun)
            .map(CacheRootId::dir_name)
            .collect();
        assert_eq!(per_run, vec!["cargo-target-runs"]);
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
        assert_eq!(xdg_cache_root(), CacheRootId::Xdg.host_path());
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

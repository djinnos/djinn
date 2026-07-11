//! ADR-050 Chunk C: server-managed canonical indexing worktree.
//!
//! Maintains a dedicated `.djinn/worktrees/_index/` checkout per project,
//! pinned to `origin/main` HEAD.  This is the only location used for SCIP
//! indexing under ADR-050: workers, the user's project root, and per-task
//! worktrees never run the indexer themselves.  The Architect and Chat
//! resolve all code-reading tools (`read`, `shell`, `lsp`, `code_graph`)
//! against this directory rather than the user's working tree.
//!
//! ## Lifecycle
//!
//! - First use per project: `git -C <project_root> worktree add
//!   .djinn/worktrees/_index <origin/main>`.
//! - Subsequent uses: `git fetch origin main` (subject to a 60s cooldown
//!   per project) followed by `git -C .djinn/worktrees/_index reset --hard
//!   origin/main`.
//! - The reserved `_`-prefix marks the directory as server infrastructure
//!   so it is excluded from task worktree enumeration.
//!
//! ## Concurrency
//!
//! Last-fetch timestamps are tracked per project in a process-wide map.
//! Subprocess invocations for SCIP indexing must additionally acquire the
//! server-wide `IndexerLock` (see `AppState::indexer_lock`) before
//! spawning, but `IndexTree` itself only governs git state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use djinn_core::clock::{Clock, SystemClock};
use djinn_git::CommandOutput;

/// Reserved file-name prefix for server-managed entries under
/// `.djinn/worktrees/`.  Task-worktree enumeration paths must skip any entry
/// whose name starts with this character (ADR-050 §3).
pub const RESERVED_WORKTREE_PREFIX: char = '_';

/// Subdirectory under a project's `.djinn/worktrees/` that hosts the
/// canonical-main indexing checkout.
pub const INDEX_TREE_DIR_NAME: &str = "_index";

/// Sibling directory used as a dedicated `CARGO_TARGET_DIR` when running
/// indexer-invoked Rust builds against the index tree **in the in-process
/// (dev/peer) path**.  Sharing sccache with the rest of the workspace is
/// preserved automatically by the user's sccache configuration; this directory
/// only isolates the build outputs so an indexer run never corrupts the host
/// server's own target directory.
///
/// In the K8s warm-Pod path this directory is **not** used: the Pod routes
/// `CARGO_TARGET_DIR=/cache/cargo-target/<project>` (a persistent PVC
/// pre-warmed by `warm_cargo_target_base`) and the indexer inherits that
/// warmed base instead — see [`IndexTreeHandle::indexer_target_dir_override`].
pub const INDEX_TREE_TARGET_DIR_NAME: &str = "_index-target";

/// Default fetch cooldown.  `IndexTreeHandle::fetch_if_stale` skips a fetch
/// when the most recent fetch for the same project is younger than this.
pub const DEFAULT_FETCH_COOLDOWN: Duration = Duration::from_secs(60);

/// Returns `true` when `entry_name` should be treated as a reserved server
/// infrastructure entry under `.djinn/worktrees/` (ADR-050 §3).
#[inline]
pub fn is_reserved_worktree_entry(entry_name: &str) -> bool {
    entry_name.starts_with(RESERVED_WORKTREE_PREFIX)
}

/// Pure resolution behind [`IndexTreeHandle::indexer_target_dir_override`].
///
/// Returns `None` (inherit the ambient warmed `CARGO_TARGET_DIR`) only in
/// pod-workspace mode when the process already carries a `CARGO_TARGET_DIR`;
/// otherwise returns `Some(isolated_target_dir)` so the in-process path keeps
/// isolating indexer builds from the host server's target directory. Kept
/// side-effect-free so it can be unit-tested without mutating process env.
fn resolve_indexer_target_dir_override(
    pod_workspace_mode: bool,
    cargo_target_dir_env_present: bool,
    isolated_target_dir: &Path,
) -> Option<PathBuf> {
    if pod_workspace_mode && cargo_target_dir_env_present {
        None
    } else {
        Some(isolated_target_dir.to_path_buf())
    }
}

fn last_fetch_map() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Process-wide async mutex serializing concurrent `IndexTree::ensure`
/// invocations against the same project.  Without this, two callers can
/// race the initial `git worktree add` and one races to a "dir already
/// exists" error.
fn ensure_locks()
-> &'static tokio::sync::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> {
    static MAP: OnceLock<
        tokio::sync::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = OnceLock::new();
    MAP.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

async fn ensure_lock_for(project_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut map = ensure_locks().lock().await;
    map.entry(project_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Process-wide handle to a project's canonical indexing worktree.  Cheap to
/// construct via [`IndexTree::ensure`].
#[derive(Debug, Clone)]
pub struct IndexTreeHandle {
    project_id: String,
    project_root: PathBuf,
    index_tree_path: PathBuf,
    target_dir: PathBuf,
    commit_sha: String,
    /// `true` when this handle was constructed in K8s warm-Pod workspace mode
    /// (`DJINN_PROJECT_ROOT` set at [`IndexTree::ensure`] time). In that mode
    /// the Pod's whole filesystem is the index tree and the persistent warm
    /// cargo target base is routed via the process's `CARGO_TARGET_DIR`, so
    /// indexer builds inherit it rather than isolating into `_index-target`.
    pod_workspace_mode: bool,
}

impl IndexTreeHandle {
    /// Filesystem path of the index-tree checkout.
    pub fn path(&self) -> &Path {
        &self.index_tree_path
    }

    /// Dedicated `CARGO_TARGET_DIR` for indexer-invoked builds against the
    /// index tree in the in-process path. Prefer
    /// [`Self::indexer_target_dir_override`] at the indexer call site — it
    /// applies the pod-mode carve-out.
    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    /// Resolve the `CARGO_TARGET_DIR` override to install for indexer-invoked
    /// Rust builds against this index tree, or `None` to leave the ambient
    /// `CARGO_TARGET_DIR` untouched.
    ///
    /// - **In-process (dev/peer) mode** — always returns
    ///   `Some(<_index-target>)`. The `CargoTargetDirGuard` installed from
    ///   this value isolates indexer builds from the host server's own target
    ///   directory (the guard's SAFETY contract), so the server's own builds
    ///   are never corrupted by an indexer run.
    /// - **Pod-workspace warm mode** with an ambient `CARGO_TARGET_DIR` — the
    ///   warm Pod routes `CARGO_TARGET_DIR=/cache/cargo-target/<project>` (a
    ///   persistent PVC pre-seeded and pre-warmed by `warm_cargo_target_base`
    ///   with the exact compiles `rust-analyzer scip` needs). Returns `None`
    ///   so the indexer inherits that warmed base instead of recompiling the
    ///   whole workspace into the Pod's ephemeral emptyDir `_index-target`
    ///   every warm — the bug that blew the 1200s indexer timeout and yielded
    ///   0 nodes. The Pod's per-run isolation makes the host-corruption
    ///   concern moot: nothing else shares the Pod's process env, so skipping
    ///   the guard is safe.
    /// - **Pod-workspace mode without** an ambient `CARGO_TARGET_DIR` (e.g. a
    ///   non-Rust repo, or the warm-cache env wiring absent) — falls back to
    ///   `Some(<_index-target>)`, leaving behaviour unchanged from before.
    pub fn indexer_target_dir_override(&self) -> Option<PathBuf> {
        resolve_indexer_target_dir_override(
            self.pod_workspace_mode,
            std::env::var_os("CARGO_TARGET_DIR").is_some(),
            &self.target_dir,
        )
    }

    /// Project ID this index tree belongs to.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Project root the canonical fetch is performed against.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Commit SHA the checkout currently points at (the result of the most
    /// recent `reset_to_origin_main`, or HEAD at construction time).
    pub fn commit_sha(&self) -> &str {
        &self.commit_sha
    }

    /// Run `git fetch origin main` against the project root unless a fetch
    /// already happened within `cooldown`.  Tracks the last-fetch timestamp
    /// per project in a process-wide map.
    pub async fn fetch_if_stale(&self, cooldown: Duration) -> Result<bool> {
        {
            let map = last_fetch_map().lock().expect("poisoned fetch map");
            if let Some(last) = map.get(&self.project_id)
                && last.elapsed() < cooldown
            {
                return Ok(false);
            }
        }
        run_git(&self.project_root, &["fetch", "origin", "main"]).await?;
        {
            let mut map = last_fetch_map().lock().expect("poisoned fetch map");
            map.insert(self.project_id.clone(), SystemClock::new().now_instant());
        }
        Ok(true)
    }

    /// `git -C <index_tree> reset --hard origin/main` and refresh the cached
    /// commit SHA.
    pub async fn reset_to_origin_main(&mut self) -> Result<()> {
        run_git(&self.index_tree_path, &["reset", "--hard", "origin/main"]).await?;
        self.commit_sha = read_head_sha(&self.index_tree_path).await?;
        Ok(())
    }
}

/// API entry point: idempotently ensure a project has a managed indexing
/// worktree at `<project_root>/.djinn/worktrees/_index/`.
pub struct IndexTree;

impl IndexTree {
    /// Ensure the index tree exists for `project_id` rooted at `project_root`.
    /// Creates `.djinn/worktrees/_index/` via `git worktree add` on first use.
    ///
    /// When `DJINN_PROJECT_ROOT` is set (the K8s warm-Pod path: the Pod
    /// clones the mirror directly into an emptyDir workspace before
    /// invoking the binary), we treat `project_root` as the canonical
    /// index location and skip the `.djinn/worktrees/_index` worktree
    /// dance entirely. Pod-per-task isolation makes the nested worktree
    /// redundant — the Pod's whole filesystem is already the "index
    /// tree" for the warm run.
    pub async fn ensure(project_id: &str, project_root: &Path) -> Result<IndexTreeHandle> {
        let worktrees_dir = project_root.join(".djinn").join("worktrees");
        let target_dir = worktrees_dir.join(INDEX_TREE_TARGET_DIR_NAME);

        let pod_workspace_mode = std::env::var("DJINN_PROJECT_ROOT").is_ok();
        let index_tree_path = if pod_workspace_mode {
            project_root.to_path_buf()
        } else {
            worktrees_dir.join(INDEX_TREE_DIR_NAME)
        };

        // Serialise concurrent `ensure` calls for the same project so we
        // do not race the initial `git worktree add`.  Different projects
        // remain independent.
        let lock = ensure_lock_for(project_id).await;
        let _permit = lock.lock().await;

        if pod_workspace_mode {
            // Validate that the caller did actually clone into this path.
            // If not, fail clearly instead of silently running against an
            // empty directory.
            if !index_tree_path.join(".git").exists() {
                return Err(anyhow!(
                    "DJINN_PROJECT_ROOT={} has no .git directory — the warm Pod's clone step must run before the binary",
                    index_tree_path.display()
                ));
            }
        } else if !index_tree_path.join(".git").exists() {
            tokio::fs::create_dir_all(&worktrees_dir)
                .await
                .with_context(|| format!("create {}", worktrees_dir.display()))?;

            // Best-effort: prune stale worktree metadata before adding.
            let _ = run_git(project_root, &["worktree", "prune"]).await;

            // Use `origin/main` as the starting commit so the worktree is
            // pinned to the canonical view from the very first call.  Fall
            // back to `HEAD` if `origin/main` is not yet known (fresh clone
            // / brand-new project).
            let attempt_origin = run_git(
                project_root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    index_tree_path.to_string_lossy().as_ref(),
                    "origin/main",
                ],
            )
            .await;

            if attempt_origin.is_err() {
                let attempt_head = run_git(
                    project_root,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        index_tree_path.to_string_lossy().as_ref(),
                        "HEAD",
                    ],
                )
                .await;
                if attempt_head.is_err() && !index_tree_path.join(".git").exists() {
                    return Err(anyhow!(
                        "create index tree at {}",
                        index_tree_path.display()
                    ));
                }
            }
        }

        let commit_sha = read_head_sha(&index_tree_path).await?;

        Ok(IndexTreeHandle {
            project_id: project_id.to_string(),
            project_root: project_root.to_path_buf(),
            index_tree_path,
            target_dir,
            commit_sha,
            pod_workspace_mode,
        })
    }
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    // `run_git_command_in` returns `Err(GitError::CommandFailed { .. })`
    // on non-zero exit, so a successful `Ok` here means git exited 0 — we
    // surface its trimmed stdout regardless of stderr (git often emits
    // advisory hints to stderr on a successful run).
    let CommandOutput { stdout, .. } = djinn_git::run_git_command_in(cwd, owned_args)
        .await
        .with_context(|| format!("spawn git {} in {}", args.join(" "), cwd.display()))?;
    Ok(stdout.trim().to_string())
}

async fn read_head_sha(path: &Path) -> Result<String> {
    run_git(path, &["rev-parse", "HEAD"]).await
}

/// Test-only helper to clear the process-wide last-fetch map between
/// independent test cases.
#[cfg(test)]
pub fn reset_last_fetch_for_tests() {
    last_fetch_map().lock().expect("poisoned fetch map").clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::workspace_tempdir;

    #[test]
    fn indexer_target_dir_override_in_process_mode_isolates() {
        // Dev/peer (in-process) mode: always isolate into `_index-target` so
        // an indexer run can't corrupt the host server's own target dir. This
        // holds regardless of whether the ambient CARGO_TARGET_DIR is set.
        let isolated = PathBuf::from("/proj/.djinn/worktrees/_index-target");
        assert_eq!(
            resolve_indexer_target_dir_override(false, false, &isolated),
            Some(isolated.clone()),
            "non-pod, no ambient CARGO_TARGET_DIR must isolate"
        );
        assert_eq!(
            resolve_indexer_target_dir_override(false, true, &isolated),
            Some(isolated.clone()),
            "non-pod with ambient CARGO_TARGET_DIR must still isolate"
        );
    }

    #[test]
    fn indexer_target_dir_override_pod_mode_inherits_warmed_base() {
        // Warm-Pod mode WITH an ambient CARGO_TARGET_DIR (the warm base at
        // /cache/cargo-target/<project>): inherit it (None) so the indexer
        // reuses the pre-warmed target instead of recompiling into the Pod's
        // ephemeral `_index-target` every warm.
        let isolated = PathBuf::from("/workspace/proj/.djinn/worktrees/_index-target");
        assert_eq!(
            resolve_indexer_target_dir_override(true, true, &isolated),
            None,
            "pod mode with warmed CARGO_TARGET_DIR must inherit it"
        );
        // Warm-Pod mode WITHOUT an ambient CARGO_TARGET_DIR (non-Rust repo or
        // env wiring absent): fall back to isolation, unchanged from before.
        assert_eq!(
            resolve_indexer_target_dir_override(true, false, &isolated),
            Some(isolated),
            "pod mode without CARGO_TARGET_DIR must fall back to isolation"
        );
    }

    #[test]
    fn reserved_prefix_filter_matches_underscore_entries() {
        assert!(is_reserved_worktree_entry("_index"));
        assert!(is_reserved_worktree_entry("_index-target"));
        assert!(is_reserved_worktree_entry("_anything"));
        assert!(!is_reserved_worktree_entry("task-123"));
        assert!(!is_reserved_worktree_entry("worker"));
        assert!(!is_reserved_worktree_entry(".hidden"));
    }

    /// Builds a tiny on-disk git repository with a single commit so we can
    /// exercise the index-tree git plumbing without touching a remote.
    async fn make_repo(tmp: &Path) -> PathBuf {
        let project_root = tmp.join("repo");
        tokio::fs::create_dir_all(&project_root).await.unwrap();
        run_git(&project_root, &["init", "-q", "-b", "main"])
            .await
            .unwrap();
        run_git(&project_root, &["config", "user.email", "t@t"])
            .await
            .unwrap();
        run_git(&project_root, &["config", "user.name", "t"])
            .await
            .unwrap();
        tokio::fs::write(project_root.join("a.txt"), "hi")
            .await
            .unwrap();
        run_git(&project_root, &["add", "a.txt"]).await.unwrap();
        run_git(&project_root, &["commit", "-q", "-m", "init"])
            .await
            .unwrap();
        project_root
    }

    #[tokio::test]
    async fn ensure_creates_index_tree_on_first_call_and_is_idempotent() {
        let tmp = workspace_tempdir("index-tree-");
        let project_root = make_repo(tmp.path()).await;

        let handle = IndexTree::ensure("p1", &project_root).await.unwrap();
        assert!(handle.path().join(".git").exists());
        assert!(!handle.commit_sha().is_empty());

        // Second call must reuse the existing checkout, not error.
        let handle2 = IndexTree::ensure("p1", &project_root).await.unwrap();
        assert_eq!(handle.path(), handle2.path());
        assert_eq!(handle.commit_sha(), handle2.commit_sha());
    }

    #[tokio::test]
    async fn fetch_if_stale_honors_cooldown() {
        reset_last_fetch_for_tests();
        let tmp = workspace_tempdir("index-tree-");
        let project_root = make_repo(tmp.path()).await;
        // Add a fake `origin` so `fetch` has somewhere to go.  We use a
        // bare clone of the project itself so the fetch succeeds locally
        // without any network access.
        let bare = tmp.path().join("origin.git");
        run_git(
            &project_root,
            &[
                "clone",
                "--bare",
                "-q",
                project_root.to_string_lossy().as_ref(),
                bare.to_string_lossy().as_ref(),
            ],
        )
        .await
        .unwrap();
        run_git(
            &project_root,
            &["remote", "add", "origin", bare.to_string_lossy().as_ref()],
        )
        .await
        .unwrap();

        let handle = IndexTree::ensure("p2", &project_root).await.unwrap();

        // First call performs a fetch.
        let fetched_first = handle
            .fetch_if_stale(Duration::from_secs(60))
            .await
            .unwrap();
        assert!(fetched_first, "first call should fetch");

        // Second call within cooldown short-circuits.
        let fetched_second = handle
            .fetch_if_stale(Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            !fetched_second,
            "second call within cooldown must skip fetch"
        );

        // Zero cooldown forces a fetch again.
        let fetched_third = handle.fetch_if_stale(Duration::from_secs(0)).await.unwrap();
        assert!(fetched_third, "zero cooldown must allow fetch");
    }
}

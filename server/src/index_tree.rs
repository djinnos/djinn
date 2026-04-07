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
use tokio::process::Command;

/// Reserved file-name prefix for server-managed entries under
/// `.djinn/worktrees/`.  Task-worktree enumeration paths must skip any entry
/// whose name starts with this character (ADR-050 §3).
pub const RESERVED_WORKTREE_PREFIX: char = '_';

/// Subdirectory under a project's `.djinn/worktrees/` that hosts the
/// canonical-main indexing checkout.
pub const INDEX_TREE_DIR_NAME: &str = "_index";

/// Sibling directory used as a dedicated `CARGO_TARGET_DIR` when running
/// indexer-invoked Rust builds against the index tree.  Sharing sccache with
/// the rest of the workspace is preserved automatically by the user's sccache
/// configuration; this directory only isolates the build outputs.
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

fn last_fetch_map() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
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
}

impl IndexTreeHandle {
    /// Filesystem path of the index-tree checkout.
    pub fn path(&self) -> &Path {
        &self.index_tree_path
    }

    /// Dedicated `CARGO_TARGET_DIR` for indexer-invoked builds against the
    /// index tree.
    pub fn target_dir(&self) -> &Path {
        &self.target_dir
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
            if let Some(last) = map.get(&self.project_id) {
                if last.elapsed() < cooldown {
                    return Ok(false);
                }
            }
        }
        run_git(&self.project_root, &["fetch", "origin", "main"]).await?;
        {
            let mut map = last_fetch_map().lock().expect("poisoned fetch map");
            map.insert(self.project_id.clone(), Instant::now());
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
    pub async fn ensure(project_id: &str, project_root: &Path) -> Result<IndexTreeHandle> {
        let worktrees_dir = project_root.join(".djinn").join("worktrees");
        let index_tree_path = worktrees_dir.join(INDEX_TREE_DIR_NAME);
        let target_dir = worktrees_dir.join(INDEX_TREE_TARGET_DIR_NAME);

        if !index_tree_path.join(".git").exists() {
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
                run_git(
                    project_root,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        index_tree_path.to_string_lossy().as_ref(),
                        "HEAD",
                    ],
                )
                .await
                .with_context(|| {
                    format!("create index tree at {}", index_tree_path.display())
                })?;
            }
        }

        let commit_sha = read_head_sha(&index_tree_path).await?;

        Ok(IndexTreeHandle {
            project_id: project_id.to_string(),
            project_root: project_root.to_path_buf(),
            index_tree_path,
            target_dir,
            commit_sha,
        })
    }
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawn git {} in {}", args.join(" "), cwd.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn read_head_sha(path: &Path) -> Result<String> {
    run_git(path, &["rev-parse", "HEAD"]).await
}

/// Test-only helper to clear the process-wide last-fetch map between
/// independent test cases.
#[cfg(test)]
pub(crate) fn reset_last_fetch_for_tests() {
    last_fetch_map()
        .lock()
        .expect("poisoned fetch map")
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

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
        tokio::fs::write(project_root.join("a.txt"), "hi").await.unwrap();
        run_git(&project_root, &["add", "a.txt"]).await.unwrap();
        run_git(&project_root, &["commit", "-q", "-m", "init"])
            .await
            .unwrap();
        project_root
    }

    #[tokio::test]
    async fn ensure_creates_index_tree_on_first_call_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let project_root = make_repo(tmp.path()).await;
        // Add a fake `origin` so `fetch` has somewhere to go.  We use a
        // bare clone of the project itself so the fetch succeeds locally
        // without any network access.
        let bare = tmp.path().join("origin.git");
        run_git(
            &project_root,
            &["clone", "--bare", "-q", project_root.to_string_lossy().as_ref(),
              bare.to_string_lossy().as_ref()],
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

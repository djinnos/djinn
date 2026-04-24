//! Persistent per-project read-only working-tree clones for the chat
//! subsystem.
//!
//! Replaces the `(chat_session_id, project_id)`-keyed
//! `ChatCloneCache`. The chat shell sandbox denies every write path
//! inside the clone, so the working tree is effectively read-only to
//! chat consumers. Per-session isolation was unnecessary: one
//! `--local --shared` clone per project, kept in sync with the local
//! bare mirror, serves every chat session simultaneously.
//!
//! Layout:
//! ```text
//! {workspaces_root}/{project_id}/
//! ```
//!
//! ## Lifecycle
//!
//!  - [`WorkspaceStore::ensure_workspace`] is called per tool-call from
//!    the chat resolver. It returns the on-disk path after confirming
//!    the clone exists and is up-to-date relative to the bare mirror.
//!  - [`WorkspaceStore::sync_workspace`] runs `git fetch origin` +
//!    `git reset --hard origin/<default_branch>`. The mirror fetcher
//!    calls it after every successful `MirrorManager::fetch_mirror`
//!    that advanced refs.
//!  - Workspaces persist across restarts; [`AppState::initialize`]
//!    kicks a detached backfill task that walks
//!    [`ProjectRepository::list`] on boot so a cold server warms its
//!    working trees once rather than per first-tool-call.
//!
//! ## Concurrency & torn reads
//!
//! A per-project `Mutex` serialises `ensure_workspace` /
//! `sync_workspace` so two calls never race a duplicate clone or a
//! duplicate reset. Readers (chat tool calls that `grep`, `cat`, etc.)
//! do NOT take the lock — we tolerate a brief torn read if a
//! `sync_workspace` is rewriting the tree at the same moment. This is
//! a deliberate tradeoff: coupling every chat read to sync latency via
//! a `RwLock` would make chat tool calls block on mirror-fetch ticks
//! for no real safety gain (torn reads during `grep` are not
//! catastrophic — the tool call simply returns partially-stale
//! output). If this ever becomes a problem, upgrading to a
//! per-project `RwLock` with `sync_workspace` taking the write and
//! chat tool calls taking the read is a localised change.
//!
//! ## Path-traversal safety
//!
//! `project_id` flows in from tool args (via the resolver's DB
//! lookup). We still re-validate the UUID shape at this boundary as a
//! defence-in-depth layer — the DB could conceivably hold a
//! non-UUID-shaped id from a legacy project row, and we must not
//! concatenate such a value into a path segment. Same regex as
//! `ChatCloneCache` used internally.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_git::run_git_command;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::mirror::MirrorManager;

/// Resolve the workspaces root from environment, mirroring
/// [`crate::mirror::mirrors_root`]:
///  * `$DJINN_HOME/workspaces` when `DJINN_HOME` is set,
///  * else `$HOME/.djinn/workspaces`,
///  * else `/tmp/.djinn/workspaces`.
pub fn workspaces_root() -> PathBuf {
    if let Ok(djinn_home) = std::env::var("DJINN_HOME")
        && !djinn_home.is_empty()
    {
        return PathBuf::from(djinn_home).join("workspaces");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("workspaces")
}

/// Canonical on-disk path of a project's persistent workspace:
/// `{workspaces_root}/{project_id}`.
///
/// Matches [`crate::mirror::mirror_path_for`] in spirit — a single
/// resolver so downstream crates don't reconstruct paths by hand.
pub fn workspace_path_for(project_id: &str) -> PathBuf {
    workspaces_root().join(project_id)
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("invalid project id (must be UUID shape)")]
    InvalidId,

    #[error("mirror for {0} does not exist; cannot create workspace")]
    MirrorMissing(String),

    #[error("git: {0}")]
    Git(String),

    #[error("i/o: {0}")]
    Io(#[from] io::Error),
}

/// Persistent per-project read-only working-tree clones.
pub struct WorkspaceStore {
    root: PathBuf,
    mirror_manager: Arc<MirrorManager>,
    /// Per-project single-flight lock — `ensure_workspace` and
    /// `sync_workspace` for the same project serialise on this. Same
    /// pattern as [`MirrorManager::lock_for`].
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl WorkspaceStore {
    pub fn new(root: PathBuf, mirror_manager: Arc<MirrorManager>) -> Self {
        Self {
            root,
            mirror_manager,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical workspace path for a given project id.
    pub fn workspace_path(&self, project_id: &str) -> PathBuf {
        self.root.join(project_id)
    }

    async fn lock_for(&self, project_id: &str) -> Arc<Mutex<()>> {
        let mut guard = self.locks.lock().await;
        guard
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Ensure a working-tree clone exists at
    /// `{root}/{project_id}`, creating it via `git clone --local
    /// --shared` from the local bare mirror when absent, otherwise
    /// running [`Self::sync_workspace`] to catch up to the mirror's
    /// current `default_branch` head.
    ///
    /// Returns the on-disk path.
    pub async fn ensure_workspace(
        &self,
        project_id: &str,
        default_branch: &str,
    ) -> Result<PathBuf, WorkspaceError> {
        if !is_uuid(project_id) {
            return Err(WorkspaceError::InvalidId);
        }

        let target = self.workspace_path(project_id);
        let mirror = self.mirror_manager.mirror_path(project_id);
        if !mirror.exists() {
            return Err(WorkspaceError::MirrorMissing(project_id.to_string()));
        }

        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;

        if target.exists() {
            // Fast-path: sync so the next reader sees fresh refs.
            // We already hold the per-project lock, so inline the
            // sync body rather than re-entering `sync_workspace`.
            debug!(
                project_id,
                branch = default_branch,
                "ensure_workspace: fast-path sync"
            );
            run_git_command(
                target.clone(),
                vec!["fetch".into(), "origin".into()],
            )
            .await
            .map_err(|e| git_err("git fetch origin", e))?;
            run_git_command(
                target.clone(),
                vec![
                    "reset".into(),
                    "--hard".into(),
                    format!("origin/{default_branch}"),
                ],
            )
            .await
            .map_err(|e| git_err("git reset --hard", e))?;
            return Ok(target);
        }

        tokio::fs::create_dir_all(&self.root).await?;

        info!(project_id, path = ?target, branch = default_branch, "cloning persistent workspace");

        run_git_command(
            self.root.clone(),
            vec![
                "clone".into(),
                "--local".into(),
                "--shared".into(),
                "--branch".into(),
                default_branch.to_string(),
                mirror.display().to_string(),
                target.display().to_string(),
            ],
        )
        .await
        .map_err(|e| git_err("git clone --local --shared", e))?;

        Ok(target)
    }

    /// Fast-forward the working tree to the mirror's current
    /// `default_branch` head via
    /// `git fetch origin` (origin is the local bare mirror, not
    /// GitHub) + `git reset --hard origin/<default_branch>`.
    ///
    /// The workspace is treated as strictly read-only by outside
    /// consumers: no local commits, no branch switches, so a hard
    /// reset is always safe.
    pub async fn sync_workspace(
        &self,
        project_id: &str,
        default_branch: &str,
    ) -> Result<(), WorkspaceError> {
        if !is_uuid(project_id) {
            return Err(WorkspaceError::InvalidId);
        }
        let target = self.workspace_path(project_id);
        if !target.exists() {
            return Err(WorkspaceError::MirrorMissing(project_id.to_string()));
        }

        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;

        debug!(project_id, branch = default_branch, "syncing persistent workspace");

        run_git_command(
            target.clone(),
            vec!["fetch".into(), "origin".into()],
        )
        .await
        .map_err(|e| git_err("git fetch origin", e))?;

        run_git_command(
            target.clone(),
            vec![
                "reset".into(),
                "--hard".into(),
                format!("origin/{default_branch}"),
            ],
        )
        .await
        .map_err(|e| git_err("git reset --hard", e))?;

        Ok(())
    }
}

/// Shared `djinn_git::GitError` → `WorkspaceError::Git(String)` shim.
fn git_err(op: &str, err: djinn_git::GitError) -> WorkspaceError {
    match err {
        djinn_git::GitError::CommandFailed { stderr, .. } => {
            WorkspaceError::Git(format!("{op}: {stderr}"))
        }
        other => WorkspaceError::Git(format!("{op}: {other}")),
    }
}

/// UUID-shape check (8-4-4-4-12, lowercase hex + hyphens).
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    for (i, c) in s.chars().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != '-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Spin up a real source repo with one commit on `main`, register
    /// a bare mirror for `project_id`, and return the source TempDir
    /// guard (kept alive by the caller).
    async fn make_mirror(mm: &MirrorManager, project_id: &str) -> TempDir {
        let source = TempDir::new().unwrap();
        run_git_command(
            source.path().to_path_buf(),
            vec!["init".into(), "-b".into(), "main".into()],
        )
        .await
        .expect("git init");
        run_git_command(
            source.path().to_path_buf(),
            vec![
                "config".into(),
                "user.email".into(),
                "test@example.com".into(),
            ],
        )
        .await
        .expect("git config email");
        run_git_command(
            source.path().to_path_buf(),
            vec!["config".into(), "user.name".into(), "Test".into()],
        )
        .await
        .expect("git config name");
        tokio::fs::write(source.path().join("hello.txt"), b"hi\n")
            .await
            .expect("write file");
        run_git_command(
            source.path().to_path_buf(),
            vec!["add".into(), "hello.txt".into()],
        )
        .await
        .expect("git add");
        run_git_command(
            source.path().to_path_buf(),
            vec!["commit".into(), "-m".into(), "init".into()],
        )
        .await
        .expect("git commit");

        let url = source.path().display().to_string();
        mm.ensure_mirror(project_id, &url)
            .await
            .expect("ensure_mirror");
        source
    }

    #[tokio::test]
    async fn ensure_workspace_rejects_non_uuid() {
        let ws_root = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let store = WorkspaceStore::new(ws_root.path().to_path_buf(), mm);
        let err = store
            .ensure_workspace("../../etc/passwd", "main")
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidId), "got {err:?}");
    }

    #[tokio::test]
    async fn ensure_workspace_creates_clone_first_time() {
        let ws_root = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let project_id = "11111111-1111-1111-1111-111111111111";
        let _src = make_mirror(&mm, project_id).await;

        let store = WorkspaceStore::new(ws_root.path().to_path_buf(), mm);
        let path = store
            .ensure_workspace(project_id, "main")
            .await
            .expect("ensure_workspace");

        assert_eq!(path, ws_root.path().join(project_id));
        assert!(path.is_dir(), "workspace path should be a directory");
        assert!(
            path.join("hello.txt").is_file(),
            "committed file should be present in the workspace"
        );
    }

    #[tokio::test]
    async fn ensure_workspace_idempotent_second_call() {
        let ws_root = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let project_id = "22222222-2222-2222-2222-222222222222";
        let _src = make_mirror(&mm, project_id).await;

        let store = WorkspaceStore::new(ws_root.path().to_path_buf(), mm);
        let first = store
            .ensure_workspace(project_id, "main")
            .await
            .expect("first ensure");
        let second = store
            .ensure_workspace(project_id, "main")
            .await
            .expect("second ensure");

        assert_eq!(first, second);
        assert!(first.join("hello.txt").is_file());
    }

    #[tokio::test]
    async fn sync_workspace_fast_forwards_after_mirror_advance() {
        let ws_root = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let project_id = "33333333-3333-3333-3333-333333333333";
        let source = make_mirror(&mm, project_id).await;

        let store = WorkspaceStore::new(ws_root.path().to_path_buf(), Arc::clone(&mm));
        let path = store
            .ensure_workspace(project_id, "main")
            .await
            .expect("ensure_workspace");
        assert!(path.join("hello.txt").is_file());
        assert!(!path.join("second.txt").exists());

        // Add a new commit in the source repo, then fetch it into
        // the bare mirror so `origin/main` inside the workspace
        // advances on the next sync.
        tokio::fs::write(source.path().join("second.txt"), b"two\n")
            .await
            .unwrap();
        run_git_command(
            source.path().to_path_buf(),
            vec!["add".into(), "second.txt".into()],
        )
        .await
        .unwrap();
        run_git_command(
            source.path().to_path_buf(),
            vec!["commit".into(), "-m".into(), "two".into()],
        )
        .await
        .unwrap();

        let url = source.path().display().to_string();
        let advanced = mm
            .fetch_mirror(project_id, &url)
            .await
            .expect("fetch_mirror");
        assert!(advanced, "mirror fetch should have advanced");

        store
            .sync_workspace(project_id, "main")
            .await
            .expect("sync_workspace");

        assert!(
            path.join("second.txt").is_file(),
            "sync_workspace should surface the new commit's file in the working tree"
        );
    }

    #[tokio::test]
    async fn sync_workspace_missing_project_is_error() {
        let ws_root = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let store = WorkspaceStore::new(ws_root.path().to_path_buf(), mm);

        let err = store
            .sync_workspace("44444444-4444-4444-4444-444444444444", "main")
            .await
            .unwrap_err();
        assert!(
            matches!(err, WorkspaceError::MirrorMissing(_)),
            "got {err:?}"
        );
    }
}

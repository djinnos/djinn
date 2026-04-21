use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::workspace::Workspace;

/// Resolve the bare-mirror root directory from environment:
/// `$DJINN_HOME/mirrors` if set, else `$HOME/.djinn/mirrors`
/// (falling back to `/tmp/.djinn/mirrors` if `$HOME` is unset).
///
/// This is the canonical resolver. Every crate that needs a mirror path
/// must go through this helper (or [`MirrorManager::mirror_path`]) — do
/// NOT re-implement it locally, or the `.git` suffix will drift.
pub fn mirrors_root() -> PathBuf {
    if let Ok(djinn_home) = std::env::var("DJINN_HOME")
        && !djinn_home.is_empty()
    {
        return PathBuf::from(djinn_home).join("mirrors");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("mirrors")
}

/// Canonical on-disk path of a project's bare mirror: `{mirrors_root}/{project_id}.git`.
///
/// Use this from downstream crates (image-controller, k8s warmer) instead of
/// reconstructing the path by hand; this was the source of the "Cold forever"
/// bug where the suffix was dropped in two copies.
pub fn mirror_path_for(project_id: &str) -> PathBuf {
    mirrors_root().join(format!("{project_id}.git"))
}

#[derive(Debug, Error)]
pub enum MirrorError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("git: {0}")]
    Git(String),

    #[error("mirror for {0} does not exist; call ensure_mirror first")]
    Missing(String),
}

/// Owns the on-disk directory of per-project bare mirrors.
///
/// Layout:
/// ```text
/// {root}/
///   {project_id}.git/      <- bare mirror, source of truth for clones
/// ```
///
/// Single-flight serialization is per-project in-memory: concurrent
/// `ensure_mirror` / `fetch_mirror` calls for the same project queue behind
/// one another. Reads (`clone_ephemeral`) do not take the lock — git is safe
/// to clone-from while a fetch writes, since fetches are append-then-atomic-ref-update.
pub struct MirrorManager {
    root: PathBuf,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl MirrorManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mirror_path(&self, project_id: &str) -> PathBuf {
        self.root.join(format!("{project_id}.git"))
    }

    async fn lock_for(&self, project_id: &str) -> Arc<Mutex<()>> {
        let mut guard = self.locks.lock().await;
        guard
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Create the mirror directory if it doesn't exist by `git clone --bare`
    /// from `origin_url`. Authentication is carried in `origin_url`
    /// (e.g. `https://x-access-token:{token}@github.com/org/repo.git`).
    ///
    /// Idempotent: returns the existing mirror path if one is already present.
    pub async fn ensure_mirror(
        &self,
        project_id: &str,
        origin_url: &str,
    ) -> Result<PathBuf, MirrorError> {
        let mirror = self.mirror_path(project_id);
        if mirror.exists() {
            return Ok(mirror);
        }
        tokio::fs::create_dir_all(&self.root).await?;

        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;
        if mirror.exists() {
            return Ok(mirror);
        }

        info!(project_id, path = ?mirror, "cloning bare mirror");
        let output = Command::new("git")
            .args(["clone", "--bare", "--filter=blob:none", origin_url])
            .arg(&mirror)
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::Git(format!("git clone --bare: {stderr}")));
        }
        Ok(mirror)
    }

    /// Refresh an existing mirror via `git fetch --prune origin`.
    ///
    /// `origin_url` is passed on every call (rather than remembered from
    /// `ensure_mirror`) because installation tokens rotate. Callers mint a
    /// fresh token per fetch and embed it in the URL.
    pub async fn fetch_mirror(
        &self,
        project_id: &str,
        origin_url: &str,
    ) -> Result<(), MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;

        debug!(project_id, "fetching mirror");
        let set_url = Command::new("git")
            .args(["-C"])
            .arg(&mirror)
            .args(["remote", "set-url", "origin", origin_url])
            .output()
            .await?;
        if !set_url.status.success() {
            let stderr = String::from_utf8_lossy(&set_url.stderr);
            return Err(MirrorError::Git(format!("git remote set-url: {stderr}")));
        }

        // `git clone --bare --filter=blob:none` does NOT write a
        // `fetch` refspec into `remote.origin`, so a plain
        // `git fetch origin` ends up fetching objects for the default
        // branch only and never advances any local refs. That's why a
        // merged PR on the remote was invisible to stack detection —
        // the mirror's `refs/heads/main` was frozen at clone time.
        // Passing an explicit `+refs/heads/*:refs/heads/*` refspec
        // mirrors every head on every fetch, with force-update so
        // force-pushes and branch resets also sync. Tags follow so
        // release-detection stays current.
        let output = Command::new("git")
            .args(["-C"])
            .arg(&mirror)
            .args([
                "fetch",
                "--prune",
                "origin",
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::Git(format!("git fetch: {stderr}")));
        }
        Ok(())
    }

    /// Hardlinked local clone of the mirror, returned as a [`Workspace`].
    ///
    /// Uses `git clone --local --shared file://{mirror}` — object db is
    /// hardlinked + alternates, so the workspace is essentially free in disk
    /// terms. `branch` must exist in the mirror (base branch typically —
    /// callers create task-branches after clone via `git checkout -b`).
    pub async fn clone_ephemeral(
        &self,
        project_id: &str,
        branch: &str,
    ) -> Result<Workspace, MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let dir = TempDir::new()?;

        debug!(project_id, branch, path = ?dir.path(), "cloning ephemeral workspace");
        let output = Command::new("git")
            .args(["clone", "--local", "--shared", "--branch", branch])
            .arg(&mirror)
            .arg(dir.path())
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MirrorError::Git(format!("git clone --local: {stderr}")));
        }

        Ok(Workspace::new(dir, branch.to_string()))
    }
}

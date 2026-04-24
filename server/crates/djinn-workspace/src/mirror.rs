use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_git::run_git_command;
use tempfile::TempDir;
use thiserror::Error;
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

/// Convert a [`djinn_git::GitError`] into the legacy `MirrorError::Git(String)`
/// shape used by this module's public API. `op` is the high-level operation
/// label (e.g. `"git clone --bare"`) — matches the pre-refactor wording so
/// callers' log-greps keep working.
///
/// We pull `stderr` directly out of [`djinn_git::GitError::CommandFailed`] to
/// preserve the old `"{op}: {stderr}"` format; other variants fall back to
/// the full `Display`.
fn git_err_to_mirror(op: &str, err: djinn_git::GitError) -> MirrorError {
    match err {
        djinn_git::GitError::CommandFailed { stderr, .. } => {
            MirrorError::Git(format!("{op}: {stderr}"))
        }
        other => MirrorError::Git(format!("{op}: {other}")),
    }
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
        // cwd = self.root — just created above by create_dir_all, and the
        // destination `mirror` does not yet exist so it cannot itself be cwd.
        //
        // NOTE: no `--filter=blob:none`. Chat's ephemeral-clone path
        // needs full history locally so `git clone --local --shared`
        // is pure hardlink-and-alternates (no lazy-fetch round-trip
        // inside the sandboxed shell, which has no network). Mirrors
        // pre-dating this change are upgraded in place by
        // `ensure_full_mirror` on server boot.
        run_git_command(
            self.root.clone(),
            vec![
                "clone".into(),
                "--bare".into(),
                origin_url.to_string(),
                mirror.display().to_string(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git clone --bare", e))?;
        Ok(mirror)
    }

    /// Promote a pre-existing blobless mirror to a full mirror.
    ///
    /// Old mirrors were cloned with `--filter=blob:none`, which leaves
    /// `extensions.partialClone` set in the repo config and a
    /// `objects/info/promisor` file pointing at `origin`. Ephemeral
    /// clones of such a mirror still work, but they're no longer
    /// hardlink-and-alternates-only — any missing blob triggers a
    /// lazy fetch at read time. The chat shell runs with
    /// `CLONE_NEWNET`, so a lazy fetch inside the sandbox fails with
    /// network-unreachable and the tool call errors out.
    ///
    /// The backfill strategy is:
    ///   1. Detect partial-clone state via `git config --get
    ///      extensions.partialClone`. Unset → already full → no-op.
    ///   2. `git fetch --refetch` with an explicit
    ///      `+refs/heads/*:refs/heads/* +refs/tags/*:refs/tags/*`
    ///      refspec so every branch and tag is re-fetched with
    ///      promisor blobs materialised locally.
    ///   3. Unset `extensions.partialClone` and remove the
    ///      `objects/info/promisor` marker so subsequent fetches
    ///      don't re-enter partial-clone semantics. Bare repos put
    ///      the marker at `objects/info/promisor`; non-bare repos
    ///      put it at `.git/objects/info/promisor`. Handle both
    ///      layouts; missing files are ignored.
    ///
    /// Idempotent: running twice on an already-full mirror is a
    /// fast no-op (step 1 short-circuits). `git fetch --refetch` is
    /// itself idempotent so a partial run (e.g. process killed
    /// mid-fetch) is safe to retry.
    pub async fn ensure_full_mirror(&self, project_id: &str) -> Result<(), MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }

        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;

        // `git config --get` exits 1 when the key is unset; treat
        // that specifically as "already full, nothing to do" rather
        // than a hard error.
        let partial_clone = run_git_command(
            mirror.clone(),
            vec![
                "config".into(),
                "--get".into(),
                "extensions.partialClone".into(),
            ],
        )
        .await;
        match partial_clone {
            Ok(out) if out.stdout.trim().is_empty() => return Ok(()),
            Ok(_) => { /* value present — fall through and backfill */ }
            Err(djinn_git::GitError::CommandFailed { code: 1, .. }) => return Ok(()),
            Err(e) => return Err(git_err_to_mirror("git config --get", e)),
        }

        info!(project_id, path = ?mirror, "backfilling full mirror");

        run_git_command(
            mirror.clone(),
            vec![
                "fetch".into(),
                "--refetch".into(),
                "origin".into(),
                "+refs/heads/*:refs/heads/*".into(),
                "+refs/tags/*:refs/tags/*".into(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git fetch --refetch", e))?;

        run_git_command(
            mirror.clone(),
            vec![
                "config".into(),
                "--unset".into(),
                "extensions.partialClone".into(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git config --unset", e))?;

        // Bare and non-bare repos place the promisor marker in
        // different locations; neither is required to exist once
        // `extensions.partialClone` is gone, so NotFound on either
        // path is a success case.
        for candidate in [
            mirror.join(".git/objects/info/promisor"),
            mirror.join("objects/info/promisor"),
        ] {
            match tokio::fs::remove_file(&candidate).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(MirrorError::Io(e)),
            }
        }

        info!(project_id, "backfilled full mirror");
        Ok(())
    }

    /// Refresh an existing mirror via `git fetch --prune origin`.
    ///
    /// `origin_url` is passed on every call (rather than remembered from
    /// `ensure_mirror`) because installation tokens rotate. Callers mint a
    /// fresh token per fetch and embed it in the URL.
    ///
    /// Returns `true` when the fetch advanced at least one local ref
    /// (new commits, new/deleted branch, new/deleted tag). `false` means
    /// the mirror's ref set is byte-identical to what it was before —
    /// callers use this to skip the per-tick stack detect + graph warmer
    /// when nothing changed upstream.
    pub async fn fetch_mirror(
        &self,
        project_id: &str,
        origin_url: &str,
    ) -> Result<bool, MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;

        debug!(project_id, "fetching mirror");
        run_git_command(
            mirror.clone(),
            vec![
                "remote".into(),
                "set-url".into(),
                "origin".into(),
                origin_url.to_string(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git remote set-url", e))?;

        let before = snapshot_refs(&mirror).await?;

        // `git clone --bare` does NOT write a `fetch` refspec into
        // `remote.origin`, so a plain `git fetch origin` ends up
        // fetching objects for the default branch only and never
        // advances any local refs. That's why a merged PR on the
        // remote was invisible to stack detection — the mirror's
        // `refs/heads/main` was frozen at clone time. Passing an
        // explicit `+refs/heads/*:refs/heads/*` refspec mirrors every
        // head on every fetch, with force-update so force-pushes and
        // branch resets also sync. Tags follow so release-detection
        // stays current.
        run_git_command(
            mirror.clone(),
            vec![
                "fetch".into(),
                "--prune".into(),
                "origin".into(),
                "+refs/heads/*:refs/heads/*".into(),
                "+refs/tags/*:refs/tags/*".into(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git fetch", e))?;

        let after = snapshot_refs(&mirror).await?;
        Ok(before != after)
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
        // cwd = self.root (exists; mirrors dir). Explicit src/dst args are
        // absolute paths so cwd does not influence resolution.
        run_git_command(
            self.root.clone(),
            vec![
                "clone".into(),
                "--local".into(),
                "--shared".into(),
                "--branch".into(),
                branch.to_string(),
                mirror.display().to_string(),
                dir.path().display().to_string(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git clone --local", e))?;

        Ok(Workspace::new(dir, branch.to_string()))
    }
}

async fn snapshot_refs(mirror: &Path) -> Result<String, MirrorError> {
    let out = run_git_command(
        mirror.to_path_buf(),
        vec!["show-ref".into(), "--heads".into(), "--tags".into()],
    )
    .await;
    match out {
        Ok(o) => Ok(o.stdout),
        // `git show-ref` exits 1 with empty output when the repo has no
        // matching refs (e.g. a freshly cloned empty mirror). Treat that
        // as an empty snapshot rather than an error.
        Err(djinn_git::GitError::CommandFailed { code: 1, stdout, .. }) if stdout.is_empty() => {
            Ok(String::new())
        }
        Err(e) => Err(git_err_to_mirror("git show-ref", e)),
    }
}

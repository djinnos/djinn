use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use djinn_git::{run_git_command, run_git_command_with_timeout};
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
    /// Old mirrors were cloned with `--filter=blob:none`. Git records
    /// partialness across THREE different places in repo config
    /// depending on version/client:
    ///   - `extensions.partialClone = origin`
    ///   - `remote.origin.promisor = true`
    ///   - `remote.origin.partialclonefilter = blob:none`
    ///
    /// Plus an `objects/info/promisor` marker file on disk. Different
    /// mirrors on a single server can have different subsets set.
    ///
    /// The backfill strategy is:
    ///   1. Probe every partialness signal. If ALL are clear → already
    ///      full → no-op. Otherwise proceed.
    ///   2. Unset all three config keys BEFORE the refetch — leaving
    ///      `remote.origin.partialclonefilter` set would make
    ///      `git fetch --refetch` itself apply `blob:none`, so the
    ///      refetch would be a no-op with respect to blobs.
    ///   3. `git fetch --refetch` with an explicit
    ///      `+refs/heads/*:refs/heads/* +refs/tags/*:refs/tags/*`
    ///      refspec so every branch and tag is re-fetched with all
    ///      blobs materialised locally.
    ///   4. Remove the `objects/info/promisor` marker. Bare and
    ///      non-bare repos put it in different locations; handle both.
    ///
    /// Idempotent: running twice on an already-full mirror is a fast
    /// no-op (step 1 short-circuits). `git config --unset` on an
    /// already-unset key exits 5; that's treated as success.
    /// `git fetch --refetch` is itself idempotent so a partial run
    /// (e.g. process killed mid-fetch) is safe to retry.
    pub async fn ensure_full_mirror(&self, project_id: &str) -> Result<(), MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }

        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;

        let partial_keys = [
            "extensions.partialClone",
            "remote.origin.promisor",
            "remote.origin.partialclonefilter",
        ];
        let promisor_markers = [
            mirror.join(".git/objects/info/promisor"),
            mirror.join("objects/info/promisor"),
        ];

        // Probe every signal. If all config keys are unset AND no
        // promisor marker exists, the mirror is fully hydrated.
        let mut any_partial_signal = false;
        for key in partial_keys {
            if config_key_is_set(&mirror, key).await? {
                any_partial_signal = true;
                break;
            }
        }
        if !any_partial_signal {
            for marker in &promisor_markers {
                if tokio::fs::try_exists(marker).await.unwrap_or(false) {
                    any_partial_signal = true;
                    break;
                }
            }
        }
        if !any_partial_signal {
            return Ok(());
        }

        info!(project_id, path = ?mirror, "backfilling full mirror");

        // Unset partial-clone config FIRST so the refetch isn't
        // itself filtered. `--unset` exits 5 when the key doesn't
        // exist; treat that as success.
        for key in partial_keys {
            unset_config_key(&mirror, key).await?;
        }

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

        // Remove the promisor marker last so an interrupted backfill
        // (marker still present) would re-enter on next call.
        for marker in promisor_markers {
            match tokio::fs::remove_file(&marker).await {
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

    /// Run git housekeeping (`gc`) on the bare mirror to reclaim disk.
    ///
    /// Every mirror fetch already runs `--prune`, so refs to branches deleted
    /// upstream are dropped — but their *objects* linger until a `gc`, which
    /// we never ran. With djinn's branch-per-task churn (create → PR → merge →
    /// delete), that unreferenced-object pile is the dominant source of mirror
    /// bloat. This repacks and drops it.
    ///
    /// Held under the same per-project lock as `fetch_mirror` / `ensure_mirror`
    /// so it never races a concurrent fetch or ephemeral clone. A generous
    /// prune expiry (`2.weeks.ago`) leaves a safety window for any in-flight
    /// `--shared` ephemeral clone that borrows objects via alternates.
    pub async fn gc(&self, project_id: &str) -> Result<(), MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;
        debug!(project_id, "git gc (mirror)");
        git_gc(&mirror)
            .await
            .map_err(|e| git_err_to_mirror("git gc (mirror)", e))
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

    /// Cheap host-side check that `branch`'s commits are durably present in the
    /// mirror AND carry work not already on `base` — i.e. `base` is NOT an
    /// ancestor-or-equal of `branch`.
    ///
    /// Used by the stage-aware-resume decision (`supervisor_runner`): a
    /// reviewer-stage run that died after the worker already pushed its commits
    /// to `task_branch` can be resumed at the reviewer instead of redoing the
    /// worker — but ONLY if that output is actually durable. A missing branch
    /// (first cycle, or the worker never pushed) or a branch with nothing ahead
    /// of base means there is no worker diff to review, so the caller must fall
    /// back to the full worker redo.
    ///
    /// Reads the bare mirror directly (no clone): resolves both refs and runs
    /// `git merge-base --is-ancestor base branch`. `branch` is durably ahead
    /// when `base` is an ancestor of `branch` but the two are not equal. Any
    /// resolution failure (mirror missing, branch absent) yields `false` — the
    /// safe answer is "not durable, redo the worker".
    pub async fn branch_ahead_of_base(&self, project_id: &str, branch: &str, base: &str) -> bool {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return false;
        }
        let rev_parse = |refname: String| {
            let mirror = mirror.clone();
            async move {
                run_git_command(
                    mirror,
                    vec![
                        "rev-parse".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        refname,
                    ],
                )
                .await
                .ok()
                .map(|o| o.stdout.trim().to_string())
                .filter(|s| !s.is_empty())
            }
        };
        let (Some(branch_sha), Some(base_sha)) = (
            rev_parse(format!("refs/heads/{branch}")).await,
            rev_parse(format!("refs/heads/{base}")).await,
        ) else {
            return false;
        };
        // Equal heads = no worker diff to review.
        if branch_sha == base_sha {
            return false;
        }
        // `merge-base --is-ancestor base branch` exits 0 (→ `Ok`) when base is
        // an ancestor of branch — given the non-equal check above, branch is
        // then strictly ahead with the worker's commits on top — and exits 1
        // (→ `Err(CommandFailed { code: 1 })`) when it is not (branch diverged
        // from / does not contain base). For the resume decision we require the
        // ancestor relationship: a durable, fast-forwardable worker output the
        // reviewer can review against base.
        run_git_command(
            mirror,
            vec![
                "merge-base".into(),
                "--is-ancestor".into(),
                base_sha,
                branch_sha,
            ],
        )
        .await
        .is_ok()
    }
}

/// Is `key` set in the repo config at `mirror`? `git config --get`
/// exits 1 when the key is unset — treat that as "not set" rather
/// than an error.
async fn config_key_is_set(mirror: &Path, key: &str) -> Result<bool, MirrorError> {
    let out = run_git_command(
        mirror.to_path_buf(),
        vec!["config".into(), "--get".into(), key.to_string()],
    )
    .await;
    match out {
        Ok(o) => Ok(!o.stdout.trim().is_empty()),
        Err(djinn_git::GitError::CommandFailed { code: 1, .. }) => Ok(false),
        Err(e) => Err(git_err_to_mirror("git config --get", e)),
    }
}

/// Unset `key` in the repo config at `mirror`. `git config --unset`
/// exits 5 when the key doesn't exist — treat that as success so
/// this function is idempotent on already-clean repos.
async fn unset_config_key(mirror: &Path, key: &str) -> Result<(), MirrorError> {
    let out = run_git_command(
        mirror.to_path_buf(),
        vec!["config".into(), "--unset".into(), key.to_string()],
    )
    .await;
    match out {
        Ok(_) => Ok(()),
        Err(djinn_git::GitError::CommandFailed { code: 5, .. }) => Ok(()),
        Err(e) => Err(git_err_to_mirror("git config --unset", e)),
    }
}

/// Ceiling for a single `git gc` run. Repacking a large, long-churned repo
/// can take a while; the daily maintenance cadence means a slow gc never
/// piles up, and the bounded timeout stops a wedged gc from holding the
/// per-project lock forever.
const GC_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// `git gc` a repo (bare mirror or working clone): prune now-unreferenced
/// objects left behind by branches deleted upstream, then repack.
///
/// `git worktree prune` runs first to clear any stale worktree administrative
/// entries so their once-referenced objects also become collectable. A
/// generous `--prune=2.weeks.ago` expiry keeps very recent loose objects so a
/// concurrent borrower (e.g. a `--shared` ephemeral clone) is never starved
/// mid-operation. Process priority is already lowered by the git runner.
pub async fn git_gc(repo: &Path) -> Result<(), djinn_git::GitError> {
    // Best-effort worktree prune; a repo with no worktrees still succeeds.
    let _ = run_git_command_with_timeout(
        repo.to_path_buf(),
        vec!["worktree".into(), "prune".into()],
        GC_TIMEOUT,
    )
    .await;
    run_git_command_with_timeout(
        repo.to_path_buf(),
        vec!["gc".into(), "--prune=2.weeks.ago".into(), "--quiet".into()],
        GC_TIMEOUT,
    )
    .await
    .map(|_| ())
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
        Err(djinn_git::GitError::CommandFailed {
            code: 1, stdout, ..
        }) if stdout.is_empty() => Ok(String::new()),
        Err(e) => Err(git_err_to_mirror("git show-ref", e)),
    }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    /// `git_gc` succeeds on a real bare mirror (the worktree-prune + gc
    /// sequence is a valid no-op on an empty repo) — guards the command shape.
    #[tokio::test]
    async fn git_gc_succeeds_on_bare_repo() {
        let dir = TempDir::new().unwrap();
        run_git_command(
            dir.path().to_path_buf(),
            vec!["init".into(), "--bare".into(), "--quiet".into()],
        )
        .await
        .expect("git init --bare");

        git_gc(dir.path()).await.expect("git_gc on bare repo");
    }

    /// `MirrorManager::gc` errors `Missing` (not a git failure) when the
    /// project has no mirror on disk yet — the maintenance loop treats that as
    /// "nothing to gc", never a hard error.
    #[tokio::test]
    async fn mirror_gc_missing_when_absent() {
        let root = TempDir::new().unwrap();
        let mgr = MirrorManager::new(root.path().to_path_buf());
        let err = mgr.gc("does-not-exist").await.unwrap_err();
        assert!(matches!(err, MirrorError::Missing(_)));
    }
}

#[cfg(test)]
mod ahead_of_base_tests {
    use super::*;

    async fn git(repo: &Path, args: &[&str]) {
        run_git_command(
            repo.to_path_buf(),
            args.iter().map(|s| s.to_string()).collect(),
        )
        .await
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    }

    /// Build a bare mirror under `{root}/{project_id}.git` seeded with `main`
    /// and (optionally) a `task` branch one commit ahead. Returns the
    /// `MirrorManager`.
    async fn seed_mirror(root: &Path, project_id: &str, with_ahead_task: bool) -> MirrorManager {
        let mirror = root.join(format!("{project_id}.git"));
        git(
            root,
            &["init", "--bare", "--quiet", mirror.to_str().unwrap()],
        )
        .await;

        // Working clone to author commits, then push refs into the bare mirror.
        let work = TempDir::new().unwrap();
        git(
            root,
            &[
                "clone",
                "--quiet",
                mirror.to_str().unwrap(),
                work.path().to_str().unwrap(),
            ],
        )
        .await;
        let wp = work.path();
        git(wp, &["config", "user.email", "t@t"]).await;
        git(wp, &["config", "user.name", "t"]).await;
        git(wp, &["checkout", "-q", "-b", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "base"]).await;
        git(wp, &["push", "-q", "origin", "main"]).await;

        if with_ahead_task {
            git(wp, &["checkout", "-q", "-b", "task"]).await;
            git(wp, &["commit", "--allow-empty", "-qm", "worker work"]).await;
            git(wp, &["push", "-q", "origin", "task"]).await;
        }

        MirrorManager::new(root.to_path_buf())
    }

    #[tokio::test]
    async fn true_when_task_branch_is_ahead_of_base() {
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "p1", true).await;
        assert!(
            mgr.branch_ahead_of_base("p1", "task", "main").await,
            "task branch one commit ahead of main must read as durable"
        );
    }

    #[tokio::test]
    async fn false_when_task_branch_absent() {
        // First-cycle / worker-never-pushed: no task branch → not durable.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "p2", false).await;
        assert!(!mgr.branch_ahead_of_base("p2", "task", "main").await);
    }

    #[tokio::test]
    async fn false_when_task_branch_equals_base() {
        // Worker pushed nothing new (branch == base HEAD) → no diff to review.
        let root = TempDir::new().unwrap();
        let mirror = root.path().join("p3.git");
        git(
            root.path(),
            &["init", "--bare", "--quiet", mirror.to_str().unwrap()],
        )
        .await;
        let work = TempDir::new().unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                mirror.to_str().unwrap(),
                work.path().to_str().unwrap(),
            ],
        )
        .await;
        let wp = work.path();
        git(wp, &["config", "user.email", "t@t"]).await;
        git(wp, &["config", "user.name", "t"]).await;
        git(wp, &["checkout", "-q", "-b", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "base"]).await;
        git(wp, &["branch", "task"]).await;
        git(wp, &["push", "-q", "origin", "main", "task"]).await;

        let mgr = MirrorManager::new(root.path().to_path_buf());
        assert!(!mgr.branch_ahead_of_base("p3", "task", "main").await);
    }

    #[tokio::test]
    async fn false_when_mirror_missing() {
        let root = TempDir::new().unwrap();
        let mgr = MirrorManager::new(root.path().to_path_buf());
        assert!(!mgr.branch_ahead_of_base("absent", "task", "main").await);
    }
}

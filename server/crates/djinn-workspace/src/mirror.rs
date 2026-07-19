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

    #[error("refusing mirror gc: {0}")]
    GcGuard(#[from] GcGuardError),
}

#[derive(Debug, Error)]
pub enum GcGuardError {
    #[error("invalid path segment `{0}`")]
    InvalidSegment(String),

    #[error("store root `{path}` is not accessible: {source}")]
    RootIo {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("repo path `{path}` is not accessible: {source}")]
    RepoIo {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("repo path `{repo}` escapes expected root `{root}`")]
    OutsideRoot { root: PathBuf, repo: PathBuf },

    #[error("git: {0}")]
    Git(#[from] djinn_git::GitError),
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
        //
        // `^refs/heads/task/*` (negative refspec, git ≥2.29) carves the
        // task branches OUT of the mirroring: djinn owns `task/<short_id>`
        // refs LOCALLY — worker pods push them to the mirror for durability
        // (post-stage push, periodic OOM-proof push, SIGTERM checkpoint),
        // and redispatch / verification / ReviewResume all read them back.
        // Without the exclusion, `--prune` + the catch-all refspec deleted
        // every local-only task ref within one fetch tick (~60s) because it
        // doesn't exist on GitHub — every durability push silently raced a
        // shredder, verification's `git fetch task/<id>` exit-128'd, and
        // tasks looped through full worker redos. Closed tasks' mirror refs
        // are pruned by the coordinator's stale sweep instead.
        run_git_command(
            mirror.clone(),
            vec![
                "fetch".into(),
                "--prune".into(),
                "origin".into(),
                "+refs/heads/*:refs/heads/*".into(),
                "^refs/heads/task/*".into(),
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
        gc_mirror_under(&self.root, project_id).await?;
        Ok(())
    }

    /// Delete a local branch ref from the bare mirror (`git update-ref -d`).
    ///
    /// Task branches (`task/<short_id>`) are djinn-owned and excluded from
    /// `fetch_mirror`'s `--prune` (see the negative refspec there), so the
    /// coordinator's stale sweep calls this for CLOSED tasks to keep the
    /// mirror's ref set from growing without bound. Deleting a ref that
    /// doesn't exist is an error from git; callers treat that as already-done.
    pub async fn delete_branch(&self, project_id: &str, branch: &str) -> Result<(), MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let lock = self.lock_for(project_id).await;
        let _held = lock.lock().await;
        run_git_command(
            mirror,
            vec![
                "update-ref".into(),
                "-d".into(),
                format!("refs/heads/{branch}"),
            ],
        )
        .await
        .map(|_| ())
        .map_err(|e| git_err_to_mirror("git update-ref -d", e))
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

    /// Hardlinked local clone checked out at an arbitrary ref/SHA, returned as
    /// a detached-HEAD [`Workspace`].
    ///
    /// Used by the resume-via-git worktree-setup path to materialize the work
    /// pod from a non-branch source: a safety-scanned checkpoint ref
    /// (`refs/djinn/checkpoints/...`), a checkpoint SHA, or any other named ref
    /// already present in the mirror. The resulting workspace is intentionally
    /// NOT pinned to a branch ref: callers (the supervisor's `ensure_branch`
    /// following step) move `task_branch` to the resumed commit so a downstream
    /// `push_to_origin(task_branch)` lands the resumed state durably.
    ///
    /// Performs the clone WITHOUT `--branch` (so no branch ref is required),
    /// then resolves and checks out the chosen ref in detached HEAD against
    /// the resulting clone. `ref` may be:
    /// - a fully-qualified branch ref (`refs/heads/<branch>`) — already in
    ///   the clone, resolved via `git rev-parse`;
    /// - any other fully-qualified ref (`refs/djinn/checkpoints/...`, etc.) —
    ///   explicitly fetched into a namespaced local ref so `git checkout`
    ///   resolves it as a ref rather than treating slashes as a path
    ///   separator;
    /// - a short ref (`task/<id>`, `main`, …) — resolved against the
    ///   `refs/remotes/origin/*` namespace populated by the initial clone;
    /// - a full commit SHA (`abc123def…`) — verified via `git cat-file -e`
    ///   against the shared object db.
    ///
    /// Underlying error is surfaced as `MirrorError::Git` so the caller can
    /// machine-classify the failure and fall back to the legacy task-branch
    /// clone when the chosen ref is missing or otherwise unavailable.
    pub async fn clone_ephemeral_at_ref(
        &self,
        project_id: &str,
        ref_selector: &str,
    ) -> Result<Workspace, MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let dir = TempDir::new()?;

        debug!(
            project_id,
            ref_selector,
            path = ?dir.path(),
            "cloning ephemeral workspace at ref"
        );
        // Clone WITHOUT `--branch` so the mirror need not contain `ref_selector`
        // as a `refs/heads/*` ref — checks out the mirror's HEAD (whatever
        // local branch git happens to pick) and we will immediately switch to
        // `ref_selector`. Hardlinked object db is preserved (no network).
        run_git_command(
            self.root.clone(),
            vec![
                "clone".into(),
                "--local".into(),
                "--shared".into(),
                mirror.display().to_string(),
                dir.path().display().to_string(),
            ],
        )
        .await
        .map_err(|e| git_err_to_mirror("git clone --local (at-ref)", e))?;

        let resolved_sha = resolve_to_sha(dir.path(), ref_selector)
            .await
            .map_err(|e| git_err_to_mirror("git resolve (at-ref)", e))?;

        // Detach HEAD on the resolved SHA. `--detach` is required so the
        // subsequent `ensure_branch(task_branch)` creates a new branch
        // instead of failing on a detached HEAD; the workspace still
        // carries every tracked file at the chosen ref's tree.
        run_git_command(
            dir.path().to_path_buf(),
            vec!["checkout".into(), "--detach".into(), resolved_sha.clone()],
        )
        .await
        .map_err(|e| git_err_to_mirror("git checkout --detach (at-ref)", e))?;

        // Mirror `clone_ephemeral`'s `Workspace::new(dir, branch)` shape: the
        // branch label is informational (used for `push_to_origin`-style
        // helpers) and `ensure_branch(task_branch)` will rewrite it.
        Ok(Workspace::new(dir, resolved_sha))
    }

    /// Cheap host-side check that `branch`'s commits are durably present in the
    /// mirror AND carry work not already on `base` — i.e. `branch` has at least
    /// one commit beyond its merge-base with `base`.
    ///
    /// Used by the stage-aware-resume decision (`supervisor_runner`): a
    /// reviewer-stage run that died after the worker already pushed its commits
    /// to `task_branch` can be resumed at the reviewer instead of redoing the
    /// worker — but ONLY if that output is actually durable. A missing branch
    /// (first cycle, or the worker never pushed) or a branch with nothing ahead
    /// of base means there is no worker diff to review, so the caller must fall
    /// back to the full worker redo.
    ///
    /// `base` having moved on does NOT invalidate the worker's output. This
    /// deliberately does NOT require `base` to be an ancestor of `branch`
    /// (fast-forwardability): on a busy board some other task's PR merges into
    /// base during nearly every review-cycle run, so an is-ancestor probe reads
    /// "diverged" on each redispatch and the worker redo loops forever — a
    /// livelock where fleet throughput itself wedges every review-stage task
    /// (the t9wi/32bk wedge, 2026-06-11). The reviewer reviews the diff against
    /// the merge-base; integrating a moved base is the merge/PR stage's job.
    ///
    /// Reads the bare mirror directly (no clone): resolves both refs and runs
    /// `git rev-list --count base..branch`. Any resolution failure (mirror
    /// missing, branch absent) yields `false` — the safe answer is "not
    /// durable, redo the worker".
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
        // `rev-list --count base..branch` counts commits reachable from
        // `branch` but not from `base` — the worker's durable output beyond
        // the merge-base. Any positive count means there is a diff for the
        // reviewer, regardless of whether `base` has since moved on (the
        // branch may be "behind" base and still carry reviewable work).
        run_git_command(
            mirror,
            vec![
                "rev-list".into(),
                "--count".into(),
                format!("{base_sha}..{branch_sha}"),
            ],
        )
        .await
        .ok()
        .and_then(|o| o.stdout.trim().parse::<u64>().ok())
        .is_some_and(|ahead| ahead > 0)
    }

    /// Does `refs/heads/{branch}` exist in the bare mirror for `project_id`?
    ///
    /// Exists so a caller can classify a failed
    /// [`MirrorManager::clone_ephemeral`] into "the branch genuinely does not
    /// exist yet" versus "a transient failure". That classification CANNOT be
    /// done on the [`MirrorError`] variant: `MirrorError::Missing` means the
    /// mirror DIRECTORY is absent, while an absent branch fails inside
    /// `git clone --branch` (`fatal: Remote branch <b> not found in upstream
    /// origin`) and arrives as the same `MirrorError::Git` as a genuinely
    /// transient git failure. Only a direct ref probe separates them.
    ///
    /// Probes with `git show-ref --verify`, which asks purely about ref
    /// existence. A ref that exists but does not resolve (partially fetched /
    /// corrupt mirror) exits 128 and surfaces as `Err` — deliberately NOT
    /// `Ok(false)`, because "I cannot answer" must never be read as "the
    /// branch is absent".
    ///
    /// A missing mirror is likewise an `Err(MirrorError::Missing)`: there is
    /// no repository to answer the question against.
    pub async fn branch_exists(&self, project_id: &str, branch: &str) -> Result<bool, MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let out = run_git_command(
            mirror,
            vec![
                "show-ref".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/heads/{branch}"),
            ],
        )
        .await;
        match out {
            Ok(_) => Ok(true),
            // `show-ref --verify --quiet` exits 1 (silently) when the ref does
            // not exist. That is the genuine-absence answer, not a failure.
            Err(djinn_git::GitError::CommandFailed { code: 1, .. }) => Ok(false),
            Err(e) => Err(git_err_to_mirror("git show-ref --verify", e)),
        }
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

/// Run guarded gc for the expected bare mirror path `{root}/{project_id}.git`.
pub async fn gc_mirror_under(root: &Path, project_id: &str) -> Result<(), GcGuardError> {
    validate_path_segment(project_id)?;
    let repo = root.join(format!("{project_id}.git"));
    let repo = validate_repo_under_root(root, &repo)?;
    run_git_gc(&repo).await.map_err(GcGuardError::Git)
}

/// Run guarded gc for the expected project clone path `{projects_root}/{owner}/{repo}`.
pub async fn gc_project_clone_under(
    projects_root: &Path,
    owner: &str,
    repo: &str,
) -> Result<(), GcGuardError> {
    validate_path_segment(owner)?;
    validate_path_segment(repo)?;
    let repo_path = projects_root.join(owner).join(repo);
    let repo_path = validate_repo_under_root(projects_root, &repo_path)?;
    run_git_gc(&repo_path).await.map_err(GcGuardError::Git)
}

fn validate_path_segment(segment: &str) -> Result<(), GcGuardError> {
    let mut components = Path::new(segment).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(()),
        _ => Err(GcGuardError::InvalidSegment(segment.to_string())),
    }
}

fn validate_repo_under_root(root: &Path, repo: &Path) -> Result<PathBuf, GcGuardError> {
    let root = root.canonicalize().map_err(|source| GcGuardError::RootIo {
        path: root.to_path_buf(),
        source,
    })?;
    let repo = repo.canonicalize().map_err(|source| GcGuardError::RepoIo {
        path: repo.to_path_buf(),
        source,
    })?;
    if !repo.starts_with(&root) {
        return Err(GcGuardError::OutsideRoot { root, repo });
    }
    Ok(repo)
}

/// Low-level git-gc command sequence. Keep private; public entry points must
/// validate repo paths against their expected store roots first.
async fn run_git_gc(repo: &Path) -> Result<(), djinn_git::GitError> {
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

    async fn init_bare(path: &Path) {
        run_git_command(
            path.parent().unwrap().to_path_buf(),
            vec![
                "init".into(),
                "--bare".into(),
                "--quiet".into(),
                path.display().to_string(),
            ],
        )
        .await
        .expect("git init --bare");
    }

    async fn init_clone(path: &Path) {
        tokio::fs::create_dir_all(path)
            .await
            .expect("create clone dir");
        run_git_command(path.to_path_buf(), vec!["init".into(), "--quiet".into()])
            .await
            .expect("git init");
    }

    /// Guarded mirror gc succeeds on a real bare mirror — guards the command shape.
    #[tokio::test]
    async fn guarded_mirror_gc_succeeds_on_bare_repo() {
        let root = TempDir::new().unwrap();
        init_bare(&root.path().join("p1.git")).await;

        gc_mirror_under(root.path(), "p1")
            .await
            .expect("guarded mirror gc");
    }

    /// Guarded project-clone gc succeeds on the expected `{root}/{owner}/{repo}` clone.
    #[tokio::test]
    async fn guarded_project_clone_gc_succeeds_on_clone_repo() {
        let root = TempDir::new().unwrap();
        init_clone(&root.path().join("owner").join("repo")).await;

        gc_project_clone_under(root.path(), "owner", "repo")
            .await
            .expect("guarded project clone gc");
    }

    #[tokio::test]
    async fn mirror_gc_rejects_traversal_before_git() {
        let root = TempDir::new().unwrap();
        let err = gc_mirror_under(root.path(), "../outside")
            .await
            .unwrap_err();
        assert!(matches!(err, GcGuardError::InvalidSegment(_)));
    }

    #[tokio::test]
    async fn project_clone_gc_rejects_traversal_before_git() {
        let root = TempDir::new().unwrap();
        let err = gc_project_clone_under(root.path(), "owner", "../repo")
            .await
            .unwrap_err();
        assert!(matches!(err, GcGuardError::InvalidSegment(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mirror_gc_rejects_symlink_escape_before_git() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        init_bare(&outside.path().join("escape.git")).await;
        symlink(
            outside.path().join("escape.git"),
            root.path().join("escape.git"),
        )
        .expect("symlink mirror escape");

        let err = gc_mirror_under(root.path(), "escape").await.unwrap_err();
        assert!(matches!(err, GcGuardError::OutsideRoot { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_clone_gc_rejects_symlink_escape_before_git() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        init_clone(&outside.path().join("repo")).await;
        symlink(outside.path(), root.path().join("owner")).expect("symlink owner escape");

        let err = gc_project_clone_under(root.path(), "owner", "repo")
            .await
            .unwrap_err();
        assert!(matches!(err, GcGuardError::OutsideRoot { .. }));
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
    async fn true_when_base_advanced_past_task_branch() {
        // The review-cycle livelock case (t9wi/32bk, 2026-06-11): the worker
        // pushed durable commits, then ANOTHER task's PR merged into base, so
        // base is no longer an ancestor of the task branch. The worker output
        // is still durable and reviewable — this must read as durable, or
        // every redispatch on a busy board redoes the worker forever.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "p5", true).await;

        // Advance base past the point the task branch forked from.
        let work = TempDir::new().unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                root.path().join("p5.git").to_str().unwrap(),
                work.path().to_str().unwrap(),
            ],
        )
        .await;
        let wp = work.path();
        git(wp, &["config", "user.email", "t@t"]).await;
        git(wp, &["config", "user.name", "t"]).await;
        git(wp, &["checkout", "-q", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "other task merged"]).await;
        git(wp, &["push", "-q", "origin", "main"]).await;

        assert!(
            mgr.branch_ahead_of_base("p5", "task", "main").await,
            "a task branch with durable commits must read as durable even \
             when base has moved on (diverged ≠ no work to review)"
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

    #[tokio::test]
    async fn branch_exists_true_for_present_branch() {
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "be1", true).await;
        assert!(mgr.branch_exists("be1", "task").await.expect("probe"));
        assert!(mgr.branch_exists("be1", "main").await.expect("probe"));
    }

    #[tokio::test]
    async fn branch_exists_false_for_absent_branch() {
        // The genuine first-cycle shape: mirror is healthy, task branch has
        // never been pushed. This is the ONLY case that may fall back to base.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "be2", false).await;
        assert!(!mgr.branch_exists("be2", "task").await.expect("probe"));
    }

    #[tokio::test]
    async fn branch_exists_errs_when_mirror_missing() {
        let root = TempDir::new().unwrap();
        let mgr = MirrorManager::new(root.path().to_path_buf());
        assert!(
            matches!(
                mgr.branch_exists("absent", "task").await,
                Err(MirrorError::Missing(_))
            ),
            "a missing mirror cannot answer the question and must not read as \
             'branch absent'"
        );
    }

    #[tokio::test]
    async fn branch_exists_errs_when_ref_is_unresolvable() {
        // Partially fetched / corrupt mirror: the ref file exists but points
        // at an object the mirror does not have. `git clone --branch` fails
        // here, and this probe must NOT answer `Ok(false)` — reading that as
        // "first cycle" is exactly what rewinds the task branch to base.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "be3", false).await;
        let broken = root.path().join("be3.git/refs/heads/task");
        tokio::fs::create_dir_all(broken.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &broken,
            "0000000000000000000000000000000000000001\n".as_bytes(),
        )
        .await
        .unwrap();

        assert!(
            mgr.branch_exists("be3", "task").await.is_err(),
            "an unresolvable ref must surface as an error, never as absence"
        );
    }
}

#[cfg(test)]
mod fetch_prune_tests {
    use super::*;
    use tempfile::TempDir;

    async fn git(cwd: &Path, args: &[&str]) {
        run_git_command(
            cwd.to_path_buf(),
            args.iter().map(|s| s.to_string()).collect(),
        )
        .await
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    }

    /// `fetch_mirror`'s `--prune` must NOT delete djinn-owned local-only
    /// `task/*` refs (the durability refs worker pods push), while still
    /// pruning upstream-deleted ordinary branches and advancing `main`.
    /// Regression: the catch-all `+refs/heads/*:refs/heads/*` + `--prune`
    /// deleted every task ref within one fetch tick because it doesn't exist
    /// on the upstream — every durability push silently raced the shredder.
    #[tokio::test]
    async fn fetch_prune_spares_task_refs_and_prunes_upstream_deletions() {
        let root = TempDir::new().unwrap();

        // "GitHub": an upstream bare repo with main + a feature branch.
        let upstream = root.path().join("upstream.git");
        git(
            root.path(),
            &["init", "--bare", "--quiet", upstream.to_str().unwrap()],
        )
        .await;
        let work = TempDir::new().unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                upstream.to_str().unwrap(),
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
        git(wp, &["checkout", "-q", "-b", "feature"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "feat"]).await;
        git(wp, &["push", "-q", "origin", "feature"]).await;

        // The mirror: cloned bare from upstream (has main + feature), plus a
        // LOCAL-ONLY task ref a worker pod pushed for durability.
        let mirror = root.path().join("p1.git");
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                upstream.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        )
        .await;
        git(wp, &["checkout", "-q", "-b", "task/ab12"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "worker work"]).await;
        git(
            wp,
            &[
                "push",
                "-q",
                mirror.to_str().unwrap(),
                "task/ab12:refs/heads/task/ab12",
            ],
        )
        .await;

        // Upstream moves: main advances, feature is deleted.
        git(wp, &["checkout", "-q", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "advance"]).await;
        git(wp, &["push", "-q", "origin", "main"]).await;
        git(wp, &["push", "-q", "origin", "--delete", "feature"]).await;

        let mgr = MirrorManager::new(root.path().to_path_buf());
        let changed = mgr
            .fetch_mirror("p1", upstream.to_str().unwrap())
            .await
            .expect("fetch_mirror");
        assert!(changed, "ref set moved (main advanced, feature pruned)");

        let refs = run_git_command(
            mirror.clone(),
            vec!["for-each-ref".into(), "--format=%(refname)".into()],
        )
        .await
        .expect("for-each-ref")
        .stdout;
        assert!(
            refs.contains("refs/heads/task/ab12"),
            "local-only task ref must survive the prune; refs:\n{refs}"
        );
        assert!(
            !refs.contains("refs/heads/feature"),
            "upstream-deleted branch must be pruned; refs:\n{refs}"
        );

        // And the mirror-side cleanup path: delete_branch removes the task
        // ref once its task is closed.
        mgr.delete_branch("p1", "task/ab12")
            .await
            .expect("delete_branch");
        let refs = run_git_command(
            mirror,
            vec!["for-each-ref".into(), "--format=%(refname)".into()],
        )
        .await
        .expect("for-each-ref")
        .stdout;
        assert!(!refs.contains("refs/heads/task/ab12"));
    }
}

/// Cheap SHA hex check used to disambiguate raw-SHA selectors from short refs
/// in [`resolve_to_sha`]. Conservative: only matches ≥7 hex chars (the
/// shortest unique SHA length git accepts by default).
fn looks_like_sha(s: &str) -> bool {
    let len = s.len();
    (7..=40).contains(&len) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Monotonic counter feeding the temporary local-ref name used by
/// [`resolve_to_sha`] when fetching non-branch refs. Used as a uniqueness
/// suffix so concurrent calls never collide on the same local ref name.
/// A 64-bit counter never repeats in any realistic process lifetime.
static NEXT_RESUME_FETCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Resolve `ref_selector` against `clone_path` to a full commit SHA, fetching
/// the ref into a namespaced local ref first when git's default clone refspec
/// does not cover it (i.e. when the ref is non-branch).
///
/// Git's `git clone --local` default refspec is `refs/heads/*:refs/remotes/origin/*`
/// which leaves non-branch refs (e.g. `refs/djinn/checkpoints/...`) invisible
/// to `git checkout` because they were never pulled. Worse, `git fetch <remote>
/// <ref>` puts the ref only in `FETCH_HEAD`, which `git checkout` does not
/// resolve as a ref — slashes in `refs/djinn/...` get parsed as path
/// separators and the call errors with `--detach does not take a path
/// argument`. The fix is the explicit `<remote>:<local>` refspec that lands
/// the ref in the local namespaced form `refs/remotes/origin/<ref>` so
/// callers (the subsequent `git checkout --detach`) can resolve it as a ref.
async fn resolve_to_sha(
    clone_path: &Path,
    ref_selector: &str,
) -> Result<String, djinn_git::GitError> {
    // Branch refs (`refs/heads/*`) were already pulled by the clone.
    if let Some(branch) = ref_selector.strip_prefix("refs/heads/") {
        return rev_parse_in(clone_path, branch).await;
    }

    if ref_selector.starts_with("refs/") {
        // Non-branch ref — must fetch it explicitly into a namespaced local
        // ref so `git checkout` later sees it as a ref. Suffix is a unique
        // counter so concurrent resume-setup calls cannot race on the same
        // local ref name (the clone is a fresh TempDir per call so the
        // collision would only happen if two calls ran in the same tempdir,
        // but the counter keeps it bulletproof either way).
        let local_ref = format!(
            "refs/remotes/origin/__djinn_resume__{}",
            std::sync::atomic::AtomicU64::fetch_add(
                &NEXT_RESUME_FETCH_SEQ,
                1,
                std::sync::atomic::Ordering::Relaxed,
            )
        );
        run_git_command(
            clone_path.to_path_buf(),
            vec![
                "fetch".into(),
                "--no-tags".into(),
                "origin".into(),
                format!("{}:{}", ref_selector, local_ref),
            ],
        )
        .await?;
        return rev_parse_in(clone_path, &local_ref).await;
    }

    if looks_like_sha(ref_selector) {
        // Verify the object exists in the shared object db and return the
        // SHA unchanged. `cat-file -e` is the canonical existence probe.
        run_git_command(
            clone_path.to_path_buf(),
            vec!["cat-file".into(), "-e".into(), ref_selector.to_string()],
        )
        .await?;
        return Ok(ref_selector.to_string());
    }

    // Short ref (`main`, `task/1`, …) — resolved against the
    // `refs/remotes/origin/*` namespace populated by the initial clone.
    rev_parse_in(clone_path, &format!("refs/remotes/origin/{ref_selector}")).await
}

/// `git rev-parse --verify <rev>` against `clone_path`. Returns the trimmed
/// full SHA on success, surfaces the underlying `GitError` otherwise so the
/// caller can classify unknown / ambiguous refs.
async fn rev_parse_in(clone_path: &Path, rev: &str) -> Result<String, djinn_git::GitError> {
    let out = run_git_command(
        clone_path.to_path_buf(),
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "-q".into(),
            rev.to_string(),
        ],
    )
    .await?;
    let sha = out.stdout.trim().to_string();
    if sha.is_empty() {
        return Err(djinn_git::GitError::Other(anyhow::anyhow!(
            "git rev-parse returned no SHA for {rev}"
        )));
    }
    Ok(sha)
}

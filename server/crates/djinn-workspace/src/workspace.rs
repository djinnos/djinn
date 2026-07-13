// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use djinn_core::clock::{Clock, SystemClock};

use tempfile::TempDir;
use thiserror::Error;
use tracing::{debug, warn};

use crate::git_helpers;

/// Outcome of a [`Workspace::commit`] attempt.
///
/// Replaces the previous `Result<bool, _>` return where `Ok(true)` meant a
/// commit was created and `Ok(false)` meant nothing to commit.  The new
/// variant carries enough information for callers to distinguish a clean tree
/// from a junk-only tree without re-running `git status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// A legitimate diff was staged and committed.  The `excluded` field lists
    /// any repo-relative paths that were rejected by the commit-safety filter
    /// but did not prevent the commit (there were also legitimate changes).
    Committed {
        /// Repo-relative paths of scratch/junk files excluded alongside the
        /// legitimate commit (may be empty).
        excluded: Vec<String>,
    },
    /// The tree was clean — nothing to commit.
    NoChanges,
    /// Only files excluded by the commit-safety filter changed; no commit was
    /// created.  The `excluded` field lists the repo-relative paths that were
    /// rejected by [`crate::commit_safety::classify_path`].
    NoLegitimateChanges {
        /// Repo-relative paths of files that were excluded.
        excluded: Vec<String>,
    },
}

impl CommitOutcome {
    /// Returns `true` if a commit was created.
    pub fn committed(&self) -> bool {
        matches!(self, CommitOutcome::Committed { .. })
    }

    /// Returns the list of excluded paths, regardless of outcome variant.
    pub fn excluded(&self) -> &[String] {
        match self {
            CommitOutcome::Committed { excluded } => excluded,
            CommitOutcome::NoLegitimateChanges { excluded } => excluded,
            CommitOutcome::NoChanges => &[],
        }
    }
}

#[derive(Debug, Error)]
pub enum EphemeralWorkspaceError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("git: {0}")]
    Git(String),
}

impl From<djinn_git::GitError> for EphemeralWorkspaceError {
    fn from(err: djinn_git::GitError) -> Self {
        Self::Git(err.to_string())
    }
}

/// A committer / author identity used for automated commits inside a workspace.
///
/// Decoupled from any specific bot identity so `djinn-workspace` has no
/// dependency on `djinn-provider`. Callers (typically the supervisor) supply
/// the GitHub App bot identity resolved at runtime.
#[derive(Debug, Clone, Copy)]
pub struct GitIdentity<'a> {
    pub name: &'a str,
    pub email: &'a str,
}

/// Result of a [`Workspace::try_merge`] attempt.
#[derive(Debug, Clone)]
pub enum MergeOutcome {
    /// Merge applied cleanly. Changes are staged in the index (no commit
    /// created — caller's normal `commit` path picks them up as a merge
    /// commit because `.git/MERGE_HEAD` is set).
    Clean,
    /// Merge stopped on conflicts. The conflicting files are left on disk
    /// with standard git markers (`<<<<<<<`, `=======`, `>>>>>>>`).
    Conflicts { files: Vec<String> },
}

/// Outcome of [`Workspace::enforce_merge_parent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeParentOutcome {
    /// `merge_target_sha` was already an ancestor of HEAD — the worktree's
    /// commit history already records the merge (either MERGE_HEAD survived
    /// and the auto-commit produced a real two-parent merge, or a prior run
    /// already landed it). No new commit created.
    AlreadyMerged,
    /// HEAD did not record the merge (the worker cleared `.git/MERGE_HEAD`
    /// and/or committed a single-parent "resolution"). A synthetic two-parent
    /// "merge-completion" commit was created — tree identical to the worker's
    /// resolved content, parents = [worker HEAD, merge_target_sha] — and the
    /// branch was advanced to it. `new_head` is the SHA of that commit.
    Recovered { new_head: String },
}

/// How the workspace's on-disk root is owned.
///
/// `Owned` drops the underlying `TempDir` when the workspace is dropped —
/// the in-process / host-side path. `Attached` just remembers a borrowed
/// path; someone else (e.g. the Docker runtime that bind-mounted
/// `/workspace` into the container) is responsible for cleanup.
#[derive(Debug)]
enum WorkspaceRoot {
    Owned(TempDir),
    Attached(PathBuf),
}

impl WorkspaceRoot {
    fn path(&self) -> &Path {
        match self {
            WorkspaceRoot::Owned(dir) => dir.path(),
            WorkspaceRoot::Attached(path) => path.as_path(),
        }
    }
}

/// A tempdir-backed (or externally-bound) ephemeral clone of a project mirror.
///
/// Scope = one task-run. For the default `Owned` variant, contents (including
/// the git object db, since clones are `--local --shared`) are discarded when
/// the `TempDir` is dropped.  The `Attached` variant — constructed via
/// [`Workspace::attach_existing`] — wraps a directory the caller manages
/// (e.g. a bind-mounted `/workspace` inside a container); drop is a no-op.
///
/// Mutations that must survive the task-run are pushed to the origin remote
/// via `commit` → push-by-the-supervisor.
#[derive(Debug)]
pub struct Workspace {
    root: WorkspaceRoot,
    branch: String,
}

impl Workspace {
    pub(crate) fn new(dir: TempDir, branch: String) -> Self {
        Self {
            root: WorkspaceRoot::Owned(dir),
            branch,
        }
    }

    /// Attach to an existing on-disk workspace the caller already owns.
    ///
    /// Used by `djinn-agent-worker` when the host-side runtime has already
    /// cloned the mirror into a bind mount (`/workspace` inside the
    /// container) — the in-container supervisor reuses the same path instead
    /// of re-cloning.  The returned [`Workspace`] never drops the directory
    /// itself; lifetime is bound to the caller's mount lifecycle.
    ///
    /// Fails if `path` does not exist or is not a directory — the runtime is
    /// expected to materialise the clone before calling this.
    pub fn attach_existing(
        path: impl Into<PathBuf>,
        branch: impl Into<String>,
    ) -> Result<Self, EphemeralWorkspaceError> {
        let path = path.into();
        let meta = std::fs::metadata(&path).map_err(EphemeralWorkspaceError::Io)?;
        if !meta.is_dir() {
            return Err(EphemeralWorkspaceError::Git(format!(
                "attach_existing: {} is not a directory",
                path.display()
            )));
        }
        Ok(Self {
            root: WorkspaceRoot::Attached(path),
            branch: branch.into(),
        })
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn path_buf(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Stage filter-eligible changes and commit with `message` under `identity`.
    ///
    /// Unlike the old implementation that ran unconditional `git add -A`, this
    /// method enumerates dirty/untracked paths via `git status --porcelain=v1`,
    /// classifies **every** changed path through the shared
    /// [`crate::commit_safety`] filter, and stages/strips paths accordingly:
    ///
    /// - **Working-tree / untracked entries** that are allowed get staged;
    ///   excluded ones are skipped.
    /// - **Pre-staged (index-only) entries** that are allowed are left in the
    ///   index (preserving merge staging from a prior `try_merge`); excluded
    ///   ones are **unstaged** via `git reset HEAD` so scratch files a worker
    ///   may have pre-staged cannot sneak into the commit.
    ///
    /// Root-level scratch files (`patch.txt`, `test2.txt`, etc.) are excluded
    /// while intentional fixture/testdata paths are preserved.
    ///
    /// Returns a [`CommitOutcome`] distinguishing a real commit, a clean tree,
    /// and a junk-only tree with excluded-path details.
    pub async fn commit(
        &self,
        message: &str,
        identity: GitIdentity<'_>,
    ) -> Result<CommitOutcome, EphemeralWorkspaceError> {
        // ── Step 1: Enumerate dirty/untracked paths ──────────────────────
        let status_raw = self.run_git(&["status", "--porcelain=v1"], &[]).await?;

        let entries = parse_porcelain_status(&status_raw);

        if entries.is_empty() {
            return Ok(CommitOutcome::NoChanges);
        }

        // ── Step 2: Classify and separate ────────────────────────────────
        let config = crate::commit_safety::CommitSafetyConfig::default();
        let mut eligible: Vec<String> = Vec::new();
        let mut excluded: Vec<String> = Vec::new();
        let mut paths_to_unstage: Vec<String> = Vec::new();

        for entry in &entries {
            // Ignored files (rarely seen without --ignored) — skip.
            if entry.index == '!' && entry.worktree == '!' {
                continue;
            }

            match crate::commit_safety::classify_path(&entry.path, &config) {
                crate::commit_safety::PathClassification::Allowed => {
                    // Only add to staging list if there are working-tree
                    // changes or the file is untracked.  Index-only allowed
                    // entries (e.g. merge staging from `try_merge`) are
                    // already in the index — leave them alone.
                    if entry.worktree != ' ' {
                        eligible.push(entry.path.clone());
                    }
                }
                crate::commit_safety::PathClassification::Excluded(reason) => {
                    tracing::debug!(
                        path = %entry.path,
                        reason = ?reason,
                        "commit: excluding path from staging"
                    );
                    excluded.push(entry.path.clone());
                    // If this excluded path is already staged (index-only
                    // or both index+worktree), queue it for unstaging so
                    // pre-staged scratch files don't end up in the commit.
                    if entry.index != ' ' && entry.index != '?' {
                        paths_to_unstage.push(entry.path.clone());
                    }
                }
            }
        }

        // ── Step 3: Unstage excluded pre-staged paths ────────────────────
        if !paths_to_unstage.is_empty() {
            // `git reset HEAD -- <paths>` reverts index entries to HEAD.
            // For new files (not in HEAD) this removes them from the index;
            // for modified files it reverts the index entry.  Files remain
            // on disk in both cases.  Errors are non-fatal — the subsequent
            // `git diff --cached` check catches any residual issues.
            let mut reset_args: Vec<&str> = vec!["reset", "HEAD", "--"];
            for path in &paths_to_unstage {
                reset_args.push(path.as_str());
            }
            let _ = self.run_git(&reset_args, &[]).await;
        }

        // ── Step 4: Stage eligible paths ─────────────────────────────────
        if !eligible.is_empty() {
            let mut add_args: Vec<&str> = vec!["add", "--"];
            for path in &eligible {
                add_args.push(path.as_str());
            }
            self.run_git(&add_args, &[]).await?;
        }

        // ── Step 5: Check staged content ─────────────────────────────────
        let staged = self
            .run_git(&["diff", "--cached", "--name-only"], &[])
            .await?;
        if staged.trim().is_empty() {
            if excluded.is_empty() {
                return Ok(CommitOutcome::NoChanges);
            }
            tracing::info!(
                excluded_paths = ?excluded,
                "commit: no legitimate changes; only excluded files present"
            );
            return Ok(CommitOutcome::NoLegitimateChanges { excluded });
        }

        // ── Step 6: Defense-in-depth oversized file check ────────────────
        self.reject_oversized_staged_files(&staged).await?;

        // ── Step 7: Commit ───────────────────────────────────────────────
        self.run_git(
            &["commit", "-m", message],
            &[
                ("GIT_AUTHOR_NAME", identity.name),
                ("GIT_AUTHOR_EMAIL", identity.email),
                ("GIT_COMMITTER_NAME", identity.name),
                ("GIT_COMMITTER_EMAIL", identity.email),
            ],
        )
        .await?;
        Ok(CommitOutcome::Committed { excluded })
    }

    /// Refuse to commit any staged file larger than GitHub's 100 MiB hard limit.
    ///
    /// GitHub's pre-receive hook rejects a push containing a blob > 100 MiB
    /// (`GH001`), and once such a blob is in branch history the only fix is a
    /// history rewrite — there is no in-band recovery from the push itself. The
    /// blob is almost always a cache/store directory swept in by staging after a
    /// tool wrote it inside the worktree (observed: a `pnpm` store under
    /// `.local/share/pnpm` when `HOME` drifted into the worktree; also cargo
    /// target dirs and `node_modules`). Catching it here, at commit time, turns a
    /// dead-end push rejection into an actionable error the worker can fix within
    /// its own run (gitignore or delete the file) before it ever enters history.
    ///
    /// Staged content equals on-disk content (we just staged eligible paths),
    /// so the on-disk size of each staged regular file is its blob size. Deleted
    /// paths don't stat (skipped — a deletion can't be oversized); symlinks report
    /// their own small size via `symlink_metadata` and never false-trigger.
    async fn reject_oversized_staged_files(
        &self,
        staged: &str,
    ) -> Result<(), EphemeralWorkspaceError> {
        const MAX_COMMIT_FILE_BYTES: u64 = 100 * 1024 * 1024;
        let root = self.path().to_path_buf();
        let names: Vec<String> = staged
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();
        let offenders = tokio::task::spawn_blocking(move || {
            let mut offenders: Vec<(String, u64)> = Vec::new();
            for name in names {
                if let Ok(meta) = std::fs::symlink_metadata(root.join(&name))
                    && meta.is_file()
                    && meta.len() > MAX_COMMIT_FILE_BYTES
                {
                    offenders.push((name, meta.len()));
                }
            }
            offenders
        })
        .await
        .map_err(|e| {
            EphemeralWorkspaceError::Git(format!("oversized-file scan join error: {e}"))
        })?;
        if offenders.is_empty() {
            return Ok(());
        }
        let detail = offenders
            .iter()
            .map(|(p, n)| format!("  {p} ({:.2} MB)", *n as f64 / (1024.0 * 1024.0)))
            .collect::<Vec<_>>()
            .join("\n");
        Err(EphemeralWorkspaceError::Git(format!(
            "refusing to commit: {} staged file(s) exceed GitHub's 100 MB limit and would make the \
             task_branch push fail (GH001 — pre-receive hook declined). These are almost always \
             cache/store artifacts written inside the worktree (e.g. a pnpm store under \
             `.local/share/pnpm`, a cargo target dir, or `node_modules`) — add them to `.gitignore` \
             or remove them; do not commit them:\n{detail}",
            offenders.len()
        )))
    }

    /// Explicit teardown. Equivalent to `drop(self)` — the `TempDir` cleans
    /// itself up on drop. Callers may prefer the explicit form to document
    /// lifecycle points in supervisor code.
    #[deprecated(note = "use teardown_owned for cleanup telemetry")]
    pub fn teardown(self) {}

    /// Explicit teardown of an owned ephemeral workspace, returning whether the
    /// underlying `TempDir::close()` succeeded.
    ///
    /// For `Owned` workspaces this consumes `self` and calls `TempDir::close()`,
    /// which removes the directory AND prevents the subsequent `Drop` from
    /// deleting it again (idempotent). The returned `Result` exposes cleanup
    /// success/error so callers can time the operation and record a bounded
    /// `outcome=ok|error` telemetry sample.
    ///
    /// For `Attached` workspaces this is a no-op that returns `Ok(())` — the
    /// directory is externally owned (e.g. a bind-mounted `/workspace`) and must
    /// never be deleted by this process. No telemetry should be emitted for
    /// attached teardowns because there is nothing to observe.
    pub fn teardown_owned(self) -> std::io::Result<()> {
        match self.root {
            WorkspaceRoot::Owned(dir) => dir.close(),
            // Attached directories are externally owned; do not delete.
            WorkspaceRoot::Attached(_) => Ok(()),
        }
    }

    /// Returns `true` when this workspace owns its directory (the `TempDir`
    /// variant). Attached workspaces are externally owned and their teardown is
    /// a no-op that must not be observed.
    pub fn is_owned(&self) -> bool {
        matches!(self.root, WorkspaceRoot::Owned(_))
    }

    /// Ensure the named branch exists and is checked out.
    ///
    /// Uses `git checkout -B <branch>` which:
    /// - Creates the branch from current HEAD if it doesn't exist.
    /// - Resets it to current HEAD if it does (idempotent).
    /// - Checks it out in either case.
    ///
    /// Needed because [`crate::MirrorManager::clone_ephemeral`] clones the
    /// mirror on `base_branch`; the worker's commits and the eventual
    /// `push_to_origin(task_branch)` need `task_branch` to actually exist as
    /// a local ref pointing at the worker's commits.
    pub async fn ensure_branch(&self, branch: &str) -> Result<(), EphemeralWorkspaceError> {
        self.run_git(&["checkout", "-B", branch], &[])
            .await
            .map(|_| ())
    }

    /// Move HEAD to `ref_selector` in detached mode WITHOUT advancing any
    /// branch ref. Used by the resume-via-git worktree-setup path to land the
    /// workspace on a chosen checkpoint SHA / alternate checkpoint ref while
    /// preserving the supervisor's subsequent `ensure_branch(task_branch)`
    /// step (which creates / refreshes the task branch from current HEAD on
    /// top of the resumed commit).
    ///
    /// `ref_selector` may be a fully-qualified ref (`refs/...`), a short ref
    /// name, or a full commit SHA — git's normal resolution applies. Errors
    /// (unknown ref, missing object) surface as [`EphemeralWorkspaceError::Git`]
    /// so the caller can fall back to the legacy task-branch path.
    pub async fn checkout_ref(&self, ref_selector: &str) -> Result<(), EphemeralWorkspaceError> {
        self.run_git(&["checkout", "--detach", ref_selector], &[])
            .await
            .map(|_| ())
    }

    /// Rewrite every tracked file's mtime to the commit time of the last commit
    /// that touched it — the `git restore-mtime` technique.
    ///
    /// ## Why
    /// An ephemeral clone (`MirrorManager::clone_ephemeral`) checks every tracked
    /// file out fresh, so they all get *checkout-time* mtimes. Cargo fingerprints
    /// path (workspace) crates by source mtime, so against the shared
    /// `CARGO_TARGET_DIR` every workspace crate looks dirty and recompiles on the
    /// first build of every run — even when its sources are byte-identical to
    /// what produced the cached artifacts. Resetting each file's mtime to its
    /// last-touched commit time makes byte-identical files get *identical* mtimes
    /// across runs, so cargo's fingerprint matches the cache and only crates the
    /// task's branch actually changed rebuild. Files the worker edits afterward
    /// get fresh mtimes naturally, so within-run incremental builds are
    /// unaffected.
    ///
    /// ## Algorithm
    /// Single newest→oldest `git log` walk with `--name-only`: the first commit
    /// (i.e. most recent) that names a path wins, and we stop once every tracked
    /// file has been assigned (or the commit cap / time budget is hit). This is
    /// O(commits until covered), not O(files), and never shells out per file.
    /// Renames: `--no-renames` is intentional — a renamed path shows up as an
    /// add in the commit that performed the rename, which carries exactly the
    /// timestamp we want for the new path.
    ///
    /// Runs against whatever branch is currently checked out, so it composes with
    /// the v0.5.21 task_branch-clone behavior (the walk naturally sees that
    /// branch's history).
    ///
    /// ## Best-effort
    /// Every failure is logged and swallowed — this is a pure cache optimization
    /// and must NEVER fail a run. Any file not covered when the cap hits simply
    /// keeps its checkout mtime (correct; only loses a cache hit). Directories
    /// are skipped (cargo doesn't fingerprint them); symlinks are skipped (we
    /// don't want to chase them to their targets).
    pub async fn normalize_mtimes(&self) {
        normalize_mtimes_at(self.root.path()).await;
    }

    /// Whether `origin/<target_branch>` is already an ancestor of the current
    /// `HEAD` — i.e. the checked-out branch already contains every commit on
    /// the target, so a merge would be a no-op.
    ///
    /// Used by the supervisor's proactive dispatch-time sync to skip the merge
    /// entirely when the task branch is already current with the target. This
    /// covers the first cycle of a task, where `ensure_branch` has just created
    /// `task/<id>` from `origin/<base>` (origin/<base> == HEAD ⇒ ancestor), so
    /// the proactive sync produces no churn / no spurious merge commit.
    ///
    /// Fetches `origin/<target_branch>` first so the ancestry check is made
    /// against the *current* remote tip, not whatever the ephemeral clone last
    /// saw. Returns `Ok(true)` when up-to-date, `Ok(false)` when behind (a merge
    /// would do something), and `Err(_)` on fetch / git failure (caller should
    /// log-and-skip — falling through to the merge is safe).
    pub async fn is_up_to_date_with(
        &self,
        target_branch: &str,
    ) -> Result<bool, EphemeralWorkspaceError> {
        self.run_git(&["fetch", "origin", target_branch], &[])
            .await?;
        let merge_ref = format!("origin/{target_branch}");
        // `git merge-base --is-ancestor A B` exits 0 when A is an ancestor of
        // B, 1 when it is not. Other non-zero exits are real errors. We can't
        // use `run_git` (which treats every non-zero exit as an error), so call
        // git directly and discriminate on the exit code.
        git_helpers::is_ancestor(self.root.path(), &merge_ref, "HEAD")
            .await
            .map_err(EphemeralWorkspaceError::Git)
    }

    /// Fetch `target_branch` from `origin` and attempt a no-fast-forward merge
    /// into the currently checked-out branch, stopping short of the commit.
    ///
    /// Used by the supervisor on `ConflictRetry` runs so the worker pod sees
    /// the merge state (with conflict markers in files) when it inspects the
    /// workspace, instead of just a clean checkout of `task_branch` and a list
    /// of file paths in its prompt.
    ///
    /// `--no-commit` leaves the result staged so the subsequent worker stage's
    /// auto-commit in the supervisor body produces a merge commit (because
    /// `.git/MERGE_HEAD` is still set). `--no-ff` ensures a real merge
    /// commit even when `target_branch` is strictly ahead — keeps the topology
    /// predictable for the reviewer.
    ///
    /// Returns:
    /// - `Ok(MergeOutcome::Clean)` — merge applied without conflicts.
    /// - `Ok(MergeOutcome::Conflicts { files })` — merge stopped on conflicts;
    ///   the workspace is mid-merge with markers on disk.
    /// - `Err(_)` — fetch failed, or git merge failed for reasons other than
    ///   conflicts (e.g. unknown ref). Caller should log-and-skip rather than
    ///   abort the run.
    pub async fn try_merge(
        &self,
        target_branch: &str,
    ) -> Result<MergeOutcome, EphemeralWorkspaceError> {
        self.run_git(&["fetch", "origin", target_branch], &[])
            .await?;

        // `git merge` validates the committer identity at the start of the
        // operation even with `--no-commit`, so we must supply it here.  The
        // values match what `commit` injects so any merge commit produced by
        // the subsequent auto-commit stage carries a consistent author.
        let merge_ref = format!("origin/{target_branch}");
        let clean = git_helpers::try_merge_no_commit_no_ff(self.root.path(), &merge_ref)
            .await
            .map_err(EphemeralWorkspaceError::Git)?;
        if clean {
            return Ok(MergeOutcome::Clean);
        }

        let files = git_helpers::unmerged_files(self.root.path())
            .await
            .map_err(EphemeralWorkspaceError::Git)?;

        if files.is_empty() {
            return Err(EphemeralWorkspaceError::Git(format!(
                "git merge --no-commit --no-ff {merge_ref} failed with no conflicts"
            )));
        }

        Ok(MergeOutcome::Conflicts { files })
    }

    /// Resolve a ref / revision to its full commit SHA (`git rev-parse`).
    ///
    /// Used to snapshot `origin/<merge_target>` at the moment the supervisor
    /// stages a conflicted merge for the worker, so the post-worker
    /// [`Self::enforce_merge_parent`] can re-assert that exact commit as the
    /// merge's second parent regardless of what the model did to `.git` state.
    pub async fn resolve_ref(&self, rev: &str) -> Result<String, EphemeralWorkspaceError> {
        let out = self.run_git(&["rev-parse", rev], &[]).await?;
        Ok(out.trim().to_string())
    }

    /// Whether `ancestor` is an ancestor of `descendant` (`git merge-base
    /// --is-ancestor`). `Ok(true)` when it is, `Ok(false)` when not, `Err`
    /// on any other git failure (bad rev, etc.).
    pub async fn is_ancestor(
        &self,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, EphemeralWorkspaceError> {
        git_helpers::is_ancestor(self.root.path(), ancestor, descendant)
            .await
            .map_err(EphemeralWorkspaceError::Git)
    }

    /// Supervisor-owned guarantee that a ConflictRetry resolution lands as a
    /// TRUE two-parent merge commit — regardless of what the worker did to the
    /// `.git` state during its session.
    ///
    /// ## Why this exists
    /// On a ConflictRetry run the supervisor stages a conflicted merge of
    /// `origin/<merge_target>` into the task branch (markers on disk,
    /// `.git/MERGE_HEAD` set) and lets the worker resolve the content. The
    /// *intended* path is: worker edits the markers out → post-worker
    /// auto-commit sees `MERGE_HEAD` and records a two-parent merge commit →
    /// the branch's merge-base with the target advances → GitHub flips the PR
    /// back to mergeable.
    ///
    /// But workers run their OWN git commands. Many of them, on seeing
    /// "unmerged paths", run `git merge --abort` / `git reset` (which clears
    /// `MERGE_HEAD`) and then hand-commit a single-parent "resolution". The
    /// content is correct, but git history never records the merge: the
    /// branch's merge-base with the target is unchanged, so GitHub keeps the
    /// PR `CONFLICTING` forever and the poller re-flags it — an infinite,
    /// token-burning retry loop (production task 3hrr, commit 9920477a).
    ///
    /// ## What it does
    /// Given the SHA of `origin/<merge_target>` captured when the conflicted
    /// merge was staged (`merge_target_sha`):
    /// 1. Stage everything (`git add -A`) so the worker's on-disk resolution —
    ///    whether committed or merely saved — is reflected in the index/tree.
    /// 2. If `merge_target_sha` is already an ancestor of HEAD, the merge is
    ///    already recorded (MERGE_HEAD survived, or a prior run landed it):
    ///    return [`MergeParentOutcome::AlreadyMerged`]. No new commit.
    /// 3. Otherwise construct a synthetic two-parent commit:
    ///    `git write-tree` (current resolved tree) → `git commit-tree <tree>
    ///    -p HEAD -p <merge_target_sha>` → reset the branch to it. The tree is
    ///    EXACTLY the worker's resolved content (so the diff is unchanged); the
    ///    history now records the merge. This works identically whether the
    ///    worker left the resolution uncommitted, hand-committed a single
    ///    parent, or aborted the merge — the tree is taken from the worktree
    ///    either way.
    ///
    /// After either outcome the caller MUST verify
    /// `is_ancestor(merge_target_sha, "HEAD")` holds and fail loudly if not —
    /// never push a "resolution" that leaves the PR conflicting silently.
    pub async fn enforce_merge_parent(
        &self,
        merge_target_sha: &str,
        identity: GitIdentity<'_>,
    ) -> Result<MergeParentOutcome, EphemeralWorkspaceError> {
        // Refuse to fabricate a merge over a still-conflicted tree. This MUST
        // be checked BEFORE `git add -A`, because `add -A` would stage the
        // conflict markers as ordinary resolved content (clearing the unmerged
        // index entries) and mask an unresolved merge. `ls-files --unmerged`
        // reports any index entry at stage > 0 — i.e. a path git still
        // considers conflicted (worker never resolved, MERGE_HEAD still set).
        let unmerged = self.run_git(&["ls-files", "--unmerged"], &[]).await?;
        if !unmerged.trim().is_empty() {
            let paths: Vec<&str> = unmerged
                .lines()
                .filter_map(|l| l.split('\t').nth(1))
                .collect();
            return Err(EphemeralWorkspaceError::Git(format!(
                "enforce_merge_parent: index still has unmerged paths: {}",
                paths.join(", ")
            )));
        }

        // Reflect the worker's on-disk resolution (committed or not) into the
        // index so `write-tree` captures it. Idempotent when the tree is clean.
        self.run_git(&["add", "-A"], &[]).await?;

        // Already a proper merge? (MERGE_HEAD survived → auto-commit recorded a
        // two-parent merge; or a prior run already landed it.)
        if self.is_ancestor(merge_target_sha, "HEAD").await? {
            return Ok(MergeParentOutcome::AlreadyMerged);
        }

        // Construct the two-parent "merge-completion" commit. Its tree is the
        // worker's resolved tree (write-tree of the staged index); its parents
        // are the worker's current HEAD and the captured merge target.
        let head_sha = self.run_git(&["rev-parse", "HEAD"], &[]).await?;
        let head_sha = head_sha.trim().to_string();
        let tree_sha = self.run_git(&["write-tree"], &[]).await?;
        let tree_sha = tree_sha.trim().to_string();

        let message = "Merge completion: record two-parent merge after conflict resolution";
        let commit_out = self
            .run_git(
                &[
                    "commit-tree",
                    &tree_sha,
                    "-p",
                    &head_sha,
                    "-p",
                    merge_target_sha,
                    "-m",
                    message,
                ],
                &[
                    ("GIT_AUTHOR_NAME", identity.name),
                    ("GIT_AUTHOR_EMAIL", identity.email),
                    ("GIT_COMMITTER_NAME", identity.name),
                    ("GIT_COMMITTER_EMAIL", identity.email),
                ],
            )
            .await?;
        let new_head = commit_out.trim().to_string();

        // Advance the checked-out branch to the new merge commit, keeping the
        // worktree/index intact (tree is identical, so this is a pure history
        // rewrite — no file churn). `reset --soft` moves HEAD + branch ref
        // without touching the index or working tree.
        self.run_git(&["reset", "--soft", &new_head], &[]).await?;

        // Defensive post-check: the target MUST now be an ancestor. If git
        // somehow produced a commit that doesn't record the parent, surface it
        // loudly rather than returning a false success.
        if !self.is_ancestor(merge_target_sha, "HEAD").await? {
            return Err(EphemeralWorkspaceError::Git(format!(
                "enforce_merge_parent: constructed merge commit {new_head} but \
                 {merge_target_sha} is still not an ancestor of HEAD"
            )));
        }

        Ok(MergeParentOutcome::Recovered { new_head })
    }

    /// Push the named branch from this workspace to its `origin` remote.
    ///
    /// Called by the worker after a task-run completes its stages but
    /// before `open_pr` — so the host's `squash_merge_via_mirror` can find
    /// the worker's commits in the mirror.  Without this, the worker's
    /// commits live only in the ephemeral TempDir clone (whose origin is
    /// the mirror) and vanish when the Pod exits.
    ///
    /// Idempotent: if the branch has no new commits beyond what `origin`
    /// already has, the push is a no-op.  If the push fails (origin is
    /// read-only, network error, etc.), returns the underlying
    /// [`djinn_git::GitError`].
    ///
    /// Refspec is `branch:branch`; the source must be a local ref in this
    /// workspace.
    pub async fn push_to_origin(&self, branch: &str) -> Result<(), djinn_git::GitError> {
        djinn_git::run_git_command(
            self.root.path().to_path_buf(),
            vec!["push".into(), "origin".into(), format!("{branch}:{branch}")],
        )
        .await
        .map(|_| ())
    }

    async fn run_git(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<String, EphemeralWorkspaceError> {
        let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let owned_env: Vec<(String, String)> = extra_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let out =
            djinn_git::run_git_command_in_with_env(self.root.path(), owned_args, owned_env).await?;
        if out.code != 0 {
            return Err(EphemeralWorkspaceError::Git(format!(
                "git {}: {}",
                args.join(" "),
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }
}

// ─── git-status porcelain parsing helpers ──────────────────────────────────

/// A single parsed entry from `git status --porcelain=v1` output.
struct PorcelainEntry {
    /// Index (staging-area) status character.
    index: char,
    /// Working-tree status character.
    worktree: char,
    /// Repo-relative path (the destination path for renames/copies).
    path: String,
}

/// Parse `git status --porcelain=v1` output into structured entries.
///
/// Handles quoted paths and rename/copy entries (`XY old -> new`) by
/// extracting the destination path.
fn parse_porcelain_status(raw: &str) -> Vec<PorcelainEntry> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        if line.len() < 3 {
            continue;
        }
        let x = line.as_bytes()[0] as char;
        let y = line.as_bytes()[1] as char;
        let rest = &line[3..]; // skip "XY "

        let path = if x == 'R' || x == 'C' {
            // Rename or copy: "XY old_path -> new_path"
            if let Some(pos) = rest.find(" -> ") {
                unquote_path(&rest[pos + 4..])
            } else {
                unquote_path(rest)
            }
        } else {
            unquote_path(rest)
        };

        entries.push(PorcelainEntry {
            index: x,
            worktree: y,
            path,
        });
    }
    entries
}

/// Unquote a path from git-status porcelain output.
///
/// Paths containing special characters are double-quoted with C-style escapes
/// (e.g. `"path with spaces"`).  This function strips the quotes and decodes
/// the escapes.
fn unquote_path(path: &str) -> String {
    if path.len() >= 2 && path.starts_with('"') && path.ends_with('"') {
        let inner = &path[1..path.len() - 1];
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => result.push('\n'),
                    Some('t') => result.push('\t'),
                    Some('\\') => result.push('\\'),
                    Some('"') => result.push('"'),
                    Some(other) => {
                        result.push('\\');
                        result.push(other);
                    }
                    None => result.push('\\'),
                }
            } else {
                result.push(c);
            }
        }
        result
    } else {
        path.to_string()
    }
}

/// Cap on the `git log` walk so a deep-history repo can't make this run
/// unbounded. 10k commits comfortably covers every tracked file in practice
/// (the walk stops the instant the seen-set covers the universe); if it
/// doesn't, the uncovered files keep their checkout mtime — a correct, merely
/// suboptimal fallback.
const MAX_COMMITS: usize = 10_000;

/// Wall-clock budget for the whole walk. Bounds the cost on pathological
/// repos / slow disks; on hitting it we apply whatever we resolved so far and
/// leave the rest at their checkout mtime.
const TIME_BUDGET: Duration = Duration::from_secs(5);

struct NormalizeStats {
    touched: usize,
    tracked: usize,
    commits_walked: usize,
    duration: Duration,
}

/// Best-effort tracked-file mtime normalization on an arbitrary checked-out
/// working tree at `root`.
///
/// Free-function form of [`Workspace::normalize_mtimes`] for callers that hold
/// a plain directory rather than a [`Workspace`] handle — e.g. the verification
/// Job pod, which clones the task branch into its own workspace before running
/// the pipeline. Same best-effort contract: every failure is logged and
/// swallowed; this is a pure cargo-cache optimization and must NEVER fail a run.
pub async fn normalize_mtimes_at(root: &Path) {
    let root = root.to_path_buf();
    // The whole thing is CPU/syscall-bound filesystem work over a `git log`
    // pipe; run it off the async runtime so we don't park a worker thread.
    let result = tokio::task::spawn_blocking(move || normalize_mtimes_blocking(&root)).await;
    match result {
        Ok(Ok(stats)) => debug!(
            files_touched = stats.touched,
            tracked = stats.tracked,
            commits_walked = stats.commits_walked,
            duration_ms = stats.duration.as_millis() as u64,
            "normalize_mtimes: reset tracked-file mtimes to commit times"
        ),
        Ok(Err(e)) => warn!(error = %e, "normalize_mtimes: skipped (non-fatal)"),
        Err(e) => warn!(error = %e, "normalize_mtimes: blocking task panicked (non-fatal)"),
    }
}

/// Synchronous core of [`Workspace::normalize_mtimes`]; see that method's docs
/// for the rationale and algorithm. Uses blocking `std::process::Command` +
/// `File::set_modified`, so it must run on a blocking thread.
fn normalize_mtimes_blocking(root: &Path) -> Result<NormalizeStats, EphemeralWorkspaceError> {
    let start = SystemClock::new().now_instant();

    // 1. Universe of tracked files (NUL-delimited so paths with newlines/spaces
    //    survive). This is the set we must cover.
    let ls = git_capture(root, &["ls-files", "-z"])?;
    let universe: HashSet<&[u8]> = ls.split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
    let tracked = universe.len();
    if tracked == 0 {
        return Ok(NormalizeStats {
            touched: 0,
            tracked: 0,
            commits_walked: 0,
            duration: start.elapsed(),
        });
    }

    // 2. Walk history newest→oldest, name-only, with a `\x1e` (record separator)
    //    marker before each commit's timestamp so commit boundaries are
    //    unambiguous even for empty commits and odd filenames. `--no-renames`:
    //    a rename shows as an add in the renaming commit (correct timestamp for
    //    the new path).
    let log = git_capture(
        root,
        &[
            "log",
            "-z",
            "--no-renames",
            &format!("--max-count={MAX_COMMITS}"),
            "--pretty=tformat:\x1e%ct",
            "--name-only",
        ],
    )?;

    // path -> mtime (seconds since epoch). First (newest) commit naming a path wins.
    let mut resolved: std::collections::HashMap<Vec<u8>, i64> =
        std::collections::HashMap::with_capacity(tracked);
    let mut commits_walked = 0usize;

    // Each `\x1e`-prefixed record is `<ts>\0[\n<path>\0<path>\0...]`. Split on
    // the record marker first, then on NUL within each record.
    'records: for record in log.split(|&b| b == 0x1e) {
        if record.is_empty() {
            continue;
        }
        commits_walked += 1;
        if commits_walked.is_multiple_of(256) && start.elapsed() > TIME_BUDGET {
            break;
        }
        let mut fields = record.split(|&b| b == 0);
        let Some(ts_field) = fields.next() else {
            continue;
        };
        let Ok(ts) = std::str::from_utf8(ts_field)
            .unwrap_or("")
            .trim()
            .parse::<i64>()
        else {
            continue;
        };
        for field in fields {
            // The first path field carries a leading '\n' from the format's
            // header/name-only boundary; strip any leading newlines.
            let path: &[u8] = {
                let mut p = field;
                while let [b'\n', rest @ ..] = p {
                    p = rest;
                }
                p
            };
            if path.is_empty() || !universe.contains(path) {
                continue;
            }
            // First (newest) commit naming this path wins.
            resolved.entry(path.to_vec()).or_insert(ts);
            if resolved.len() == tracked {
                // Every tracked file covered — no need to walk further history.
                break 'records;
            }
        }
    }

    // 3. Apply. Best-effort per file: a failure on one file (symlink, perms,
    //    raced delete) must not abort the batch.
    let mut touched = 0usize;
    for (path, ts) in &resolved {
        let Ok(rel) = std::str::from_utf8(path) else {
            continue;
        };
        let full = root.join(rel);
        // Skip symlinks (don't chase to target) and anything that isn't a
        // regular file. `symlink_metadata` does not follow the link.
        match std::fs::symlink_metadata(&full) {
            Ok(meta) if meta.file_type().is_file() => {}
            _ => continue,
        }
        let mtime = match u64::try_from(*ts) {
            Ok(secs) => UNIX_EPOCH + Duration::from_secs(secs),
            // Pre-1970 commit dates (or negative) are nonsensical for source —
            // leave the checkout mtime in place.
            Err(_) => continue,
        };
        if let Ok(f) = File::options().write(true).open(&full)
            && f.set_modified(mtime).is_ok()
        {
            touched += 1;
        }
    }

    Ok(NormalizeStats {
        touched,
        tracked,
        commits_walked,
        duration: start.elapsed(),
    })
}

/// Run `git -C <root> <args>` synchronously and return raw stdout bytes.
/// Used by [`normalize_mtimes_blocking`] where output is NUL/`\x1e`-delimited
/// binary that must not pass through lossy UTF-8 conversion.
fn git_capture(root: &Path, args: &[&str]) -> Result<Vec<u8>, EphemeralWorkspaceError> {
    let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = djinn_git::run_git_command_binary_in(root, owned_args)
        .map_err(|e| EphemeralWorkspaceError::Git(e.to_string()))?;
    if !out.is_success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(EphemeralWorkspaceError::Git(format!(
            "git {}: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Run `git <args>` in `dir`, panicking with stderr on failure.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write file");
    }

    /// Build a bare `origin` with a `main` branch and a clone checked out on a
    /// `task` branch cut from `main`'s tip. Returns `(origin_tmp, clone_ws)`.
    /// The `Workspace` is `Attached` so it borrows the clone dir; the two
    /// `TempDir`s keep both alive for the test.
    fn fixture() -> (TempDir, TempDir, Workspace) {
        let origin = TempDir::new().expect("origin tmp");
        let op = origin.path();
        git(op, &["init", "--bare", "-b", "main"]);

        // Seed `main` via a throwaway working clone.
        let seed = TempDir::new().expect("seed tmp");
        let sp = seed.path();
        git(sp, &["clone", op.to_str().unwrap(), "."]);
        write(sp, "shared.txt", "base-v1\n");
        git(sp, &["add", "-A"]);
        git(sp, &["commit", "-m", "base v1"]);
        git(sp, &["push", "origin", "main"]);

        // The "task workspace": clone on main, branch off to `task`.
        let clone = TempDir::new().expect("clone tmp");
        let cp = clone.path();
        git(cp, &["clone", op.to_str().unwrap(), "."]);
        git(cp, &["checkout", "-b", "task"]);

        let ws = Workspace::attach_existing(cp, "task").expect("attach");
        (origin, clone, ws)
    }

    /// Advance `origin/main` by one commit (made in a fresh clone, then pushed).
    /// `contents` is written to `file` so callers control conflict vs. clean.
    fn advance_main(origin: &Path, file: &str, contents: &str, msg: &str) {
        let pusher = TempDir::new().expect("pusher tmp");
        let pp = pusher.path();
        git(pp, &["clone", origin.to_str().unwrap(), "."]);
        git(pp, &["checkout", "main"]);
        write(pp, file, contents);
        git(pp, &["add", "-A"]);
        git(pp, &["commit", "-m", msg]);
        git(pp, &["push", "origin", "main"]);
    }

    #[tokio::test]
    async fn is_up_to_date_true_when_branch_just_cut_from_target() {
        // task was cut from origin/main and nothing advanced main → current.
        let (_origin, _clone, ws) = fixture();
        assert!(
            ws.is_up_to_date_with("main").await.expect("check"),
            "freshly-cut task branch must report up-to-date with main"
        );
    }

    #[tokio::test]
    async fn behind_base_non_conflicting_merges_and_commits() {
        let (origin, clone, ws) = fixture();
        // Add a task-side commit touching a DIFFERENT file (no conflict).
        let cp = clone.path();
        write(cp, "task.txt", "task work\n");
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "task work"]);

        // main advances on a non-overlapping file.
        advance_main(origin.path(), "newfile.txt", "from-main\n", "main v2");

        // Now behind base.
        assert!(
            !ws.is_up_to_date_with("main").await.expect("check"),
            "task branch must report behind after main advances"
        );

        let head_before = git(cp, &["rev-parse", "HEAD"]);
        match ws.try_merge("main").await.expect("merge") {
            MergeOutcome::Clean => {}
            other => panic!("expected clean merge, got {other:?}"),
        }
        // Commit the staged merge (mirrors the supervisor's proactive-sync commit).
        let outcome = ws
            .commit(
                "Merge main into task",
                GitIdentity {
                    name: "t",
                    email: "t@t",
                },
            )
            .await
            .expect("commit");
        assert!(
            outcome.committed(),
            "clean behind-base merge must produce a commit"
        );

        let head_after = git(cp, &["rev-parse", "HEAD"]);
        assert_ne!(
            head_before.trim(),
            head_after.trim(),
            "HEAD must advance after the merge commit"
        );
        // origin/main is now an ancestor of HEAD → idempotent (next cycle skips).
        assert!(
            ws.is_up_to_date_with("main").await.expect("recheck"),
            "after merge+commit, origin/main must be an ancestor of HEAD"
        );
        // It's a real merge commit (two parents).
        let parents = git(cp, &["rev-list", "--parents", "-n", "1", "HEAD"]);
        assert_eq!(
            parents.split_whitespace().count(),
            3,
            "expected a merge commit (sha + two parents): {parents}"
        );
        // The main-side file landed on the task branch.
        assert!(
            cp.join("newfile.txt").exists(),
            "main's file must be merged in"
        );
    }

    #[tokio::test]
    async fn conflicting_base_change_leaves_markers_and_lists_files() {
        let (origin, clone, ws) = fixture();
        let cp = clone.path();
        // Task edits shared.txt one way...
        write(cp, "shared.txt", "task-edit\n");
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "task edits shared"]);

        // ...main edits the SAME file differently → conflict.
        advance_main(
            origin.path(),
            "shared.txt",
            "main-edit\n",
            "main edits shared",
        );

        assert!(
            !ws.is_up_to_date_with("main").await.expect("check"),
            "conflicting divergence must report behind"
        );

        match ws.try_merge("main").await.expect("merge") {
            MergeOutcome::Conflicts { files } => {
                assert_eq!(files, vec!["shared.txt".to_string()]);
            }
            other => panic!("expected conflicts, got {other:?}"),
        }
        // Standard conflict markers are on disk for the worker's tools to edit.
        let disk = std::fs::read_to_string(cp.join("shared.txt")).expect("read");
        assert!(
            disk.contains("<<<<<<<") && disk.contains("=======") && disk.contains(">>>>>>>"),
            "conflict markers must be present on disk:\n{disk}"
        );
    }

    // ---- enforce_merge_parent (ConflictRetry guarantee) ------------------

    /// Identity used for the synthetic merge-completion commit in tests.
    const TEST_IDENT: GitIdentity<'static> = GitIdentity {
        name: "djinn-bot",
        email: "bot@djinn.local",
    };

    /// Set up a conflicted merge of `main` into `task` and return
    /// `(origin, clone, ws, main_sha)`. The worktree is mid-merge with markers
    /// on `shared.txt` (MERGE_HEAD set); callers then simulate the worker.
    async fn conflicted_merge_fixture() -> (TempDir, TempDir, Workspace, String) {
        let (origin, clone, ws) = fixture();
        let cp = clone.path();
        write(cp, "shared.txt", "task-edit\n");
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "task edits shared"]);
        advance_main(
            origin.path(),
            "shared.txt",
            "main-edit\n",
            "main edits shared",
        );
        // Stage the conflicted merge (fetches origin/main, sets MERGE_HEAD).
        match ws.try_merge("main").await.expect("merge") {
            MergeOutcome::Conflicts { .. } => {}
            other => panic!("expected conflicts, got {other:?}"),
        }
        let main_sha = git(cp, &["rev-parse", "origin/main"]).trim().to_string();
        (origin, clone, ws, main_sha)
    }

    fn parent_count(dir: &Path) -> usize {
        let parents = git(dir, &["rev-list", "--parents", "-n", "1", "HEAD"]);
        // Output is "<sha> <parent1> <parent2> ..."; subtract the commit itself.
        parents.split_whitespace().count() - 1
    }

    /// Worker preserved MERGE_HEAD and the post-worker auto-commit recorded a
    /// real two-parent merge: enforce_merge_parent reports AlreadyMerged.
    #[tokio::test]
    async fn enforce_merge_parent_already_merged_when_merge_head_survived() {
        let (_origin, clone, ws, main_sha) = conflicted_merge_fixture().await;
        let cp = clone.path();
        // Worker resolves the markers, leaving MERGE_HEAD intact.
        write(cp, "shared.txt", "resolved-both\n");
        // The post-worker auto-commit (MERGE_HEAD set) → real merge commit.
        ws.commit("resolution", TEST_IDENT).await.expect("commit");
        assert_eq!(
            parent_count(cp),
            2,
            "auto-commit should be a 2-parent merge"
        );

        let outcome = ws
            .enforce_merge_parent(&main_sha, TEST_IDENT)
            .await
            .expect("enforce");
        assert_eq!(outcome, MergeParentOutcome::AlreadyMerged);
        assert!(
            ws.is_ancestor(&main_sha, "HEAD").await.expect("anc"),
            "merge target must be an ancestor"
        );
        assert_eq!(
            std::fs::read_to_string(cp.join("shared.txt")).unwrap(),
            "resolved-both\n"
        );
    }

    /// Worker aborted the merge (cleared MERGE_HEAD) then hand-committed a
    /// SINGLE-parent "resolution": enforce_merge_parent reconstructs a true
    /// two-parent merge whose tree equals the worker's resolved content.
    #[tokio::test]
    async fn enforce_merge_parent_recovers_when_worker_hand_committed_single_parent() {
        let (_origin, clone, ws, main_sha) = conflicted_merge_fixture().await;
        let cp = clone.path();
        // Worker resolves content, then aborts the merge and commits one parent.
        write(cp, "shared.txt", "resolved-both\n");
        git(cp, &["merge", "--abort"]);
        // merge --abort discards the worktree edit too; re-apply the resolution.
        write(cp, "shared.txt", "resolved-both\n");
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "single-parent resolution"]);
        assert_eq!(parent_count(cp), 1, "worker commit is single-parent");
        assert!(
            !ws.is_ancestor(&main_sha, "HEAD").await.expect("anc"),
            "precondition: target not yet an ancestor"
        );

        let outcome = ws
            .enforce_merge_parent(&main_sha, TEST_IDENT)
            .await
            .expect("enforce");
        let new_head = match outcome {
            MergeParentOutcome::Recovered { new_head } => new_head,
            other => panic!("expected Recovered, got {other:?}"),
        };
        assert_eq!(
            git(cp, &["rev-parse", "HEAD"]).trim(),
            new_head,
            "branch must point at the reconstructed merge commit"
        );
        assert_eq!(parent_count(cp), 2, "reconstructed commit has two parents");
        assert!(
            ws.is_ancestor(&main_sha, "HEAD").await.expect("anc"),
            "merge target must now be an ancestor"
        );
        // Tree is unchanged — the diff equals the worker's resolution.
        assert_eq!(
            std::fs::read_to_string(cp.join("shared.txt")).unwrap(),
            "resolved-both\n"
        );
    }

    /// Worker resolved the markers but left the result UNCOMMITTED (no
    /// MERGE_HEAD because it cleared it, e.g. via reset): enforce_merge_parent
    /// stages the worktree and records the two-parent merge over it.
    #[tokio::test]
    async fn enforce_merge_parent_recovers_when_resolution_uncommitted() {
        let (_origin, clone, ws, main_sha) = conflicted_merge_fixture().await;
        let cp = clone.path();
        // Worker resolved content but then `git reset` cleared MERGE_HEAD and
        // unstaged everything, leaving the resolution as an uncommitted edit.
        write(cp, "shared.txt", "resolved-both\n");
        git(cp, &["add", "-A"]);
        git(cp, &["reset"]); // unstage + clear MERGE_HEAD, keep worktree
        let head_before = git(cp, &["rev-parse", "HEAD"]).trim().to_string();

        let outcome = ws
            .enforce_merge_parent(&main_sha, TEST_IDENT)
            .await
            .expect("enforce");
        let new_head = match outcome {
            MergeParentOutcome::Recovered { new_head } => new_head,
            other => panic!("expected Recovered, got {other:?}"),
        };
        assert_ne!(new_head, head_before, "a new merge commit was created");
        assert_eq!(parent_count(cp), 2, "two parents");
        assert!(
            ws.is_ancestor(&main_sha, "HEAD").await.expect("anc"),
            "merge target must now be an ancestor"
        );
        assert_eq!(
            std::fs::read_to_string(cp.join("shared.txt")).unwrap(),
            "resolved-both\n"
        );
        // The first parent is the worker's pre-merge HEAD (content preserved).
        let first_parent = git(cp, &["rev-parse", "HEAD^1"]).trim().to_string();
        assert_eq!(first_parent, head_before);
    }

    /// If the worktree still has UNMERGED paths (worker never resolved), the
    /// guarantee refuses to fabricate a merge — it errors loudly so the caller
    /// fails the stage instead of pushing a conflicting "resolution".
    #[tokio::test]
    async fn enforce_merge_parent_errors_on_unmerged_index() {
        let (_origin, clone, ws, main_sha) = conflicted_merge_fixture().await;
        let cp = clone.path();
        // Leave the conflict markers in place (worker did nothing).
        let disk = std::fs::read_to_string(cp.join("shared.txt")).unwrap();
        assert!(disk.contains("<<<<<<<"), "precondition: markers present");

        let err = ws
            .enforce_merge_parent(&main_sha, TEST_IDENT)
            .await
            .expect_err("must refuse an unmerged tree");
        match err {
            EphemeralWorkspaceError::Git(msg) => {
                assert!(
                    msg.contains("unmerged"),
                    "error must mention unmerged paths: {msg}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn attach_existing_wraps_existing_dir_without_temp_ownership() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().to_path_buf();
        let ws = Workspace::attach_existing(&path, "main").expect("attach");
        assert_eq!(ws.path(), path);
        assert_eq!(ws.branch(), "main");
        // Dropping the workspace must NOT remove the caller-owned directory.
        drop(ws);
        assert!(path.exists(), "attach_existing must not delete the dir");
    }

    #[test]
    fn attach_existing_rejects_missing_path() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("nope");
        let err = Workspace::attach_existing(&missing, "main").unwrap_err();
        assert!(matches!(err, EphemeralWorkspaceError::Io(_)));
    }

    #[test]
    fn attach_existing_rejects_non_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, b"not a dir").expect("write");
        let err = Workspace::attach_existing(&file, "main").unwrap_err();
        match err {
            EphemeralWorkspaceError::Git(msg) => assert!(msg.contains("not a directory")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- teardown_owned / is_owned --------------------------------------

    #[test]
    fn teardown_owned_removes_directory_and_prevents_double_drop() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().to_path_buf();
        let ws = Workspace::new(dir, "main".to_string());
        assert!(ws.is_owned(), "new workspace must be owned");
        // teardown_owned consumes self and closes the TempDir.
        ws.teardown_owned().expect("teardown_owned must succeed");
        assert!(
            !path.exists(),
            "teardown_owned must remove the owned directory"
        );
    }

    #[test]
    fn teardown_owned_on_attached_is_noop_and_does_not_delete() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().to_path_buf();
        let ws = Workspace::attach_existing(&path, "main").expect("attach");
        assert!(!ws.is_owned(), "attached workspace must not be owned");
        // teardown_owned returns Ok and does NOT delete the externally owned dir.
        ws.teardown_owned().expect("attached teardown must be Ok");
        assert!(
            path.exists(),
            "teardown_owned must NOT delete attached directory"
        );
    }

    // ---- normalize_mtimes -------------------------------------------------

    /// Commit the staged tree at `dir` with both author + committer date pinned
    /// to `unix_ts`, so the resulting commit's `%ct` is deterministic.
    fn commit_at(dir: &Path, msg: &str, unix_ts: i64) {
        let date = format!("{unix_ts} +0000");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-m", msg])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("spawn git commit");
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn mtime_secs(p: &Path) -> i64 {
        let m = std::fs::metadata(p).expect("metadata");
        m.modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("post-epoch")
            .as_secs() as i64
    }

    /// Build a real repo with two commits at distinct timestamps touching
    /// different files; after normalize each file's mtime must equal the commit
    /// that last touched it.
    #[tokio::test]
    async fn normalize_sets_mtime_to_last_touching_commit() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);

        // Commit 1 @ T1: a.txt + shared.txt.
        const T1: i64 = 1_600_000_000;
        write(dir, "a.txt", "a-v1\n");
        write(dir, "shared.txt", "s-v1\n");
        git(dir, &["add", "-A"]);
        commit_at(dir, "c1", T1);

        // Commit 2 @ T2: only b.txt (a.txt + shared.txt untouched since T1).
        const T2: i64 = 1_600_086_400; // T1 + 1 day
        write(dir, "b.txt", "b-v1\n");
        git(dir, &["add", "-A"]);
        commit_at(dir, "c2", T2);

        let ws = Workspace::attach_existing(dir, "main").expect("attach");
        ws.normalize_mtimes().await;

        assert_eq!(
            mtime_secs(&dir.join("a.txt")),
            T1,
            "a.txt last touched at T1"
        );
        assert_eq!(
            mtime_secs(&dir.join("shared.txt")),
            T1,
            "shared.txt last touched at T1"
        );
        assert_eq!(
            mtime_secs(&dir.join("b.txt")),
            T2,
            "b.txt last touched at T2"
        );
    }

    /// A file edited AFTER normalize keeps its fresh (current) mtime — within-run
    /// incremental builds must not be clobbered by a re-normalize.
    #[tokio::test]
    async fn normalize_does_not_clobber_post_edit_mtime() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);

        const T1: i64 = 1_600_000_000;
        write(dir, "a.txt", "a-v1\n");
        git(dir, &["add", "-A"]);
        commit_at(dir, "c1", T1);

        let ws = Workspace::attach_existing(dir, "main").expect("attach");
        ws.normalize_mtimes().await;
        assert_eq!(mtime_secs(&dir.join("a.txt")), T1);

        // Worker edits a.txt (uncommitted). Its mtime is now ~now, far ahead of T1.
        write(dir, "a.txt", "a-v2 worker edit\n");
        let after_edit = mtime_secs(&dir.join("a.txt"));
        assert!(after_edit > T1, "post-edit mtime must be current, not T1");

        // A second normalize must NOT reset the uncommitted edit back to T1: the
        // newest commit naming a.txt is still c1@T1, so it WOULD — but the worker
        // edit is uncommitted and we accept that normalize only runs once per
        // materialization (pre-stage), never after edits. Assert idempotency on
        // the COMMITTED state instead: re-running on the clean tree is harmless.
        git(dir, &["checkout", "--", "a.txt"]); // discard the edit → back to committed
        ws.normalize_mtimes().await;
        ws.normalize_mtimes().await; // twice = idempotent
        assert_eq!(
            mtime_secs(&dir.join("a.txt")),
            T1,
            "idempotent: repeated normalize on clean tree yields the same mtime"
        );
    }

    /// Empty / merge-style commits (no name-only output) must not derail the
    /// walk, and files whose last touch predates them keep the right timestamp.
    #[tokio::test]
    async fn normalize_handles_empty_commits_in_history() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);

        const T1: i64 = 1_600_000_000;
        write(dir, "a.txt", "a-v1\n");
        git(dir, &["add", "-A"]);
        commit_at(dir, "c1", T1);

        // Empty commit on top at T2 — names no files.
        const T2: i64 = 1_600_086_400;
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "--allow-empty", "-m", "empty"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", format!("{T2} +0000"))
            .env("GIT_COMMITTER_DATE", format!("{T2} +0000"))
            .output()
            .expect("spawn");
        assert!(out.status.success());

        let ws = Workspace::attach_existing(dir, "main").expect("attach");
        ws.normalize_mtimes().await;

        assert_eq!(
            mtime_secs(&dir.join("a.txt")),
            T1,
            "a.txt keeps T1 despite the empty T2 commit on top"
        );
    }

    /// Empty repo (no tracked files) is a clean no-op, not an error.
    #[tokio::test]
    async fn normalize_noop_on_empty_repo() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        let ws = Workspace::attach_existing(dir, "main").expect("attach");
        // Must not panic / error even with zero commits + zero tracked files.
        ws.normalize_mtimes().await;
    }

    /// Create a sparse file of `len` bytes at `path` (reports `len` via
    /// `metadata().len()` without writing real data — keeps the >100 MB test
    /// cheap on disk).
    fn sparse_file(path: &Path, len: u64) {
        let f = std::fs::File::create(path).expect("create sparse file");
        f.set_len(len).expect("set_len");
    }

    /// `commit` must refuse to stage a file over GitHub's 100 MB hard limit —
    /// the exact footgun behind the `task/aqmk` GH001 push rejection (a pnpm
    /// store swept in by staging when HOME drifted into the worktree). The
    /// error must name the offending path, and NO commit may be created.
    #[tokio::test]
    async fn commit_rejects_oversized_staged_file() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();
        let head_before = git(cp, &["rev-parse", "HEAD"]).trim().to_string();

        // 101 MB > the 100 MiB limit; a normal small file is also staged to
        // prove the guard fires on the big one specifically, not "any change".
        sparse_file(&cp.join("cache.bin"), 101 * 1024 * 1024);
        write(cp, "real.txt", "legit change\n");

        let err = ws
            .commit("work", TEST_IDENT)
            .await
            .expect_err("commit must be refused when an oversized file is staged");
        let msg = format!("{err}");
        assert!(
            msg.contains("cache.bin") && msg.contains("100 MB"),
            "error must name the oversized path and the limit, got: {msg}"
        );
        assert_eq!(
            git(cp, &["rev-parse", "HEAD"]).trim(),
            head_before,
            "no commit may be created when the guard trips"
        );
    }

    /// Control: an ordinary (sub-limit) change still commits cleanly — the
    /// guard must not block normal work.
    #[tokio::test]
    async fn commit_allows_normal_sized_files() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();
        write(cp, "real.txt", "legit change\n");
        // A few MB is well under the limit and must pass.
        sparse_file(&cp.join("medium.bin"), 5 * 1024 * 1024);

        let outcome = ws.commit("work", TEST_IDENT).await.expect("commit");
        assert!(outcome.committed(), "a sub-limit change must commit");
        assert_eq!(
            git(cp, &["log", "--oneline"]).lines().count(),
            2,
            "exactly the base commit plus the worker commit"
        );
    }

    // ── Filtered-staging regression tests ──────────────────────────────────

    /// Legitimate source edit plus root-level scratch files: only the
    /// legitimate file must be committed; scratch files must be excluded.
    #[tokio::test]
    async fn commit_filters_scratch_stages_only_legitimate() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        // Legitimate change
        write(cp, "real.rs", "fn main() {}\n");
        // Scratch files at root
        write(cp, "patch.txt", "scratch content\n");
        write(cp, "test2.txt", "scratch content\n");
        write(cp, "test3.txt", "scratch content\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed with mixed legitimate + scratch");
        assert!(
            outcome.committed(),
            "legitimate changes must produce a CommitOutcome::Committed"
        );

        // Verify only the legitimate file was committed.
        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("real.rs"),
            "legitimate file must be committed, got: {committed_files}"
        );
        assert!(
            !committed_files.contains("patch.txt"),
            "scratch patch.txt must not be committed, got: {committed_files}"
        );
        assert!(
            !committed_files.contains("test2.txt"),
            "scratch test2.txt must not be committed, got: {committed_files}"
        );
        assert!(
            !committed_files.contains("test3.txt"),
            "scratch test3.txt must not be committed, got: {committed_files}"
        );

        // The committed outcome must surface the excluded scratch paths.
        let excluded = outcome.excluded();
        assert!(
            excluded.contains(&"patch.txt".to_string()),
            "patch.txt must appear in excluded list, got: {excluded:?}"
        );
        assert!(
            excluded.contains(&"test2.txt".to_string()),
            "test2.txt must appear in excluded list, got: {excluded:?}"
        );
        assert!(
            excluded.contains(&"test3.txt".to_string()),
            "test3.txt must appear in excluded list, got: {excluded:?}"
        );
    }

    /// Junk-only changes produce NoLegitimateChanges with excluded paths.
    #[tokio::test]
    async fn commit_junk_only_returns_no_legitimate_changes() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();
        let head_before = git(cp, &["rev-parse", "HEAD"]).trim().to_string();

        // Only scratch files at root
        write(cp, "patch.txt", "scratch content\n");
        write(cp, "test.txt", "scratch content\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed (not error) even for junk-only");
        match &outcome {
            CommitOutcome::NoLegitimateChanges { excluded } => {
                assert!(
                    excluded.contains(&"patch.txt".to_string()),
                    "patch.txt must be in excluded list, got: {excluded:?}"
                );
                assert!(
                    excluded.contains(&"test.txt".to_string()),
                    "test.txt must be in excluded list, got: {excluded:?}"
                );
            }
            other => panic!("expected NoLegitimateChanges, got {other:?}"),
        }

        // No new commit may be created.
        assert_eq!(
            git(cp, &["rev-parse", "HEAD"]).trim(),
            head_before,
            "no commit may be created for junk-only changes"
        );
    }

    /// Fixture/testdata paths with scratch-like basenames are allowed.
    #[tokio::test]
    async fn commit_allows_fixture_paths_even_with_scratch_basename() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        // Create fixture directory structure
        std::fs::create_dir_all(cp.join("tests/fixtures")).expect("mkdir");
        write(cp, "tests/fixtures/patch.txt", "fixture data\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(outcome.committed(), "fixture paths must be committed");

        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("tests/fixtures/patch.txt"),
            "fixture file must be committed even with scratch-like basename, got: {committed_files}"
        );
    }

    /// Clean tree returns NoChanges.
    #[tokio::test]
    async fn commit_clean_tree_returns_no_changes() {
        let (_origin, _clone, ws) = fixture();

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed on clean tree");
        assert_eq!(
            outcome,
            CommitOutcome::NoChanges,
            "clean tree must return NoChanges"
        );
    }

    /// Fixture path with testdata directory component is also allowed.
    #[tokio::test]
    async fn commit_allows_testdata_fixture_paths() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        std::fs::create_dir_all(cp.join("testdata")).expect("mkdir");
        write(cp, "testdata/test.txt", "testdata fixture\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(
            outcome.committed(),
            "testdata fixture paths must be committed"
        );

        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("testdata/test.txt"),
            "testdata fixture must be committed even with scratch-like basename, got: {committed_files}"
        );
    }

    /// Root-level patch prefix file (e.g. `patch_output`) is also excluded.
    #[tokio::test]
    async fn commit_excludes_root_patch_prefix_files() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        // Legitimate change (write to the already-tracked file)
        write(cp, "shared.txt", "modified\n");
        // Root-level file matching "patch" prefix
        write(cp, "patch_output", "scratch\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(outcome.committed(), "legitimate change must commit");

        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("shared.txt"),
            "legitimate file must be committed, got: {committed_files}"
        );
        assert!(
            !committed_files.contains("patch_output"),
            "patch prefix file must not be committed, got: {committed_files}"
        );
    }

    /// Nested scratch files (not at root) are allowed.
    #[tokio::test]
    async fn commit_allows_nested_scratch_like_files() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        // Create subdirectory and write a scratch-like file there
        std::fs::create_dir_all(cp.join("src")).expect("mkdir");
        write(cp, "src/patch.txt", "// nested\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(
            outcome.committed(),
            "nested scratch-like files must be allowed"
        );

        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("src/patch.txt"),
            "nested scratch-like file must be committed, got: {committed_files}"
        );
    }

    // ---- Pre-staged scratch file regressions (reviewer feedback) -----------

    /// Pre-staged scratch-only (worker ran `git add patch.txt`): must produce
    /// `NoLegitimateChanges`, NOT `Committed`.
    #[tokio::test]
    async fn commit_prestaged_scratch_only_returns_no_legitimate_changes() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();
        let head_before = git(cp, &["rev-parse", "HEAD"]).trim().to_string();

        // Worker writes and pre-stages a scratch file before supervisor calls commit.
        write(cp, "patch.txt", "scratch content\n");
        git(cp, &["add", "patch.txt"]);

        // Verify precondition: patch.txt is staged.
        let cached = git(cp, &["diff", "--cached", "--name-only"]);
        assert!(
            cached.contains("patch.txt"),
            "precondition: patch.txt must be staged before commit, got: {cached}"
        );

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must not error for pre-staged scratch");
        match &outcome {
            CommitOutcome::NoLegitimateChanges { excluded } => {
                assert!(
                    excluded.contains(&"patch.txt".to_string()),
                    "patch.txt must be in excluded list, got: {excluded:?}"
                );
            }
            other => {
                panic!("expected NoLegitimateChanges for pre-staged scratch-only, got {other:?}")
            }
        }

        // No commit may be created.
        assert_eq!(
            git(cp, &["rev-parse", "HEAD"]).trim(),
            head_before,
            "no commit may be created for pre-staged scratch-only"
        );

        // patch.txt must have been unstaged.
        let cached_after = git(cp, &["diff", "--cached", "--name-only"]);
        assert!(
            !cached_after.contains("patch.txt"),
            "pre-staged scratch must be unstaged, cached still has: {cached_after}"
        );
    }

    /// Pre-staged scratch + working-tree legitimate edit: must commit only the
    /// legitimate file and unstage the scratch.
    #[tokio::test]
    async fn commit_prestaged_scratch_with_legitimate_edit_commits_only_legitimate() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        // Worker pre-stages scratch files.
        write(cp, "patch.txt", "scratch\n");
        write(cp, "test2.txt", "scratch\n");
        git(cp, &["add", "patch.txt", "test2.txt"]);

        // Supervisor then writes a legitimate change (working-tree dirty).
        write(cp, "real.rs", "fn main() {}\n");

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(
            outcome.committed(),
            "legitimate change must produce Committed"
        );

        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("real.rs"),
            "legitimate file must be committed, got: {committed_files}"
        );
        assert!(
            !committed_files.contains("patch.txt"),
            "pre-staged scratch patch.txt must not be committed, got: {committed_files}"
        );
        assert!(
            !committed_files.contains("test2.txt"),
            "pre-staged scratch test2.txt must not be committed, got: {committed_files}"
        );
    }

    /// Pre-staged fixture file (`tests/fixtures/patch.txt`): must be allowed
    /// through and committed, since fixture paths bypass scratch checks.
    #[tokio::test]
    async fn commit_prestaged_fixture_file_is_allowed() {
        let (_origin, clone, ws) = fixture();
        let cp = clone.path();

        // Worker creates and pre-stages a fixture file.
        std::fs::create_dir_all(cp.join("tests/fixtures")).expect("mkdir");
        write(cp, "tests/fixtures/patch.txt", "fixture data\n");
        git(cp, &["add", "tests/fixtures/patch.txt"]);

        let outcome = ws
            .commit("work", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(
            outcome.committed(),
            "pre-staged fixture file must be committed"
        );

        let committed_files = git(cp, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            committed_files.contains("tests/fixtures/patch.txt"),
            "fixture file must be in commit even when pre-staged, got: {committed_files}"
        );
    }

    /// Pre-staged scratch alongside clean merge staging: the merge files must
    /// survive while scratch files are stripped.
    #[tokio::test]
    async fn commit_prestaged_scratch_with_clean_merge_preserves_merge_files() {
        let (origin, clone, ws) = fixture();
        let cp = clone.path();

        // Task-side commit on a different file (no conflict).
        write(cp, "task.txt", "task work\n");
        git(cp, &["add", "-A"]);
        git(cp, &["commit", "-m", "task work"]);

        // Main advances on a non-overlapping file.
        advance_main(origin.path(), "newfile.txt", "from-main\n", "main v2");

        // Clean merge into task.
        match ws.try_merge("main").await.expect("merge") {
            MergeOutcome::Clean => {}
            other => panic!("expected clean merge, got {other:?}"),
        }

        // The merge staged `newfile.txt` (index-only).
        // Now simulate a worker that also pre-staged a scratch file.
        write(cp, "patch.txt", "scratch\n");
        git(cp, &["add", "patch.txt"]);

        let outcome = ws
            .commit("Merge main into task", TEST_IDENT)
            .await
            .expect("commit must succeed");
        assert!(
            outcome.committed(),
            "merge + scratch must still produce a committed outcome"
        );

        // Use `git ls-tree -r HEAD` to inspect the committed tree —
        // `git show --name-only` uses combined-diff for merge commits which
        // omits cleanly-merged paths.
        let tree_files = git(cp, &["ls-tree", "--name-only", "-r", "HEAD"]);
        assert!(
            tree_files.contains("newfile.txt"),
            "merge file must be in the committed tree, got: {tree_files}"
        );
        assert!(
            !tree_files.contains("patch.txt"),
            "scratch file must not sneak in via pre-staging, got: {tree_files}"
        );
    }
}

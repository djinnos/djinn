use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum EphemeralWorkspaceError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("git: {0}")]
    Git(String),
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

    /// Stage every change and commit with `message` under `identity`.
    ///
    /// Returns `Ok(true)` if a commit was created, `Ok(false)` if the tree
    /// was clean (nothing to commit). Both outcomes are success — callers
    /// that require a commit should check the return value.
    pub async fn commit(
        &self,
        message: &str,
        identity: GitIdentity<'_>,
    ) -> Result<bool, EphemeralWorkspaceError> {
        self.run_git(&["add", "-A"], &[]).await?;
        let staged = self
            .run_git(&["diff", "--cached", "--name-only"], &[])
            .await?;
        if staged.trim().is_empty() {
            return Ok(false);
        }
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
        Ok(true)
    }

    /// Explicit teardown. Equivalent to `drop(self)` — the `TempDir` cleans
    /// itself up on drop. Callers may prefer the explicit form to document
    /// lifecycle points in supervisor code.
    pub fn teardown(self) {}

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
        let output = Command::new("git")
            .arg("-C")
            .arg(self.root.path())
            .args(["merge-base", "--is-ancestor", &merge_ref, "HEAD"])
            .output()
            .await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(EphemeralWorkspaceError::Git(format!(
                    "git merge-base --is-ancestor {merge_ref} HEAD: {}",
                    stderr.trim()
                )))
            }
        }
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
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(self.root.path())
            .args(["merge", "--no-commit", "--no-ff", &merge_ref])
            .env("GIT_AUTHOR_NAME", "djinn-bot")
            .env("GIT_AUTHOR_EMAIL", "bot@djinn.local")
            .env("GIT_COMMITTER_NAME", "djinn-bot")
            .env("GIT_COMMITTER_EMAIL", "bot@djinn.local");
        let output = cmd.output().await?;
        if output.status.success() {
            return Ok(MergeOutcome::Clean);
        }

        let unmerged = self
            .run_git(&["diff", "--name-only", "--diff-filter=U"], &[])
            .await?;
        let files: Vec<String> = unmerged
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if files.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EphemeralWorkspaceError::Git(format!(
                "git merge --no-commit --no-ff {merge_ref}: {}",
                stderr.trim()
            )));
        }

        Ok(MergeOutcome::Conflicts { files })
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
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.root.path()).args(args);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EphemeralWorkspaceError::Git(format!(
                "git {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
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
        let committed = ws
            .commit(
                "Merge main into task",
                GitIdentity {
                    name: "t",
                    email: "t@t",
                },
            )
            .await
            .expect("commit");
        assert!(committed, "clean behind-base merge must produce a commit");

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
}

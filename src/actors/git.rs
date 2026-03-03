//! GitActor — serializes all git operations for a single repository.
//!
//! One actor per project path, stored in `AppState`. Uses the Ryhl hand-rolled
//! actor pattern: `GitActorHandle` (mpsc sender) is the public API; `GitActor`
//! (mpsc receiver) runs in a dedicated tokio task.
//!
//! Hybrid approach (GIT-05):
//!   - Reads → git2 crate (status, diff, ref queries)
//!   - Writes → `tokio::process::Command` git CLI (worktree, merge, push)

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git2: {0}")]
    Git(#[from] git2::Error),

    #[error("git command failed (exit {code}): {stderr}")]
    CommandFailed { code: i32, stderr: String },

    /// A git hook (pre-commit, commit-msg, etc.) rejected the commit (GIT-07).
    /// `output` contains the hook's stdout/stderr for surfacing in the activity log.
    #[error("git hook failed (exit {code}): {output}")]
    HookFailed { code: i32, output: String },

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("actor channel closed")]
    ActorDead,

    #[error("no response from actor")]
    NoResponse,
}

// ─── Value types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StatusSummary {
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Result of a successful squash-merge (GIT-03).
#[derive(Debug, Clone)]
pub struct MergeResult {
    /// SHA of the squash commit on the target branch.
    pub commit_sha: String,
}

// ─── Messages ─────────────────────────────────────────────────────────────────

type Reply<T> = oneshot::Sender<Result<T, GitError>>;

enum GitMessage {
    /// Return the short name of the current branch (git2 read).
    GetCurrentBranch { respond_to: Reply<String> },
    /// Return a summary of the working-tree status (git2 read).
    GetStatus { respond_to: Reply<StatusSummary> },
    /// Return the HEAD commit SHA and first-line message (git2 read).
    GetHeadCommit { respond_to: Reply<CommitInfo> },
    /// Run an arbitrary `git <args>` CLI command (write path).
    RunCommand {
        args: Vec<String>,
        respond_to: Reply<CommandOutput>,
    },
    /// Create `task/{short_id}` from `target_branch` and push to origin (GIT-01).
    CreateBranch {
        short_id: String,
        target_branch: String,
        respond_to: Reply<()>,
    },
    /// Squash-merge `branch` into `target_branch` with `message` (GIT-03).
    /// Returns `Err(HookFailed)` if a git hook rejects the commit.
    SquashMerge {
        branch: String,
        target_branch: String,
        message: String,
        respond_to: Reply<MergeResult>,
    },
    /// Force-delete `branch` locally and from origin (post-merge cleanup).
    DeleteBranch {
        branch: String,
        respond_to: Reply<()>,
    },
}

// ─── Actor ───────────────────────────────────────────────────────────────────

struct GitActor {
    path: PathBuf,
    repo: git2::Repository,
    receiver: mpsc::Receiver<GitMessage>,
}

impl GitActor {
    fn new(path: PathBuf, receiver: mpsc::Receiver<GitMessage>) -> Result<Self, GitError> {
        let repo = git2::Repository::open(&path)?;
        Ok(Self { path, repo, receiver })
    }

    async fn run(mut self) {
        tracing::debug!(path = %self.path.display(), "GitActor started");
        while let Some(msg) = self.receiver.recv().await {
            self.handle(msg).await;
        }
        tracing::debug!(path = %self.path.display(), "GitActor stopped");
    }

    async fn handle(&mut self, msg: GitMessage) {
        match msg {
            GitMessage::GetCurrentBranch { respond_to } => {
                let result = tokio::task::block_in_place(|| self.current_branch());
                let _ = respond_to.send(result);
            }
            GitMessage::GetStatus { respond_to } => {
                let result = tokio::task::block_in_place(|| self.status());
                let _ = respond_to.send(result);
            }
            GitMessage::GetHeadCommit { respond_to } => {
                let result = tokio::task::block_in_place(|| self.head_commit());
                let _ = respond_to.send(result);
            }
            GitMessage::RunCommand { args, respond_to } => {
                // Clone path so no &self reference crosses the await point
                // (git2::Repository is Send but not Sync).
                let path = self.path.clone();
                let result = Self::run_git_command(path, args).await;
                let _ = respond_to.send(result);
            }
            GitMessage::CreateBranch { short_id, target_branch, respond_to } => {
                let path = self.path.clone();
                let result = Self::create_branch_impl(path, short_id, target_branch).await;
                let _ = respond_to.send(result);
            }
            GitMessage::SquashMerge { branch, target_branch, message, respond_to } => {
                let path = self.path.clone();
                let result = Self::squash_merge_impl(path, branch, target_branch, message).await;
                let _ = respond_to.send(result);
            }
            GitMessage::DeleteBranch { branch, respond_to } => {
                let path = self.path.clone();
                let result = Self::delete_branch_impl(path, branch).await;
                let _ = respond_to.send(result);
            }
        }
    }

    // ── git2 reads ───────────────────────────────────────────────────────────

    fn current_branch(&self) -> Result<String, GitError> {
        let head = self.repo.head()?;
        Ok(head.shorthand().unwrap_or("HEAD").to_string())
    }

    fn status(&self) -> Result<StatusSummary, GitError> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        let statuses = self.repo.statuses(Some(&mut opts))?;

        let mut staged = Vec::new();
        let mut modified = Vec::new();
        let mut untracked = Vec::new();

        for entry in statuses.iter() {
            let path = entry.path().unwrap_or("").to_string();
            let s = entry.status();

            if s.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED,
            ) {
                staged.push(path.clone());
            }
            if s.intersects(git2::Status::WT_MODIFIED | git2::Status::WT_DELETED) {
                modified.push(path.clone());
            }
            if s.contains(git2::Status::WT_NEW) {
                untracked.push(path);
            }
        }

        Ok(StatusSummary { staged, modified, untracked })
    }

    fn head_commit(&self) -> Result<CommitInfo, GitError> {
        let head = self.repo.head()?;
        let commit = head.peel_to_commit()?;
        Ok(CommitInfo {
            sha: commit.id().to_string(),
            message: commit.summary().unwrap_or("").to_string(),
        })
    }

    // ── CLI writes ────────────────────────────────────────────────────────────

    /// Static so no `&self` crosses the await point (git2::Repository: !Sync).
    async fn run_git_command(path: PathBuf, args: Vec<String>) -> Result<CommandOutput, GitError> {
        let output = tokio::process::Command::new("git")
            .args(&args)
            .current_dir(&path)
            .output()
            .await?;

        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            return Err(GitError::CommandFailed { code, stderr });
        }

        Ok(CommandOutput { stdout, stderr, code })
    }

    /// Create `task/{short_id}` from `target_branch`, push to origin (GIT-01).
    async fn create_branch_impl(
        path: PathBuf,
        short_id: String,
        target_branch: String,
    ) -> Result<(), GitError> {
        let branch_name = format!("task/{short_id}");

        // Fetch latest from remote (best effort — local-only repos just skip this).
        let _ = Self::run_git_command(
            path.clone(),
            vec!["fetch".into(), "origin".into(), target_branch.clone()],
        )
        .await;

        // Prefer remote tracking ref; fall back to local branch.
        let remote_ref = format!("origin/{target_branch}");
        let checkout = Self::run_git_command(
            path.clone(),
            vec!["checkout".into(), "-b".into(), branch_name.clone(), remote_ref],
        )
        .await;

        if checkout.is_err() {
            Self::run_git_command(
                path.clone(),
                vec!["checkout".into(), "-b".into(), branch_name.clone(), target_branch],
            )
            .await?;
        }

        // Push new branch to remote (GIT-01 requires it on the remote).
        Self::run_git_command(
            path,
            vec!["push".into(), "-u".into(), "origin".into(), branch_name],
        )
        .await?;

        Ok(())
    }

    /// Squash-merge `branch` into `target_branch` with `message` (GIT-03).
    ///
    /// Hook awareness (GIT-07): any non-zero exit from `git commit` is wrapped
    /// in `GitError::HookFailed` so the coordinator can log it to the activity log.
    async fn squash_merge_impl(
        path: PathBuf,
        branch: String,
        target_branch: String,
        message: String,
    ) -> Result<MergeResult, GitError> {
        // Switch to target branch.
        Self::run_git_command(path.clone(), vec!["checkout".into(), target_branch]).await?;

        // Stage all changes from the task branch as a squash (no commit yet).
        Self::run_git_command(path.clone(), vec!["merge".into(), "--squash".into(), branch])
            .await?;

        // Commit — hooks run here. Any failure → HookFailed (GIT-07).
        match Self::run_git_command(path.clone(), vec!["commit".into(), "-m".into(), message])
            .await
        {
            Ok(_) => {}
            Err(GitError::CommandFailed { code, stderr }) => {
                return Err(GitError::HookFailed { code, output: stderr });
            }
            Err(e) => return Err(e),
        }

        // Read the resulting commit SHA.
        let out =
            Self::run_git_command(path, vec!["rev-parse".into(), "HEAD".into()]).await?;

        Ok(MergeResult { commit_sha: out.stdout.trim().into() })
    }

    /// Force-delete `branch` locally; also removes from origin (best effort).
    ///
    /// Uses `-D` because squash merges don't produce a merge commit, so git
    /// considers task branches "unmerged" even after a successful squash.
    async fn delete_branch_impl(path: PathBuf, branch: String) -> Result<(), GitError> {
        // Force-delete local branch.
        Self::run_git_command(path.clone(), vec!["branch".into(), "-D".into(), branch.clone()])
            .await?;

        // Delete remote branch (best effort — ignore if not pushed or no remote).
        let _ = Self::run_git_command(
            path,
            vec!["push".into(), "origin".into(), "--delete".into(), branch],
        )
        .await;

        Ok(())
    }
}

// ─── Handle ──────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to a per-repository `GitActor`.
#[derive(Clone)]
pub struct GitActorHandle {
    sender: mpsc::Sender<GitMessage>,
}

impl GitActorHandle {
    /// Open the repository at `path` and spawn its actor task.
    pub fn spawn(path: PathBuf) -> Result<Self, GitError> {
        let (sender, receiver) = mpsc::channel(32);
        let actor = GitActor::new(path, receiver)?;
        tokio::spawn(actor.run());
        Ok(Self { sender })
    }

    /// Send a message and await the reply.
    async fn request<T>(
        &self,
        f: impl FnOnce(Reply<T>) -> GitMessage,
    ) -> Result<T, GitError> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(f(tx)).await.map_err(|_| GitError::ActorDead)?;
        rx.await.map_err(|_| GitError::NoResponse)?
    }

    /// Return the short branch name of HEAD (git2 read).
    pub async fn current_branch(&self) -> Result<String, GitError> {
        self.request(|tx| GitMessage::GetCurrentBranch { respond_to: tx }).await
    }

    /// Return the working-tree status summary (git2 read).
    pub async fn status(&self) -> Result<StatusSummary, GitError> {
        self.request(|tx| GitMessage::GetStatus { respond_to: tx }).await
    }

    /// Return the HEAD commit SHA and message (git2 read).
    pub async fn head_commit(&self) -> Result<CommitInfo, GitError> {
        self.request(|tx| GitMessage::GetHeadCommit { respond_to: tx }).await
    }

    /// Run an arbitrary `git <args>` command in the repo (CLI write).
    pub async fn run_command(&self, args: Vec<String>) -> Result<CommandOutput, GitError> {
        self.request(|tx| GitMessage::RunCommand { args, respond_to: tx }).await
    }

    /// Create `task/{short_id}` from `target_branch` and push to origin (GIT-01).
    pub async fn create_branch(
        &self,
        short_id: &str,
        target_branch: &str,
    ) -> Result<(), GitError> {
        self.request(|tx| GitMessage::CreateBranch {
            short_id: short_id.into(),
            target_branch: target_branch.into(),
            respond_to: tx,
        })
        .await
    }

    /// Squash-merge `branch` into `target_branch` with `message` (GIT-03).
    ///
    /// Returns `Err(GitError::HookFailed)` if a git hook rejects the commit (GIT-07).
    pub async fn squash_merge(
        &self,
        branch: &str,
        target_branch: &str,
        message: &str,
    ) -> Result<MergeResult, GitError> {
        self.request(|tx| GitMessage::SquashMerge {
            branch: branch.into(),
            target_branch: target_branch.into(),
            message: message.into(),
            respond_to: tx,
        })
        .await
    }

    /// Force-delete `branch` locally and from origin (GIT-03 post-merge cleanup).
    pub async fn delete_branch(&self, branch: &str) -> Result<(), GitError> {
        self.request(|tx| GitMessage::DeleteBranch {
            branch: branch.into(),
            respond_to: tx,
        })
        .await
    }
}

// ─── Registry helper ──────────────────────────────────────────────────────────

/// Get-or-create a `GitActorHandle` for a project path, backed by a registry.
pub fn get_or_spawn(
    registry: &mut std::collections::HashMap<PathBuf, GitActorHandle>,
    path: &Path,
) -> Result<GitActorHandle, GitError> {
    if let Some(h) = registry.get(path) {
        return Ok(h.clone());
    }
    let handle = GitActorHandle::spawn(path.to_path_buf())?;
    registry.insert(path.to_path_buf(), handle.clone());
    Ok(handle)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Spin up a GitActorHandle on the server's own repo and verify basic reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_from_server_repo() {
        let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let handle = GitActorHandle::spawn(repo_path).expect("failed to spawn actor");

        let branch = handle.current_branch().await.expect("current_branch");
        assert!(!branch.is_empty(), "branch name should be non-empty");

        let commit = handle.head_commit().await.expect("head_commit");
        assert_eq!(commit.sha.len(), 40, "SHA should be 40 hex chars");

        let status = handle.status().await.expect("status");
        drop(status);
    }

    /// Verify that RunCommand works for a read-only git command.
    #[tokio::test]
    async fn run_command_git_log() {
        let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let handle = GitActorHandle::spawn(repo_path).expect("spawn");

        let out = handle
            .run_command(vec!["log".into(), "--oneline".into(), "-1".into()])
            .await
            .expect("git log");
        assert!(!out.stdout.is_empty(), "git log should produce output");
    }

    // ── Branch management tests ───────────────────────────────────────────────

    /// Create a local repo with an initial commit on `main` and a local bare remote.
    /// Both TempDirs must be kept alive for the test duration.
    fn setup_git_repo() -> (TempDir, TempDir) {
        let remote_dir = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();

        // Init bare remote.
        std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(remote_dir.path())
            .output()
            .unwrap();

        // Init local repo.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(local_dir.path())
            .output()
            .unwrap();

        // Set identity and disable GPG signing (required for `git commit` in CI).
        for (k, v) in [
            ("user.email", "test@test.com"),
            ("user.name", "Test User"),
            ("commit.gpgsign", "false"),
        ] {
            std::process::Command::new("git")
                .args(["config", k, v])
                .current_dir(local_dir.path())
                .output()
                .unwrap();
        }

        // Initial commit.
        std::fs::write(local_dir.path().join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(local_dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(local_dir.path())
            .output()
            .unwrap();

        // Rename default branch to `main`.
        std::process::Command::new("git")
            .args(["branch", "-m", "main"])
            .current_dir(local_dir.path())
            .output()
            .unwrap();

        // Wire up local bare remote and push.
        std::process::Command::new("git")
            .args(["remote", "add", "origin", remote_dir.path().to_str().unwrap()])
            .current_dir(local_dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["push", "-u", "origin", "main"])
            .current_dir(local_dir.path())
            .output()
            .unwrap();

        (local_dir, remote_dir)
    }

    /// `create_branch` creates `task/{short_id}` from `main` and pushes to origin (GIT-01).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_branch_creates_and_pushes() {
        let (local, _remote) = setup_git_repo();
        let handle = GitActorHandle::spawn(local.path().to_path_buf()).unwrap();

        handle.create_branch("abc1", "main").await.unwrap();

        // Branch exists locally.
        let out = handle
            .run_command(vec!["branch".into(), "--list".into(), "task/abc1".into()])
            .await
            .unwrap();
        assert!(out.stdout.contains("task/abc1"));

        // HEAD is on the new branch.
        let branch = handle.current_branch().await.unwrap();
        assert_eq!(branch, "task/abc1");
    }

    /// `squash_merge` produces a single commit on the target branch (GIT-03).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn squash_merge_produces_single_commit() {
        let (local, _remote) = setup_git_repo();
        let path = local.path();

        // Set up a feature branch with two commits (setup outside the actor).
        std::process::Command::new("git")
            .args(["checkout", "-b", "task/xyz1", "main"])
            .current_dir(path)
            .output()
            .unwrap();

        for (file, content, msg) in [
            ("feat.txt", "feature content", "feat: add file"),
            ("feat.txt", "updated content", "feat: update file"),
        ] {
            std::fs::write(path.join(file), content).unwrap();
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(path)
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["commit", "-m", msg])
                .current_dir(path)
                .output()
                .unwrap();
        }

        // Squash-merge via actor.
        let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
        let result = handle
            .squash_merge("task/xyz1", "main", "feat: squashed feature")
            .await
            .unwrap();

        assert_eq!(result.commit_sha.len(), 40, "commit SHA should be 40 chars");

        // `main` should have exactly 2 commits: init + squash.
        let log = handle
            .run_command(vec!["log".into(), "--oneline".into(), "main".into()])
            .await
            .unwrap();
        let lines: Vec<&str> = log.stdout.lines().collect();
        assert_eq!(lines.len(), 2, "main should have init + squash commit");
        assert!(lines[0].contains("squashed feature"));
    }

    /// `delete_branch` removes the local branch (GIT-03 post-merge cleanup).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_branch_removes_local() {
        let (local, _remote) = setup_git_repo();
        let path = local.path();

        // Create a branch manually and return to main.
        std::process::Command::new("git")
            .args(["checkout", "-b", "task/del1", "main"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(path)
            .output()
            .unwrap();

        let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
        handle.delete_branch("task/del1").await.unwrap();

        // Branch should no longer exist locally.
        let out = handle
            .run_command(vec!["branch".into(), "--list".into(), "task/del1".into()])
            .await
            .unwrap();
        assert!(out.stdout.trim().is_empty(), "branch should be deleted");
    }

    /// `squash_merge` returns `HookFailed` when a git hook rejects the commit (GIT-07).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn squash_merge_surfaces_hook_failure() {
        let (local, _remote) = setup_git_repo();
        let path = local.path();

        // Install a pre-commit hook that always fails.
        let hooks_dir = path.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        std::fs::write(&hook_path, "#!/bin/sh\necho 'lint failed'\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Create a feature branch with a commit (committed before hook was installed).
        std::process::Command::new("git")
            .args(["checkout", "-b", "task/hook1", "main"])
            .current_dir(path)
            .output()
            .unwrap();
        std::fs::write(path.join("x.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add x", "--no-verify"])
            .current_dir(path)
            .output()
            .unwrap();

        let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
        let err = handle
            .squash_merge("task/hook1", "main", "feat: trigger hook")
            .await
            .unwrap_err();

        assert!(
            matches!(err, GitError::HookFailed { .. }),
            "expected HookFailed, got: {err}"
        );
        if let GitError::HookFailed { output, .. } = err {
            assert!(output.contains("lint failed"), "hook output should be surfaced");
        }
    }
}

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

    /// Spin up a GitActorHandle on the server's own repo and verify basic reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_from_server_repo() {
        // Use the workspace root (two levels up from the crate root src/)
        let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let handle = GitActorHandle::spawn(repo_path).expect("failed to spawn actor");

        let branch = handle.current_branch().await.expect("current_branch");
        assert!(!branch.is_empty(), "branch name should be non-empty");

        let commit = handle.head_commit().await.expect("head_commit");
        assert_eq!(commit.sha.len(), 40, "SHA should be 40 hex chars");

        let status = handle.status().await.expect("status");
        // Just verify the call succeeded and returns a valid struct
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
}

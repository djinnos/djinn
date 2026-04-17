use std::path::Path;

use super::*;

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
    async fn request<T>(&self, f: impl FnOnce(Reply<T>) -> GitMessage) -> Result<T, GitError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(f(tx))
            .await
            .map_err(|_| GitError::ActorDead)?;
        rx.await.map_err(|_| GitError::NoResponse)?
    }

    /// Return the short branch name of HEAD (git2 read).
    pub async fn current_branch(&self) -> Result<String, GitError> {
        self.request(|tx| GitMessage::GetCurrentBranch { respond_to: tx })
            .await
    }

    /// Return the working-tree status summary (git2 read).
    pub async fn status(&self) -> Result<StatusSummary, GitError> {
        self.request(|tx| GitMessage::GetStatus { respond_to: tx })
            .await
    }

    /// Return the HEAD commit SHA and message (git2 read).
    pub async fn head_commit(&self) -> Result<CommitInfo, GitError> {
        self.request(|tx| GitMessage::GetHeadCommit { respond_to: tx })
            .await
    }

    /// Run an arbitrary `git <args>` command in the repo (CLI write).
    pub async fn run_command(&self, args: Vec<String>) -> Result<CommandOutput, GitError> {
        self.request(|tx| GitMessage::RunCommand {
            args,
            respond_to: tx,
        })
        .await
    }

    /// Check if a local branch exists (git2 read — no process spawn).
    pub async fn branch_exists(&self, name: &str) -> Result<bool, GitError> {
        self.request(|tx| GitMessage::BranchExists {
            branch: name.into(),
            respond_to: tx,
        })
        .await
    }

    /// Check if the repo has any commits (git2 read — no process spawn).
    pub async fn has_commits(&self) -> Result<bool, GitError> {
        self.request(|tx| GitMessage::HasCommits { respond_to: tx })
            .await
    }

    /// Rebase onto `upstream` with short retry/jitter for transient git-state failures.
    pub async fn rebase_with_retry(&self, upstream: &str) -> Result<(), GitError> {
        let cwd = self.current_dir().await?;
        crate::rebase_with_retry(cwd.as_path(), upstream).await
    }

    async fn current_dir(&self) -> Result<PathBuf, GitError> {
        let out = self
            .run_command(vec!["rev-parse".into(), "--show-toplevel".into()])
            .await?;
        Ok(PathBuf::from(out.stdout.trim()))
    }

    /// Create local `task/{short_id}` from `target_branch` (GIT-01).
    pub async fn create_branch(&self, short_id: &str, target_branch: &str) -> Result<(), GitError> {
        self.request(|tx| GitMessage::CreateBranch {
            short_id: short_id.into(),
            target_branch: target_branch.into(),
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

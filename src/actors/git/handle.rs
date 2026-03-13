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
        let mut last_error: Option<GitError> = None;
        for attempt in 1..=REBASE_MAX_ATTEMPTS {
            match self
                .run_command(vec!["rebase".into(), upstream.to_string()])
                .await
            {
                Ok(_) => {
                    last_error = None;
                    break;
                }
                Err(e) if attempt < REBASE_MAX_ATTEMPTS && is_retryable_git_command_error(&e) => {
                    let _ = self
                        .run_command(vec!["rebase".into(), "--abort".into()])
                        .await;
                    let delay = retry_delay(attempt);
                    tracing::warn!(
                        attempt,
                        max_attempts = REBASE_MAX_ATTEMPTS,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        upstream = %upstream,
                        "rebase failed with transient error; retrying"
                    );
                    last_error = Some(e);
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    let _ = self
                        .run_command(vec!["rebase".into(), "--abort".into()])
                        .await;
                    return Err(e);
                }
            }
        }
        if let Some(e) = last_error {
            return Err(e);
        }
        Ok(())
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

    /// Squash-merge `branch` into `target_branch` with `message` (GIT-03).
    ///
    /// Returns `Err(GitError::CommitRejected)` if `git commit` fails (GIT-07).
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

    /// Create a worktree at `.djinn/worktrees/{task_short_id}/` on `branch` (GIT-02).
    ///
    /// When `detach` is true, passes `--detach` to `git worktree add` so the
    /// worktree gets a detached HEAD instead of checking out the branch.  Use
    /// this for ephemeral worktrees (health checks, validation) that only need
    /// the code at a point-in-time and would otherwise fail when the branch is
    /// already checked out in the main working tree.
    pub async fn create_worktree(
        &self,
        task_short_id: &str,
        branch: &str,
        detach: bool,
    ) -> Result<PathBuf, GitError> {
        self.request(|tx| GitMessage::CreateWorktree {
            task_short_id: task_short_id.into(),
            branch: branch.into(),
            detach,
            respond_to: tx,
        })
        .await
    }

    /// Remove a worktree by path and prune stale entries (GIT-06).
    pub async fn remove_worktree(&self, path: &Path) -> Result<(), GitError> {
        self.request(|tx| GitMessage::RemoveWorktree {
            path: path.to_path_buf(),
            respond_to: tx,
        })
        .await
    }

    /// List all worktrees with structured metadata (GIT-02).
    pub async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        self.request(|tx| GitMessage::ListWorktrees { respond_to: tx })
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

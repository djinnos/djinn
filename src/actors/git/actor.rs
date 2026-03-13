use super::*;

// ─── Actor ───────────────────────────────────────────────────────────────────

pub(super) struct GitActor {
    pub(super) path: PathBuf,
    pub(super) repo: git2::Repository,
    pub(super) receiver: mpsc::Receiver<GitMessage>,
}

impl GitActor {
    pub(super) fn new(
        path: PathBuf,
        receiver: mpsc::Receiver<GitMessage>,
    ) -> Result<Self, GitError> {
        let repo = git2::Repository::open(&path)?;
        Ok(Self {
            path,
            repo,
            receiver,
        })
    }

    pub(super) async fn run(mut self) {
        tracing::debug!(path = %self.path.display(), "GitActor started");
        while let Some(msg) = self.receiver.recv().await {
            self.handle(msg).await;
        }
        tracing::debug!(path = %self.path.display(), "GitActor stopped");
    }

    pub(super) async fn handle(&mut self, msg: GitMessage) {
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
            GitMessage::BranchExists { branch, respond_to } => {
                let result = tokio::task::block_in_place(|| self.branch_exists(&branch));
                let _ = respond_to.send(result);
            }
            GitMessage::HasCommits { respond_to } => {
                let result = tokio::task::block_in_place(|| self.has_commits());
                let _ = respond_to.send(result);
            }
            GitMessage::CreateBranch {
                short_id,
                target_branch,
                respond_to,
            } => {
                let path = self.path.clone();
                let result = Self::create_branch_impl(path, short_id, target_branch).await;
                let _ = respond_to.send(result);
            }
            GitMessage::SquashMerge {
                branch,
                target_branch,
                message,
                respond_to,
            } => {
                let path = self.path.clone();
                let result = Self::squash_merge_impl(path, branch, target_branch, message).await;
                let _ = respond_to.send(result);
            }
            GitMessage::DeleteBranch { branch, respond_to } => {
                let path = self.path.clone();
                let result = Self::delete_branch_impl(path, branch).await;
                let _ = respond_to.send(result);
            }
            GitMessage::CreateWorktree {
                task_short_id,
                branch,
                detach,
                respond_to,
            } => {
                let path = self.path.clone();
                let result = Self::create_worktree_impl(path, task_short_id, branch, detach).await;
                let _ = respond_to.send(result);
            }
            GitMessage::RemoveWorktree {
                path: wt_path,
                respond_to,
            } => {
                let path = self.path.clone();
                let result = Self::remove_worktree_impl(path, wt_path).await;
                let _ = respond_to.send(result);
            }
            GitMessage::ListWorktrees { respond_to } => {
                let path = self.path.clone();
                let result = Self::list_worktrees_impl(path).await;
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

        Ok(StatusSummary {
            staged,
            modified,
            untracked,
        })
    }

    fn head_commit(&self) -> Result<CommitInfo, GitError> {
        let head = self.repo.head()?;
        let commit = head.peel_to_commit()?;
        Ok(CommitInfo {
            sha: commit.id().to_string(),
            message: commit.summary().unwrap_or("").to_string(),
        })
    }

    fn branch_exists(&self, name: &str) -> Result<bool, GitError> {
        match self.repo.find_branch(name, git2::BranchType::Local) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(false),
            Err(e) => Err(GitError::Git(e)),
        }
    }

    fn has_commits(&self) -> Result<bool, GitError> {
        match self.repo.head() {
            Ok(head) => Ok(head.peel_to_commit().is_ok()),
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch
                || e.code() == git2::ErrorCode::NotFound =>
            {
                Ok(false)
            }
            Err(e) => Err(GitError::Git(e)),
        }
    }

    // ── CLI writes ────────────────────────────────────────────────────────────

    /// Static so no `&self` crosses the await point (git2::Repository: !Sync).
    pub(super) async fn run_git_command(
        path: PathBuf,
        args: Vec<String>,
    ) -> Result<CommandOutput, GitError> {
        use std::process::Stdio;
        let mut cmd = std::process::Command::new("git");
        cmd.args(&args)
            .current_dir(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = crate::process::output(cmd).await.map_err(|e| {
            tracing::error!(
                error = %e,
                args = %args.join(" "),
                cwd = %path.display(),
                "git command failed"
            );
            e
        })?;

        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                code,
                command: args.join(" "),
                cwd: path.display().to_string(),
                stdout,
                stderr,
            });
        }

        Ok(CommandOutput {
            stdout,
            stderr,
            code,
        })
    }

    /// Create local `task/{short_id}` from `target_branch` (GIT-01).
    pub(super) async fn create_branch_impl(
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
        // IMPORTANT: do not checkout in repo root; create branch ref only.
        let remote_ref = format!("origin/{target_branch}");
        let create = Self::run_git_command(
            path.clone(),
            vec!["branch".into(), branch_name.clone(), remote_ref],
        )
        .await;

        if create.is_err() {
            Self::run_git_command(
                path.clone(),
                vec!["branch".into(), branch_name.clone(), target_branch],
            )
            .await?;
        }

        Ok(())
    }

    /// Force-delete `branch` locally; also removes from origin (best effort).
    ///
    /// Uses `-D` because squash merges don't produce a merge commit, so git
    /// considers task branches "unmerged" even after a successful squash.
    pub(super) async fn delete_branch_impl(path: PathBuf, branch: String) -> Result<(), GitError> {
        // Force-delete local branch.
        Self::run_git_command(
            path.clone(),
            vec!["branch".into(), "-D".into(), branch.clone()],
        )
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

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
                let path = self.path.clone();
                let result = crate::run_git_command(path, args).await;
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
                let result = crate::create_branch(path, short_id, target_branch).await;
                let _ = respond_to.send(result);
            }
            GitMessage::DeleteBranch { branch, respond_to } => {
                let path = self.path.clone();
                let result = crate::delete_branch(path, branch).await;
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
            Err(e)
                if e.code() == git2::ErrorCode::UnbornBranch
                    || e.code() == git2::ErrorCode::NotFound =>
            {
                Ok(false)
            }
            Err(e) => Err(GitError::Git(e)),
        }
    }
}

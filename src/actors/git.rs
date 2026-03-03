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

/// Metadata for a managed worktree under `.djinn/worktrees/{task_short_id}`.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub task_short_id: String,
    pub head_sha: String,
    /// Short branch name (e.g. `task/abc1`), empty if detached.
    pub branch: String,
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
    /// Create an isolated worktree at `.djinn/worktrees/{task_short_id}` on
    /// the given branch. Prunes stale metadata first (GIT-02, GIT-06).
    CreateWorktree {
        task_short_id: String,
        branch: String,
        respond_to: Reply<PathBuf>,
    },
    /// Remove the worktree for `task_short_id`. Uses double `--force` to
    /// handle locked/dirty worktrees, then prunes metadata (GIT-06).
    RemoveWorktree {
        task_short_id: String,
        respond_to: Reply<()>,
    },
    /// List all managed worktrees under `.djinn/worktrees/` (GIT-06).
    ListWorktrees { respond_to: Reply<Vec<WorktreeInfo>> },
    /// Remove worktrees whose `task_short_id` is not in `active_session_ids`.
    /// Returns the short IDs of pruned worktrees (GIT-06).
    PruneOrphans {
        active_session_ids: Vec<String>,
        respond_to: Reply<Vec<String>>,
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
            GitMessage::CreateWorktree { task_short_id, branch, respond_to } => {
                let path = self.path.clone();
                let result = Self::create_worktree(path, task_short_id, branch).await;
                let _ = respond_to.send(result);
            }
            GitMessage::RemoveWorktree { task_short_id, respond_to } => {
                let path = self.path.clone();
                let result = Self::remove_worktree(path, task_short_id).await;
                let _ = respond_to.send(result);
            }
            GitMessage::ListWorktrees { respond_to } => {
                let path = self.path.clone();
                let result = Self::list_worktrees(path).await;
                let _ = respond_to.send(result);
            }
            GitMessage::PruneOrphans { active_session_ids, respond_to } => {
                let path = self.path.clone();
                let result = Self::prune_orphans(path, active_session_ids).await;
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

    // ── Worktree lifecycle ────────────────────────────────────────────────────

    /// Create `.djinn/worktrees/{task_short_id}` checked out to `branch`.
    ///
    /// Runs `git worktree prune` first to clear any stale metadata entries
    /// from previous unclean shutdowns. The branch must already exist.
    async fn create_worktree(
        path: PathBuf,
        task_short_id: String,
        branch: String,
    ) -> Result<PathBuf, GitError> {
        // Prune stale metadata so git doesn't complain about ghosts.
        let _ =
            Self::run_git_command(path.clone(), vec!["worktree".into(), "prune".into()]).await;

        let worktrees_dir = path.join(".djinn").join("worktrees");
        tokio::fs::create_dir_all(&worktrees_dir).await?;

        let worktree_path = worktrees_dir.join(&task_short_id);

        Self::run_git_command(
            path,
            vec![
                "worktree".into(),
                "add".into(),
                worktree_path.to_string_lossy().into_owned(),
                branch,
            ],
        )
        .await?;

        Ok(worktree_path)
    }

    /// Remove the worktree for `task_short_id` and prune metadata.
    ///
    /// Uses double `--force` to handle locked or dirty worktrees, matching
    /// the Go server's behaviour for crash-recovery cleanup.
    async fn remove_worktree(path: PathBuf, task_short_id: String) -> Result<(), GitError> {
        let worktree_path = path.join(".djinn").join("worktrees").join(&task_short_id);

        Self::run_git_command(
            path.clone(),
            vec![
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                "--force".into(),
                worktree_path.to_string_lossy().into_owned(),
            ],
        )
        .await?;

        // Clean up git's internal metadata after removal.
        let _ =
            Self::run_git_command(path, vec!["worktree".into(), "prune".into()]).await;

        Ok(())
    }

    /// List managed worktrees by parsing `git worktree list --porcelain` and
    /// filtering to entries under `.djinn/worktrees/`.
    async fn list_worktrees(path: PathBuf) -> Result<Vec<WorktreeInfo>, GitError> {
        let output = Self::run_git_command(
            path.clone(),
            vec!["worktree".into(), "list".into(), "--porcelain".into()],
        )
        .await?;

        let worktrees_dir = path.join(".djinn").join("worktrees");
        Ok(parse_worktree_list_output(&output.stdout, &worktrees_dir))
    }

    /// Remove all worktrees whose short ID is not in `active_session_ids`.
    ///
    /// Returns the short IDs of pruned worktrees. Errors on individual
    /// removals are logged and skipped so one bad worktree doesn't block
    /// the rest.
    async fn prune_orphans(
        path: PathBuf,
        active_session_ids: Vec<String>,
    ) -> Result<Vec<String>, GitError> {
        let worktrees = Self::list_worktrees(path.clone()).await?;
        let mut pruned = Vec::new();

        for wt in worktrees {
            if active_session_ids.contains(&wt.task_short_id) {
                continue;
            }
            match Self::remove_worktree(path.clone(), wt.task_short_id.clone()).await {
                Ok(()) => {
                    tracing::info!(short_id = %wt.task_short_id, "pruned orphan worktree");
                    pruned.push(wt.task_short_id);
                }
                Err(e) => {
                    tracing::warn!(
                        short_id = %wt.task_short_id,
                        error = %e,
                        "failed to prune orphan worktree"
                    );
                }
            }
        }

        Ok(pruned)
    }
}

// ─── Porcelain parser ─────────────────────────────────────────────────────────

/// Parse `git worktree list --porcelain` output and return only worktrees
/// rooted under `worktrees_dir`.
fn parse_worktree_list_output(output: &str, worktrees_dir: &Path) -> Vec<WorktreeInfo> {
    struct Wt {
        path: PathBuf,
        head_sha: String,
        branch: String,
    }

    let mut all: Vec<Wt> = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head = String::new();
    let mut cur_branch = String::new();

    for line in output.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(path) = cur_path.take() {
                all.push(Wt {
                    path,
                    head_sha: std::mem::take(&mut cur_head),
                    branch: std::mem::take(&mut cur_branch),
                });
            }
            cur_path = Some(PathBuf::from(p));
        } else if let Some(sha) = line.strip_prefix("HEAD ") {
            cur_head = sha.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        }
    }
    if let Some(path) = cur_path {
        all.push(Wt { path, head_sha: cur_head, branch: cur_branch });
    }

    all.into_iter()
        .filter_map(|wt| {
            if !wt.path.starts_with(worktrees_dir) {
                return None;
            }
            let short_id = wt.path.file_name()?.to_str()?.to_string();
            Some(WorktreeInfo {
                task_short_id: short_id,
                path: wt.path,
                head_sha: wt.head_sha,
                branch: wt.branch,
            })
        })
        .collect()
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

    /// Create `.djinn/worktrees/{task_short_id}` checked out to `branch`.
    ///
    /// The branch must already exist in the repo. Prunes stale worktree
    /// metadata before creating (GIT-02, GIT-06).
    pub async fn create_worktree(
        &self,
        task_short_id: String,
        branch: String,
    ) -> Result<PathBuf, GitError> {
        self.request(|tx| GitMessage::CreateWorktree { task_short_id, branch, respond_to: tx })
            .await
    }

    /// Remove the worktree for `task_short_id` (GIT-06).
    pub async fn remove_worktree(&self, task_short_id: String) -> Result<(), GitError> {
        self.request(|tx| GitMessage::RemoveWorktree { task_short_id, respond_to: tx }).await
    }

    /// List all managed worktrees under `.djinn/worktrees/` (GIT-06).
    pub async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        self.request(|tx| GitMessage::ListWorktrees { respond_to: tx }).await
    }

    /// Remove worktrees whose short ID is absent from `active_session_ids`.
    /// Returns pruned short IDs (GIT-06).
    pub async fn prune_orphans(
        &self,
        active_session_ids: Vec<String>,
    ) -> Result<Vec<String>, GitError> {
        self.request(|tx| GitMessage::PruneOrphans { active_session_ids, respond_to: tx }).await
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

    // ── Worktree tests (use a throw-away temp repo) ───────────────────────────

    fn git(args: &[&str], dir: &Path) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|_| panic!("git {args:?} failed to run"));
        assert!(status.success(), "git {args:?} exited non-zero");
    }

    /// Bootstrap a minimal git repo in a temp directory and return a handle.
    fn setup_temp_repo() -> (tempfile::TempDir, GitActorHandle) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();

        git(&["init"], path);
        git(&["config", "user.email", "test@djinn.test"], path);
        git(&["config", "user.name", "Djinn Test"], path);
        git(&["commit", "--allow-empty", "-m", "init"], path);

        let handle = GitActorHandle::spawn(path.to_path_buf()).expect("spawn GitActor");
        (dir, handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_create_and_remove() {
        let (dir, handle) = setup_temp_repo();
        let path = dir.path();

        // Create the branch the worktree will check out.
        git(&["branch", "task/abc1"], path);

        // Create the worktree.
        let wt_path = handle
            .create_worktree("abc1".into(), "task/abc1".into())
            .await
            .expect("create_worktree");

        assert_eq!(wt_path, path.join(".djinn").join("worktrees").join("abc1"));
        assert!(wt_path.is_dir(), "worktree directory should exist");

        // It shows up in the list with correct metadata.
        let worktrees = handle.list_worktrees().await.expect("list_worktrees");
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].task_short_id, "abc1");
        assert_eq!(worktrees[0].branch, "task/abc1");
        assert_eq!(worktrees[0].path, wt_path);

        // Remove the worktree.
        handle.remove_worktree("abc1".into()).await.expect("remove_worktree");
        assert!(!wt_path.exists(), "worktree directory should be gone");

        // List is now empty.
        let worktrees = handle.list_worktrees().await.expect("list after remove");
        assert!(worktrees.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_prune_orphans() {
        let (dir, handle) = setup_temp_repo();
        let path = dir.path();

        git(&["branch", "task/abc1"], path);
        git(&["branch", "task/abc2"], path);

        handle
            .create_worktree("abc1".into(), "task/abc1".into())
            .await
            .expect("create abc1");
        handle
            .create_worktree("abc2".into(), "task/abc2".into())
            .await
            .expect("create abc2");

        // Declare only abc1 as active — abc2 should be pruned.
        let pruned =
            handle.prune_orphans(vec!["abc1".into()]).await.expect("prune_orphans");
        assert_eq!(pruned, vec!["abc2".to_string()]);

        let remaining = handle.list_worktrees().await.expect("list after prune");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].task_short_id, "abc1");

        // abc2 directory is gone.
        let abc2_path = path.join(".djinn").join("worktrees").join("abc2");
        assert!(!abc2_path.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_worktrees_empty_when_none_exist() {
        let (_dir, handle) = setup_temp_repo();
        let worktrees = handle.list_worktrees().await.expect("list_worktrees");
        assert!(worktrees.is_empty(), "fresh repo has no managed worktrees");
    }

    #[test]
    fn parse_worktree_list_output_filters_correctly() {
        let output = "\
worktree /repo
HEAD abc123def456abc123def456abc123def456abc1
branch refs/heads/main

worktree /repo/.djinn/worktrees/abc1
HEAD def456abc123def456abc123def456abc123def4
branch refs/heads/task/abc1

worktree /repo/.djinn/worktrees/xyz9
HEAD 111111abc123def456abc123def456abc123def4
detached

";
        let worktrees_dir = PathBuf::from("/repo/.djinn/worktrees");
        let result = parse_worktree_list_output(output, &worktrees_dir);

        assert_eq!(result.len(), 2);

        let abc1 = result.iter().find(|w| w.task_short_id == "abc1").unwrap();
        assert_eq!(abc1.branch, "task/abc1");
        assert_eq!(abc1.head_sha, "def456abc123def456abc123def456abc123def4");

        // Detached worktrees have an empty branch string.
        let xyz9 = result.iter().find(|w| w.task_short_id == "xyz9").unwrap();
        assert_eq!(xyz9.branch, "");

        // Main worktree is excluded.
        assert!(result.iter().all(|w| w.task_short_id != "main"));
    }
}

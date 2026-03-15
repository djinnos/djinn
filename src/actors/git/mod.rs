//! GitActor — serializes all git operations for a single repository.
//!
//! One actor per project path, stored in `AppState`. Uses the Ryhl hand-rolled
//! actor pattern: `GitActorHandle` (mpsc sender) is the public API; `GitActor`
//! (mpsc receiver) runs in a dedicated tokio task.
//!
//! Hybrid approach (GIT-05):
//!   - Reads → git2 crate (status, diff, ref queries)
//!   - Writes → `std::process::Command` via `crate::process` (worktree, merge, push)

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use djinn_git as gitlib;

// ─── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git2: {0}")]
    Git(#[from] git2::Error),

    #[error(
        "git command failed (exit {code}) in {cwd}: git {command}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    CommandFailed {
        code: i32,
        command: String,
        cwd: String,
        stdout: String,
        stderr: String,
    },

    /// `git commit` failed after squash staging.
    /// Contains the exact command and outputs for deterministic routing.
    #[error(
        "git commit rejected (exit {code}) in {cwd}: git {command}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    CommitRejected {
        code: i32,
        command: String,
        cwd: String,
        stdout: String,
        stderr: String,
    },

    /// Squash merge encountered file-level conflicts.
    #[error("merge conflict while squashing into {target_branch}: {files:?}")]
    MergeConflict {
        target_branch: String,
        files: Vec<String>,
    },

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

/// A single worktree entry parsed from `git worktree list --porcelain` (GIT-02).
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
}

impl From<gitlib::CommandOutput> for CommandOutput {
    fn from(value: gitlib::CommandOutput) -> Self {
        Self {
            stdout: value.stdout,
            stderr: value.stderr,
            code: value.code,
        }
    }
}

impl From<gitlib::MergeResult> for MergeResult {
    fn from(value: gitlib::MergeResult) -> Self {
        Self {
            commit_sha: value.commit_sha,
        }
    }
}

impl From<gitlib::WorktreeInfo> for WorktreeInfo {
    fn from(value: gitlib::WorktreeInfo) -> Self {
        Self {
            path: value.path,
            branch: value.branch,
            head: value.head,
        }
    }
}

impl From<gitlib::GitError> for GitError {
    fn from(value: gitlib::GitError) -> Self {
        match value {
            gitlib::GitError::Git(e) => Self::Git(e),
            gitlib::GitError::CommandFailed {
                code,
                command,
                cwd,
                stdout,
                stderr,
            } => Self::CommandFailed {
                code,
                command,
                cwd,
                stdout,
                stderr,
            },
            gitlib::GitError::CommitRejected {
                code,
                command,
                cwd,
                stdout,
                stderr,
            } => Self::CommitRejected {
                code,
                command,
                cwd,
                stdout,
                stderr,
            },
            gitlib::GitError::MergeConflict {
                target_branch,
                files,
            } => Self::MergeConflict {
                target_branch,
                files,
            },
            gitlib::GitError::Io(e) => Self::Io(e),
            gitlib::GitError::Other(e) => Self::Io(std::io::Error::other(e.to_string())),
        }
    }
}

// ─── Messages ─────────────────────────────────────────────────────────────────

pub(super) type Reply<T> = oneshot::Sender<Result<T, GitError>>;

pub(super) enum GitMessage {
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
    /// Check if a local branch exists (git2 read — no process spawn).
    BranchExists {
        branch: String,
        respond_to: Reply<bool>,
    },
    /// Check if the repo has any commits (git2 read — no process spawn).
    HasCommits { respond_to: Reply<bool> },
    /// Create local `task/{short_id}` from `target_branch` (GIT-01).
    CreateBranch {
        short_id: String,
        target_branch: String,
        respond_to: Reply<()>,
    },
    /// Squash-merge `branch` into `target_branch` with `message` (GIT-03).
    /// Returns `Err(CommitRejected)` when `git commit` fails.
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
    /// Create a worktree at `.djinn/worktrees/{task_short_id}/` on `branch` (GIT-02).
    CreateWorktree {
        task_short_id: String,
        branch: String,
        detach: bool,
        respond_to: Reply<PathBuf>,
    },
    /// Remove a worktree by path and prune stale entries (GIT-06).
    RemoveWorktree {
        path: PathBuf,
        respond_to: Reply<()>,
    },
    /// List all worktrees with structured metadata (GIT-02).
    ListWorktrees {
        respond_to: Reply<Vec<WorktreeInfo>>,
    },
}

// ─── Submodules ──────────────────────────────────────────────────────────────

mod actor;
mod handle;

use actor::GitActor;
pub use handle::{GitActorHandle, get_or_spawn};

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

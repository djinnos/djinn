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

pub(super) const PUSH_MAX_ATTEMPTS: u32 = 3;
pub(super) const REBASE_MAX_ATTEMPTS: u32 = 3;

pub(super) fn is_retryable_git_command_error(err: &GitError) -> bool {
    let GitError::CommandFailed { stderr, .. } = err else {
        return false;
    };
    let s = stderr.to_lowercase();
    [
        "cannot lock ref",
        "failed to lock",
        "another git process",
        "resource temporarily unavailable",
        "connection reset",
        "connection timed out",
        "timed out",
        "remote end hung up unexpectedly",
    ]
    .iter()
    .any(|needle| s.contains(needle))
}

pub(super) fn is_non_fast_forward_error(err: &GitError) -> bool {
    let GitError::CommandFailed { stderr, .. } = err else {
        return false;
    };
    let s = stderr.to_lowercase();
    s.contains("non-fast-forward") || s.contains("fetch first") || s.contains("rejected")
}

pub(super) fn retry_delay(attempt: u32) -> std::time::Duration {
    let exp = attempt.saturating_sub(1).min(4);
    let base_ms = 200u64.saturating_mul(1u64 << exp);
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() as u64) % 151)
        .unwrap_or(0);
    std::time::Duration::from_millis(base_ms + jitter_ms)
}

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
mod merge_ops;
mod worktree;

use actor::GitActor;
pub use handle::{GitActorHandle, get_or_spawn};

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

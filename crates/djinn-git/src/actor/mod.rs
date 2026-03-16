//! GitActor — serializes all git operations for a single repository.
//!
//! One actor per project path. Uses the Ryhl hand-rolled actor pattern:
//! `GitActorHandle` (mpsc sender) is the public API; `GitActor` (mpsc receiver)
//! runs in a dedicated tokio task.
//!
//! Hybrid approach (GIT-05):
//!   - Reads → git2 crate (status, diff, ref queries)
//!   - Writes → `std::process::Command` via djinn-git functions (worktree, merge, push)

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

pub use crate::{
    CommandOutput, CommitInfo, GitError, MergeResult, StatusSummary, WorktreeInfo,
};

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

mod git_actor;
mod handle;

use git_actor::GitActor;
pub use handle::{GitActorHandle, get_or_spawn};

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

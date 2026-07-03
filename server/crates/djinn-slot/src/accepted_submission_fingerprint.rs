use std::path::{Path, PathBuf};

use djinn_git::compute_submission_diff_fingerprint;

use crate::host::SlotContext;

/// Result of resolving the task worktree and computing the shared complete
/// submission diff fingerprint for accepted/auto-submit metadata persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptedSubmissionFingerprint {
    /// A stable digest was computed from the current worktree.
    Diff(String),
    /// The worktree has no complete submission diff relative to the base.
    NoDiff,
    /// The worktree path could not be resolved (e.g. no `task_runs` record with
    /// a workspace path and no configured working root).
    Unavailable,
}

impl AcceptedSubmissionFingerprint {
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Diff(fp) => Some(fp),
            _ => None,
        }
    }
}

/// Resolve the task worktree path from the task run or slot context, then
/// compute the shared complete submission diff fingerprint.
///
/// This is used by accepted/auto-submit paths to replace any ad-hoc or
/// payload-provided fingerprint with the canonical digest produced by
/// `djinn_git::compute_submission_diff_fingerprint`.
pub async fn compute_accepted_submission_fingerprint(
    task_id: &str,
    task_run_id: &str,
    ctx: &SlotContext,
) -> AcceptedSubmissionFingerprint {
    let worktree = match resolve_task_worktree_path(task_id, task_run_id, ctx).await {
        Some(p) => p,
        None => return AcceptedSubmissionFingerprint::Unavailable,
    };

    if !worktree.exists() {
        return AcceptedSubmissionFingerprint::Unavailable;
    }

    match compute_submission_diff_fingerprint(&worktree).await {
        Ok(djinn_git::SubmissionDiffFingerprint::Diff(digest)) => {
            AcceptedSubmissionFingerprint::Diff(digest.fingerprint)
        }
        Ok(djinn_git::SubmissionDiffFingerprint::NoDiff(_)) => AcceptedSubmissionFingerprint::NoDiff,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                worktree = %worktree.display(),
                error = %e,
                "accepted_submission_fingerprint: failed to compute shared diff fingerprint"
            );
            AcceptedSubmissionFingerprint::Unavailable
        }
    }
}

async fn resolve_task_worktree_path(
    task_id: &str,
    task_run_id: &str,
    ctx: &SlotContext,
) -> Option<PathBuf> {
    // Prefer the explicitly-configured slot working root, which points at the
    // active task worktree in production.
    if let Some(root) = ctx.working_root.as_ref() {
        let root = root.to_path_buf();
        if root.exists() {
            return Some(root);
        }
    }

    // Fall back to the latest recorded workspace_path for this task run or,
    // failing that, the latest workspace path for the task.
    let task_run_repo = djinn_db::repositories::task_run::TaskRunRepository::new(ctx.db.clone());
    let run_path = task_run_repo
        .get(task_run_id)
        .await
        .ok()
        .flatten()
        .and_then(|run| run.workspace_path)
        .map(PathBuf::from);

    if let Some(path) = run_path {
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(Some(path)) = task_run_repo
        .latest_workspace_path_for_task(task_id)
        .await
        .map(|p| p.map(PathBuf::from))
    {
        if path.exists() {
            return Some(path);
        }
    }

    None
}

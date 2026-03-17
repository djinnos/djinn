use std::path::PathBuf;

use djinn_git::GitError;
use crate::db::{ProjectRepository, SessionRepository, TaskRepository};
use crate::models::{SessionStatus, TransitionAction};
use crate::agent::context::AgentContext;

const MERGE_CONFLICT_PREFIX: &str = "merge_conflict:";
const MERGE_VALIDATION_PREFIX: &str = "merge_validation_failed:";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MergeConflictMetadata {
    conflicting_files: Vec<String>,
    base_branch: String,
    merge_target: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MergeValidationFailureMetadata {
    base_branch: String,
    merge_target: String,
    command: String,
    cwd: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Transition actions to use for each merge outcome.
/// Allows both the reviewer and PM approval paths to reuse the same merge logic.
pub struct MergeActions {
    pub approve: TransitionAction,
    pub conflict: TransitionAction,
    pub release: TransitionAction,
}

/// Standard actions used by the task reviewer path.
pub const REVIEWER_MERGE_ACTIONS: MergeActions = MergeActions {
    approve: TransitionAction::TaskReviewApprove,
    conflict: TransitionAction::TaskReviewRejectConflict,
    release: TransitionAction::ReleaseTaskReview,
};

/// Actions used when PM approves directly from intervention.
///
/// `release` uses `PmInterventionComplete` (→ Open) instead of
/// `PmInterventionRelease` (→ NeedsPmIntervention) so that verification
/// or git failures route the task back to a worker who can fix the code,
/// rather than looping the PM in a re-dispatch cycle it cannot resolve.
pub const PM_MERGE_ACTIONS: MergeActions = MergeActions {
    approve: TransitionAction::PmApprove,
    conflict: TransitionAction::PmApproveConflict,
    release: TransitionAction::PmInterventionComplete,
};

pub async fn merge_after_task_review(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<(TransitionAction, Option<String>)> {
    merge_and_transition(task_id, app_state, &REVIEWER_MERGE_ACTIONS).await
}

pub async fn merge_and_transition(
    task_id: &str,
    app_state: &AgentContext,
    actions: &MergeActions,
) -> Option<(TransitionAction, Option<String>)> {
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match repo.get(task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return Some((
                actions.release.clone(),
                Some("task missing during post-review merge".to_string()),
            ));
        }
        Err(e) => {
            return Some((
                actions.release.clone(),
                Some(format!("failed to load task for merge: {e}")),
            ));
        }
    };

    let project_dir = project_path_for_id(&task.project_id, app_state).await;
    let git = match app_state.git_actor(&project_dir).await {
        Ok(git) => git,
        Err(e) => {
            return Some((
                actions.release.clone(),
                Some(format!("failed to open git actor for merge: {e}")),
            ));
        }
    };

    let project_path_str = project_dir.to_string_lossy().to_string();
    if let Err(feedback) = run_verification_gate(task_id, &project_path_str, app_state).await {
        tracing::warn!(
            task_id = %task_id,
            "pre-merge verification failed; releasing task"
        );
        let payload = serde_json::json!({ "body": feedback }).to_string();
        let _ = repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "comment",
                &payload,
            )
            .await;
        return Some((
            actions.release.clone(),
            Some(format!("pre-merge verification failed: {feedback}")),
        ));
    }

    let base_branch = format!("task/{}", task.short_id);
    let merge_target = default_target_branch(&task.project_id, app_state).await;
    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

    match git
        .squash_merge(&base_branch, &merge_target, &message)
        .await
    {
        Ok(result) => {
            tracing::info!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                base_branch = %base_branch,
                merge_target = %merge_target,
                commit_sha = %result.commit_sha,
                "Lifecycle: post-review squash merge succeeded"
            );
            if let Err(e) = git.delete_branch(&base_branch).await {
                tracing::warn!(
                    task_id = %task.short_id,
                    branch = %base_branch,
                    error = %e,
                    "failed to delete task branch after successful merge"
                );
            }
            if let Err(e) = repo.set_merge_commit_sha(task_id, &result.commit_sha).await {
                return Some((
                    actions.release.clone(),
                    Some(format!("merged but failed to store merge SHA: {e}")),
                ));
            }
            cleanup_paused_worker_session(task_id, app_state).await;
            Some((actions.approve.clone(), None))
        }
        Err(GitError::MergeConflict { files, .. }) => {
            tracing::warn!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                conflict_count = files.len(),
                conflicting_files = ?files,
                "Lifecycle: post-review merge conflict"
            );
            let metadata = MergeConflictMetadata {
                conflicting_files: files,
                base_branch,
                merge_target,
            };
            let reason = match serde_json::to_string(&metadata) {
                Ok(v) => format!("{MERGE_CONFLICT_PREFIX}{v}"),
                Err(_) => format!("{MERGE_CONFLICT_PREFIX}{{}}"),
            };
            let payload = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "merge_conflict",
                    &payload,
                )
                .await;
            Some((actions.conflict.clone(), Some(reason)))
        }
        Err(GitError::CommitRejected {
            code,
            command,
            cwd,
            stdout,
            stderr,
        }) => {
            tracing::warn!(
                task_id = %task.short_id,
                exit_code = code,
                command = %command,
                "Lifecycle: post-review merge commit rejected"
            );
            let metadata = MergeValidationFailureMetadata {
                base_branch,
                merge_target,
                command,
                cwd,
                exit_code: code,
                stdout,
                stderr,
            };
            let reason_payload =
                serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());
            let reason = format!("{MERGE_VALIDATION_PREFIX}{reason_payload}");
            let _ = repo
                .log_activity(
                    Some(task_id),
                    "agent-supervisor",
                    "system",
                    "merge_validation_failed",
                    &reason_payload,
                )
                .await;
            Some((actions.conflict.clone(), Some(reason)))
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: post-review squash merge failed"
            );
            Some((
                actions.release.clone(),
                Some(format!("post-review squash merge failed: {e} ({e:?})")),
            ))
        }
    }
}
async fn run_verification_gate(
    task_id: &str,
    project_path: &str,
    app_state: &AgentContext,
) -> Result<(), String> {
    crate::actors::slot::verification::run_verification_gate(task_id, project_path, app_state).await
}

pub(crate) async fn cleanup_paused_worker_session(task_id: &str, app_state: &AgentContext) {
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    if let Err(e) = repo
        .update(
            &paused.id,
            SessionStatus::Completed,
            paused.tokens_in,
            paused.tokens_out,
        )
        .await
    {
        tracing::warn!(
            record_id = %paused.id,
            error = %e,
            "failed to finalize paused session record on task approval"
        );
    }

    if let Some(worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
        let _ = tokio::fs::remove_dir_all(&worktree_path).await;
    }
}

pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &AgentContext) {
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    if let Err(e) = repo
        .update(
            &paused.id,
            SessionStatus::Interrupted,
            paused.tokens_in,
            paused.tokens_out,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            record_id = %paused.id,
            error = %e,
            "failed to interrupt paused worker session after reviewer rejection"
        );
    }
}
pub(crate) async fn resolve_project_path_for_id(
    project_id: &str,
    app_state: &AgentContext,
) -> Option<String> {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    repo.get_path(project_id).await.ok().flatten()
}

async fn default_target_branch(project_id: &str, app_state: &AgentContext) -> String {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Ok(Some(config)) = repo.get_config(project_id).await {
        return config.target_branch;
    }
    "main".to_string()
}

async fn project_path_for_id(project_id: &str, app_state: &AgentContext) -> PathBuf {
    let project_path = resolve_project_path_for_id(project_id, app_state)
        .await
        .unwrap_or_else(|| ".".to_string());
    PathBuf::from(project_path)
}

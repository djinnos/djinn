use std::path::PathBuf;

use crate::actors::git::GitError;
use crate::agent::AgentType;
use crate::agent::output_parser::ParsedAgentOutput;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::SessionStatus;
use crate::models::task::TransitionAction;
use crate::server::AppState;

use super::*;

// ─── Epic review helpers ──────────────────────────────────────────────────────

pub(crate) async fn merge_after_task_review(
    task_id: &str,
    app_state: &AppState,
) -> Option<(TransitionAction, Option<String>)> {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let task = match repo.get(task_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return Some((
                TransitionAction::ReleaseTaskReview,
                Some("task missing during post-review merge".to_string()),
            ));
        }
        Err(e) => {
            return Some((
                TransitionAction::ReleaseTaskReview,
                Some(format!("failed to load task for merge: {e}")),
            ));
        }
    };

    let project_dir = project_path_for_id(&task.project_id, app_state)
        .await
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let git = match app_state.git_actor(&project_dir).await {
        Ok(git) => git,
        Err(e) => {
            return Some((
                TransitionAction::ReleaseTaskReview,
                Some(format!("failed to open git actor for merge: {e}")),
            ));
        }
    };

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
                    TransitionAction::ReleaseTaskReview,
                    Some(format!("merged but failed to store merge SHA: {e}")),
                ));
            }
            cleanup_paused_worker_session(task_id, app_state).await;
            Some((TransitionAction::TaskReviewApprove, None))
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
            Some((TransitionAction::TaskReviewRejectConflict, Some(reason)))
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
            Some((TransitionAction::TaskReviewRejectConflict, Some(reason)))
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: post-review squash merge failed"
            );
            Some((
                TransitionAction::ReleaseTaskReview,
                Some(format!("post-review squash merge failed: {e} ({e:?})")),
            ))
        }
    }
}

pub(crate) async fn cleanup_paused_worker_session(task_id: &str, app_state: &AppState) {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    let (tokens_in, tokens_out) = if let Some(ref gsid) = paused.goose_session_id {
        let from_sqlite = tokens_from_goose_sqlite(gsid).await;
        from_sqlite.unwrap_or((paused.tokens_in, paused.tokens_out))
    } else {
        (paused.tokens_in, paused.tokens_out)
    };

    if let Err(e) = repo
        .update(&paused.id, SessionStatus::Completed, tokens_in, tokens_out)
        .await
    {
        tracing::warn!(
            record_id = %paused.id,
            error = %e,
            "failed to finalize paused session record on task approval"
        );
    }

    if let Some(worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
        cleanup_worktree(task_id, &worktree_path, app_state).await;
    }
}

pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &AppState) {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
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
    } else {
        tracing::info!(
            task_id = %task_id,
            record_id = %paused.id,
            goose_session_id = paused.goose_session_id.as_deref().unwrap_or("<none>"),
            "Lifecycle: interrupted paused worker session after reviewer rejection"
        );
    }
}

// ─── Success transition ───────────────────────────────────────────────────────

/// Determine the post-session transition for a successfully completed session.
///
/// - **Workers/ConflictResolvers**: always proceed to task review (the agent
///   stopping tool calls is the completion signal).
/// - **TaskReviewers**: check acceptance criteria on the task — all met means
///   approve (merge), any unmet means reject back to worker.
pub(crate) async fn success_transition(
    task_id: &str,
    agent_type: AgentType,
    output: &ParsedAgentOutput,
    app_state: &AppState,
) -> Option<(TransitionAction, Option<String>)> {
    match agent_type {
        AgentType::Worker | AgentType::ConflictResolver => {
            // Worker completed — proceed to task review.
            Some((TransitionAction::SubmitTaskReview, None))
        }
        AgentType::TaskReviewer => {
            // Derive verdict from acceptance criteria state on the task.
            let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
            match repo.get(task_id).await {
                Ok(Some(task)) => {
                    if all_acceptance_criteria_met(&task.acceptance_criteria) {
                        tracing::info!(task_id = %task_id, "task reviewer: all AC met → approve");
                        merge_after_task_review(task_id, app_state).await
                    } else {
                        let feedback = output
                            .reviewer_feedback
                            .clone()
                            .unwrap_or_else(|| "reviewer found unmet acceptance criteria".to_string());
                        tracing::info!(task_id = %task_id, feedback = %feedback, "task reviewer: unmet AC → reject");
                        Some((TransitionAction::TaskReviewReject, Some(feedback)))
                    }
                }
                Ok(None) => {
                    tracing::warn!(task_id = %task_id, "task missing during reviewer verdict");
                    Some((
                        TransitionAction::ReleaseTaskReview,
                        Some("task not found during reviewer verdict".to_string()),
                    ))
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to load task for reviewer verdict");
                    Some((
                        TransitionAction::ReleaseTaskReview,
                        Some(format!("failed to load task for verdict: {e}")),
                    ))
                }
            }
        }
    }
}

/// Check if all acceptance criteria are met.
fn all_acceptance_criteria_met(ac_json: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Criterion {
        #[serde(default)]
        met: bool,
    }

    match serde_json::from_str::<Vec<Criterion>>(ac_json) {
        Ok(criteria) => !criteria.is_empty() && criteria.iter().all(|c| c.met),
        Err(_) => false,
    }
}

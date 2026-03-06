use std::path::PathBuf;

use crate::actors::git::GitError;
use crate::agent::AgentType;
use crate::agent::output_parser::{EpicReviewVerdict, ParsedAgentOutput, ReviewerVerdict};
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::epic_review_batch::EpicReviewBatchRepository;
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

pub(crate) async fn finalize_epic_batch(
    task_id: &str,
    output: &ParsedAgentOutput,
    error_reason: Option<&str>,
    app_state: &AppState,
) {
    let Some(batch_id) = active_epic_batch_for_task(task_id, app_state).await else {
        return;
    };
    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let Some(task) = task_repo.get(task_id).await.ok().flatten() else {
        return;
    };
    let Some(epic_id) = task.epic_id.as_deref() else {
        return;
    };

    let batch_repo =
        EpicReviewBatchRepository::new(app_state.db().clone(), app_state.events().clone());
    let epic_repo = EpicRepository::new(app_state.db().clone(), app_state.events().clone());

    match output.epic_verdict {
        Some(EpicReviewVerdict::Clean) => {
            if let Err(e) = batch_repo.mark_clean(&batch_id).await {
                tracing::warn!(batch_id = %batch_id, error = %e, "failed to mark epic review batch clean");
                return;
            }
            let tasks = match task_repo.list_by_epic(epic_id).await {
                Ok(tasks) => tasks,
                Err(e) => {
                    tracing::warn!(epic_id = %epic_id, error = %e, "failed to list epic tasks after clean review");
                    return;
                }
            };
            if tasks.iter().all(|t| t.status == "closed") {
                let _ = epic_repo.close(epic_id).await;
            }
        }
        Some(EpicReviewVerdict::IssuesFound) => {
            let verdict = "epic reviewer reported EPIC_REVIEW_RESULT: ISSUES_FOUND";
            let _ = batch_repo.mark_issues_found(&batch_id, verdict).await;
            if let Ok(Some(epic)) = epic_repo.get(epic_id).await
                && epic.status == "in_review"
            {
                let _ = epic_repo.reopen(epic_id).await;
            }
        }
        None => {
            let verdict = error_reason
                .unwrap_or("epic reviewer ended without required EPIC_REVIEW_RESULT marker");
            let _ = batch_repo.mark_issues_found(&batch_id, verdict).await;
            if let Ok(Some(epic)) = epic_repo.get(epic_id).await
                && epic.status == "in_review"
            {
                let _ = epic_repo.reopen(epic_id).await;
            }
        }
    }
}

pub(crate) async fn cleanup_paused_worker_session(task_id: &str, app_state: &AppState) {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    let (tokens_in, tokens_out) = if let Some(ref gsid) = paused.goose_session_id {
        // Best effort — use stored tokens if sqlite unavailable
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

pub(crate) async fn success_transition(
    task_id: &str,
    agent_type: AgentType,
    output: &ParsedAgentOutput,
    app_state: &AppState,
) -> Option<(TransitionAction, Option<String>)> {
    match agent_type {
        AgentType::Worker | AgentType::ConflictResolver => match output.worker_signal {
            Some(crate::agent::output_parser::WorkerSignal::Done) => {
                Some((TransitionAction::SubmitTaskReview, None))
            }
            None => {
                let reason = output
                    .runtime_error
                    .clone()
                    .unwrap_or_else(|| "worker session completed without DONE marker".to_string());
                tracing::warn!(reason = %reason, "worker session completed without structured result marker");
                Some((TransitionAction::Release, Some(reason)))
            }
        },
        AgentType::TaskReviewer => match output.reviewer_verdict {
            Some(ReviewerVerdict::Verified) => merge_after_task_review(task_id, app_state).await,
            Some(ReviewerVerdict::Reopen) => Some((
                TransitionAction::TaskReviewReject,
                Some(
                    output
                        .reviewer_feedback
                        .clone()
                        .unwrap_or_else(|| "reviewer requested REOPEN".to_string()),
                ),
            )),
            None => {
                tracing::warn!("task reviewer session completed without REVIEW_RESULT marker");
                Some((
                    TransitionAction::ReleaseTaskReview,
                    Some("reviewer session completed without REVIEW_RESULT marker".to_string()),
                ))
            }
        },
        AgentType::EpicReviewer => match output.epic_verdict {
            Some(EpicReviewVerdict::Clean) => None,
            Some(EpicReviewVerdict::IssuesFound) => None,
            None => {
                tracing::warn!("epic reviewer session completed without EPIC_REVIEW_RESULT marker");
                None
            }
        },
    }
}

use std::path::PathBuf;

use crate::actors::git::GitError;
use crate::agent::AgentType;
use crate::agent::output_parser::ParsedAgentOutput;
use crate::db::SessionRepository;
use crate::db::TaskRepository;
use crate::models::SessionStatus;
use crate::models::TransitionAction;
use crate::server::AppState;

// Stale cycle threshold: escalate after this many stale continuations.
const STALE_ESCALATION_THRESHOLD: i64 = 3;

use super::*;

/// Previously read token counts from Goose SQLite; now always returns None
/// since new sessions use the Djinn-native reply loop (no Goose session ID).
pub(crate) async fn tokens_from_goose_sqlite(_goose_session_id: &str) -> Option<(i64, i64)> {
    None
}

// ─── Merge helpers ───────────────────────────────────────────────────────────

/// Transition actions to use for each merge outcome.
/// Allows both the reviewer and PM approval paths to reuse the same merge logic.
pub(crate) struct MergeActions {
    pub approve: TransitionAction,
    pub conflict: TransitionAction,
    pub release: TransitionAction,
}

/// Standard actions used by the task reviewer path.
pub(crate) const REVIEWER_MERGE_ACTIONS: MergeActions = MergeActions {
    approve: TransitionAction::TaskReviewApprove,
    conflict: TransitionAction::TaskReviewRejectConflict,
    release: TransitionAction::ReleaseTaskReview,
};

/// Actions used when PM approves directly from intervention.
pub(crate) const PM_MERGE_ACTIONS: MergeActions = MergeActions {
    approve: TransitionAction::PmApprove,
    conflict: TransitionAction::PmApproveConflict,
    release: TransitionAction::PmInterventionRelease,
};

pub(crate) async fn merge_after_task_review(
    task_id: &str,
    app_state: &AppState,
) -> Option<(TransitionAction, Option<String>)> {
    merge_and_transition(task_id, app_state, &REVIEWER_MERGE_ACTIONS).await
}

pub(crate) async fn merge_and_transition(
    task_id: &str,
    app_state: &AppState,
    actions: &MergeActions,
) -> Option<(TransitionAction, Option<String>)> {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
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

    let project_dir = project_path_for_id(&task.project_id, app_state)
        .await
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let git = match app_state.git_actor(&project_dir).await {
        Ok(git) => git,
        Err(e) => {
            return Some((
                actions.release.clone(),
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

    // Delete saved conversation file (no longer needed after approval).
    super::conversation_store::delete(&paused.id).await;

    if let Some(worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
        cleanup_worktree(task_id, &worktree_path, app_state).await;
    }
}

pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &AppState) {
    let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };
    // Delete saved conversation file (session is being discarded).
    super::conversation_store::delete(&paused.id).await;

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
            // Worker completed — submit for background verification.
            Some((TransitionAction::SubmitVerification, None))
        }
        AgentType::PM => {
            // Check if the PM already transitioned the task via tools during
            // its session (e.g. pm_intervention_complete, force_close).  If the
            // task is no longer in_pm_intervention, the PM acted — no fallback
            // transition needed.
            let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
            if let Ok(Some(task)) = repo.get(task_id).await
                && task.status != "in_pm_intervention"
            {
                tracing::info!(
                    task_id = %task_id,
                    current_status = %task.status,
                    "PM agent: task already transitioned by PM tools — no fallback needed"
                );
                return None;
            }
            // PM session ended without acting — release back so it gets re-dispatched.
            tracing::warn!(task_id = %task_id, "PM agent: session ended without explicit completion → releasing back to needs_pm_intervention");
            Some((TransitionAction::PmInterventionRelease, Some("PM session ended without completing intervention".to_string())))
        }
        AgentType::Groomer => {
            // Groomer has no lifecycle transition wiring yet.
            None
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

                        // Detect stale reopen cycle: check AC met-state against snapshot from
                        // when the current review cycle started.
                        let is_stale = is_stale_review_cycle(task_id, &task.acceptance_criteria, app_state).await;
                        let continuation_count = task.continuation_count;

                        if is_stale && continuation_count + 1 >= STALE_ESCALATION_THRESHOLD {
                            tracing::info!(
                                task_id = %task_id,
                                continuation_count = continuation_count,
                                "task reviewer: stale cycle limit reached → escalating to PM intervention"
                            );
                            Some((TransitionAction::Escalate, Some(format!("stale reopen limit reached after {} cycles: {}", continuation_count + 1, feedback))))
                        } else if is_stale {
                            tracing::info!(
                                task_id = %task_id,
                                continuation_count = continuation_count,
                                "task reviewer: stale cycle detected → increment continuation"
                            );
                            Some((TransitionAction::TaskReviewRejectStale, Some(feedback)))
                        } else {
                            tracing::info!(task_id = %task_id, "task reviewer: unmet AC, AC progress detected → reject");
                            Some((TransitionAction::TaskReviewReject, Some(feedback)))
                        }
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

/// Returns true if the AC met-state is identical to the snapshot from when
/// the current review cycle started (i.e. the worker made no AC progress).
async fn is_stale_review_cycle(
    task_id: &str,
    current_ac_json: &str,
    app_state: &AppState,
) -> bool {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    let snapshot_json = match repo.last_review_start_ac_snapshot(task_id).await {
        Ok(Some(s)) => s,
        _ => return false, // no snapshot → assume not stale
    };

    // Compare only the `met` booleans, not the full AC (description may differ).
    fn extract_met_pattern(json: &str) -> Vec<bool> {
        #[derive(serde::Deserialize)]
        struct Criterion {
            #[serde(default)]
            met: bool,
        }
        serde_json::from_str::<Vec<Criterion>>(json)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.met)
            .collect()
    }

    extract_met_pattern(current_ac_json) == extract_met_pattern(&snapshot_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::task::TaskRepository;
    use crate::models::task::Task;
    use crate::test_helpers;

    fn ac(items: &[bool]) -> String {
        serde_json::to_string(
            &items
                .iter()
                .map(|met| serde_json::json!({"description": "x", "met": met}))
                .collect::<Vec<_>>(),
        )
        .expect("serialize AC json")
    }

    fn parsed_output_with_feedback(feedback: &str) -> ParsedAgentOutput {
        let mut out = ParsedAgentOutput::new(AgentType::TaskReviewer);
        out.reviewer_feedback = Some(feedback.to_string());
        out
    }

    async fn create_task_with_ac(app: &AppState, ac_json: &str) -> Task {
        let project = test_helpers::create_test_project(app.db()).await;
        let epic = test_helpers::create_test_epic(app.db(), &project.id).await;
        let task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;

        sqlx::query("UPDATE tasks SET acceptance_criteria = ?1 WHERE id = ?2")
            .bind(ac_json)
            .bind(&task.id)
            .execute(app.db().pool())
            .await
            .expect("update AC");

        TaskRepository::new(app.db().clone(), app.events().clone())
            .get(&task.id)
            .await
            .expect("read task")
            .expect("task exists")
    }

    #[test]
    fn all_acceptance_criteria_met_cases() {
        assert!(!all_acceptance_criteria_met("[]"));
        assert!(all_acceptance_criteria_met(&ac(&[true, true])));
        assert!(!all_acceptance_criteria_met(&ac(&[true, false])));
        assert!(!all_acceptance_criteria_met("{not json}"));
        assert!(all_acceptance_criteria_met(&ac(&[true])));
        assert!(!all_acceptance_criteria_met(&ac(&[false, true, false])));
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    use crate::db::repositories::task::TaskRepository;
    use crate::models::task::TransitionAction;
    use crate::test_helpers;

    async fn set_task_status(app: &AppState, task_id: &str, status: &str) {
        sqlx::query("UPDATE tasks SET status = ?1 WHERE id = ?2")
            .bind(status)
            .bind(task_id)
            .execute(app.db().pool())
            .await
            .expect("update task status");
    }

    async fn set_task_ac(app: &AppState, task_id: &str, ac_json: &str) {
        sqlx::query("UPDATE tasks SET acceptance_criteria = ?1 WHERE id = ?2")
            .bind(ac_json)
            .bind(task_id)
            .execute(app.db().pool())
            .await
            .expect("update AC");
    }

    async fn set_continuation_count(app: &AppState, task_id: &str, count: i64) {
        sqlx::query("UPDATE tasks SET continuation_count = ?1 WHERE id = ?2")
            .bind(count)
            .bind(task_id)
            .execute(app.db().pool())
            .await
            .expect("update continuation_count");
    }

    async fn insert_review_snapshot(app: &AppState, task_id: &str, ac_json: &str) {
        let payload = serde_json::json!({"to_status":"in_task_review","ac_snapshot":serde_json::from_str::<serde_json::Value>(ac_json).expect("valid ac json")}).to_string();
        sqlx::query("INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) VALUES (?1, ?2, 'test', 'system', 'status_changed', ?3)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(task_id)
            .bind(payload)
            .execute(app.db().pool())
            .await
            .expect("insert snapshot");
    }

    fn ac(items: &[bool]) -> String {
        serde_json::to_string(
            &items
                .iter()
                .map(|met| serde_json::json!({"description": "x", "met": met}))
                .collect::<Vec<_>>(),
        )
        .expect("serialize AC json")
    }

    fn parsed_output_with_feedback(feedback: &str) -> ParsedAgentOutput {
        let mut out = ParsedAgentOutput::new(AgentType::TaskReviewer);
        out.reviewer_feedback = Some(feedback.to_string());
        out
    }

    #[tokio::test]
    async fn is_stale_review_cycle_cases() {
        let app = test_helpers::test_app_state_in_memory().await;
        let project = test_helpers::create_test_project(app.db()).await;
        let epic = test_helpers::create_test_epic(app.db(), &project.id).await;
        let task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;

        let same = ac(&[true, false]);
        insert_review_snapshot(&app, &task.id, &same).await;
        assert!(is_stale_review_cycle(&task.id, &same, &app).await);

        let progressed = ac(&[true, true]);
        assert!(!is_stale_review_cycle(&task.id, &progressed, &app).await);

        let task2 = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        assert!(!is_stale_review_cycle(&task2.id, &same, &app).await);

        let empty = "[]".to_string();
        insert_review_snapshot(&app, &task2.id, &empty).await;
        assert!(is_stale_review_cycle(&task2.id, &empty, &app).await);

        let task3 = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        let three = ac(&[true, false, true]);
        let five = ac(&[true, false, true, false, true]);
        insert_review_snapshot(&app, &task3.id, &three).await;
        assert!(!is_stale_review_cycle(&task3.id, &five, &app).await);
    }

    #[tokio::test]
    async fn success_transition_agent_variants_and_stale_threshold() {
        let app = test_helpers::test_app_state_in_memory().await;
        let project = test_helpers::create_test_project(app.db()).await;
        let epic = test_helpers::create_test_epic(app.db(), &project.id).await;

        let worker_task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        let out = ParsedAgentOutput::new(AgentType::Worker);
        assert_eq!(
            success_transition(&worker_task.id, AgentType::Worker, &out, &app).await,
            Some((TransitionAction::SubmitVerification, None))
        );
        let conflict_out = ParsedAgentOutput::new(AgentType::ConflictResolver);
        assert_eq!(
            success_transition(
                &worker_task.id,
                AgentType::ConflictResolver,
                &conflict_out,
                &app
            )
            .await,
            Some((TransitionAction::SubmitVerification, None))
        );

        let pm_task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        set_task_status(&app, &pm_task.id, "in_pm_intervention").await;
        let pm_out = ParsedAgentOutput::new(AgentType::PM);
        assert_eq!(
            success_transition(&pm_task.id, AgentType::PM, &pm_out, &app).await,
            Some((
                TransitionAction::PmInterventionRelease,
                Some("PM session ended without completing intervention".to_string())
            ))
        );

        let pm_done_task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        set_task_status(&app, &pm_done_task.id, "done").await;
        assert_eq!(
            success_transition(&pm_done_task.id, AgentType::PM, &pm_out, &app).await,
            None
        );

        let groomer_out = ParsedAgentOutput::new(AgentType::Groomer);
        assert_eq!(
            success_transition(&worker_task.id, AgentType::Groomer, &groomer_out, &app).await,
            None
        );

        let reviewer_task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        set_task_ac(&app, &reviewer_task.id, &ac(&[true, false])).await;
        insert_review_snapshot(&app, &reviewer_task.id, &ac(&[true, true])).await;
        let reviewer_out = parsed_output_with_feedback("needs work");
        assert_eq!(
            success_transition(
                &reviewer_task.id,
                AgentType::TaskReviewer,
                &reviewer_out,
                &app
            )
            .await,
            Some((TransitionAction::TaskReviewReject, Some("needs work".to_string())))
        );

        let stale_task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        let stale_ac = ac(&[false]);
        set_task_ac(&app, &stale_task.id, &stale_ac).await;
        set_continuation_count(&app, &stale_task.id, 0).await;
        insert_review_snapshot(&app, &stale_task.id, &stale_ac).await;
        assert_eq!(
            success_transition(&stale_task.id, AgentType::TaskReviewer, &reviewer_out, &app).await,
            Some((
                TransitionAction::TaskReviewRejectStale,
                Some("needs work".to_string())
            ))
        );

        let escalate_task = test_helpers::create_test_task(app.db(), &project.id, &epic.id).await;
        set_task_ac(&app, &escalate_task.id, &stale_ac).await;
        set_continuation_count(&app, &escalate_task.id, 2).await;
        insert_review_snapshot(&app, &escalate_task.id, &stale_ac).await;
        let escalation = success_transition(
            &escalate_task.id,
            AgentType::TaskReviewer,
            &reviewer_out,
            &app,
        )
        .await;
        assert!(matches!(escalation, Some((TransitionAction::Escalate, _))));

        let missing = success_transition(
            "00000000-0000-0000-0000-000000000000",
            AgentType::TaskReviewer,
            &reviewer_out,
            &app,
        )
        .await;
        assert!(matches!(missing, Some((TransitionAction::ReleaseTaskReview, _))));
    }
}

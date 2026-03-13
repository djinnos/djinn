use crate::agent::AgentType;
use crate::agent::output_parser::ParsedAgentOutput;
use crate::db::TaskRepository;
use crate::db::repositories::task::transitions::merge_after_task_review;
use crate::models::TransitionAction;
use crate::server::AppState;

const STALE_ESCALATION_THRESHOLD: i64 = 3;

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
            Some((
                TransitionAction::PmInterventionRelease,
                Some("PM session ended without completing intervention".to_string()),
            ))
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
                        let feedback = output.reviewer_feedback.clone().unwrap_or_else(|| {
                            "reviewer found unmet acceptance criteria".to_string()
                        });

                        // Detect stale reopen cycle: check AC met-state against snapshot from
                        // when the current review cycle started.
                        let is_stale =
                            is_stale_review_cycle(task_id, &task.acceptance_criteria, app_state)
                                .await;
                        let continuation_count = task.continuation_count;

                        if is_stale && continuation_count + 1 >= STALE_ESCALATION_THRESHOLD {
                            tracing::info!(
                                task_id = %task_id,
                                continuation_count = continuation_count,
                                "task reviewer: stale cycle limit reached → escalating to PM intervention"
                            );
                            Some((
                                TransitionAction::Escalate,
                                Some(format!(
                                    "stale reopen limit reached after {} cycles: {}",
                                    continuation_count + 1,
                                    feedback
                                )),
                            ))
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
async fn is_stale_review_cycle(task_id: &str, current_ac_json: &str, app_state: &AppState) -> bool {
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

    fn ac(items: &[bool]) -> String {
        serde_json::to_string(
            &items
                .iter()
                .map(|met| serde_json::json!({"description": "x", "met": met}))
                .collect::<Vec<_>>(),
        )
        .expect("serialize AC json")
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
        let db = test_helpers::create_test_db();
        let app = crate::server::AppState::new(db, tokio_util::sync::CancellationToken::new());
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
        let db = test_helpers::create_test_db();
        let app = crate::server::AppState::new(db, tokio_util::sync::CancellationToken::new());
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
        set_task_status(&app, &pm_done_task.id, "closed").await;
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
            Some((
                TransitionAction::TaskReviewReject,
                Some("needs work".to_string())
            ))
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
        assert!(matches!(
            missing,
            Some((TransitionAction::ReleaseTaskReview, _))
        ));
    }
}

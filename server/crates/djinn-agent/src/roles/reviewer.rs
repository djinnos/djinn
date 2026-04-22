use crate::actors::slot::task_review::{
    STALE_ESCALATION_THRESHOLD, all_acceptance_criteria_met, is_stale_review_cycle,
};
use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use crate::roles::finalize::SubmitReview;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::TaskRepository;
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct ReviewerRole;

impl AgentRole for ReviewerRole {
    fn config(&self) -> &RoleConfig {
        &REVIEWER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn on_complete<'a>(
        &'a self,
        task_id: &'a str,
        output: &'a ParsedAgentOutput,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async move {
            // If the session ended via request_lead, the task already transitioned
            // to needs_lead_intervention — no further transition needed.
            if output.finalize_tool_name.as_deref() == Some("request_lead") {
                return None;
            }

            // ADR-036: use the explicit verdict from the finalize payload when present.
            // process_finalize_payload already updated AC state on the task before
            // on_complete is called, so the DB reflects the reviewer's verdicts.
            if let Some(payload) = &output.finalize_payload
                && let Ok(review) = serde_json::from_value::<SubmitReview>(payload.clone())
            {
                if review.verdict == "approved" {
                    tracing::info!(
                        task_id = %task_id,
                        "task reviewer: submit_review verdict=approved → approve"
                    );
                    return Some((TransitionAction::TaskReviewApprove, None));
                } else {
                    // Rejected — check staleness to pick the right reject action.
                    tracing::info!(
                        task_id = %task_id,
                        "task reviewer: submit_review verdict=rejected"
                    );
                    let feedback = review
                        .feedback
                        .unwrap_or_else(|| "reviewer found unmet acceptance criteria".to_string());
                    let repo =
                        TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                    let (is_stale, continuation_count) =
                        match repo.get(task_id).await.ok().flatten() {
                            Some(t) => {
                                let stale = is_stale_review_cycle(
                                    task_id,
                                    &t.acceptance_criteria,
                                    app_state,
                                )
                                .await;
                                (stale, t.continuation_count)
                            }
                            None => (false, 0),
                        };
                    if is_stale && continuation_count + 1 >= STALE_ESCALATION_THRESHOLD {
                        tracing::info!(
                            task_id = %task_id,
                            continuation_count = continuation_count,
                            "task reviewer: stale cycle limit reached → escalating to lead"
                        );
                        return Some((
                            TransitionAction::Escalate,
                            Some(format!(
                                "stale reopen limit reached after {} cycles: {}",
                                continuation_count + 1,
                                feedback
                            )),
                        ));
                    } else if is_stale {
                        tracing::info!(
                            task_id = %task_id,
                            continuation_count = continuation_count,
                            "task reviewer: stale cycle detected → increment continuation"
                        );
                        return Some((TransitionAction::TaskReviewRejectStale, Some(feedback)));
                    } else {
                        tracing::info!(
                            task_id = %task_id,
                            "task reviewer: unmet AC, rejected → reject"
                        );
                        return Some((TransitionAction::TaskReviewReject, Some(feedback)));
                    }
                }
            }

            // Fallback: read AC from DB (for sessions without a finalize payload).
            let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            match repo.get(task_id).await {
                Ok(Some(task)) => {
                    if all_acceptance_criteria_met(&task.acceptance_criteria) {
                        tracing::info!(task_id = %task_id, "task reviewer: all AC met → approve");
                        Some((TransitionAction::TaskReviewApprove, None))
                    } else {
                        let feedback = output.reviewer_feedback.clone().unwrap_or_else(|| {
                            "reviewer found unmet acceptance criteria".to_string()
                        });
                        let is_stale =
                            is_stale_review_cycle(task_id, &task.acceptance_criteria, app_state)
                                .await;
                        let continuation_count = task.continuation_count;
                        if is_stale && continuation_count + 1 >= STALE_ESCALATION_THRESHOLD {
                            tracing::info!(
                                task_id = %task_id,
                                continuation_count = continuation_count,
                                "task reviewer: stale cycle limit reached → escalating to lead"
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
                            tracing::info!(
                                task_id = %task_id,
                                "task reviewer: unmet AC, AC progress detected → reject"
                            );
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
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "failed to load task for reviewer verdict"
                    );
                    Some((
                        TransitionAction::ReleaseTaskReview,
                        Some(format!("failed to load task for verdict: {e}")),
                    ))
                }
            }
        })
    }
}

pub(crate) const REVIEWER_CONFIG: RoleConfig = RoleConfig {
    name: "reviewer",
    display_name: "Reviewer",
    dispatch_role: "reviewer",
    tool_schemas: extension::tool_schemas_reviewer,
    release_action: || TransitionAction::ReleaseTaskReview,
    initial_message: crate::prompts::REVIEWER_TEMPLATE,
    finalize_tool_names: &["submit_review", "request_lead"],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_parser::ParsedAgentOutput;
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

    async fn set_task_ac(db: &djinn_db::Database, task_id: &str, ac_json: &str) {
        TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .set_acceptance_criteria(task_id, ac_json)
            .await
            .expect("update AC");
    }

    async fn set_continuation_count(db: &djinn_db::Database, task_id: &str, count: i64) {
        TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .set_continuation_count(task_id, count)
            .await
            .expect("update continuation_count");
    }

    async fn insert_review_snapshot(db: &djinn_db::Database, task_id: &str, ac_json: &str) {
        let payload = serde_json::json!({"to_status":"in_task_review","ac_snapshot":serde_json::from_str::<serde_json::Value>(ac_json).expect("valid ac json")}).to_string();
        TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .log_activity(Some(task_id), "test", "system", "status_changed", &payload)
            .await
            .expect("insert snapshot");
    }

    #[tokio::test]
    async fn on_complete_unmet_ac_without_snapshot_rejects() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        set_task_ac(&ctx.db, &task.id, &ac(&[true, false])).await;

        let role = ReviewerRole;
        let output = ParsedAgentOutput::new(true);
        let result = role.on_complete(&task.id, &output, &ctx).await;

        assert_eq!(
            result,
            Some((
                TransitionAction::TaskReviewReject,
                Some("reviewer found unmet acceptance criteria".to_string()),
            ))
        );
    }

    #[tokio::test]
    async fn on_complete_unmet_ac_stale_cycle_rejects_stale() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let current = ac(&[true, false]);
        set_task_ac(&ctx.db, &task.id, &current).await;
        insert_review_snapshot(&ctx.db, &task.id, &current).await;
        set_continuation_count(&ctx.db, &task.id, 0).await;

        let role = ReviewerRole;
        let output = ParsedAgentOutput::new(true);
        let result = role.on_complete(&task.id, &output, &ctx).await;

        assert_eq!(
            result,
            Some((
                TransitionAction::TaskReviewRejectStale,
                Some("reviewer found unmet acceptance criteria".to_string()),
            ))
        );
    }

    #[tokio::test]
    async fn on_complete_unmet_ac_stale_cycle_at_threshold_escalates() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let current = ac(&[false, false]);
        set_task_ac(&ctx.db, &task.id, &current).await;
        insert_review_snapshot(&ctx.db, &task.id, &current).await;
        set_continuation_count(&ctx.db, &task.id, STALE_ESCALATION_THRESHOLD - 1).await;

        let role = ReviewerRole;
        let output = ParsedAgentOutput::new(true);
        let result = role.on_complete(&task.id, &output, &ctx).await;

        assert_eq!(
            result,
            Some((
                TransitionAction::Escalate,
                Some(format!(
                    "stale reopen limit reached after {} cycles: reviewer found unmet acceptance criteria",
                    STALE_ESCALATION_THRESHOLD
                )),
            )),
        );
    }

    #[tokio::test]
    async fn on_complete_missing_task_releases_review() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );

        let role = ReviewerRole;
        let output = ParsedAgentOutput::new(true);
        let result = role.on_complete("missing-task-id", &output, &ctx).await;

        assert_eq!(
            result,
            Some((
                TransitionAction::ReleaseTaskReview,
                Some("task not found during reviewer verdict".to_string()),
            )),
        );
    }
}

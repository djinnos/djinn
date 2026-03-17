use crate::actors::slot::task_review::{
    STALE_ESCALATION_THRESHOLD, all_acceptance_criteria_met, is_stale_review_cycle,
};
use crate::compaction::{REVIEWER_PROMPT, SUMMARISER_SYSTEM_TASK_REVIEWER};
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_db::TaskRepository;
use crate::task_merge::{VerificationGateFn, merge_after_task_review};
use djinn_core::models::{Task, TransitionAction};
use crate::context::AgentContext;
use futures::future::BoxFuture;

use super::{AgentRole, CompactionPrompts, RoleConfig};

pub(crate) struct TaskReviewerRole;

#[allow(dead_code)]
impl AgentRole for TaskReviewerRole {
    fn config(&self) -> &RoleConfig {
        &TASK_REVIEWER_CONFIG
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
            let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            match repo.get(task_id).await {
                Ok(Some(task)) => {
                    if all_acceptance_criteria_met(&task.acceptance_criteria) {
                        tracing::info!(task_id = %task_id, "task reviewer: all AC met → approve");
                        let gate_state = app_state.clone();
                        let gate: VerificationGateFn = Box::new(move |task_id: String, project_path: String| {
                            let s = gate_state.clone();
                            Box::pin(async move {
                                crate::actors::slot::verification::run_verification_gate(&task_id, &project_path, &s).await
                            })
                        });
                        merge_after_task_review(task_id, app_state, Some(gate)).await
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
                                "task reviewer: stale cycle limit reached → escalating to PM"
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

pub(crate) const TASK_REVIEWER_CONFIG: RoleConfig = RoleConfig {
    name: "task_reviewer",
    display_name: "Task Reviewer",
    dispatch_role: "task_reviewer",
    tool_schemas: extension::tool_schemas_reviewer,
    start_action: |status| match status {
        "needs_task_review" => Some(TransitionAction::TaskReviewStart),
        _ => None,
    },
    release_action: || TransitionAction::ReleaseTaskReview,
    initial_message: crate::prompts::TASK_REVIEWER_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: REVIEWER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_TASK_REVIEWER,
        pre_resume: REVIEWER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_TASK_REVIEWER,
    },
    preserves_session: false,
    is_project_scoped: false,
};

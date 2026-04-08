use crate::actors::coordinator::rules;
use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::TaskRepository;
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct PlannerRole;

impl AgentRole for PlannerRole {
    fn config(&self) -> &RoleConfig {
        &PLANNER_CONFIG
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
            let task = match repo.get(task_id).await {
                Ok(Some(t)) => t,
                _ => return None,
            };

            // Per ADR-051 §1: when the Planner finishes a review-type patrol task
            // and reports `next_patrol_minutes` in its submit_grooming payload,
            // log a `patrol_schedule` activity so the coordinator can update
            // `next_patrol_interval` on its next tick. (Moved from architect.rs
            // as part of the patrol ownership migration.)
            if task.issue_type == "review"
                && task.title.contains("patrol")
                && let Some(minutes) = output
                    .finalize_payload
                    .as_ref()
                    .and_then(|p| p.get("next_patrol_minutes"))
                    .and_then(|v| v.as_u64())
            {
                let minutes = (minutes as u32).clamp(
                    rules::MIN_PLANNER_PATROL_MINUTES,
                    rules::MAX_PLANNER_PATROL_MINUTES,
                );
                let payload_json =
                    serde_json::json!({ "next_patrol_minutes": minutes }).to_string();
                if let Err(e) = repo
                    .log_activity(
                        Some(task_id),
                        "planner",
                        "planner",
                        "patrol_schedule",
                        &payload_json,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        task_id,
                        "Planner: failed to log patrol_schedule activity"
                    );
                } else {
                    tracing::info!(
                        task_id,
                        next_patrol_minutes = minutes,
                        "Planner: patrol self-scheduled next run"
                    );
                }
            }

            // Planning tasks route through the approved → PR pipeline so that
            // any file changes (ADRs, briefs, roadmaps) get a PR.  If the
            // branch has no unique commits, `process_approved_tasks` will
            // close the task directly.
            if matches!(task.issue_type.as_str(), "planning" | "decomposition") {
                return Some((TransitionAction::SubmitForMerge, None));
            }

            // Review-type patrol and `request_planner` escalation tasks are
            // synthetic coordinator artifacts — they don't land a PR and
            // there is no downstream lifecycle to own their closure.  They
            // MUST close on session completion, otherwise the task stays
            // `in_progress` forever and `dispatch_ready_tasks` keeps
            // re-dispatching it (observed on task `yi5q` after ADR-051:
            // 10 sessions in 20 minutes, a new one every ~2 min on the
            // same task row — a full respawn loop).
            if task.issue_type == "review" {
                return Some((
                    TransitionAction::Close,
                    Some(djinn_core::models::task::CLOSE_REASON_COMPLETED.to_string()),
                ));
            }
            None
        })
    }
}

pub(crate) const PLANNER_CONFIG: RoleConfig = RoleConfig {
    name: "planner",
    display_name: "Planner",
    dispatch_role: "planner",
    tool_schemas: extension::tool_schemas_planner,
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::PLANNER_TEMPLATE,
    preserves_session: false,
    finalize_tool_names: &["submit_grooming"],
};

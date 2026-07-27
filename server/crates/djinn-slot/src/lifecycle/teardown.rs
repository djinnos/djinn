//! Lifecycle teardown: delegates to host callbacks.
use crate::finalize_handlers::{process_finalize_payload_with_outcome, record_rejected_integrity_entry};
use crate::host::SlotContext;
use crate::output_parser::ParsedAgentOutput;
use crate::roles_support::AgentRole;
use djinn_db::repositories::task_rejected_submission_integrity::TaskRejectedSubmissionIntegrityRepository;
use std::sync::Arc;

pub(crate) struct PostSessionParams {
    pub(crate) task_id: String,
    pub(crate) project_path: String,
    pub(crate) role: Arc<dyn AgentRole>,
    pub(crate) ctx: SlotContext,
    pub(crate) final_output: ParsedAgentOutput,
    pub(crate) final_result_ok: bool,
    pub(crate) final_error: Option<String>,
    pub(crate) tokens_in: i64,
    pub(crate) tokens_out: i64,
}

pub(crate) fn spawn_post_session_work(params: PostSessionParams) {
    params.ctx.register_background_work(&params.task_id);
    tokio::spawn(async move {
        let PostSessionParams {
            task_id,
            project_path,
            role,
            ctx,
            final_output,
            final_result_ok,
            final_error: _,
            tokens_in,
            tokens_out,
        } = params;
        // When the reply loop's no-progress integrity gate detected a second
        // consecutive identical rejected-fingerprint submit_work, the reply
        // loop already settled the session via `settle_no_progress_submission`
        // (activity logging, streak increment, planner intervention routing).
        // Teardown just skips normal finalize and auto-submit processing and
        // does NOT increment the dispatch_failure_streak.
        if final_output.no_progress_submission {
            // Settlement already completed in the reply loop; nothing to do.
        } else {
            let model_called_submit_work =
                final_output.finalize_tool_name.as_deref() == Some(role.finalize_tool_name());
            if model_called_submit_work {
                if final_result_ok {
                    let _ = process_finalize_payload_with_outcome(
                        &final_output.finalize_payload,
                        final_output.finalize_tool_name.as_deref().unwrap_or(""),
                        &task_id,
                        &ctx,
                    )
                    .await;
                }
            }
        }
        apply_transition_and_dispatch(
            None,
            &task_id,
            &project_path,
            &role,
            &ctx,
            tokens_in,
            tokens_out,
        )
        .await;
        ctx.deregister_background_work(&task_id);
    });
}

/// Settle a second-strike no-progress submission: record the
/// `no_progress_submission` activity, persist a rejected integrity entry with
/// an incremented `no_progress_streak`, emit a telemetry event, and route the
/// task into planner intervention via the coordinator.
///
/// This follows the normal settle path without incrementing the `dispatch_failure_streak`.
pub(crate) async fn settle_no_progress_submission(task_id: &str, ctx: &SlotContext) {
    // Record a `no_progress_submission` activity so the coordinator and
    // any audit trail can see why this session was terminated.
    let repo = djinn_db::TaskRepository::new(ctx.db.clone(), ctx.event_bus.clone());
    let payload = serde_json::json!({
        "reason": "no_progress_submission",
        "detail": "Second consecutive identical rejected-fingerprint submit_work \
                   intercepted by the submission integrity gate. Worker submitted \
                   the same diff as the latest rejected submission without making \
                   substantive changes. Routing to planner intervention.",
    })
    .to_string();
    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "system",
            "worker",
            "no_progress_submission",
            &payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "teardown: failed to log no_progress_submission activity"
        );
    }
    // This is a rejected/no-progress result, not a successful submission.
    // Do not advance the worker attempt to `submitted`: that transition is
    // reserved for the C2-validated `handle_submit_work` boundary.
    // Increment the task-level no_progress_streak by recording a new rejected
    // integrity entry with the same fingerprint. This uses the latest rejected
    // fingerprint already on file.
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(ctx.db.clone());
    if let Ok(Some(latest)) = integrity_repo.latest_for_task(task_id).await {
        record_rejected_integrity_entry(
            task_id,
            ctx,
            "no_progress_submission",
            None,
            None,
            &latest.diff_fingerprint,
        )
        .await;
    }
    // Emit a telemetry event for observability.
    ctx.event_bus.send(DjinnEventEnvelope {
        entity_type: "submit",
        action: "no_progress_submission_settled",
        payload: serde_json::json!({
            "task_id": task_id,
        }),
        id: Some(task_id.to_string()),
        project_id: None,
        from_sync: false,
    });
    // Route the task into planner intervention via the coordinator. The
    // coordinator's route_loop_guard_planner_intervention path clears
    // dispatch_failure_streak and dispatches a Planner escalation, matching
    // the existing a8pv contract.
    if let Some(trigger) = ctx.coordinator_trigger.as_ref() {
        let reason = format!(
            "Second-strike no_progress_submission: worker submitted the same \
             rejected-fingerprint diff twice consecutively without making \
             substantive changes (task {task_id}). Routing to Planner for \
             decompose / rescope / close decision."
        );
        trigger.try_route_no_progress_intervention(task_id, &reason);
    } else {
        tracing::warn!(
            task_id = %task_id,
            "teardown: no coordinator_trigger available; \
             no_progress_submission planner intervention not routed"
        );
    }
    tracing::info!(
        task_id = %task_id,
        "teardown: no_progress_submission settled and planner intervention routed"
    );
}

pub(crate) async fn apply_transition_and_dispatch(
    _transition: Option<(djinn_core::models::TransitionAction, Option<String>)>,
    task_id: &str,
    _project_path: &str,
    _role: &Arc<dyn AgentRole>,
    ctx: &SlotContext,
    _tokens_in: i64,
    _tokens_out: i64,
) {
    if let Ok(task) = ctx.load_task(task_id).await {
        ctx.trigger_dispatch_for_project(&task.project_id).await;
    }
}

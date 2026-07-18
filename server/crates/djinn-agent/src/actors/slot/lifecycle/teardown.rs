use std::sync::Arc;

use djinn_core::models::TransitionAction;
use djinn_db::TaskRepository;

use crate::actors::slot::helpers::load_task;
use crate::context::AgentContext;
use crate::roles::AgentRole;
use crate::task_merge::interrupt_paused_worker_session;

use djinn_slot::lifecycle::retry::retry_task_transition_on_locked;

pub(crate) struct PostSessionParams {
    pub(crate) task_id: String,
    pub(crate) project_path: String,
    pub(crate) role: Arc<dyn AgentRole>,
    pub(crate) app_state: AgentContext,
    pub(crate) final_output: crate::output_parser::ParsedAgentOutput,
    pub(crate) final_result_ok: bool,
    pub(crate) final_error: Option<String>,
    pub(crate) tokens_in: i64,
    pub(crate) tokens_out: i64,
}

pub(crate) fn spawn_post_session_work(params: PostSessionParams) {
    params.app_state.register_background_work(&params.task_id);
    tokio::spawn(async move {
        let PostSessionParams {
            task_id,
            project_path,
            role,
            app_state,
            final_output,
            final_result_ok,
            final_error,
            tokens_in,
            tokens_out,
        } = params;
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        if final_output.finalize_payload.is_none()
            && let Some(feedback) = final_output.reviewer_feedback.as_deref()
        {
            let payload = serde_json::json!({ "body": feedback }).to_string();
            if let Err(e) = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "task_reviewer",
                    "comment",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
            }
        }
        if final_result_ok {
            if final_output.finalize_tool_name.as_deref() == Some("submit_work") {
                // Worker submit_work is accepted by the slot reply loop as a
                // typed completion intent. Preserve that authoritative object
                // through the host boundary so reused verification evidence is
                // available to submit finalization.
                if let Some(intent) = final_output.completion_intent.as_ref() {
                    super::super::finalize_handlers::process_completion_intent(
                        intent,
                        "submit_work",
                        &task_id,
                        &app_state,
                    )
                    .await;
                }
            } else {
                super::super::finalize_handlers::process_finalize_payload(
                    &final_output.finalize_payload,
                    final_output.finalize_tool_name.as_deref().unwrap_or(""),
                    &task_id,
                    &app_state,
                )
                .await;
            }
        }
        if let Some(reason) = &final_error {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": role.config().name,
            })
            .to_string();
            let _ = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await;
        }
        if final_result_ok && let Some(scraped) = final_output.runtime_error.as_deref() {
            // Record a TYPED event with stable `error_class` — raw model prose
            // goes in `model_excerpt`, never as the `error` itself (ecji).
            let payload = serde_json::json!({
                "error_class": "model_reported_runtime_error",
                "error": "Model narration referenced a runtime error during an otherwise-successful session",
                "model_excerpt": truncate_model_excerpt(scraped),
                "agent_type": role.config().name,
            })
            .to_string();
            let _ = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await;
        }
        // Pass None: the supervisor stage loop is the sole transition authority.
        let _ = final_result_ok;
        let _ = &final_error;
        apply_transition_and_dispatch(
            None,
            &task_id,
            &project_path,
            &role,
            &app_state,
            tokens_in,
            tokens_out,
        )
        .await;
        app_state.deregister_background_work(&task_id);
    });
}

pub(crate) async fn apply_transition_and_dispatch(
    transition: Option<(TransitionAction, Option<String>)>,
    task_id: &str,
    _project_path: &str,
    role: &Arc<dyn AgentRole>,
    app_state: &AgentContext,
    tokens_in: i64,
    tokens_out: i64,
) {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Some((action, reason)) = transition {
        tracing::info!(
            task_id = %task_id,
            role = %role.config().name,
            transition_action = ?action,
            transition_reason = reason.as_deref().unwrap_or("<none>"),
            tokens_in,
            tokens_out,
            "Lifecycle: applying session transition"
        );
        let is_conflict_rejection = action == TransitionAction::TaskReviewRejectConflict;
        let is_orphaned_tool_call = reason
            .as_deref()
            .map(djinn_provider::error_classify::is_orphaned_tool_call_error_str)
            .unwrap_or(false);
        if is_orphaned_tool_call {
            tracing::warn!(
                task_id = %task_id,
                "Lifecycle: dropping poisoned session due to orphaned tool call; next dispatch will start a fresh session"
            );
        }
        if let Err(e) = retry_task_transition_on_locked(|| async {
            task_repo
                .transition(
                    task_id,
                    action.clone(),
                    "agent-supervisor",
                    "system",
                    reason.as_deref(),
                    None,
                )
                .await
        })
        .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to transition task after session");
            if action != TransitionAction::Release {
                let fallback_reason = format!("Fallback release: {e}");
                if let Err(e2) = retry_task_transition_on_locked(|| async {
                    task_repo
                        .transition(
                            task_id,
                            TransitionAction::Release,
                            "agent-supervisor",
                            "system",
                            Some(&fallback_reason),
                            None,
                        )
                        .await
                })
                .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e2,
                        "Lifecycle: fallback Release failed (task likely already transitioned)"
                    );
                }
            }
        }
        if is_conflict_rejection || is_orphaned_tool_call {
            interrupt_paused_worker_session(task_id, app_state).await;
        }
    } else {
        tracing::info!(
            task_id = %task_id,
            role = %role.config().name,
            tokens_in,
            tokens_out,
            "Lifecycle: session completed with no task transition"
        );
    }
    if let Ok(task) = load_task(task_id, app_state).await
        && let Some(coordinator) = app_state.coordinator().await
    {
        let _ = coordinator
            .trigger_dispatch_for_project(&task.project_id)
            .await;
    }
}

const MODEL_EXCERPT_MAX_CHARS: usize = 500;

/// Truncate a scraped model line for `model_excerpt`. Char-boundary safe.
fn truncate_model_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MODEL_EXCERPT_MAX_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MODEL_EXCERPT_MAX_CHARS).collect();
    format!("{head}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_excerpt_is_returned_verbatim_trimmed() {
        let line = "  Actually, looking at the error: connection refused  ";
        assert_eq!(
            truncate_model_excerpt(line),
            "Actually, looking at the error: connection refused"
        );
    }
    #[test]
    fn long_excerpt_is_truncated_on_char_boundary() {
        let line = "é".repeat(1000);
        let out = truncate_model_excerpt(&line);
        assert!(out.ends_with("… [truncated]"));
        assert_eq!(
            out.chars().filter(|c| *c == 'é').count(),
            MODEL_EXCERPT_MAX_CHARS
        );
    }
    #[test]
    fn model_prose_records_typed_error_not_raw_prose() {
        let scraped = "Actually, looking at the error: `Database::open_in_memory()` returns \
                       connection refused, which is weird. Let me check.";
        let payload = serde_json::json!({
            "error_class": "model_reported_runtime_error",
            "error": "Model narration referenced a runtime error during an otherwise-successful session",
            "model_excerpt": truncate_model_excerpt(scraped),
            "agent_type": "task_worker",
        });
        let error = payload["error"].as_str().unwrap();
        assert_eq!(
            error,
            "Model narration referenced a runtime error during an otherwise-successful session"
        );
        assert!(!error.contains("Database::open_in_memory"));
        assert!(!error.contains("Let me check"));
        assert_eq!(
            payload["error_class"].as_str().unwrap(),
            "model_reported_runtime_error"
        );
        assert_eq!(
            payload["model_excerpt"].as_str().unwrap(),
            scraped,
            "short prose is preserved verbatim in the labeled excerpt"
        );
    }
}

//! Lifecycle teardown: delegates to host callbacks.
use crate::finalize_handlers::{
    process_auto_submit_payload, process_finalize_payload_with_outcome,
};
use crate::host::SlotContext;
use crate::output_parser::{AutoSubmitSettlement, ParsedAgentOutput};
use crate::roles_support::AgentRole;
use djinn_core::events::DjinnEventEnvelope;
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
    let ctx = params.ctx.clone();
    let task_id = params.task_id.clone();
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

        if final_result_ok {
            let model_called_submit_work =
                final_output.finalize_tool_name.as_deref() == Some(role.finalize_tool_name());
            if model_called_submit_work {
                let _ = process_finalize_payload_with_outcome(
                    &final_output.finalize_payload,
                    final_output.finalize_tool_name.as_deref().unwrap_or(""),
                    &task_id,
                    &ctx,
                )
                .await;
            } else {
                let _ = settle_auto_submit_if_eligible(&task_id, &ctx, &final_output).await;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSubmitSettlementOutcome {
    Submitted,
    Skipped,
    Failed,
}

pub(crate) async fn settle_auto_submit_if_eligible(
    task_id: &str,
    ctx: &SlotContext,
    final_output: &ParsedAgentOutput,
) -> AutoSubmitSettlementOutcome {
    let Some(settlement) = final_output.auto_submit.as_ref() else {
        emit_auto_submit_fallback_hook(task_id, ctx, "no_decision");
        return AutoSubmitSettlementOutcome::Skipped;
    };

    emit_auto_submit_decision_events(task_id, ctx, settlement);

    if !settlement.decision.eligible {
        emit_auto_submit_fallback_hook(task_id, ctx, "decision_skipped");
        return AutoSubmitSettlementOutcome::Skipped;
    }

    let payload = auto_submit_payload(task_id, settlement);
    if process_auto_submit_payload(&payload, task_id, ctx).await {
        AutoSubmitSettlementOutcome::Submitted
    } else {
        emit_auto_submit_fallback_hook(task_id, ctx, "submit_failed");
        AutoSubmitSettlementOutcome::Failed
    }
}

fn auto_submit_payload(task_id: &str, settlement: &AutoSubmitSettlement) -> serde_json::Value {
    let verify = settlement.verify_run.as_ref();
    serde_json::json!({
        "task_id": task_id,
        "commit_title": settlement.commit_title.as_deref().unwrap_or("auto-submit verified worker diff"),
        "summary": settlement.summary.as_deref().unwrap_or("Auto-submitted eligible green exact diff."),
        "files_changed": settlement.files_changed.clone(),
        "remaining_concerns": settlement.remaining_concerns.clone(),
        "auto_submit_review_metadata": {
            "task_run_id": settlement.task_run_id.clone(),
            "trigger_reason": settlement.decision.trigger_reason.as_str(),
            "diff_fingerprint": settlement.review_event.diff_fingerprint.clone(),
            "verify_source": verify.map(|v| v.verify_source.as_str()),
            "verify_run_id": verify.map(|v| v.verify_run_id.as_str()),
            "verify_timestamp": verify.map(|v| v.completed_at.as_str()),
            "session_id": settlement.review_event.session_id.clone(),
            "model_id": settlement.review_event.model_id.clone(),
            "no_progress_streak": settlement.review_event.no_progress_streak
        }
    })
}

fn emit_auto_submit_decision_events(
    task_id: &str,
    ctx: &SlotContext,
    settlement: &AutoSubmitSettlement,
) {
    ctx.event_bus.send(DjinnEventEnvelope {
        entity_type: "review",
        action: "auto_submit_decision",
        payload: serde_json::to_value(&settlement.review_event).unwrap_or_default(),
        id: Some(task_id.to_string()),
        project_id: None,
        from_sync: false,
    });
}

fn emit_auto_submit_fallback_hook(task_id: &str, ctx: &SlotContext, reason: &'static str) {
    ctx.event_bus.send(DjinnEventEnvelope {
        entity_type: "review",
        action: "auto_submit_fallback_checkpoint_requested",
        payload: serde_json::json!({
            "task_id": task_id,
            "reason": reason,
        }),
        id: Some(task_id.to_string()),
        project_id: None,
        from_sync: false,
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_core::auto_submit_decision::{AutoSubmitDecision, ReviewAutoSubmitDecisionEvent};
    use djinn_core::canonical_verify::FreshnessVerdict;
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::{AutoSubmitTriggerReason, TaskRunTrigger, VerifyRunRecord};
    use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
    use djinn_db::repositories::verify_run::AutoSubmitReviewRepository;
    use std::sync::{Arc, Mutex};

    fn test_ctx_with_events(
        db: djinn_db::Database,
        events: Arc<Mutex<Vec<DjinnEventEnvelope>>>,
    ) -> SlotContext {
        let mut ctx =
            test_helpers::agent_context_from_db(db, tokio_util::sync::CancellationToken::new());
        ctx.event_bus = EventBus::new(move |event| {
            events.lock().expect("events mutex").push(event);
        });
        ctx
    }

    async fn fixture() -> (
        djinn_db::Database,
        SlotContext,
        djinn_core::models::Task,
        String,
        Arc<Mutex<Vec<DjinnEventEnvelope>>>,
    ) {
        let db = test_helpers::create_test_db();
        let events = Arc::new(Mutex::new(Vec::new()));
        let ctx = test_ctx_with_events(db.clone(), Arc::clone(&events));
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let run_id = uuid::Uuid::now_v7().to_string();
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .expect("create task run");
        (db, ctx, task, run_id, events)
    }

    fn verify_run(task_run_id: &str) -> VerifyRunRecord {
        VerifyRunRecord {
            id: "verify-record-1".to_string(),
            task_run_id: task_run_id.to_string(),
            verify_source: "ci".to_string(),
            verify_run_id: "ci-run-1".to_string(),
            command_version: None,
            profile_version: None,
            completed_at: "2026-07-01T00:00:00.000Z".to_string(),
            result: "pass".to_string(),
            diff_fingerprint: "diff-123".to_string(),
            check_coverage: None,
            created_at: "2026-07-01T00:00:01.000Z".to_string(),
        }
    }

    fn settlement(task_run_id: &str, eligible: bool) -> AutoSubmitSettlement {
        let decision = AutoSubmitDecision {
            eligible,
            trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
            block_reason: None,
            freshness_verdict: FreshnessVerdict::accept(),
        };
        AutoSubmitSettlement {
            task_run_id: task_run_id.to_string(),
            decision,
            review_event: ReviewAutoSubmitDecisionEvent {
                eligible,
                trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
                block_reason: None,
                diff_fingerprint: "diff-123".to_string(),
                freshness_verdict: FreshnessVerdict::accept(),
                submit_id: None,
                session_id: Some("session-1".to_string()),
                model_id: Some("model-1".to_string()),
                no_progress_streak: 3,
                model_called_submit_work: false,
            },
            verify_run: Some(verify_run(task_run_id)),
            commit_title: Some("auto submit title".to_string()),
            summary: Some("auto submit summary".to_string()),
            files_changed: vec!["src/lib.rs".to_string()],
            remaining_concerns: vec![],
        }
    }

    #[tokio::test]
    async fn eligible_auto_submit_uses_work_submission_path_and_metadata() {
        let (db, ctx, task, task_run_id, _events) = fixture().await;
        let mut output = ParsedAgentOutput::empty();
        output.auto_submit = Some(settlement(&task_run_id, true));

        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Submitted);

        let task_repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let activity = task_repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .any(|entry| entry.event_type == "work_submitted")
        );

        let records = AutoSubmitReviewRepository::new(db)
            .list_for_task_run(&task_run_id)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].trigger_reason, "controlled_termination");
        assert_eq!(records[0].diff_fingerprint, "diff-123");
        assert_eq!(records[0].verify_source.as_deref(), Some("ci"));
        assert_eq!(records[0].verify_run_id.as_deref(), Some("ci-run-1"));
        assert_eq!(records[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(records[0].model_id.as_deref(), Some("model-1"));
        assert_eq!(records[0].no_progress_streak, 3);
        assert!(!records[0].model_called_submit_work);
    }

    #[tokio::test]
    async fn skipped_auto_submit_emits_fallback_hook_without_submission() {
        let (db, ctx, task, task_run_id, events) = fixture().await;
        let mut output = ParsedAgentOutput::empty();
        output.auto_submit = Some(settlement(&task_run_id, false));

        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Skipped);

        let records = AutoSubmitReviewRepository::new(db.clone())
            .list_for_task_run(&task_run_id)
            .await
            .unwrap();
        assert!(records.is_empty());

        let task_repo = djinn_db::TaskRepository::new(db, ctx.event_bus.clone());
        let activity = task_repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .all(|entry| entry.event_type != "work_submitted")
        );

        let events = events.lock().expect("events mutex");
        assert!(
            events
                .iter()
                .any(|event| event.action == "auto_submit_decision")
        );
        assert!(
            events
                .iter()
                .any(|event| event.action == "auto_submit_fallback_checkpoint_requested")
        );
    }
}

//! Lifecycle teardown: delegates to host callbacks.
use crate::final_verification::verify_completion_intent;
use crate::finalize_handlers::{
    process_auto_submit_payload, process_completion_intent_with_outcome,
    process_finalize_payload_with_outcome, record_rejected_integrity_entry,
};
use crate::host::SlotContext;
use crate::output_parser::{AutoSubmitSettlement, CompletionIntent, ParsedAgentOutput};
use crate::roles_support::AgentRole;
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository;
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
                    if final_output.finalize_tool_name.as_deref() == Some("submit_work") {
                        if let Some(intent) = final_output.completion_intent.as_ref() {
                            let _ = process_completion_intent_with_outcome(
                                intent,
                                "submit_work",
                                &task_id,
                                &ctx,
                            )
                            .await;
                        }
                    } else if final_output.finalize_tool_name.as_deref() == Some("submit_review")
                        && final_output.completion_intent.is_some()
                    {
                        // A reviewer that reached the final-verification
                        // consult-or-run boundary carries a completion intent
                        // with reused or freshly-stored evidence. Thread it
                        // through the same handler so the reviewer's verdict
                        // and AC state are processed while the evidence
                        // survives finalization.
                        if let Some(intent) = final_output.completion_intent.as_ref() {
                            let _ = process_completion_intent_with_outcome(
                                intent,
                                "submit_review",
                                &task_id,
                                &ctx,
                            )
                            .await;
                        }
                    } else {
                        let _ = process_finalize_payload_with_outcome(
                            &final_output.finalize_payload,
                            final_output.finalize_tool_name.as_deref().unwrap_or(""),
                            &task_id,
                            &ctx,
                        )
                        .await;
                    }
                }
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
    // Attempt lifecycle: advance the matching pending attempt to `submitted`
    // for the no-progress settlement. Best-effort.
    crate::attempt_lifecycle::advance_to_submitted(
        ctx,
        crate::attempt_lifecycle::SubmitAdvancementParams {
            task_id,
            role: "worker",
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some(
                "no_progress_submission: second consecutive identical rejected-fingerprint submit_work",
            ),
            summary_json: None,
        },
    )
    .await;
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
        // Record the rejected submission fingerprint at the task level so the
        // live submit-work guard can detect no-progress resubmissions across
        // task runs. The settlement's diff_fingerprint was already computed by
        // the auto-submit gate.
        let fp = &settlement.review_event.diff_fingerprint;
        if !fp.is_empty() {
            let verdict_kind =
                auto_submit_trigger_to_rejected_verdict(&settlement.decision.trigger_reason);
            crate::finalize_handlers::record_rejected_integrity_entry(
                task_id,
                ctx,
                verdict_kind.as_str(),
                None,
                Some(&settlement.task_run_id),
                fp,
            )
            .await;
        }
        emit_auto_submit_fallback_hook(task_id, ctx, "decision_skipped");
        return AutoSubmitSettlementOutcome::Skipped;
    }
    // This is the same intent/coordinator boundary as model-called submit_work.
    // Constructing or persisting an auto-submit payload is forbidden until it
    // returns `Stored`.
    let mut intent = CompletionIntent::auto_submit(&settlement.task_run_id);
    if let Err(error) = verify_completion_intent(
        &mut intent,
        task_id,
        Some(&settlement.task_run_id),
        tokio_util::sync::CancellationToken::new(),
        ctx,
        "submit_work",
    )
    .await
    {
        tracing::warn!(task_id = %task_id, error = %error, "teardown: auto-submit final verification did not store a pass");
        emit_auto_submit_fallback_hook(task_id, ctx, "final_verification_failed");
        return AutoSubmitSettlementOutcome::Failed;
    }
    intent.finalize_payload = auto_submit_payload(task_id, settlement);
    if process_auto_submit_payload(&intent, task_id, ctx).await {
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
        entity_type: "verify",
        action: "freshness_evaluated",
        payload: serde_json::to_value(&settlement.freshness_event).unwrap_or_default(),
        id: Some(task_id.to_string()),
        project_id: None,
        from_sync: false,
    });
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

/// Map an auto-submit trigger reason to the corresponding rejected verdict
/// kind for task-level integrity recording.
fn auto_submit_trigger_to_rejected_verdict(
    trigger: &djinn_core::models::AutoSubmitTriggerReason,
) -> djinn_core::models::RejectedVerdictKind {
    match trigger {
        djinn_core::models::AutoSubmitTriggerReason::NoProgress => {
            djinn_core::models::RejectedVerdictKind::NoProgress
        }
        djinn_core::models::AutoSubmitTriggerReason::Looping => {
            djinn_core::models::RejectedVerdictKind::Looping
        }
        djinn_core::models::AutoSubmitTriggerReason::SoftDeadline => {
            djinn_core::models::RejectedVerdictKind::SoftDeadline
        }
        _ => djinn_core::models::RejectedVerdictKind::Other,
    }
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
    use crate::final_verification::{
        FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
    };
    use crate::host::{ResolvedMcpTools, SlotHostCallbacks};
    use crate::test_helpers;
    use djinn_core::auto_submit_decision::{
        AutoSubmitDecision, ReviewAutoSubmitDecisionEvent, VerifyFreshnessEvaluatedEvent,
    };
    use djinn_core::canonical_verify::FreshnessVerdict;
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::{AutoSubmitTriggerReason, TaskRunTrigger, VerifyRunRecord};
    use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
    use djinn_db::repositories::verify_run::AutoSubmitReviewRepository;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    /// Host callbacks that force one deterministic non-stored coordinator
    /// outcome at the completion-intent boundary so lifecycle-generated
    /// submissions (eligible auto-submit, controlled termination) can be
    /// regression tested without the production hermetic executor.
    struct NonStoredOutcomeCallbacks(FinalVerificationRecordingOutcome);

    impl SlotHostCallbacks for NonStoredOutcomeCallbacks {
        fn final_verification_outcome_for_test(
            &self,
            _request: &FinalVerificationCoordinatorRequest,
        ) -> Option<FinalVerificationRecordingOutcome> {
            Some(self.0.clone())
        }
        fn interrupt_paused_worker_session<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn resolve_mcp_tools<'a>(
            &'a self,
            _worktree_path: &'a str,
            _role_name: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<Box<dyn Future<Output = Result<ResolvedMcpTools, String>> + Send + 'a>> {
            Box::pin(async { Err("not implemented in test".into()) })
        }
        fn render_prompt(
            &self,
            _role_name: &str,
            _task: &djinn_core::models::Task,
            _context_json: &serde_json::Value,
        ) -> String {
            String::new()
        }
        fn initial_user_message<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
            Box::pin(async { String::new() })
        }
        fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
            panic!("build_mcp_state not needed in non-stored outcome tests")
        }
        fn require_project_id_for_task_ops<'a>(
            &'a self,
            _project: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            String,
                            djinn_control_plane::tools::task_tools::ErrorResponse,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                    error: "not implemented".into(),
                })
            })
        }
        fn resolve_provider_credential<'a>(
            &'a self,
            _provider_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<crate::helpers::ProviderCredential, String>> + Send + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in test".into()) })
        }
        fn run_task_dispatch<'a>(
            &'a self,
            _task_id: String,
            _project_path: String,
            _model_id: String,
            _ctx: SlotContext,
            _kill: tokio_util::sync::CancellationToken,
            _pause: tokio_util::sync::CancellationToken,
            _resume_lifecycle_metadata: Option<serde_json::Value>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn touch_activity_rpc<'a>(
            &'a self,
            _task_id: String,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn flush_session_tokens_rpc<'a>(
            &'a self,
            _session_id: String,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

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
    /// Fixture variant whose host callbacks control the completion-intent
    /// coordinator outcome (e.g. force `Ineligible`/`Error`).
    async fn fixture_with_callbacks(
        callbacks: Arc<dyn SlotHostCallbacks>,
    ) -> (
        djinn_db::Database,
        SlotContext,
        djinn_core::models::Task,
        String,
        Arc<Mutex<Vec<DjinnEventEnvelope>>>,
    ) {
        let db = test_helpers::create_test_db();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = test_helpers::agent_context_from_db_with_callbacks(db.clone(), callbacks);
        ctx.event_bus = EventBus::new({
            let events = Arc::clone(&events);
            move |event| {
                events.lock().expect("events mutex").push(event);
            }
        });
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
            ..VerifyRunRecord::default()
        }
    }
    fn settlement(task_run_id: &str, eligible: bool) -> AutoSubmitSettlement {
        settlement_with_trigger(
            task_run_id,
            eligible,
            AutoSubmitTriggerReason::ControlledTermination,
        )
    }
    fn settlement_with_trigger(
        task_run_id: &str,
        eligible: bool,
        trigger: AutoSubmitTriggerReason,
    ) -> AutoSubmitSettlement {
        let decision = AutoSubmitDecision {
            eligible,
            trigger_reason: trigger,
            block_reason: None,
            freshness_verdict: FreshnessVerdict::accept(),
        };
        AutoSubmitSettlement {
            task_run_id: task_run_id.to_string(),
            decision,
            freshness_event: VerifyFreshnessEvaluatedEvent {
                diff_fingerprint: "diff-123".to_string(),
                has_verify_run: true,
                freshness_verdict: FreshnessVerdict::accept(),
                trigger_reason: trigger,
                submit_id: None,
            },
            review_event: ReviewAutoSubmitDecisionEvent {
                eligible,
                trigger_reason: trigger,
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
    /// Assert the lifecycle-generated completion left no trace: no
    /// `work_submitted` activity and no auto-submit review metadata.
    async fn assert_completion_did_not_advance(
        db: &djinn_db::Database,
        ctx: &SlotContext,
        task_id: &str,
        task_run_id: &str,
    ) {
        let task_repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let activity = task_repo.list_activity(task_id).await.unwrap();
        assert!(
            activity
                .iter()
                .all(|entry| entry.event_type != "work_submitted"),
            "no work_submitted activity may be logged without a stored coordinator result"
        );
        let records = AutoSubmitReviewRepository::new(db.clone())
            .list_for_task_run(task_run_id)
            .await
            .unwrap();
        assert!(
            records.is_empty(),
            "completion must not advance without a stored coordinator result"
        );
    }
    /// Eligible auto-submit (idle trigger): an ineligible coordinator outcome
    /// must block submission even though the settlement was eligible.
    #[tokio::test]
    async fn eligible_auto_submit_without_stored_verification_never_submits() {
        let callbacks = Arc::new(NonStoredOutcomeCallbacks(
            FinalVerificationRecordingOutcome::Ineligible {
                verification_attempt_id: "attempt-ineligible".into(),
                reason: "CommandFailed { check_id: \"test\", exit_code: Some(1) }".into(),
            },
        ));
        let (db, ctx, task, task_run_id, events) = fixture_with_callbacks(callbacks).await;
        let mut output = ParsedAgentOutput::empty();
        output.auto_submit = Some(settlement_with_trigger(
            &task_run_id,
            true,
            AutoSubmitTriggerReason::Idle,
        ));
        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Failed);
        assert_completion_did_not_advance(&db, &ctx, &task.id, &task_run_id).await;
        let events = events.lock().expect("events mutex");
        let fallback = events
            .iter()
            .find(|event| event.action == "auto_submit_fallback_checkpoint_requested")
            .expect("final_verification_failed fallback hook must fire");
        assert_eq!(fallback.payload["reason"], "final_verification_failed");
    }
    /// Controlled termination: an error coordinator outcome must block the
    /// final-attempt submission even though the settlement was eligible.
    #[tokio::test]
    async fn controlled_termination_without_stored_verification_never_submits() {
        let callbacks = Arc::new(NonStoredOutcomeCallbacks(
            FinalVerificationRecordingOutcome::Error {
                verification_attempt_id: "attempt-error".into(),
                detail: "final verification insert failed: db unavailable".into(),
            },
        ));
        let (db, ctx, task, task_run_id, events) = fixture_with_callbacks(callbacks).await;
        let mut output = ParsedAgentOutput::empty();
        output.auto_submit = Some(settlement_with_trigger(
            &task_run_id,
            true,
            AutoSubmitTriggerReason::ControlledTermination,
        ));
        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Failed);
        assert_completion_did_not_advance(&db, &ctx, &task.id, &task_run_id).await;
        let events = events.lock().expect("events mutex");
        let fallback = events
            .iter()
            .find(|event| event.action == "auto_submit_fallback_checkpoint_requested")
            .expect("final_verification_failed fallback hook must fire");
        assert_eq!(fallback.payload["reason"], "final_verification_failed");
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
        assert!(events.iter().any(|event| {
            event.entity_type == "verify" && event.action == "freshness_evaluated"
        }));
        assert!(
            events
                .iter()
                .any(|event| event.action == "auto_submit_fallback_checkpoint_requested")
        );
    }
    #[tokio::test]
    async fn no_auto_submit_decision_emits_no_decision_fallback() {
        let (db, ctx, task, _task_run_id, events) = fixture().await;
        let output = ParsedAgentOutput::empty();
        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Skipped);
        // No work_submitted activity should be logged.
        let task_repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let activity = task_repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .all(|entry| entry.event_type != "work_submitted")
        );
        // The fallback hook should fire with "no_decision" reason.
        let events = events.lock().expect("events mutex");
        let fallback = events
            .iter()
            .find(|event| event.action == "auto_submit_fallback_checkpoint_requested");
        assert!(fallback.is_some(), "expected no_decision fallback event");
        let payload = &fallback.unwrap().payload;
        assert_eq!(payload["reason"], "no_decision");
        // No auto_submit_decision event because there was no settlement.
        assert!(
            events
                .iter()
                .all(|event| event.action != "auto_submit_decision")
        );
    }
    #[tokio::test]
    async fn model_called_submit_work_with_metadata_persists_model_called_true() {
        let (db, ctx, task, task_run_id, _events) = fixture().await;
        // Simulate the model calling submit_work with auto_submit_review_metadata.
        let metadata = serde_json::json!({
            "task_run_id": task_run_id,
            "trigger_reason": "idle",
            "diff_fingerprint": "diff-456",
            "verify_source": "local",
            "verify_run_id": "local-run-99",
            "verify_timestamp": "2026-07-01T12:00:00.000Z",
            "session_id": "session-42",
            "model_id": "model-42",
            "no_progress_streak": 0
        });
        let payload = serde_json::json!({
            "task_id": task.short_id,
            "commit_title": "feat: worker implemented the feature",
            "summary": "implemented feature X",
            "files_changed": ["src/main.rs"],
            "remaining_concerns": [],
            "auto_submit_review_metadata": metadata
        });
        // Call through process_finalize_payload_with_outcome (normal model path).
        let ok = crate::finalize_handlers::process_finalize_payload_with_outcome(
            &Some(payload),
            "submit_work",
            &task.id,
            &ctx,
        )
        .await;
        assert!(ok);
        // work_submitted activity should exist.
        let task_repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let activity = task_repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .any(|entry| entry.event_type == "work_submitted")
        );
        // The review record should have model_called_submit_work=true.
        let records = AutoSubmitReviewRepository::new(db)
            .list_for_task_run(&task_run_id)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].model_called_submit_work);
        assert_eq!(records[0].trigger_reason, "idle");
        assert_eq!(records[0].diff_fingerprint, "diff-456");
        assert_eq!(records[0].verify_source.as_deref(), Some("local"));
        assert_eq!(records[0].verify_run_id.as_deref(), Some("local-run-99"));
        assert_eq!(
            records[0].verify_timestamp.as_deref(),
            Some("2026-07-01T12:00:00.000Z")
        );
        assert_eq!(records[0].session_id.as_deref(), Some("session-42"));
        assert_eq!(records[0].model_id.as_deref(), Some("model-42"));
        assert_eq!(records[0].no_progress_streak, 0);
    }
    #[tokio::test]
    async fn normal_submit_work_without_metadata_still_works_and_no_review_record() {
        let (db, ctx, task, task_run_id, _events) = fixture().await;
        // Normal model submit_work without auto_submit_review_metadata.
        let payload = serde_json::json!({
            "task_id": task.short_id,
            "commit_title": "feat: normal submit",
            "summary": "did the work",
            "files_changed": ["src/lib.rs"],
            "remaining_concerns": []
        });
        let ok = crate::finalize_handlers::process_finalize_payload_with_outcome(
            &Some(payload),
            "submit_work",
            &task.id,
            &ctx,
        )
        .await;
        assert!(ok);
        // work_submitted activity should exist.
        let task_repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let activity = task_repo.list_activity(&task.id).await.unwrap();
        assert!(
            activity
                .iter()
                .any(|entry| entry.event_type == "work_submitted")
        );
        // No review metadata record since the model didn't include metadata.
        let records = AutoSubmitReviewRepository::new(db)
            .list_for_task_run(&task_run_id)
            .await
            .unwrap();
        assert!(records.is_empty());
    }
    #[tokio::test]
    async fn eligible_auto_submit_records_verify_timestamp_in_metadata() {
        let (db, ctx, task, task_run_id, _events) = fixture().await;
        let mut output = ParsedAgentOutput::empty();
        output.auto_submit = Some(settlement(&task_run_id, true));
        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Submitted);
        let records = AutoSubmitReviewRepository::new(db)
            .list_for_task_run(&task_run_id)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].verify_timestamp.as_deref(),
            Some("2026-07-01T00:00:00.000Z")
        );
    }
    #[tokio::test]
    async fn ineligible_settlement_records_rejected_fingerprint() {
        let (db, ctx, task, task_run_id, _events) = fixture().await;
        let mut output = ParsedAgentOutput::empty();
        let mut s = settlement(&task_run_id, false);
        // Ensure a non-empty fingerprint so the rejection path records it.
        s.review_event.diff_fingerprint = "sha256:rejected-by-gate".to_string();
        output.auto_submit = Some(s);
        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Skipped);
        // The auto_submit_review record should NOT be created (settlement was skipped).
        let review_records = AutoSubmitReviewRepository::new(db.clone())
            .list_for_task_run(&task_run_id)
            .await
            .unwrap();
        assert!(review_records.is_empty());
        // But the task-level rejected integrity entry should be recorded.
        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("ineligible settlement must record rejected fingerprint");
        assert_eq!(latest.diff_fingerprint, "sha256:rejected-by-gate");
        assert_eq!(
            latest.verdict_kind,
            djinn_core::models::RejectedVerdictKind::Other.as_str()
        );
        assert_eq!(latest.no_progress_streak, 1);
        assert_eq!(latest.task_run_id.as_deref(), Some(task_run_id.as_str()));
    }
    #[tokio::test]
    async fn ineligible_settlement_skips_fingerprint_when_empty() {
        let (db, ctx, task, task_run_id, _events) = fixture().await;
        let mut output = ParsedAgentOutput::empty();
        let mut s = settlement(&task_run_id, false);
        // Empty fingerprint — the skip path should NOT record anything.
        s.review_event.diff_fingerprint = String::new();
        output.auto_submit = Some(s);
        let outcome = settle_auto_submit_if_eligible(&task.id, &ctx, &output).await;
        assert_eq!(outcome, AutoSubmitSettlementOutcome::Skipped);
        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
        assert!(
            latest.is_none(),
            "empty fingerprint must not produce a rejected integrity record"
        );
    }
    /// The `settle_no_progress_submission` function records a
    /// `no_progress_submission` activity, increments the no_progress_streak
    /// on the rejected integrity entry, and emits a telemetry event.
    #[tokio::test]
    async fn settle_no_progress_submission_records_activity_and_streak() {
        let (db, ctx, task, _run_id, events) = fixture().await;
        // Seed a rejected integrity entry so the settlement can increment the
        // streak.
        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(
                db.clone(),
            );
        let entry_id = uuid::Uuid::now_v7().to_string();
        integrity_repo
            .record(
                djinn_db::repositories::verify_run::RecordTaskRejectedSubmissionParams {
                    id: &entry_id,
                    task_id: &task.id,
                    task_run_id: None,
                    review_id: None,
                    verdict_kind: "reviewer_reject",
                    activity_id: None,
                    rejected_at: "2026-07-01T00:00:00Z",
                    diff_fingerprint: "sha256:same-fingerprint",
                    no_progress_streak: 1,
                },
            )
            .await
            .expect("record initial rejected entry");
        settle_no_progress_submission(&task.id, &ctx).await;
        // Verify the no_progress_submission activity was logged.
        let repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo
            .query_activity(djinn_db::repositories::task::ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some("no_progress_submission".to_string()),
                actor_role: None,
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 10,
                offset: 0,
            })
            .await
            .expect("query activity");
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one no_progress_submission activity"
        );
        // Verify the rejected integrity entry was incremented.
        let latest = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("should have a rejected integrity entry");
        assert_eq!(latest.no_progress_streak, 2, "streak should increment to 2");
        assert_eq!(latest.diff_fingerprint, "sha256:same-fingerprint");
        assert_eq!(latest.verdict_kind, "no_progress_submission");
        // Verify the telemetry event was emitted.
        let evts = events.lock().expect("events mutex");
        let settle_event = evts
            .iter()
            .find(|e| e.action == "no_progress_submission_settled");
        assert!(
            settle_event.is_some(),
            "no_progress_submission_settled telemetry event must be emitted"
        );
    }
    /// Regression: when no rejected integrity entry exists for the task,
    /// `settle_no_progress_submission` still records the activity and emits
    /// telemetry but does NOT create a new rejected integrity entry (no
    /// fingerprint to use).
    #[tokio::test]
    async fn settle_no_progress_submission_no_prior_entry() {
        let (db, ctx, task, _run_id, events) = fixture().await;
        // No prior rejected integrity entry.
        settle_no_progress_submission(&task.id, &ctx).await;
        // The activity should still be recorded.
        let repo = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo
            .query_activity(djinn_db::repositories::task::ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some("no_progress_submission".to_string()),
                actor_role: None,
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 10,
                offset: 0,
            })
            .await
            .expect("query activity");
        assert_eq!(entries.len(), 1);
        // No rejected integrity entry should be created (nothing to increment).
        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
        assert!(
            latest.is_none(),
            "should not create a rejected entry when no prior entry exists"
        );
        // Telemetry event should still fire.
        let evts = events.lock().expect("events mutex");
        let settle_event = evts
            .iter()
            .find(|e| e.action == "no_progress_submission_settled");
        assert!(settle_event.is_some());
    }
}

// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use djinn_core::models::Task;
use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::repositories::task_attempt::TaskAttemptRepository;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::task_run_outcome::TaskRunOutcomeRepository;
use djinn_db::{TaskRepository, task_branch_name};
use djinn_runtime::{
    BiStream, InfraDeathLogTailCapture, LoopGuardKind, ProviderFailureClass, ResolvedCredentials,
    ResumeLifecycleMetadata, SessionRuntime, StreamEvent, TaskRunOutcome, TaskRunReport,
    TestRuntime,
};
use djinn_slot::{TerminalExtractionContext, TerminalExtractionOutcome};

use crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference;
use crate::context::AgentContext;
use crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind};
use crate::supervisor::{RoleKind, SupervisorFlow, TaskRunSpec, services_for_agent_context};

use super::helpers::{
    build_restamp_target, conflict_context_for_dispatch, default_target_branch,
    load_provider_credential, parse_model_id, refresh_oauth_credential_after_401,
};

fn supervisor_rpc_span(op: &'static str, session_id: &str, task_id: &str) -> tracing::Span {
    tracing::info_span!(
        "djinn.supervisor.rpc",
        op,
        session_id = %session_id,
        task_id = %task_id,
    )
}

/// Pre-session liveness deadline (8 min default).
const PRE_SESSION_DEADLINE_SECS_DEFAULT: u64 = 480;

/// Step label before the worker emits its first stage marker.
const PRE_SESSION_INITIAL_STEP: &str = "run_create";

fn pre_session_deadline() -> std::time::Duration {
    let secs = std::env::var("DJINN_PRESESSION_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(PRE_SESSION_DEADLINE_SECS_DEFAULT);
    std::time::Duration::from_secs(secs)
}

/// Pre-session stage-init deadline exceeded; names the hung step for diagnostics.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "pre-session stage-init deadline exceeded: no session / first provider turn after \
     {elapsed_secs}s (hung at step '{step}')"
)]
pub struct PreSessionTimeout {
    pub step: String,
    pub elapsed_secs: u64,
}

/// Outcome of awaiting the worker's terminal report or pre-session deadline.
enum ReportAwait {
    Report(Option<TaskRunReport>),
    PreSessionTimeout(PreSessionTimeout),
}

/// Best-effort mark credential revoked and emit event after a 401.
async fn surface_credential_revocation(
    app_state: &AgentContext,
    owner: Option<&str>,
    model_id: &str,
) {
    let Ok((provider_id, _model_name)) = parse_model_id(model_id) else {
        return;
    };
    let cred_provider = djinn_provider::catalog::builtin::resolve_oauth_provider(&provider_id)
        .map(|s| s.to_string())
        .unwrap_or(provider_id);
    let reason = format!(
        "{cred_provider} rejected the credential (HTTP 401 — token revoked or invalid). \
         Reconnect this provider to resume."
    );
    let repo = djinn_provider::repos::CredentialRepository::new(
        app_state.db.clone(),
        app_state.event_bus.clone(),
    );
    match repo.mark_revoked(&cred_provider, owner, &reason).await {
        Ok(n) if n > 0 => tracing::warn!(
            provider = %cred_provider,
            owner = ?owner,
            "supervisor: marked credential revoked after 401 — owner must reconnect"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            provider = %cred_provider,
            error = %e,
            "supervisor: failed to mark credential revoked after 401"
        ),
    }
}

/// Record stall + failover + activity when the worker never completed its
/// startup handshake within the deadline.
async fn apply_handshake_timeout_failover(
    app_state: &AgentContext,
    task_repo: &TaskRepository,
    task: &Task,
    model_id: &str,
    creator_scope: Option<&str>,
) {
    app_state
        .health_tracker
        .record_stall(creator_scope, model_id, true);
    app_state.health_tracker.note_task_provider_failure(
        &task.id,
        djinn_provider::catalog::health::TaskFailureSignal {
            throttle: true,
            retry_after_ms: None,
        },
    );
    let _ = task_repo
        .log_activity(
            Some(&task.id),
            "system",
            "system",
            "comment",
            &serde_json::json!({
                "body": format!(
                    "Worker Pod failed to complete its startup handshake within the \
                     deadline (image pull, unschedulable, or crash-loop). Tore down the \
                     Job and failed over off model {model_id}."
                )
            })
            .to_string(),
        )
        .await;
    tracing::warn!(
        task_id = %task.short_id,
        %model_id,
        "supervisor dispatch: worker handshake timed out; recorded stall + failover"
    );
}

/// Log a `session_error` activity entry and finalize any orphaned running
/// session rows when the worker infrastructure died before completing a run.
async fn finalize_infra_death_session(
    task_repo: &TaskRepository,
    task: &Task,
    app_state: &AgentContext,
    reason: &str,
) {
    let payload = serde_json::json!({
        "error": format!("Worker infrastructure died before completing the run: {reason}"),
        "agent_type": "system",
    })
    .to_string();
    let _ = task_repo
        .log_activity(
            Some(&task.id),
            "agent-supervisor",
            "system",
            "session_error",
            &payload,
        )
        .await;
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match session_repo.interrupt_running_for_task(&task.id).await {
        Ok(n) if n > 0 => tracing::warn!(
            task_id = %task.short_id,
            %reason,
            sessions = n,
            "supervisor dispatch: finalized orphaned running session(s) after infra death"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "supervisor dispatch: failed to finalize session row after infra death"
        ),
    }
}

/// Best-effort persist infra-death log-tail capture on the latest
/// pending/submitted attempt for the task.  Failures are logged and swallowed —
/// this is purely diagnostic enrichment and must never block teardown.
async fn persist_infra_death_on_attempt(
    app_state: &AgentContext,
    task: &Task,
    reason: &str,
    capture: &InfraDeathLogTailCapture,
) {
    let attempt_repo = TaskAttemptRepository::new(app_state.db.clone());
    let attempt = match attempt_repo
        .latest_pending_or_submitted(&task.id, None)
        .await
    {
        Ok(Some(a)) => a,
        Ok(None) => {
            tracing::debug!(
                task_id = %task.short_id,
                "supervisor dispatch: no pending/submitted attempt for infra-death log-tail persist; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "supervisor dispatch: failed to look up attempt for infra-death log-tail persist"
            );
            return;
        }
    };

    let meta = serde_json::json!({
        "infra_death_log_tail": {
            "fetched": capture.log_tail.is_some(),
            "fetch_error_class": capture.fetch_error_class,
            "fetch_error_detail": capture.fetch_error_detail,
            "death_reason": reason,
        }
    })
    .to_string();

    match attempt_repo
        .persist_infra_death_log_tail(&attempt.id, capture.log_tail.as_deref(), &meta)
        .await
    {
        Ok(_) => tracing::info!(
            task_id = %task.short_id,
            attempt_id = %attempt.id,
            has_log_tail = capture.log_tail.is_some(),
            error_class = ?capture.fetch_error_class,
            "supervisor dispatch: persisted infra-death log-tail on attempt"
        ),
        Err(e) => tracing::warn!(
            task_id = %task.short_id,
            attempt_id = %attempt.id,
            error = %e,
            "supervisor dispatch: failed to persist infra-death log-tail on attempt"
        ),
    }
}

/// Best-effort: advance this dispatch's still-`pending` attempt row to
/// `crashed` when the run's terminal report is `Failed`. Without this the row
/// keeps its `pending` outcome and the respawn guard defers every subsequent
/// (task, role) dispatch until the periodic orphaned-attempt reaper catches it
/// (5-minute threshold on a 15-minute sweep — up to ~20 minutes of dead board
/// time per provider failure; incident 8lb0, 2026-07-16). A `submitted` row is
/// deliberately left alone: submitted work is owned by the review/PR lifecycle
/// and must keep its submitted signal.
async fn terminalize_failed_run_attempt(
    app_state: &AgentContext,
    task_attempt_id: Option<&str>,
    dispatch_group_id: Option<&str>,
    task: &Task,
    report: &TaskRunReport,
) {
    let TaskRunOutcome::Failed { stage, reason, .. } = &report.outcome else {
        return;
    };
    let attempt_repo = TaskAttemptRepository::new(app_state.db.clone());
    let truncated_reason: String = reason.chars().take(500).collect();
    let summary = format!("run failed at stage {stage}: {truncated_reason}");
    let summary_json = serde_json::json!({
        "recovery_classifier": "failed_run_report",
        "stage": stage,
    })
    .to_string();
    if let Some(group_id) = dispatch_group_id {
        match attempt_repo
            .terminalize_dispatch_group(
                group_id,
                djinn_core::models::task_attempt::TaskAttemptOutcome::Crashed,
                djinn_db::DispatchGroupTerminalEvidence {
                    summary: Some(&summary),
                    summary_json: Some(&summary_json),
                },
            )
            .await
        {
            Ok(result) => tracing::info!(
                task_id = %task.short_id,
                dispatch_group_id = %group_id,
                terminalized_attempts = result.updated_attempt_ids.len(),
                "supervisor dispatch: terminalized failed run's pending dispatch group"
            ),
            Err(e) => tracing::warn!(
                task_id = %task.short_id,
                dispatch_group_id = %group_id,
                error = %e,
                "supervisor dispatch: failed to terminalize failed run's pending dispatch group"
            ),
        }
        return;
    }
    // Mixed-version dispatches have no group to correlate. Preserve the
    // conservative single-row cleanup rather than widening by task or role.
    let Some(attempt_id) = task_attempt_id else {
        tracing::debug!(task_id = %task.short_id, "supervisor dispatch: failed run has no dispatch group or exact attempt ID; leaving legacy attempts untouched");
        return;
    };
    let attempt = match attempt_repo.get(attempt_id).await {
        Ok(Some(attempt)) => attempt,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                attempt_id,
                error = %e,
                "supervisor dispatch: failed to load attempt for failed-run terminalization"
            );
            return;
        }
    };
    if attempt.outcome != djinn_core::models::task_attempt::TaskAttemptOutcome::Pending.as_str() {
        return;
    }
    match attempt_repo
        .advance_to_terminal(djinn_db::TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some(&summary),
            summary_json: Some(&summary_json),
            log_tail: None,
        })
        .await
    {
        Ok(updated) => tracing::info!(
            task_id = %task.short_id,
            attempt_id = %updated.id,
            outcome = %updated.outcome,
            "supervisor dispatch: terminalized failed run's pending attempt"
        ),
        Err(e) => tracing::warn!(
            task_id = %task.short_id,
            attempt_id,
            error = %e,
            "supervisor dispatch: failed to terminalize failed run's pending attempt"
        ),
    }
}

/// Feed provider-breaker feedback on the terminal report, including OAuth
/// refresh-on-401 and credential-revocation surfacing.
async fn apply_provider_breaker_feedback(
    app_state: &AgentContext,
    report: &TaskRunReport,
    model_id: &str,
    creator_scope: Option<&str>,
    task_id: &str,
) {
    if terminal_report_feeds_model_success(report) {
        app_state
            .health_tracker
            .record_success(creator_scope, model_id);
    }
    if let Some(class) = provider_failure_class_for_report(report) {
        let (is_throttle, retry_after_ms) = match class {
            djinn_runtime::ProviderFailureClass::Throttle { retry_after_ms } => {
                app_state
                    .health_tracker
                    .record_stall(creator_scope, model_id, false);
                (true, retry_after_ms)
            }
            djinn_runtime::ProviderFailureClass::Failure => {
                app_state
                    .health_tracker
                    .record_failure(creator_scope, model_id);
                (false, None)
            }
            djinn_runtime::ProviderFailureClass::AuthInvalid => {
                if refresh_oauth_credential_after_401(model_id, app_state).await {
                    app_state
                        .health_tracker
                        .record_stall(creator_scope, model_id, false);
                    (true, None)
                } else {
                    app_state
                        .health_tracker
                        .record_stall(creator_scope, model_id, true);
                    surface_credential_revocation(app_state, creator_scope, model_id).await;
                    (true, None)
                }
            }
        };
        app_state.health_tracker.note_task_provider_failure(
            task_id,
            djinn_provider::catalog::health::TaskFailureSignal {
                throttle: is_throttle,
                retry_after_ms,
            },
        );
    }
}

/// Clear budget-park dispatch state in the coordinator when a run ended
/// with a `budget` park reason.
async fn clear_budget_park_dispatch_state(app_state: &AgentContext, task: &Task) {
    match app_state.coordinator().await {
        Some(coordinator) => {
            if let Err(e) = coordinator
                .clear_planned_dispatch_completion(&task.id, "budget_park_planned_completion_clear")
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "supervisor dispatch: failed to clear budget-park dispatch state"
                );
            }
        }
        None => {
            tracing::debug!(
                task_id = %task.short_id,
                "supervisor dispatch: no coordinator handle; budget-park dispatch state clear skipped"
            );
        }
    }
}

/// Route a loop-guard trip to the Planner for intervention when the report
/// outcome is `LoopGuardTripped`.
async fn route_loop_guard_planner_intervention_if_needed(
    app_state: &AgentContext,
    report: &TaskRunReport,
    task: &Task,
    role: &'static str,
) {
    let TaskRunOutcome::LoopGuardTripped {
        kind,
        offending_signature,
        threshold,
        observed,
        turn_span,
        session_id,
    } = &report.outcome
    else {
        return;
    };
    let reason = loop_guard_planner_intervention_reason(
        *kind,
        offending_signature,
        *threshold,
        *observed,
        *turn_span,
        session_id,
    );
    tracing::warn!(
        task_id = %task.short_id,
        guard_kind = ?kind,
        offending_signature = %offending_signature,
        threshold,
        observed,
        turn_start = turn_span.0,
        turn_end = turn_span.1,
        session_id = %session_id,
        "supervisor dispatch: loop guard tripped; routing to Planner intervention"
    );
    match app_state.coordinator().await {
        Some(coordinator) => {
            if let Err(e) = coordinator
                .route_loop_guard_planner_intervention(&task.id, role, &reason)
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "supervisor dispatch: failed to enqueue loop-guard Planner intervention"
                );
            }
        }
        None => {
            tracing::warn!(
                task_id = %task.short_id,
                "supervisor dispatch: no coordinator handle; loop-guard Planner intervention not enqueued"
            );
        }
    }
}

/// Host-side dispatch: resolve task -> build spec -> construct runtime -> drive lifecycle.
///
/// `Ok(())` = terminal outcome (slot treats as `SlotEvent::Free`).
/// `Err` = infra setup failure the runtime can't express via `TaskRunReport`.
pub(super) async fn dispatch_task_runtime(
    task_id: String,
    _project_path: String,
    model_id: String,
    app_state: AgentContext,
    kill: CancellationToken,
    _pause: CancellationToken,
    resume_lifecycle_metadata: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = load_task_or_bail(&task_id, &task_repo).await?;
    // This coordinator-minted opaque identity is the only batch correlation
    // allowed on terminal paths. NULL remains NULL for mixed-version dispatches.
    let dispatch_group_id = resume_lifecycle_metadata
        .as_ref()
        .and_then(|value| serde_json::from_value::<ResumeLifecycleMetadata>(value.clone()).ok())
        .and_then(|metadata| metadata.dispatch_group_id);
    let ctx = DispatchContext::resolve(&task, &app_state).await;
    let flow = resolve_effective_flow(&ctx, &app_state).await;
    let loop_guard_intervention_role = flow
        .role_sequence()
        .first()
        .map(|role| role.as_str())
        .unwrap_or("worker");
    let _trigger = trigger_for_flow(&flow, ctx.has_conflict);
    if matches!(flow, SupervisorFlow::ReviewResume) {
        tracing::info!(
            task_id = %task.short_id,
            branch = %ctx.task_branch,
            "supervisor dispatch: worker output durable on task_branch; \
             resuming at reviewer stage (skipping worker redo)"
        );
    }
    // Any `Err` out of the body below means the dispatch died without a
    // terminal report, so the `pending` attempt rows written for it (the
    // coordinator's dispatch-start row and/or the supervisor's exact-attempt
    // row) would otherwise survive as non-terminal and make the respawn guard
    // defer every future dispatch for this task until the periodic orphan
    // sweep catches them (up to STALE_SWEEP_INTERVAL + threshold late).
    let dispatch_result: anyhow::Result<()> = async {
    let spec_inputs = TaskRunSpecInputs::resolve(
        &task,
        &flow,
        &ctx,
        &app_state,
        &model_id,
        resume_lifecycle_metadata,
    )
    .await?;
    let creator_scope = spec_inputs.created_by_user_id.clone();
    let spec = TaskRunSpec::from(spec_inputs);
    announce_dispatch(&app_state, &spec, &model_id);
    let credentials =
        resolve_credentials(&spec, &app_state, &model_id, creator_scope.clone()).await?;
    let runtime = build_runtime(&app_state, &task, &kill).await?;
    let runtime_kind = runtime_kind();
    let RuntimeExecutionOutcome {
        report_result,
        teardown,
        handshake_timed_out,
        infra_death,
        infra_death_log_tail,
        presession_timeout,
    } = execute_runtime_report_phase(
        runtime.clone(),
        &spec,
        &credentials,
        &task,
        &model_id,
        &app_state,
        &kill,
    )
    .await?;
    if handshake_timed_out {
        apply_handshake_timeout_failover(
            &app_state,
            &task_repo,
            &task,
            &model_id,
            creator_scope.as_deref(),
        )
        .await;
    }
    if let Some(reason) = infra_death.as_deref() {
        finalize_infra_death_session(&task_repo, &task, &app_state, reason).await;
        // Best-effort: persist infra-death log-tail capture on the matching
        // attempt.  This is purely diagnostic enrichment — it does not change
        // the attempt's outcome or prevent a real terminal report from being
        // authoritative.
        if let Some(capture) = &infra_death_log_tail {
            persist_infra_death_on_attempt(&app_state, &task, reason, capture).await;
        }
    }
    if let Some(timeout) = presession_timeout {
        let PreSessionTimeout { step, elapsed_secs } = &timeout;
        tracing::error!(
            task_id = %task.short_id,
            task_run_id = %spec.task_run_id,
            %model_id,
            stage_step = %step,
            elapsed_secs,
            "supervisor dispatch: pre-session stage-init deadline exceeded before first \
             provider turn; failing run fast"
        );
        app_state
            .health_tracker
            .record_stall(creator_scope.as_deref(), &model_id, true);
        app_state.health_tracker.note_task_provider_failure(
            &task.id,
            djinn_provider::catalog::health::TaskFailureSignal {
                throttle: true,
                retry_after_ms: None,
            },
        );
        let _ = task_repo
            .log_activity(
                Some(&task.id),
                "agent-supervisor",
                "system",
                "stage_init_timeout",
                &serde_json::json!({
                    "body": format!(
                        "Stage init hung at step '{step}' — no session / first provider \
                         turn within {elapsed_secs}s (pre-session liveness deadline). \
                         Tore down the Job and failed over off model {model_id}."
                    ),
                    "stage_step": step,
                    "elapsed_secs": elapsed_secs,
                })
                .to_string(),
            )
            .await;
        return Err(anyhow::Error::new(timeout));
    }
    match (report_result, teardown) {
        (Ok(streamed), Ok(teardown_report)) => {
            let report = select_terminal_report(streamed, teardown_report);
            if let TaskRunOutcome::Parked { reason, .. } = &report.outcome
                && let Err(e) = TaskRunOutcomeRepository::new(app_state.db.clone())
                    .record_parked_reason(&report.task_run_id, reason)
                    .await
            {
                tracing::warn!(task_run_id = %report.task_run_id, error = %e, "supervisor dispatch: failed to record exact parked outcome");
            }
            tracing::info!(
                task_id = %task.short_id,
                task_run_id = %report.task_run_id,
                outcome = ?report.outcome,
                stages_completed = ?report.stages_completed,
                runtime = ?runtime_kind,
                "supervisor dispatch: task-run complete"
            );
            persist_loop_guard_activity(&task_repo, &task.id, &report).await;
            terminalize_failed_run_attempt(
                &app_state,
                spec.task_attempt_id.as_deref(),
                dispatch_group_id.as_deref(),
                &task,
                &report,
            )
            .await;
            apply_provider_breaker_feedback(
                &app_state,
                &report,
                &model_id,
                creator_scope.as_deref(),
                &task.id,
            )
            .await;
            if is_budget_park_report(&report) {
                clear_budget_park_dispatch_state(&app_state, &task).await;
            }
            route_loop_guard_planner_intervention_if_needed(
                &app_state,
                &report,
                &task,
                loop_guard_intervention_role,
            )
            .await;
            // Fire-and-forget post-session knowledge extraction.
            if !report.stages_completed.is_empty() {
                let app_state_ext = app_state.clone();
                let task_id_ext = task.id.clone();
                let task_run_id_ext = report.task_run_id.clone();
                let terminal_context_ext = terminal_extraction_context(&report);
                tokio::spawn(async move {
                    crate::actors::slot::session_extraction::run_post_session_extraction(
                        task_id_ext,
                        task_run_id_ext,
                        terminal_context_ext,
                        app_state_ext,
                    )
                    .await;
                });
            }
            Ok(())
        }
        (Err(e), teardown_result) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                teardown_ok = teardown_result.is_ok(),
                runtime = ?runtime_kind,
                "supervisor dispatch: pre-teardown failure"
            );
            Err(e)
        }
        (Ok(_streamed), Err(e)) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                runtime = ?runtime_kind,
                "supervisor dispatch: teardown failure"
            );
            Err(anyhow::anyhow!("runtime.teardown failed: {e}"))
        }
    }
    }
    .await;
    if let Err(error) = &dispatch_result {
        terminalize_dispatch_group_after_dispatch_failure(
            &app_state,
            dispatch_group_id.as_deref(),
            &task,
            error,
        )
        .await;
    }
    dispatch_result
}

/// Terminalize only the supplied dispatch group's pending attempts as
/// `spawn_failed` after a dispatch failure without a terminal report.
/// Mixed-version NULL-group rows are deliberately never batch-correlated.
async fn terminalize_dispatch_group_after_dispatch_failure(
    app_state: &AgentContext,
    dispatch_group_id: Option<&str>,
    task: &Task,
    error: &anyhow::Error,
) {
    let attempt_repo = TaskAttemptRepository::new(app_state.db.clone());
    let Some(group_id) = dispatch_group_id else {
        tracing::debug!(task_id = %task.short_id, "supervisor dispatch: dispatch failure has no group ID; leaving legacy attempts for conservative single-row/orphan handling");
        return;
    };
    let truncated_error: String = format!("{error:#}").chars().take(500).collect();
    let summary = format!("dispatch failed before a terminal report: {truncated_error}");
    let summary_json =
        serde_json::json!({ "recovery_classifier": "dispatch_failure_orphan" }).to_string();
    match attempt_repo
        .terminalize_dispatch_group(
            group_id,
            djinn_core::models::task_attempt::TaskAttemptOutcome::SpawnFailed,
            djinn_db::DispatchGroupTerminalEvidence {
                summary: Some(&summary),
                summary_json: Some(&summary_json),
            },
        )
        .await
    {
        Ok(result) => {
            tracing::info!(task_id = %task.short_id, dispatch_group_id = %group_id, terminalized_attempts = result.updated_attempt_ids.len(), "supervisor dispatch: terminalized pending dispatch group after dispatch failure")
        }
        Err(e) => {
            tracing::warn!(task_id = %task.short_id, dispatch_group_id = %group_id, error = %e, "supervisor dispatch: failed to terminalize pending dispatch group after dispatch failure")
        }
    }
}

/// Everything `dispatch_task_runtime` needs back from the runtime
/// execution/report phase to run its persistence and finalization logic.
struct RuntimeExecutionOutcome {
    report_result: anyhow::Result<Option<TaskRunReport>>,
    teardown: Result<TaskRunReport, djinn_runtime::RuntimeError>,
    handshake_timed_out: bool,
    infra_death: Option<String>,
    infra_death_log_tail: Option<InfraDeathLogTailCapture>,
    presession_timeout: Option<PreSessionTimeout>,
}

/// Outcome of attaching stdio and waiting for the worker's terminal report.
struct TerminalReportAwaitOutcome {
    report_result: anyhow::Result<Option<TaskRunReport>>,
    handshake_timed_out: bool,
    infra_death: Option<String>,
    presession_timeout: Option<PreSessionTimeout>,
}

/// Drive the provider runtime execution phase: prepare, cancellation watcher,
/// stdio attach, terminal-report await (with infra-death and pre-session
/// timeout watching), teardown, orphan reaping, and cargo-target cleanup.
async fn execute_runtime_report_phase(
    runtime: Arc<dyn SessionRuntime>,
    spec: &TaskRunSpec,
    credentials: &ResolvedCredentials,
    task: &Task,
    model_id: &str,
    app_state: &AgentContext,
    kill: &CancellationToken,
) -> anyhow::Result<RuntimeExecutionOutcome> {
    let handle = runtime
        .prepare(spec, credentials)
        .await
        .map_err(|e| anyhow::anyhow!("runtime.prepare failed: {e}"))?;
    let cancel_task = spawn_runtime_cancel_watcher(
        runtime.clone(),
        handle.clone(),
        kill.clone(),
        task.id.clone(),
        model_id.to_string(),
    );
    let await_outcome =
        attach_and_await_terminal_report(runtime.clone(), &handle, app_state, spec, task, kill)
            .await;
    abort_runtime_cancel_watcher(cancel_task).await;
    // Best-effort: capture pod log tail before teardown deletes the Job.
    let infra_death_log_tail = if await_outcome.infra_death.is_some() {
        runtime.capture_infra_death_log_tail(&handle).await
    } else {
        None
    };
    let teardown = runtime.teardown(handle).await;
    let reap_status = select_orphan_reap_status(&await_outcome.presession_timeout, &teardown);
    reap_orphan_task_run(app_state, &spec.task_run_id, reap_status).await;
    teardown_cargo_target_run_dir(app_state, &spec.task_run_id).await;
    Ok(RuntimeExecutionOutcome {
        report_result: await_outcome.report_result,
        teardown,
        handshake_timed_out: await_outcome.handshake_timed_out,
        infra_death: await_outcome.infra_death,
        infra_death_log_tail,
        presession_timeout: await_outcome.presession_timeout,
    })
}

/// Watch the kill token and cancel the runtime run when it fires.
fn spawn_runtime_cancel_watcher(
    runtime: Arc<dyn SessionRuntime>,
    handle: djinn_runtime::RunHandle,
    kill: CancellationToken,
    task_id: String,
    model_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        kill.cancelled().await;
        let span = tracing::info_span!(
            "djinn.slot.kill",
            task_id = %task_id,
            model_id = %model_id,
        );
        async {
            let _ = runtime.cancel(&handle).await;
        }
        .instrument(span)
        .await;
    })
}

/// Abort the cancellation watcher once the run has reached its terminal state.
async fn abort_runtime_cancel_watcher(cancel_task: tokio::task::JoinHandle<()>) {
    cancel_task.abort();
    let _ = cancel_task.await;
}

/// Attach stdio and wait for the authoritative terminal report, racing the
/// runtime's infra-death watcher and tracking pre-session timeouts.
async fn attach_and_await_terminal_report(
    runtime: Arc<dyn SessionRuntime>,
    handle: &djinn_runtime::RunHandle,
    app_state: &AgentContext,
    spec: &TaskRunSpec,
    task: &Task,
    kill: &CancellationToken,
) -> TerminalReportAwaitOutcome {
    let bistream_result = runtime.attach_stdio(handle).await;
    let handshake_timed_out = matches!(
        &bistream_result,
        Err(djinn_runtime::RuntimeError::HandshakeTimeout(_))
    );
    let mut infra_death: Option<String> = None;
    let mut presession_timeout: Option<PreSessionTimeout> = None;
    let report_result: anyhow::Result<Option<TaskRunReport>> = match bistream_result {
        Ok(bistream) => {
            let await_outcome = tokio::select! {
                biased;
                res = await_report_from_stream(
                    bistream,
                    kill,
                    app_state.db.clone(),
                    &spec.task_run_id,
                    &spec.task_id,
                    pre_session_deadline(),
                ) => res,
                reason = runtime.watch_infra_death(handle) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        %reason,
                        runtime = ?runtime_kind(),
                        "supervisor dispatch: worker infra died before terminal report \
                         (OOM / eviction / Job failure); finalizing run as interrupted"
                    );
                    infra_death = Some(reason);
                    Ok(ReportAwait::Report(None))
                }
            };
            match await_outcome {
                Ok(ReportAwait::Report(report)) => Ok(report),
                Ok(ReportAwait::PreSessionTimeout(timeout)) => {
                    presession_timeout = Some(timeout);
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(anyhow::anyhow!("runtime.attach_stdio failed: {e}")),
    };
    TerminalReportAwaitOutcome {
        report_result,
        handshake_timed_out,
        infra_death,
        presession_timeout,
    }
}

/// Pick the status used when reaping an orphaned `task_runs` row after the
/// runtime phase: pre-session timeouts fail fast; otherwise mirror the
/// teardown report's terminal status, defaulting to interrupted.
fn select_orphan_reap_status(
    presession_timeout: &Option<PreSessionTimeout>,
    teardown: &Result<TaskRunReport, djinn_runtime::RuntimeError>,
) -> TaskRunStatus {
    if presession_timeout.is_some() {
        TaskRunStatus::Failed
    } else {
        teardown
            .as_ref()
            .ok()
            .map(report_to_terminal_status)
            .unwrap_or(TaskRunStatus::Interrupted)
    }
}

/// Preflight context: task row, conflict/review state, branches, base flow.
struct DispatchContext<'a> {
    task: &'a Task,
    has_conflict: bool,
    base_branch: String,
    task_branch: String,
    base_flow: SupervisorFlow,
}

impl<'a> DispatchContext<'a> {
    /// Resolve the dispatch context for a task.
    async fn resolve(task: &'a Task, app_state: &'a AgentContext) -> Self {
        let conflict_ctx = conflict_context_for_dispatch(&task.id, app_state).await;
        let has_conflict = conflict_ctx.is_some();
        let has_review_response =
            matches!(task.status.as_str(), "needs_task_review" | "in_task_review");
        let base_branch = default_target_branch(&task.project_id, app_state).await;
        let task_branch = task_branch_name(&task.short_id);
        let base_flow =
            crate::roles::flow_for_task_dispatch(task, has_conflict, has_review_response);
        Self {
            task,
            has_conflict,
            base_branch,
            task_branch,
            base_flow,
        }
    }
}

/// For ReviewResponse, probe mirror for durable output. Conservative on failure.
async fn worker_output_durable(ctx: &DispatchContext<'_>, app_state: &AgentContext) -> bool {
    if !matches!(ctx.base_flow, SupervisorFlow::ReviewResponse) {
        return false;
    }
    let Some(mirror) = app_state.mirror.as_ref() else {
        return false;
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        mirror.branch_ahead_of_base(&ctx.task.project_id, &ctx.task_branch, &ctx.base_branch),
    )
    .await
    {
        Ok(durable) => durable,
        Err(_) => {
            tracing::warn!(
                task_id = %ctx.task.short_id,
                branch = %ctx.task_branch,
                "supervisor dispatch: branch_ahead_of_base durability probe timed out \
                 (>10s); keeping full worker redo (ReviewResponse)"
            );
            false
        }
    }
}

/// Resolve effective supervisor flow; apply ReviewResume when durable.
async fn resolve_effective_flow(
    ctx: &DispatchContext<'_>,
    app_state: &AgentContext,
) -> SupervisorFlow {
    let durable = worker_output_durable(ctx, app_state).await;
    resume_flow(ctx.base_flow, durable)
}

fn trigger_for_flow(flow: &SupervisorFlow, has_conflict: bool) -> TaskRunTrigger {
    if has_conflict {
        TaskRunTrigger::ConflictRetry
    } else if matches!(
        flow,
        SupervisorFlow::ReviewResponse | SupervisorFlow::ReviewResume
    ) {
        TaskRunTrigger::ReviewResponse
    } else {
        TaskRunTrigger::NewTask
    }
}

/// Load a task by id or bail.
async fn load_task_or_bail(task_id: &str, task_repo: &TaskRepository) -> anyhow::Result<Task> {
    match task_repo.get(task_id).await {
        Ok(Some(t)) => Ok(t),
        Ok(None) => {
            anyhow::bail!("supervisor dispatch: task {task_id} not found")
        }
        Err(e) => {
            anyhow::bail!("supervisor dispatch: failed to load task {task_id}: {e}")
        }
    }
}

/// Inputs to TaskRunSpec construction resolved from task row, dispatch context, and repos.
struct TaskRunSpecInputs {
    task_run_id: String,
    task_attempt_id: Option<String>,
    task_id: String,
    project_id: String,
    trigger: TaskRunTrigger,
    base_branch: String,
    task_branch: String,
    flow: SupervisorFlow,
    model_id_per_role: HashMap<RoleKind, String>,
    read_source_project_ids: Vec<String>,
    github_owner: Option<String>,
    github_install_token: Option<String>,
    commit_author_name: Option<String>,
    commit_author_email: Option<String>,
    resume_lifecycle_metadata: Option<ResumeLifecycleMetadata>,
    created_by_user_id: Option<String>,
    is_evidence_spike: bool,
}

impl TaskRunSpecInputs {
    async fn resolve(
        task: &Task,
        flow: &SupervisorFlow,
        ctx: &DispatchContext<'_>,
        app_state: &AgentContext,
        model_id: &str,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> anyhow::Result<Self> {
        let mut model_id_per_role: HashMap<RoleKind, String> = HashMap::new();
        for role in flow.role_sequence() {
            let resolved =
                resolve_role_model_preference(&task.project_id, role.as_str(), app_state)
                    .await
                    .unwrap_or_else(|| model_id.to_string());
            model_id_per_role.insert(*role, resolved);
        }
        let read_source_project_ids = djinn_db::EpicRepository::new(
            app_state.db.clone(),
            app_state.event_bus.clone(),
        )
        .read_sources_for_task(task.epic_id.as_deref())
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "supervisor dispatch: read-source authorization lookup is uncertain for task {}: {error}",
                task.id
            )
        })?;
        let pd_project_repo =
            djinn_db::ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        let github_owner = pd_project_repo
            .get_github_coords(&task.project_id)
            .await
            .ok()
            .flatten()
            .map(|(owner, _repo)| owner);
        let github_install_token = match pd_project_repo.get_installation_id(&task.project_id).await
        {
            Ok(Some(installation_id)) => {
                djinn_provider::github_app::installations::get_installation_token(installation_id)
                    .await
                    .map(|t| t.token)
                    .ok()
            }
            _ => None,
        };
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        let created_by_user_id: Option<String> =
            task_repo.created_by_user_id(&task.id).await.ok().flatten();
        let (commit_author_name, commit_author_email) =
            resolve_commit_author(app_state, created_by_user_id.as_deref()).await;
        let resume_lifecycle_metadata =
            decode_resume_lifecycle_metadata(resume_lifecycle_metadata, &task.id);
        let (dispatch_owner_incarnation_id, dispatch_group_id) =
            validated_dispatch_identity(resume_lifecycle_metadata.as_ref())?;
        let task_run_id = uuid::Uuid::now_v7().to_string();
        let attempt_id = uuid::Uuid::now_v7().to_string();
        // A run must never be created without the identity of the attempt that
        // dispatch allocated for it. Do not turn an allocation failure into an
        // optional ID: that would create an unattributable run.
        let task_attempt_id = TaskAttemptRepository::new(app_state.db.clone())
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &attempt_id,
                task_id: &task.id,
                role: flow
                    .role_sequence()
                    .first()
                    .map(|role| role.as_str())
                    .unwrap_or("worker"),
                dispatch_key: &format!("task-run:{task_run_id}"),
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: dispatch_owner_incarnation_id.as_deref(),
                dispatch_group_id: dispatch_group_id.as_deref(),
            })
            .await
            .map(|attempt| attempt.id)
            .map_err(|e| {
                anyhow::anyhow!(
                    "supervisor dispatch: failed to allocate exact attempt for task {}: {e}",
                    task.id
                )
            })?;
        let is_evidence_spike = djinn_core::models::task::is_evidence_spike(&task.labels);
        Ok(Self {
            task_run_id,
            task_attempt_id: Some(task_attempt_id),
            task_id: task.id.clone(),
            project_id: task.project_id.clone(),
            trigger: trigger_for_flow(flow, ctx.has_conflict),
            base_branch: ctx.base_branch.clone(),
            task_branch: ctx.task_branch.clone(),
            flow: *flow,
            model_id_per_role,
            read_source_project_ids,
            github_owner,
            github_install_token,
            commit_author_name,
            commit_author_email,
            resume_lifecycle_metadata,
            created_by_user_id,
            is_evidence_spike,
        })
    }
}

impl From<TaskRunSpecInputs> for TaskRunSpec {
    fn from(inputs: TaskRunSpecInputs) -> Self {
        Self {
            task_run_id: inputs.task_run_id,
            task_attempt_id: inputs.task_attempt_id,
            task_id: inputs.task_id,
            project_id: inputs.project_id,
            trigger: inputs.trigger,
            base_branch: inputs.base_branch,
            task_branch: inputs.task_branch,
            flow: inputs.flow,
            model_id_per_role: inputs.model_id_per_role,
            read_source_project_ids: inputs.read_source_project_ids,
            github_owner: inputs.github_owner,
            github_install_token: inputs.github_install_token,
            commit_author_name: inputs.commit_author_name,
            commit_author_email: inputs.commit_author_email,
            resume_lifecycle_metadata: inputs.resume_lifecycle_metadata,
            is_evidence_spike: inputs.is_evidence_spike,
        }
    }
}

/// Resolve commit-author identity for Vercel compatibility.
async fn resolve_commit_author(
    app_state: &AgentContext,
    created_by_user_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    match created_by_user_id {
        Some(uid) => match djinn_db::UserRepository::new(app_state.db.clone())
            .get_by_id(uid)
            .await
        {
            Ok(Some(user)) => (
                Some(
                    user.github_name
                        .clone()
                        .unwrap_or_else(|| user.github_login.clone()),
                ),
                Some(format!(
                    "{}+{}@users.noreply.github.com",
                    user.github_id, user.github_login
                )),
            ),
            _ => (None, None),
        },
        None => (None, None),
    }
}

fn decode_resume_lifecycle_metadata(
    value: Option<serde_json::Value>,
    task_id: &str,
) -> Option<ResumeLifecycleMetadata> {
    match value {
        Some(value) => match serde_json::from_value::<ResumeLifecycleMetadata>(value) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %err,
                    "dispatch_task_runtime: failed to decode resume_lifecycle_metadata blob; \
                     proceeding without resume metadata"
                );
                None
            }
        },
        None => None,
    }
}

/// Identity is coordinator-minted and remains opaque downstream. Omitted
/// mixed-version values are NULL; malformed present values fail before persistence.
fn validated_dispatch_identity(
    metadata: Option<&ResumeLifecycleMetadata>,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let owner = metadata.and_then(|metadata| metadata.dispatch_owner_incarnation_id.clone());
    let group = metadata.and_then(|metadata| metadata.dispatch_group_id.clone());
    for (field, value) in [
        ("dispatch_owner_incarnation_id", owner.as_deref()),
        ("dispatch_group_id", group.as_deref()),
    ] {
        if let Some(value) = value {
            uuid::Uuid::parse_str(value)
                .map_err(|_| anyhow::anyhow!("{field} must be a UUID when present"))?;
        }
    }
    Ok((owner, group))
}

fn announce_dispatch(app_state: &AgentContext, spec: &TaskRunSpec, model_id: &str) {
    let agent_type = spec
        .flow
        .role_sequence()
        .first()
        .map(|role| role.as_str())
        .unwrap_or("worker");
    app_state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::session_dispatched(
            &spec.project_id,
            &spec.task_id,
            model_id,
            agent_type,
        ));
}

/// Resolve per-role provider credentials scoped to task creator.
async fn resolve_credentials(
    spec: &TaskRunSpec,
    app_state: &AgentContext,
    dispatch_model_id: &str,
    created_by_user_id: Option<String>,
) -> anyhow::Result<ResolvedCredentials> {
    let mut credentials = ResolvedCredentials::default();
    let resolve_creds = async {
        for role in spec.flow.role_sequence() {
            let model_id = spec
                .model_id_per_role
                .get(role)
                .cloned()
                .unwrap_or_else(|| dispatch_model_id.to_string());
            let (provider_id, model_name) = parse_model_id(&model_id).map_err(|e| {
                anyhow::anyhow!(
                    "supervisor dispatch: cannot parse model id `{model_id}` for role {role:?}: {e}"
                )
            })?;
            let cred = load_provider_credential(&provider_id, app_state)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "supervisor dispatch: load_provider_credential({provider_id}) for role {role:?}: {e}"
                    )
                })?;
            // Build a RestampTarget from catalog metadata so OAuth credential
            // restamp resolves model-dependent defaults (reasoning_effort,
            // max_tokens_default, etc.) for the target model.
            let restamp_target =
                build_restamp_target(&provider_id, &model_name, 0, &app_state.catalog);
            credentials.insert(*role, cred.with_model_id(&restamp_target).to_serializable());
        }
        Ok::<(), anyhow::Error>(())
    };
    djinn_core::auth_context::SESSION_USER_ID
        .scope(created_by_user_id, resolve_creds)
        .await?;
    Ok(credentials)
}

/// Construct the SessionRuntime for this dispatch (Kubernetes or Test).
async fn build_runtime(
    app_state: &AgentContext,
    task: &Task,
    kill: &CancellationToken,
) -> anyhow::Result<Arc<dyn SessionRuntime>> {
    let mirror = match app_state.mirror.as_ref() {
        Some(m) => m.clone(),
        None => {
            anyhow::bail!(
                "supervisor dispatch: AgentContext has no MirrorManager configured — \
                 cannot run supervisor-driven task-run for {}",
                task.short_id
            );
        }
    };
    let runtime: Arc<dyn SessionRuntime> = match runtime_kind() {
        RuntimeKind::Kubernetes => {
            let config = djinn_k8s::KubernetesConfig::from_env();
            let registry = match app_state.rpc_registry.as_ref() {
                Some(reg) => reg.clone(),
                None => {
                    anyhow::bail!(
                        "supervisor dispatch: AgentContext has no ConnectionRegistry \
                         — the djinn-server boot path must plumb `rpc_registry` into \
                         `AppState::agent_context()` before the Kubernetes runtime can \
                         be constructed"
                    );
                }
            };
            match djinn_k8s::KubernetesRuntime::with_db(config, registry, app_state.db.clone())
                .await
            {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    anyhow::bail!(
                        "supervisor dispatch: failed to construct KubernetesRuntime \
                         (is a kubeconfig available?): {e}"
                    );
                }
            }
        }
        RuntimeKind::Test => {
            let services = services_for_agent_context(app_state.clone(), kill.clone());
            let runner = SupervisorTaskRunner::new(mirror.clone(), services);
            Arc::new(TestRuntime::new(runner))
        }
    };
    Ok(runtime)
}

/// Upgrade ReviewResponse to ReviewResume when durable.
fn resume_flow(base_flow: SupervisorFlow, worker_output_durable: bool) -> SupervisorFlow {
    if matches!(base_flow, SupervisorFlow::ReviewResponse) && worker_output_durable {
        SupervisorFlow::ReviewResume
    } else {
        base_flow
    }
}

fn provider_failure_class_for_report(report: &TaskRunReport) -> Option<ProviderFailureClass> {
    match &report.outcome {
        TaskRunOutcome::Failed {
            provider_failure: Some(class),
            ..
        } => Some(*class),
        _ => None,
    }
}

/// Translate the authoritative runtime report for extraction without inferring
/// a review verdict that the report does not contain.
fn terminal_extraction_context(report: &TaskRunReport) -> TerminalExtractionContext {
    let outcome = match &report.outcome {
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        | TaskRunOutcome::WorkerSubmitted => TerminalExtractionOutcome::Completed,
        TaskRunOutcome::Parked { reason, .. } => TerminalExtractionOutcome::Parked {
            // Park reasons are already terminal classifications (for example,
            // `ci_failure` versus `acceptance_criteria`), so preserve them
            // verbatim rather than collapsing distinct failures.
            classification: reason.clone(),
            reason: Some(reason.clone()),
        },
        TaskRunOutcome::Escalated { reason } => TerminalExtractionOutcome::Parked {
            classification: "escalated".to_string(),
            reason: Some(reason.clone()),
        },
        TaskRunOutcome::Failed { stage, reason, .. } => TerminalExtractionOutcome::Failed {
            classification: stage.clone(),
            reason: Some(reason.clone()),
        },
        TaskRunOutcome::LoopGuardTripped {
            kind,
            offending_signature,
            ..
        } => TerminalExtractionOutcome::Failed {
            classification: format!("loop_guard_{}", loop_guard_kind_label(*kind)),
            reason: Some(offending_signature.clone()),
        },
        TaskRunOutcome::Interrupted => TerminalExtractionOutcome::Failed {
            classification: "interrupted".to_string(),
            reason: None,
        },
        TaskRunOutcome::EnvironmentalNonAttempt { reason } => TerminalExtractionOutcome::Failed {
            classification: "environmental_non_attempt".to_string(),
            reason: Some(reason.clone()),
        },
    };

    // TaskRunReport has no typed review-decision field. In particular, a park
    // reason such as `acceptance_criteria` must not be promoted to a synthetic
    // reviewer rejection; only a future explicit terminal verdict may do so.
    TerminalExtractionContext {
        outcome,
        review_decision: None,
    }
}

fn is_budget_park_report(report: &TaskRunReport) -> bool {
    matches!(
        &report.outcome,
        TaskRunOutcome::Parked { reason, .. } if reason == "budget"
    )
}

fn terminal_report_feeds_model_success(report: &TaskRunReport) -> bool {
    !report.stages_completed.is_empty()
        && matches!(report_to_terminal_status(report), TaskRunStatus::Completed)
        && !is_budget_park_report(report)
}

async fn persist_loop_guard_activity(
    task_repo: &TaskRepository,
    task_id: &str,
    report: &TaskRunReport,
) {
    let TaskRunOutcome::LoopGuardTripped {
        kind,
        offending_signature,
        threshold,
        observed,
        turn_span,
        session_id,
    } = &report.outcome
    else {
        return;
    };
    let details = serde_json::json!({
        "kind": loop_guard_kind_label(*kind),
        "offending_signature": offending_signature,
        "threshold": threshold,
        "observed": observed,
        "turn_span": {
            "start": turn_span.0,
            "end": turn_span.1,
        },
        "session_id": session_id,
        "task_run_id": report.task_run_id,
    });
    let payload = serde_json::json!({
        "kind": "loop_guard_tripped",
        "details": details,
        "body": format!(
            "Reply-loop guard `{}` tripped in session `{}` on turns {}..={}: `{}` \
             (observed {}/{})",
            loop_guard_kind_label(*kind),
            session_id,
            turn_span.0,
            turn_span.1,
            offending_signature,
            observed,
            threshold,
        ),
    })
    .to_string();
    if let Err(e) = task_repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "system",
            "loop_guard_tripped",
            &payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "supervisor dispatch: failed to persist loop_guard_tripped activity"
        );
    }
}

fn loop_guard_kind_label(kind: LoopGuardKind) -> &'static str {
    match kind {
        LoopGuardKind::IdenticalToolFailure => "identical_tool_failure",
        LoopGuardKind::PermissionDenial => "permission_denial",
        LoopGuardKind::IdenticalOutput => "identical_output",
        LoopGuardKind::ConsecutiveFailures => "consecutive_failures",
    }
}

fn loop_guard_planner_intervention_reason(
    kind: LoopGuardKind,
    offending_signature: &str,
    threshold: u32,
    observed: u32,
    turn_span: (u32, u32),
    session_id: &str,
) -> String {
    format!(
        "Reply-loop guard `{}` tripped in session `{}`: offending_signature=`{}`, \
         threshold={}, observed={}, turn_span={}..={}. The run completed in a \
         degenerate loop rather than failing to dispatch; do not re-dispatch the \
         identical worker attempt. Decide how to unstick this: DECOMPOSE into \
         focused subtasks, RESCOPE/clarify the acceptance criteria and re-dispatch, \
         or CLOSE if the work is moot/duplicate/already-done.",
        loop_guard_kind_label(kind),
        session_id,
        offending_signature,
        threshold,
        observed,
        turn_span.0,
        turn_span.1,
    )
}

fn report_to_terminal_status(report: &TaskRunReport) -> TaskRunStatus {
    match &report.outcome {
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        // The worker stage succeeded and handed off to review — the task-run
        // completed cleanly.
        | TaskRunOutcome::WorkerSubmitted
        | TaskRunOutcome::Parked { .. }
        | TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
        TaskRunOutcome::Failed { .. } | TaskRunOutcome::LoopGuardTripped { .. } => {
            TaskRunStatus::Failed
        }
        TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
        // Environmental non-attempt: no session/attempt was created.
        // Map to Completed so the run is terminal without triggering model
        // breaker failures.  `terminal_report_feeds_model_success` returns
        // false because `stages_completed` is always empty for this outcome,
        // so no quality/arbiter/park penalties are applied.
        TaskRunOutcome::EnvironmentalNonAttempt { .. } => TaskRunStatus::Completed,
    }
}

fn select_terminal_report(
    streamed: Option<TaskRunReport>,
    teardown: TaskRunReport,
) -> TaskRunReport {
    streamed.unwrap_or(teardown)
}

async fn reap_orphan_task_run(
    app_state: &AgentContext,
    task_run_id: &str,
    terminal_status: TaskRunStatus,
) {
    let repo = TaskRunRepository::new(app_state.db.clone());
    match repo.get(task_run_id).await {
        Ok(Some(run)) if !matches!(run.status.as_str(), "completed" | "failed" | "interrupted") => {
            if let Err(e) = repo.update_status(task_run_id, terminal_status).await {
                tracing::warn!(task_run_id, error = %e, "supervisor dispatch: failed to reap exact orphan task_run row");
                return;
            }
            if let Err(e) = TaskRunOutcomeRepository::new(app_state.db.clone())
                .record_parked_reason(task_run_id, "orphaned")
                .await
            {
                tracing::warn!(task_run_id, error = %e, "supervisor dispatch: failed to record exact orphan parked reason");
            }
            tracing::warn!(task_run_id, status = %terminal_status, "supervisor dispatch: reaped orphan task_run row (in-pod supervisor never sent terminal RPC)");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(task_run_id, error = %e, "supervisor dispatch: failed to load exact orphan task_run row")
        }
    }
}

async fn teardown_cargo_target_run_dir(app_state: &AgentContext, task_run_id: &str) {
    let root = app_state
        .cargo_target_runs_root
        .clone()
        .unwrap_or_else(djinn_core::paths::cargo_target_runs_root);
    let id = task_run_id.to_string();
    let log_root = root.clone();
    let log_id = id.clone();
    match tokio::task::spawn_blocking(move || {
        djinn_core::cargo_target_runs::teardown_run_dir(&root, &id)
    })
    .await
    {
        Ok(Ok(result)) => {
            if result.removed {
                tracing::info!(
                    task_run_id = %log_id,
                    root = %log_root.display(),
                    cleanup_outcome = result.outcome(),
                    removed_count = result.removed_count(),
                    "supervisor dispatch: host teardown removed orphaned cargo target run-dir"
                );
            } else {
                tracing::debug!(
                    task_run_id = %log_id,
                    root = %log_root.display(),
                    cleanup_outcome = result.outcome(),
                    "supervisor dispatch: cargo target run-dir already absent at host teardown"
                );
            }
        }
        Ok(Err(e)) => tracing::warn!(
            task_run_id = %log_id,
            root = %log_root.display(),
            error = %e,
            cleanup_outcome = "failed",
            "supervisor dispatch: host teardown failed to remove cargo target run-dir"
        ),
        Err(e) => tracing::warn!(
            task_run_id = %log_id,
            root = %log_root.display(),
            error = %e,
            cleanup_outcome = "failed",
            "supervisor dispatch: host teardown task join failed"
        ),
    }
}

/// Drain a BiStream until the terminal Report frame, returning the TaskRunReport.
async fn await_report_from_stream(
    mut stream: BiStream,
    kill: &CancellationToken,
    db: djinn_db::Database,
    task_run_id: &str,
    task_id: &str,
    pre_session_deadline: std::time::Duration,
) -> anyhow::Result<ReportAwait> {
    let started = tokio::time::Instant::now();
    let deadline = tokio::time::sleep(pre_session_deadline);
    tokio::pin!(deadline);
    let mut last_step = PRE_SESSION_INITIAL_STEP.to_string();
    let mut session_reached = false;
    loop {
        tokio::select! {
            biased;
            _ = kill.cancelled() => {
                let rpc_span = supervisor_rpc_span("kill", task_run_id, task_id);
                async move {
                    tracing::debug!(
                        op = "kill",
                        session_id = %task_run_id,
                        task_id = %task_id,
                        "supervisor dispatch: kill fired while awaiting terminal report; \
                         proceeding to teardown"
                    );
                }
                .instrument(rpc_span)
                .await;
                return Ok(ReportAwait::Report(None));
            }
            _ = &mut deadline, if !session_reached => {
                let session_repo =
                    djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::new(|_| {}));
                match session_repo.exists_for_task_run(task_run_id).await {
                    Ok(true) => {
                        session_reached = true;
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task_id,
                            task_run_id = %task_run_id,
                            error = %e,
                            "supervisor dispatch: pre-session deadline DB check failed; \
                             disarming watchdog (falling back to coarse reapers)"
                        );
                        session_reached = true;
                        continue;
                    }
                }
                let elapsed_secs = started.elapsed().as_secs();
                tracing::warn!(
                    task_id = %task_id,
                    task_run_id = %task_run_id,
                    stage_step = %last_step,
                    elapsed_secs,
                    "supervisor dispatch: pre-session liveness deadline breached before first turn"
                );
                return Ok(ReportAwait::PreSessionTimeout(PreSessionTimeout {
                    step: last_step.clone(),
                    elapsed_secs,
                }));
            }
            frame = stream.events_rx.recv() => {
                match frame {
                    Some(StreamEvent::Report(report)) => {
                        let rpc_span = supervisor_rpc_span("terminal_report", task_run_id, task_id);
                        let report_task_run_id = report.task_run_id.clone();
                        async move {
                            tracing::info!(
                                event = "supervisor.rpc.terminal_report",
                                op = "terminal_report",
                                session_id = %task_run_id,
                                task_id = %task_id,
                                task_run_id = %report_task_run_id,
                            );
                        }
                        .instrument(rpc_span)
                        .await;
                        return Ok(ReportAwait::Report(Some(report)));
                    }
                    Some(StreamEvent::StageStep { step }) => {
                        // means the first turn is reached — disarm the deadline.
                        if step == djinn_runtime::STAGE_STEP_FIRST_TURN {
                            session_reached = true;
                        }
                        last_step = step;
                    }
                    Some(
                        StreamEvent::AssistantDelta { .. }
                        | StreamEvent::ToolCall { .. }
                        | StreamEvent::FinalizePayload { .. }
                        | StreamEvent::StageOutcome { .. },
                    ) => {
                        session_reached = true;
                    }
                    // path persists state as a side effect; the caller uses the
                    // teardown stub for the terminal status.
                    None => return Ok(ReportAwait::Report(None)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_runtime::RoleKind;
    use std::collections::HashMap;
    use std::sync::{Arc as StdArc, Mutex as StdMutex, OnceLock};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{Layer, registry::LookupSpan};
    #[derive(Clone, Debug, Default)]
    struct RecordedSpan {
        name: String,
        fields: HashMap<String, String>,
    }
    #[derive(Clone, Default)]
    struct RecordingLayer {
        spans: StdArc<StdMutex<Vec<RecordedSpan>>>,
    }
    impl RecordingLayer {
        fn spans(&self) -> Vec<RecordedSpan> {
            self.spans.lock().expect("recorded spans mutex").clone()
        }
    }
    #[derive(Default)]
    struct FieldRecorder {
        fields: HashMap<String, String>,
    }
    impl Visit for FieldRecorder {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields.insert(
                field.name().to_owned(),
                format!("{value:?}").trim_matches('\"').to_owned(),
            );
        }
    }
    impl<S> Layer<S> for RecordingLayer
    where
        S: tracing::Subscriber,
        S: for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::Id,
            ctx: Context<'_, S>,
        ) {
            let mut recorder = FieldRecorder::default();
            attrs.record(&mut recorder);
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(RecordedSpan {
                    name: attrs.metadata().name().to_owned(),
                    fields: recorder.fields,
                });
            }
        }
        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            ctx: Context<'_, S>,
        ) {
            if let Some(span) = ctx.span(id) {
                let mut recorder = FieldRecorder::default();
                values.record(&mut recorder);
                if let Some(recorded) = span.extensions_mut().get_mut::<RecordedSpan>() {
                    recorded.fields.extend(recorder.fields);
                }
            }
        }
        fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(&id)
                && let Some(recorded) = span.extensions().get::<RecordedSpan>()
            {
                self.spans
                    .lock()
                    .expect("recorded spans mutex")
                    .push(recorded.clone());
            }
        }
    }
    async fn tracing_lock() -> tokio::sync::OwnedMutexGuard<()> {
        static LOCK: OnceLock<StdArc<tokio::sync::Mutex<()>>> = OnceLock::new();
        LOCK.get_or_init(|| StdArc::new(tokio::sync::Mutex::new(())))
            .clone()
            .lock_owned()
            .await
    }
    fn report(id: &str, stages: Vec<RoleKind>, outcome: TaskRunOutcome) -> TaskRunReport {
        TaskRunReport {
            task_run_id: id.to_string(),
            outcome,
            stages_completed: stages,
        }
    }

    #[test]
    fn terminal_extraction_context_distinguishes_completion_ci_and_ac_rejection() {
        let completed = terminal_extraction_context(&report(
            "completed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::PrOpened {
                url: "https://example.test/pr/1".into(),
                sha: "deadbeef".into(),
            },
        ));
        assert_eq!(completed.outcome, TerminalExtractionOutcome::Completed);
        assert_eq!(completed.review_decision, None);

        let ci_failure = terminal_extraction_context(&report(
            "ci-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Failed {
                stage: "ci".into(),
                reason: "tests failed".into(),
                provider_failure: None,
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
        ));
        assert_eq!(
            ci_failure.outcome,
            TerminalExtractionOutcome::Failed {
                classification: "ci".to_string(),
                reason: Some("tests failed".to_string()),
            }
        );
        assert_eq!(ci_failure.review_decision, None);

        let ac_rejection = terminal_extraction_context(&report(
            "ac-run",
            vec![RoleKind::Reviewer],
            TaskRunOutcome::Parked {
                reason: "acceptance_criteria".into(),
                wind_down_ignored: false,
                session_id: "review-session".into(),
                tokens_in: 100,
                tokens_out: 10,
            },
        ));
        assert_eq!(
            ac_rejection.outcome,
            TerminalExtractionOutcome::Parked {
                classification: "acceptance_criteria".to_string(),
                reason: Some("acceptance_criteria".to_string()),
            }
        );
        assert_eq!(
            ac_rejection.review_decision, None,
            "the report does not carry a review verdict, so mapping must not invent one"
        );
        assert_ne!(ci_failure.outcome, ac_rejection.outcome);
    }
    #[test]
    fn terminal_extraction_context_preserves_park_reason_without_review_inference() {
        let report = report(
            "parked-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Parked {
                reason: "acceptance_criteria".into(),
                wind_down_ignored: false,
                session_id: "session-parked".into(),
                tokens_in: 100,
                tokens_out: 10,
            },
        );
        assert_eq!(
            terminal_extraction_context(&report),
            TerminalExtractionContext {
                outcome: TerminalExtractionOutcome::Parked {
                    classification: "acceptance_criteria".to_string(),
                    reason: Some("acceptance_criteria".to_string()),
                },
                review_decision: None,
            },
            "a runtime report must preserve its park reason and never invent a review verdict"
        );
    }

    #[test]
    fn planned_terminal_outcomes_have_no_provider_breaker_signal() {
        let guard_report = report(
            "guard-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::LoopGuardTripped {
                kind: LoopGuardKind::IdenticalToolFailure,
                offending_signature: "shell:cargo-test".into(),
                threshold: 3,
                observed: 4,
                turn_span: (7, 12),
                session_id: "session-123".into(),
            },
        );
        assert_eq!(
            provider_failure_class_for_report(&guard_report),
            None,
            "loop-guard trips must not feed the provider breaker"
        );
        let ordinary_completed = report(
            "closed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Closed {
                reason: "done".into(),
            },
        );
        assert!(
            terminal_report_feeds_model_success(&ordinary_completed),
            "ordinary completed runs still record model-health success"
        );
        for (outcome, label) in [
            (
                TaskRunOutcome::Parked {
                    reason: "budget".into(),
                    wind_down_ignored: false,
                    session_id: "session-budget-summary".into(),
                    tokens_in: 4_096,
                    tokens_out: 512,
                },
                "successful-summary budget parks",
            ),
            (
                TaskRunOutcome::Parked {
                    reason: "budget".into(),
                    wind_down_ignored: true,
                    session_id: "session-budget-ignored".into(),
                    tokens_in: 4_096,
                    tokens_out: 12,
                },
                "ignored-wind-down budget parks",
            ),
        ] {
            let budget_report = report("budget-run", vec![RoleKind::Worker], outcome);
            assert_eq!(
                report_to_terminal_status(&budget_report),
                TaskRunStatus::Completed,
                "{label} are planned completed lifecycle endings"
            );
            assert_eq!(
                provider_failure_class_for_report(&budget_report),
                None,
                "{label} must not feed provider breaker failure accounting"
            );
            assert!(
                !terminal_report_feeds_model_success(&budget_report),
                "{label} must not feed model-health success accounting either"
            );
        }
        let failed_report = report(
            "failed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Failed {
                stage: "worker".into(),
                reason: "provider rejected request".into(),
                provider_failure: Some(ProviderFailureClass::Failure),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
        );
        assert_eq!(
            provider_failure_class_for_report(&failed_report),
            Some(ProviderFailureClass::Failure),
            "typed provider failures still feed the breaker"
        );
    }
    #[test]
    fn loop_guard_reason_names_full_trip_payload() {
        let reason = loop_guard_planner_intervention_reason(
            LoopGuardKind::IdenticalToolFailure,
            "shell:cargo-test",
            3,
            4,
            (7, 12),
            "session-123",
        );
        for expected in [
            "identical_tool_failure",
            "shell:cargo-test",
            "threshold=3",
            "observed=4",
            "turn_span=7..=12",
            "session-123",
            "do not re-dispatch the identical worker attempt",
        ] {
            assert!(
                reason.contains(expected),
                "reason must include `{expected}`; got {reason}"
            );
        }
    }
    #[test]
    fn streamed_report_wins_over_teardown_stub() {
        let streamed = report(
            "id-B-persisted",
            vec![RoleKind::Worker, RoleKind::Reviewer],
            TaskRunOutcome::PrOpened {
                url: "https://example/pr/1".into(),
                sha: "deadbeef".into(),
            },
        );
        let teardown_stub = report("id-A-transport", vec![], TaskRunOutcome::Interrupted);
        let chosen = select_terminal_report(Some(streamed), teardown_stub);
        assert_eq!(
            chosen.task_run_id, "id-B-persisted",
            "extraction id must come from the streamed report, not the teardown stub"
        );
        assert!(
            !chosen.stages_completed.is_empty(),
            "real stages must survive so the extraction gate opens"
        );
    }
    #[test]
    fn teardown_stub_used_when_no_streamed_report() {
        let teardown_stub = report("id-A-transport", vec![], TaskRunOutcome::Interrupted);
        let chosen = select_terminal_report(None, teardown_stub);
        assert_eq!(chosen.task_run_id, "id-A-transport");
        assert!(chosen.stages_completed.is_empty());
    }
    #[test]
    fn resume_flow_upgrades_review_response_to_reviewer_only_when_durable() {
        // the mirror task_branch we must resume at the reviewer, NOT redo the
        assert_eq!(
            resume_flow(SupervisorFlow::ReviewResponse, true),
            SupervisorFlow::ReviewResume
        );
        assert_eq!(
            SupervisorFlow::ReviewResume.role_sequence(),
            &[djinn_runtime::RoleKind::Reviewer],
            "ReviewResume must skip the worker stage"
        );
    }
    #[test]
    fn resume_flow_keeps_review_response_when_output_not_durable() {
        assert_eq!(
            resume_flow(SupervisorFlow::ReviewResponse, false),
            SupervisorFlow::ReviewResponse
        );
    }
    #[test]
    fn resume_flow_leaves_non_review_response_flows_untouched() {
        for flow in [
            SupervisorFlow::NewTask,
            SupervisorFlow::ConflictRetry,
            SupervisorFlow::Spike,
            SupervisorFlow::Planning,
            SupervisorFlow::Lead,
        ] {
            assert_eq!(resume_flow(flow, true), flow);
            assert_eq!(resume_flow(flow, false), flow);
        }
    }
    fn lazy_db() -> djinn_db::Database {
        djinn_db::Database::open_in_memory().expect("lazy in-memory db handle")
    }
    fn no_deadline() -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
    fn expect_report(outcome: ReportAwait) -> Option<TaskRunReport> {
        match outcome {
            ReportAwait::Report(report) => report,
            ReportAwait::PreSessionTimeout(t) => panic!("unexpected pre-session timeout: {t:?}"),
        }
    }
    #[tokio::test]
    async fn await_report_returns_streamed_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        let streamed = report(
            "id-B",
            vec![RoleKind::Worker],
            TaskRunOutcome::Closed {
                reason: "done".into(),
            },
        );
        events_tx
            .send(StreamEvent::Report(streamed))
            .await
            .expect("send report");
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "session-id-b",
                "task-id-b",
                no_deadline(),
            )
            .await
            .expect("await ok"),
        );
        assert_eq!(got.expect("some report").task_run_id, "id-B");
    }
    #[tokio::test]
    async fn supervisor_rpc_terminal_report_span_records_fields() {
        let _tracing_guard = tracing_lock().await;
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::Report(report(
                "run-terminal",
                vec![RoleKind::Worker],
                TaskRunOutcome::Interrupted,
            )))
            .await
            .unwrap();
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "session-terminal",
                "task-terminal",
                no_deadline(),
            )
            .await
            .expect("await ok"),
        );
        assert!(got.is_some());
        let span = layer
            .spans()
            .into_iter()
            .find(|span| span.name == "djinn.supervisor.rpc")
            .expect("djinn.supervisor.rpc span recorded");
        assert_eq!(
            span.fields.get("op").map(String::as_str),
            Some("terminal_report")
        );
        assert_eq!(
            span.fields.get("session_id").map(String::as_str),
            Some("session-terminal")
        );
        assert_eq!(
            span.fields.get("task_id").map(String::as_str),
            Some("task-terminal")
        );
    }
    #[tokio::test]
    async fn await_report_drops_non_terminal_frames_then_returns_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::AssistantDelta {
                session_id: "s1".into(),
                text: "thinking".into(),
            })
            .await
            .unwrap();
        events_tx
            .send(StreamEvent::Report(report(
                "id-B",
                vec![RoleKind::Worker],
                TaskRunOutcome::Interrupted,
            )))
            .await
            .unwrap();
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "session-id-b",
                "task-id-b",
                no_deadline(),
            )
            .await
            .expect("await ok"),
        );
        assert_eq!(got.expect("some report").task_run_id, "id-B");
    }
    #[tokio::test]
    async fn await_report_returns_none_when_kill_fires() {
        let (bistream, _events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        kill.cancel();
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "session-kill",
                "task-kill",
                no_deadline(),
            )
            .await
            .expect("await ok"),
        );
        assert!(got.is_none());
    }
    #[tokio::test]
    async fn supervisor_rpc_kill_span_records_fields() {
        let _tracing_guard = tracing_lock().await;
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (bistream, _events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        kill.cancel();
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "session-kill",
                "task-kill",
                no_deadline(),
            )
            .await
            .expect("await ok"),
        );
        assert!(got.is_none());
        let span = layer
            .spans()
            .into_iter()
            .find(|span| span.name == "djinn.supervisor.rpc")
            .expect("djinn.supervisor.rpc kill span recorded");
        assert_eq!(span.fields.get("op").map(String::as_str), Some("kill"));
        assert_eq!(
            span.fields.get("session_id").map(String::as_str),
            Some("session-kill")
        );
        assert_eq!(
            span.fields.get("task_id").map(String::as_str),
            Some("task-kill")
        );
    }
    #[tokio::test]
    async fn await_report_returns_none_when_channel_closes_without_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        drop(events_tx);
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "session-closed",
                "task-closed",
                no_deadline(),
            )
            .await
            .expect("await ok"),
        );
        assert!(got.is_none());
    }
    #[tokio::test]
    async fn await_report_pre_session_deadline_fires_naming_last_step() {
        let db = lazy_db();
        db.ensure_initialized().await.expect("schema ready");
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::StageStep {
                step: djinn_runtime::stage_step::WORKSPACE_ATTACH.to_string(),
            })
            .await
            .unwrap();
        events_tx
            .send(StreamEvent::StageStep {
                step: djinn_runtime::stage_step::CONTEXT_BUILD.to_string(),
            })
            .await
            .unwrap();
        let outcome = await_report_from_stream(
            bistream,
            &kill,
            db,
            "run-hang-no-session",
            "task-hang",
            std::time::Duration::from_millis(150),
        )
        .await
        .expect("await ok");
        match outcome {
            ReportAwait::PreSessionTimeout(t) => {
                assert_eq!(
                    t.step,
                    djinn_runtime::stage_step::CONTEXT_BUILD,
                    "timeout names the last stage step reached"
                );
            }
            ReportAwait::Report(_) => panic!("expected pre-session timeout, got a report"),
        }
        drop(events_tx);
    }
    #[tokio::test]
    async fn await_report_first_turn_marker_disarms_tiny_deadline() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::StageStep {
                step: djinn_runtime::STAGE_STEP_FIRST_TURN.to_string(),
            })
            .await
            .unwrap();
        events_tx
            .send(StreamEvent::Report(report(
                "run-live",
                vec![RoleKind::Worker],
                TaskRunOutcome::Closed {
                    reason: "done".into(),
                },
            )))
            .await
            .unwrap();
        let got = expect_report(
            await_report_from_stream(
                bistream,
                &kill,
                lazy_db(),
                "run-live",
                "task-live",
                std::time::Duration::from_millis(50),
            )
            .await
            .expect("await ok"),
        );
        assert_eq!(
            got.expect("report after first turn").task_run_id,
            "run-live"
        );
    }
    #[test]
    fn environmental_non_attempt_has_no_provider_breaker_signal() {
        let env_report = report(
            "env-run",
            vec![],
            TaskRunOutcome::EnvironmentalNonAttempt {
                reason: "pre_task_failed".into(),
            },
        );
        assert_eq!(
            report_to_terminal_status(&env_report),
            TaskRunStatus::Completed,
            "environmental non-attempt maps to Completed (terminal, no penalty)"
        );
        assert_eq!(
            provider_failure_class_for_report(&env_report),
            None,
            "environmental non-attempt must not feed provider breaker"
        );
        assert!(
            !terminal_report_feeds_model_success(&env_report),
            "environmental non-attempt must not feed model-health success \
             (stages_completed is empty)"
        );
    }
    #[test]
    fn environmental_non_attempt_service_readiness_has_no_breaker_signal() {
        let env_report = report(
            "env-svc-run",
            vec![],
            TaskRunOutcome::EnvironmentalNonAttempt {
                reason: "service_readiness_failed".into(),
            },
        );
        assert_eq!(
            report_to_terminal_status(&env_report),
            TaskRunStatus::Completed,
            "service readiness failure maps to Completed (terminal, no penalty)"
        );
        assert!(
            !terminal_report_feeds_model_success(&env_report),
            "service readiness failure must not feed model-health success"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_run_report_terminalizes_pending_attempt() {
        use crate::test_helpers;
        use djinn_core::models::task_attempt::TaskAttemptOutcome;
        use tokio_util::sync::CancellationToken;

        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

        let attempt_repo = TaskAttemptRepository::new(db.clone());
        let attempt = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: "task-run:test-terminalize",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create pending attempt");

        let failed = report(
            "failed-run",
            vec![],
            TaskRunOutcome::Failed {
                stage: "worker".into(),
                reason: "provider API error 400: text content is empty".into(),
                provider_failure: None,
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
        );
        terminalize_failed_run_attempt(&app_state, Some(&attempt.id), None, &task, &failed).await;

        let after = attempt_repo
            .get(&attempt.id)
            .await
            .expect("read attempt")
            .expect("attempt exists");
        assert_eq!(
            after.outcome,
            TaskAttemptOutcome::Crashed.as_str(),
            "a Failed run report must terminalize its pending attempt so the \
             respawn guard does not defer dispatch until the orphan reaper"
        );

        // A submitted attempt is owned by the review/PR lifecycle — untouched.
        let submitted = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: "task-run:test-terminalize-submitted",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create second attempt");
        attempt_repo
            .advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
                id: &submitted.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .expect("advance to submitted");
        terminalize_failed_run_attempt(&app_state, Some(&submitted.id), None, &task, &failed).await;
        let after_submitted = attempt_repo
            .get(&submitted.id)
            .await
            .expect("read attempt")
            .expect("attempt exists");
        assert_eq!(
            after_submitted.outcome,
            TaskAttemptOutcome::Submitted.as_str(),
            "a submitted attempt must keep its submitted signal"
        );

        // A non-Failed outcome must not touch the attempt.
        let pending = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "reviewer",
                dispatch_key: "task-run:test-terminalize-nonfailed",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create third attempt");
        let submitted_outcome = report("ok-run", vec![], TaskRunOutcome::WorkerSubmitted);
        terminalize_failed_run_attempt(
            &app_state,
            Some(&pending.id),
            None,
            &task,
            &submitted_outcome,
        )
        .await;
        let after_pending = attempt_repo
            .get(&pending.id)
            .await
            .expect("read attempt")
            .expect("attempt exists");
        assert_eq!(
            after_pending.outcome,
            TaskAttemptOutcome::Pending.as_str(),
            "non-Failed outcomes must not touch the attempt"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_failure_terminalizes_only_its_exact_group_as_spawn_failed() {
        use crate::test_helpers;
        use djinn_core::models::task_attempt::TaskAttemptOutcome;
        use tokio_util::sync::CancellationToken;

        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

        let attempt_repo = TaskAttemptRepository::new(db.clone());
        let current_group = uuid::Uuid::now_v7().to_string();
        let other_group = uuid::Uuid::now_v7().to_string();
        // Coordinator-style dispatch-start row: role is the coordinator's
        // dispatched role, which can DIFFER from the flow's first role (a lead
        // intervention writes a `lead` coordinator row and a `worker`
        // supervisor row) — the terminalization must be role-agnostic.
        let coordinator_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "lead",
                dispatch_key: &format!("{}:lead:test-dispatch-failure", task.id),
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: Some(&current_group),
            })
            .await
            .expect("create coordinator pending row");
        // Supervisor-style exact-attempt row.
        let supervisor_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: "task-run:test-dispatch-failure",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: Some(&current_group),
            })
            .await
            .expect("create supervisor pending row");
        // Same task, distinct dispatch group: must remain pending.
        let other_group_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "reviewer",
                dispatch_key: "task-run:test-dispatch-failure-other-group",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: Some(&other_group),
            })
            .await
            .expect("create other-group pending row");
        // Legacy NULL-group rows must not be inferred into a batch.
        let legacy_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: "task-run:test-dispatch-failure-legacy",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create legacy pending row");

        // A submitted row is owned by the review/PR lifecycle — untouched.
        let submitted_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: "task-run:test-dispatch-failure-submitted",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create submitted row");
        attempt_repo
            .advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
                id: &submitted_row.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .expect("advance to submitted");

        let error = anyhow::anyhow!(
            "supervisor dispatch: failed to allocate exact attempt: duplicate key value \
             violates unique constraint \"task_attempts_task_id_attempt_seq_unique\""
        );
        terminalize_dispatch_group_after_dispatch_failure(
            &app_state,
            Some(&current_group),
            &task,
            &error,
        )
        .await;

        for (label, id) in [
            ("coordinator", &coordinator_row.id),
            ("supervisor", &supervisor_row.id),
        ] {
            let after = attempt_repo
                .get(id)
                .await
                .expect("read attempt")
                .expect("attempt exists");
            assert_eq!(
                after.outcome,
                TaskAttemptOutcome::SpawnFailed.as_str(),
                "{label} pending row must be terminalized as spawn_failed after a \
                 dispatch failure so the respawn guard does not defer until the \
                 periodic orphan sweep"
            );
            assert!(after.terminal_at.is_some(), "{label} row must be terminal");
        }
        let after_submitted = attempt_repo
            .get(&submitted_row.id)
            .await
            .expect("read attempt")
            .expect("attempt exists");
        assert_eq!(
            after_submitted.outcome,
            TaskAttemptOutcome::Submitted.as_str(),
            "submitted rows are owned by the review/PR lifecycle and must be untouched"
        );
        // Repeating the helper is idempotent, and an absent mixed-version group
        // must never fall back to task-wide correlation.
        terminalize_dispatch_group_after_dispatch_failure(&app_state, None, &task, &error).await;

        for (label, id) in [
            ("other group", &other_group_row.id),
            ("legacy NULL group", &legacy_row.id),
        ] {
            let after = attempt_repo
                .get(id)
                .await
                .expect("read attempt")
                .expect("attempt exists");
            assert_eq!(
                after.outcome,
                TaskAttemptOutcome::Pending.as_str(),
                "{label} must remain pending because it is not in the current exact group"
            );
        }
    }
}

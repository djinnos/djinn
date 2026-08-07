// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock};

use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use djinn_core::models::Task;
use djinn_core::models::{SessionFailureCause, TaskRunStatus, TaskRunTrigger};
use djinn_db::repositories::task_attempt::TaskAttemptRepository;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::task_run_outcome::TaskRunOutcomeRepository;
use djinn_db::{TaskRepository, task_branch_name};
use djinn_runtime::{
    InfraDeathLogTailCapture, LoopGuardKind, ModelTurnAdmissionTerminalOutcome,
    ProviderFailureClass, ResolvedCredentials, ResumeLifecycleMetadata, SessionRuntime, StreamEvent,
    TaskRunOutcome, TaskRunReport,
    TerminalRuntimeEvidenceKind, TerminalRuntimeObservation, TestRuntime,
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

/// Pre-session liveness deadline (8 min default).
const PRE_SESSION_DEADLINE_SECS_DEFAULT: u64 = 480;

/// Maximum time from the first terminal runtime observation to synthetic
/// settlement. Non-terminal stream frames and diagnostic cleanup never extend
/// this deadline.
const TERMINAL_RUNTIME_REPORT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Step label before the worker emits its first stage marker.
const PRE_SESSION_INITIAL_STEP: &str = "run_create";

/// Redact assignment-style runtime diagnostics before they reach logs or DB
/// evidence. Runtime backends frequently report `token=...` rather than JSON,
/// which the provider-response sanitizer intentionally does not parse.
static TERMINAL_RUNTIME_CREDENTIAL_ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)\b((?:[a-z_][a-z0-9_-]*)?(?:password|passwd|pwd|secret|token|api[_-]?key|credential|authorization)[a-z0-9_-]*)\s*[:=]\s*(?:"[^"]*"|'[^']*'|[^\s,;]+)"#,
    )
    .expect("valid terminal-runtime credential assignment expression")
});

fn sanitize_terminal_runtime_diagnostic(reason: &str) -> String {
    let provider_sanitized = djinn_provider::provider::error::redact_secrets(reason, &[]);
    TERMINAL_RUNTIME_CREDENTIAL_ASSIGNMENT
        .replace_all(&provider_sanitized, "$1=[redacted]")
        .into_owned()
}

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

/// Best-effort dedup for the repeated "failed over off model X" failover
/// comments. A role lane with a single candidate model that keeps failing
/// environmentally (image pull, unschedulable, stage-init hang) would otherwise
/// append an identical — or, when the elapsed time varies, near-identical —
/// comment every dispatch cycle, burying the task timeline (the k6hm loop,
/// 2026-07-22, logged the same comment three times in a day). Returns `true`
/// when a prior activity comment for THIS task already reports a failover off
/// `model_id`, so the caller can skip re-appending. Matches on the stable
/// `failed over off model {model_id}` phrase so it is insensitive to the
/// per-cycle elapsed-time suffix. On a read failure it returns `false` (better a
/// duplicate than a silently dropped operator signal). This only gates the
/// timeline comment; timeout diagnostics and task-side failover handling still
/// run every cycle, while generic timeout paths remain breaker-neutral.
async fn failover_comment_already_logged(
    task_repo: &TaskRepository,
    task_id: &str,
    model_id: &str,
) -> bool {
    let needle = format!("failed over off model {model_id}");
    match task_repo.list_activity(task_id).await {
        Ok(entries) => entries.iter().any(|entry| {
            serde_json::from_str::<serde_json::Value>(&entry.payload)
                .ok()
                .and_then(|v| {
                    v.get("body")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|body| body.contains(&needle))
        }),
        Err(e) => {
            tracing::warn!(
                task_id,
                error = %e,
                "supervisor dispatch: failed to read activity for failover-comment dedup; \
                 appending anyway"
            );
            false
        }
    }
}

/// Record failover diagnostics when the worker never completed its startup
/// handshake within the deadline. This infrastructure timeout has no typed
/// in-pod ProviderError, so it must not mutate model breaker health.
async fn apply_handshake_timeout_failover(task_repo: &TaskRepository, task: &Task, model_id: &str) {
    if !failover_comment_already_logged(task_repo, &task.id, model_id).await {
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
    }
    tracing::warn!(
        task_id = %task.short_id,
        %model_id,
        "supervisor dispatch: worker handshake timed out; recorded failover diagnostics"
    );
}

/// Log sanitized terminal-runtime evidence and settle any orphaned running
/// session rows with the stable cause selected from that evidence.
async fn finalize_terminal_runtime_observation(
    task_repo: &TaskRepository,
    task: &Task,
    app_state: &AgentContext,
    observation: &TerminalRuntimeObservation,
) {
    // Runtime implementations normally provide sanitized diagnostics, but this
    // is the durable evidence boundary. A backend regression must not write
    // credential-shaped text into activity rows or tracing fields.
    let reason = sanitize_terminal_runtime_diagnostic(&observation.diagnostic);
    let failure_cause = failure_cause_for_terminal_runtime_observation(observation);
    // Settle first: activity is diagnostic and must not delay the durable
    // cause-bearing transition once the report deadline has elapsed.
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match session_repo
        .interrupt_running_for_task_with_failure_cause(&task.id, failure_cause)
        .await
    {
        Ok(n) if n > 0 => tracing::warn!(
            task_id = %task.short_id,
            %reason,
            sessions = n,
            "supervisor dispatch: finalized orphaned running session(s) after terminal runtime observation"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "supervisor dispatch: failed to finalize session row after terminal runtime observation"
        ),
    }
    let payload = serde_json::json!({
        "error": format!("Terminal runtime observation before completing the run: {reason}"),
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
}

/// Collapse typed runtime evidence into the stable, durable session cause.
/// Exact evidence remains in activity and attempt diagnostics.
fn failure_cause_for_terminal_runtime_observation(
    observation: &TerminalRuntimeObservation,
) -> SessionFailureCause {
    match observation.kind {
        TerminalRuntimeEvidenceKind::Infrastructure => SessionFailureCause::Infrastructure,
        TerminalRuntimeEvidenceKind::UnknownFailure => SessionFailureCause::Unknown,
        TerminalRuntimeEvidenceKind::ProtocolNoReport => SessionFailureCause::Protocol,
    }
}

/// Best-effort persist infra-death log-tail capture on the latest
/// pending/submitted attempt for the task.  Failures are logged and swallowed —
/// this is purely diagnostic enrichment and must never block teardown.
async fn persist_terminal_runtime_observation_on_attempt(
    app_state: &AgentContext,
    task: &Task,
    reason: &str,
    capture: &InfraDeathLogTailCapture,
) {
    let reason = sanitize_terminal_runtime_diagnostic(reason);
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
        "terminal_runtime_log_tail": {
            "schema_version": capture.schema_version,
            "fetched": capture.log_tail.is_some(),
            "pod_name": capture.pod_name,
            "pod_uid": capture.pod_uid,
            "container_name": capture.container_name,
            "container_exit_reason": capture.container_exit_reason,
            "container_exit_code": capture.container_exit_code,
            "head_bytes": capture.head_bytes,
            "tail_bytes": capture.tail_bytes,
            "omitted_bytes": capture.omitted_bytes,
            "sanitizers": capture.sanitizers,
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

/// Stable snake_case name of a terminal run outcome for attempt evidence.
fn run_outcome_name(outcome: &TaskRunOutcome) -> &'static str {
    match outcome {
        TaskRunOutcome::PrOpened { .. } => "pr_opened",
        TaskRunOutcome::Closed { .. } => "closed",
        TaskRunOutcome::Escalated { .. } => "escalated",
        TaskRunOutcome::Failed { .. } => "failed",
        TaskRunOutcome::Interrupted => "interrupted",
        TaskRunOutcome::WorkerSubmitted => "worker_submitted",
        TaskRunOutcome::LoopGuardTripped { .. } => "loop_guard_tripped",
        TaskRunOutcome::Parked { .. } => "parked",
        TaskRunOutcome::EnvironmentalNonAttempt { .. } => "environmental_non_attempt",
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Wait(..)) => "model_turn_admission_wait",
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Rejected(..)) => "model_turn_admission_rejected",
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::DispatchFenced(..)) => "model_turn_admission_dispatch_fenced",
    }
}

/// Map a terminal run report to the outcome used to terminalize any attempt
/// row of the run's dispatch group that is STILL `pending` when the report
/// arrives.
fn attempt_outcome_for_terminal_report(
    outcome: &TaskRunOutcome,
) -> djinn_core::models::task_attempt::TaskAttemptOutcome {
    use djinn_core::models::task_attempt::TaskAttemptOutcome as AttemptOutcome;
    match outcome {
        TaskRunOutcome::Failed { .. } => AttemptOutcome::Crashed,
        TaskRunOutcome::LoopGuardTripped { .. } => AttemptOutcome::LoopGuardTripped,
        // Operator/host cancellation before resolution.
        TaskRunOutcome::Interrupted => AttemptOutcome::Cancelled,
        // Environmental non-attempt: terminalize so nothing wedges, but with
        // the strike-exempt environmental outcome (no quality/park penalty).
        TaskRunOutcome::EnvironmentalNonAttempt { .. } => AttemptOutcome::Interrupted,
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Wait(..))
        | TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::DispatchFenced(..)) => AttemptOutcome::Cancelled,
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Rejected(..)) => AttemptOutcome::Crashed,
        // The run genuinely completed. Any row of its dispatch group that is
        // still `pending` (the bookkeeping sibling that `submit_work`'s
        // latest-row advancement did not touch) is completed with the run.
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        | TaskRunOutcome::Escalated { .. }
        | TaskRunOutcome::WorkerSubmitted
        | TaskRunOutcome::Parked { .. } => AttemptOutcome::Completed,
    }
}

/// Best-effort: terminalize this dispatch's still-`pending` attempt rows when
/// the run's terminal report arrives — for EVERY terminal outcome, not just
/// `Failed`. Without this a leftover `pending` row (each dispatch group holds
/// both the coordinator's `<task>:<role>:<uuid>` dispatch-start row and the
/// supervisor's exact `task-run:<id>` row, and `submit_work` advances only the
/// newest one to `submitted`) survives a successful run, so the respawn guard
/// defers every subsequent (task, role) dispatch — e.g. the rework worker
/// after a PR reopen — until the periodic orphaned-attempt reaper catches it
/// and mislabels the successfully submitted run `crashed` (task pl4n,
/// 2026-07-23; failed-run flavor was incident 8lb0, 2026-07-16). A `submitted`
/// row is deliberately left alone: submitted work is owned by the review/PR
/// lifecycle and must keep its submitted signal (both the group and single-row
/// paths below only ever touch `pending` rows).
async fn terminalize_run_attempt(
    app_state: &AgentContext,
    task_attempt_id: Option<&str>,
    dispatch_group_id: Option<&str>,
    task: &Task,
    report: &TaskRunReport,
) {
    let attempt_outcome = attempt_outcome_for_terminal_report(&report.outcome);
    let outcome_name = run_outcome_name(&report.outcome);
    let attempt_repo = TaskAttemptRepository::new(app_state.db.clone());
    let (summary, summary_json) = match &report.outcome {
        TaskRunOutcome::Failed { stage, reason, .. } => {
            let truncated_reason: String = reason.chars().take(500).collect();
            (
                format!("run failed at stage {stage}: {truncated_reason}"),
                serde_json::json!({
                    "recovery_classifier": "failed_run_report",
                    "stage": stage,
                })
                .to_string(),
            )
        }
        _ => (
            format!(
                "run reached terminal outcome {outcome_name}; terminalizing leftover pending \
                 attempt rows"
            ),
            serde_json::json!({
                "recovery_classifier": "terminal_run_report",
                "run_outcome": outcome_name,
            })
            .to_string(),
        ),
    };
    if let Some(group_id) = dispatch_group_id {
        match attempt_repo
            .terminalize_dispatch_group(
                group_id,
                attempt_outcome,
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
                run_outcome = outcome_name,
                attempt_outcome = %attempt_outcome.as_str(),
                terminalized_attempts = result.updated_attempt_ids.len(),
                "supervisor dispatch: terminalized run's pending dispatch group"
            ),
            Err(e) => tracing::warn!(
                task_id = %task.short_id,
                dispatch_group_id = %group_id,
                run_outcome = outcome_name,
                error = %e,
                "supervisor dispatch: failed to terminalize run's pending dispatch group"
            ),
        }
        return;
    }
    // Mixed-version dispatches have no group to correlate. Preserve the
    // conservative single-row cleanup rather than widening by task or role.
    let Some(attempt_id) = task_attempt_id else {
        tracing::debug!(task_id = %task.short_id, "supervisor dispatch: terminal run has no dispatch group or exact attempt ID; leaving legacy attempts untouched");
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
                "supervisor dispatch: failed to load attempt for terminal-run terminalization"
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
            outcome: attempt_outcome,
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
            run_outcome = outcome_name,
            outcome = %updated.outcome,
            "supervisor dispatch: terminalized run's pending attempt"
        ),
        Err(e) => tracing::warn!(
            task_id = %task.short_id,
            attempt_id,
            run_outcome = outcome_name,
            error = %e,
            "supervisor dispatch: failed to terminalize run's pending attempt"
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
        let (is_throttle, is_transient, retry_after_ms) = match class {
            djinn_runtime::ProviderFailureClass::Throttle { retry_after_ms } => {
                app_state
                    .health_tracker
                    .record_stall(creator_scope, model_id, false);
                (true, false, retry_after_ms)
            }
            djinn_runtime::ProviderFailureClass::Failure => {
                app_state
                    .health_tracker
                    .record_failure(creator_scope, model_id);
                (false, false, None)
            }
            // A transient provider-side fault (5xx / transport death) is a LOAD
            // signal about the upstream, not a health signal about the model, so
            // it gets its own much longer breaker ladder
            // (`record_transient_failure`, `TRANSIENT_BREAKER_THRESHOLD`) rather
            // than the three-strike one. The fault stays fully visible in
            // `model_health` — `consecutive_failures` and `total_failures` move
            // exactly as a `Failure` would — but a burst of
            // `server_is_overloaded` no longer auto-disables the user's
            // preferred model (2026-07-29, task `nr41`: `openai/gpt-5.6-sol`
            // reached `auto_disabled: true`, 15 consecutive failures and 6
            // disable-TTL trips, off an OpenAI capacity blip, taking the
            // tribunal's adversary role down with it). A model whose backend is
            // actually gone still demotes, just twenty strikes in rather than
            // three. Task attribution is unchanged from the previous commit: the
            // coordinator must not blame the task for the provider's outage.
            djinn_runtime::ProviderFailureClass::Transient { retry_after_ms } => {
                app_state
                    .health_tracker
                    .record_transient_failure(creator_scope, model_id);
                (false, true, retry_after_ms)
            }
            djinn_runtime::ProviderFailureClass::AuthInvalid => {
                if refresh_oauth_credential_after_401(model_id, app_state).await {
                    app_state
                        .health_tracker
                        .record_stall(creator_scope, model_id, false);
                    (true, false, None)
                } else {
                    app_state
                        .health_tracker
                        .record_stall(creator_scope, model_id, true);
                    surface_credential_revocation(app_state, creator_scope, model_id).await;
                    (true, false, None)
                }
            }
        };
        app_state.health_tracker.note_task_provider_failure(
            task_id,
            djinn_provider::catalog::health::TaskFailureSignal {
                throttle: is_throttle,
                transient: is_transient,
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

/// Complete host-owned work which is only valid after the supervisor has
/// returned its terminal report. Routing remains awaited; extraction's caller
/// records only the `tokio::spawn` dispatch and deliberately does not await the
/// extraction work.
async fn dispatch_post_settlement_host_operations<Routing, RoutingFuture, Extraction>(
    report: &TaskRunReport,
    route_loop_guard: Routing,
    dispatch_extraction: Extraction,
) where
    Routing: FnOnce() -> RoutingFuture,
    RoutingFuture: Future<Output = ()>,
    Extraction: FnOnce(&TaskRunReport),
{
    route_loop_guard().await;
    if !report.stages_completed.is_empty() {
        dispatch_extraction(report);
    }
}

/// Host-side dispatch: resolve task -> build spec -> construct runtime -> drive lifecycle.
///
/// `Ok(())` = terminal outcome (slot treats as `SlotEvent::Free`).
/// `Err` = infra setup failure the runtime can't express via `TaskRunReport`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_task_runtime(
    task_id: String,
    execution_generation: i64,
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
        execution_generation,
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
        terminal_runtime_observation,
        terminal_runtime_log_tail,
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
        apply_handshake_timeout_failover(&task_repo, &task, &model_id)
        .await;
    }
    if let (Some(observation), Some(capture)) = (
        terminal_runtime_observation.as_ref(),
        terminal_runtime_log_tail.as_ref(),
    ) {
        // Best-effort: persist terminal-runtime log-tail capture on the matching
        // attempt. Settlement already happened at the report deadline, before
        // any potentially slow log capture or runtime teardown; this is purely
        // diagnostic enrichment.
        persist_terminal_runtime_observation_on_attempt(
            &app_state,
            &task,
            &observation.diagnostic,
            capture,
        )
        .await;
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
        if !failover_comment_already_logged(&task_repo, &task.id, &model_id).await {
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
        }
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
            terminalize_run_attempt(
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
            dispatch_post_settlement_host_operations(
                &report,
                || {
                    route_loop_guard_planner_intervention_if_needed(
                        &app_state,
                        &report,
                        &task,
                        loop_guard_intervention_role,
                    )
                },
                |report| {
                    // Fire-and-forget post-session knowledge extraction.
                    let app_state_ext = app_state.clone();
                    let task_id_ext = task.id.clone();
                    let task_run_id_ext = report.task_run_id.clone();
                    let terminal_context_ext = terminal_extraction_context(report);
                    tokio::spawn(async move {
                        crate::actors::slot::session_extraction::run_post_session_extraction(
                            task_id_ext,
                            task_run_id_ext,
                            terminal_context_ext,
                            app_state_ext,
                        )
                        .await;
                    });
                },
            )
            .await;
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
pub struct RuntimeExecutionOutcome {
    pub report_result: anyhow::Result<Option<TaskRunReport>>,
    pub teardown: Result<TaskRunReport, djinn_runtime::RuntimeError>,
    pub handshake_timed_out: bool,
    pub terminal_runtime_observation: Option<TerminalRuntimeObservation>,
    pub terminal_runtime_log_tail: Option<InfraDeathLogTailCapture>,
    pub presession_timeout: Option<PreSessionTimeout>,
}

// ── Build-pod permits and the resize birth gate (3i92 / 0ppk-1b) ───────────

/// Environment override for the durable build-pod permit ceiling.
const BUILD_POD_PERMIT_LIMIT_ENV: &str = "DJINN_BUILD_POD_PERMIT_LIMIT";

/// Default ceiling on concurrent non-`released` `build_pod_permits` rows.
///
/// This number is deliberately far above any reachable dispatch concurrency,
/// and that is the whole point. **Build-slot capacity is not this ledger's
/// job.** `BuildLeaseService` owns capacity through `build_leases`; the permit
/// relation exists here to give each task run a durable, fenced *identity* the
/// Pod resize lifecycle can be written against.
///
/// The last time a permit ceiling was treated as an admission gate
/// (`DJINN_MAX_BUILD_PODS`, v0.7.28) it was rendered by no chart, defaulted to
/// fail-closed when absent, and wedged **every** dispatch cluster-wide while
/// `build_capacity` still reported healthy. A second, unrendered capacity gate
/// in front of dispatch is a strictly worse version of the one that already
/// exists, so this one is sized not to bind and a `PoolFull` result is a warning
/// rather than a refusal for `leaf-v1`.
const BUILD_POD_PERMIT_LIMIT_DEFAULT: i64 = 4096;

fn build_pod_permit_limit() -> i64 {
    std::env::var(BUILD_POD_PERMIT_LIMIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(BUILD_POD_PERMIT_LIMIT_DEFAULT)
}

/// One dispatch's durable permit identity, as the seam holds it between
/// `acquire` and the birth gate.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AcquiredBuildPodPermit {
    task_run_id: String,
    permit_id: String,
    fencing_token: i64,
}

#[derive(Debug)]
enum BuildPodPermitAttempt {
    NotComposed,
    Acquired(AcquiredBuildPodPermit),
    Failed(String),
}

impl BuildPodPermitAttempt {
    fn permit(&self) -> Option<&AcquiredBuildPodPermit> {
        match self {
            Self::Acquired(permit) => Some(permit),
            Self::NotComposed | Self::Failed(_) => None,
        }
    }
}

/// Create the durable `task_runs` row this dispatch will be recorded against,
/// **before** the build-pod permit whose foreign key points at it.
///
/// # The ordering this exists to fix
///
/// `build_pod_permits.task_run_id` is `REFERENCES task_runs(id)` (migration
/// 162), and [`acquire_build_pod_permit`] must run before the Job POST. Until
/// this function existed, nothing on the host created the parent row: the only
/// production creator was the in-pod supervisor's `create_task_run` RPC, which
/// travels over a stdio channel [`attach_and_await_terminal_report`] has not
/// opened yet — a Pod schedule, image pull and handshake later.
///
/// So every permit insert on a fresh dispatch referenced a parent that did not
/// exist. This was never a race. It was strictly sequential and strictly
/// backwards, and it failed on *every* dispatch, not some of them.
///
/// `BuildPodPermitRepository::acquire` collapses all errors into `Unavailable`
/// to fail closed, so the foreign-key violation surfaced as "no permit" rather
/// than as anything nameable. Under `leaf-v1` that was invisible —
/// [`admit_task_run_dispatch`]'s missing-permit arm returns `Ok(())` for
/// leaf. Under `resize-v2` the same arm refuses the dispatch, which is why the
/// launcher-authority cutover looked like it broke dispatch when all it did was
/// stop masking this.
///
/// The row created here is the one the worker would have created anyway: same
/// host-minted id, same `starting` status, same `catalog_image_id` binding. The
/// worker's RPC now adopts it.
async fn ensure_durable_task_run_row(
    app_state: &AgentContext,
    spec: &TaskRunSpec,
) -> anyhow::Result<()> {
    let created = TaskRunRepository::new(app_state.db.clone())
        .create_for_dispatch(djinn_db::CreateTaskRunParams {
            id: &spec.task_run_id,
            project_id: &spec.project_id,
            task_id: &spec.task_id,
            trigger_type: spec.trigger.as_str(),
            // Byte-identical to what the in-pod supervisor sends: the run is
            // visible to the UI and to the host-side pre-session liveness
            // deadline from dispatch, and flips to `running` when the first
            // reply-loop session is created.
            status: Some(TaskRunStatus::Starting.as_str()),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: spec
                .resume_lifecycle_metadata
                .as_ref()
                .and_then(|metadata| metadata.dispatch_group_id.as_deref()),
        })
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "supervisor dispatch: could not create the durable task_runs row for task-run \
                 {}: {error}; refusing to dispatch a run that has no durable identity to \
                 fence, record a terminal status against, or reap",
                spec.task_run_id
            )
        })?;
    tracing::debug!(
        task_run_id = %spec.task_run_id,
        created,
        "supervisor dispatch: durable task_runs row ready before permit acquisition"
    );
    Ok(())
}

/// Acquire the durable `build_pod_permits` row for this dispatch, **before**
/// the Job that it fences exists.
///
/// Ordering is load-bearing in both directions. The permit's `fencing_token` is
/// what every later durable write is checked against, so a row created after the
/// Job could not fence the window in which the Job already has a Pod — and the
/// permit's own `task_run_id` is a foreign key, so [`ensure_durable_task_run_row`]
/// has to have run before this. Calling this first is what made every
/// `resize-v2` dispatch fail closed.
///
/// Returns `None` when this context composes no resize stack at all (an in-pod
/// worker, a test that dispatches no Job) or when the pool could not answer.
/// `None` is not an admission decision — see [`admit_task_run_dispatch`], which
/// is where a `resize-v2` render without a permit fails closed.
///
/// # Nothing releases these rows yet, and that is deliberate scope
///
/// `BuildPodPermitRepository::release` still has no production caller: the
/// resize lifecycle that reclaims a permit (lift, drop, quarantine, release)
/// belongs to `0ppk-3`'s reconciler. Until that lands, rows accumulate in
/// `job_created` / `birth_confirmed`, which is why
/// [`BUILD_POD_PERMIT_LIMIT_DEFAULT`] is sized not to bind. A ceiling that both
/// accumulates and fails closed is how `DJINN_MAX_BUILD_PODS` wedged the whole
/// cluster.
///
/// A `PoolFull` result is still only a warning *here*; whether it refuses is
/// [`admit_task_run_dispatch`]'s call, and it refuses for `resize-v2`. The
/// version of this note that said "a warning rather than a refusal for
/// `leaf-v1` — the arm every Pod on the current fleet takes" was true when it
/// was written and stopped being true at the `resize-v2` cutover. Do not read
/// the fleet's current protocol out of a comment.
async fn acquire_build_pod_permit(
    app_state: &AgentContext,
    spec: &TaskRunSpec,
) -> BuildPodPermitAttempt {
    if app_state.resize_admission.is_none() {
        return BuildPodPermitAttempt::NotComposed;
    }
    let permits = djinn_db::BuildPodPermitRepository::new(app_state.db.clone());
    match permits
        .acquire(&spec.task_run_id, build_pod_permit_limit())
        .await
    {
        djinn_db::AcquireBuildPodPermitResult::Acquired { row, idempotent } => {
            tracing::debug!(
                task_run_id = %spec.task_run_id,
                permit_id = %row.permit_id,
                fencing_token = row.fencing_token,
                idempotent,
                "supervisor dispatch: build-pod permit acquired"
            );
            BuildPodPermitAttempt::Acquired(AcquiredBuildPodPermit {
                task_run_id: row.task_run_id,
                permit_id: row.permit_id,
                fencing_token: row.fencing_token,
            })
        }
        other => {
            tracing::warn!(
                task_run_id = %spec.task_run_id,
                result = ?other,
                "supervisor dispatch: no build-pod permit for this run; a resize-v2 render \
                 will refuse dispatch and a leaf-v1 render will proceed ungoverned as before"
            );
            BuildPodPermitAttempt::Failed(format!("{other:?}"))
        }
    }
}

/// Bind the **Job** UID the runtime just created onto the permit row.
///
/// `prepare` returns after the Job POST and never sees a Pod, so this is a Job
/// UID and nothing else. `capture_resize_identity`'s `state = 'job_created'`
/// predicate cannot hold until this write lands, which is why an unbound permit
/// is a fail-closed condition for `resize-v2` rather than a retry.
async fn bind_build_pod_permit_job_uid(
    app_state: &AgentContext,
    permit: &AcquiredBuildPodPermit,
    handle: &djinn_runtime::RunHandle,
) -> bool {
    let Some(job_uid) = handle.job_uid.as_deref() else {
        return false;
    };
    let permits = djinn_db::BuildPodPermitRepository::new(app_state.db.clone());
    match permits
        .bind_or_refresh_job_uid(
            &permit.task_run_id,
            &permit.permit_id,
            permit.fencing_token,
            job_uid,
        )
        .await
    {
        Ok(djinn_db::BindBuildPodPermitResult::Bound(_)) => true,
        Ok(djinn_db::BindBuildPodPermitResult::AlreadyBound(_)) => true,
        Ok(djinn_db::BindBuildPodPermitResult::Rejected) => {
            tracing::warn!(
                task_run_id = %permit.task_run_id,
                job_uid,
                "supervisor dispatch: build-pod permit refused the observed Job UID"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                task_run_id = %permit.task_run_id,
                job_uid,
                error = %error,
                "supervisor dispatch: could not bind the observed Job UID to the build-pod permit"
            );
            false
        }
    }
}

/// The birth gate. Nothing downstream of this may attach stdio for a
/// `resize-v2` run whose launcher is not confirmed at the birth limit.
///
/// `leaf-v1` is deliberately untouched: the launcher owns each invocation
/// leaf's `cpu.max` under that protocol, there is no launcher CPU ceiling to
/// capture, and every failure mode below degrades it to exactly the dispatch it
/// performed before this function existed.
async fn admit_task_run_dispatch(
    app_state: &AgentContext,
    permit_attempt: &BuildPodPermitAttempt,
    spec: &TaskRunSpec,
    handle: &djinn_runtime::RunHandle,
    job_uid_bound: bool,
) -> anyhow::Result<()> {
    // A runtime that rendered no launcher sidecar reports no protocol. There is
    // no quota authority to establish, so there is nothing to gate.
    let Some(effective_protocol) = handle.launcher_authority_protocol else {
        return Ok(());
    };
    let leaf = effective_protocol.launcher_owns_leaf_quota();

    validate_resize_birth_gate_inputs(
        effective_protocol,
        app_state.resize_admission.is_some(),
        permit_attempt,
        job_uid_bound,
        &spec.task_run_id,
    )?;

    let Some(admission) = app_state.resize_admission.as_ref() else {
        if leaf {
            return Ok(());
        }
        anyhow::bail!(
            "supervisor dispatch: task-run {} rendered `{}` but this context composes no \
             resize admission bridge, so its launcher CPU ceiling can never be captured or \
             governed; refusing to start the worker session",
            spec.task_run_id,
            effective_protocol.as_wire()
        );
    };
    let Some(permit) = permit_attempt.permit() else {
        if leaf {
            return Ok(());
        }
        match permit_attempt {
            BuildPodPermitAttempt::Failed(cause) => anyhow::bail!(
                "supervisor dispatch: task-run {} rendered `{}` but build-pod permit acquisition \
                 failed ({cause}), so no resize identity can be captured for it; refusing to \
                 start the worker session",
                spec.task_run_id,
                effective_protocol.as_wire()
            ),
            BuildPodPermitAttempt::NotComposed => anyhow::bail!(
                "supervisor dispatch: task-run {} rendered `{}` but no build-pod permit was \
                 requested, so no resize identity can be captured for it; refusing to start the \
                 worker session",
                spec.task_run_id,
                effective_protocol.as_wire()
            ),
            BuildPodPermitAttempt::Acquired(_) => unreachable!(),
        }
    };
    if !job_uid_bound {
        if leaf {
            return Ok(());
        }
        anyhow::bail!(
            "supervisor dispatch: task-run {} rendered `{}` but its build-pod permit carries \
             no bound Job UID, so the write-once resize identity cannot be captured; \
             refusing to start the worker session",
            spec.task_run_id,
            effective_protocol.as_wire()
        );
    }

    let request = crate::task_run_resize_admission::ResizeAdmissionRequest {
        task_run_id: permit.task_run_id.clone(),
        permit_id: permit.permit_id.clone(),
        fencing_token: permit.fencing_token,
        effective_protocol,
    };
    match admission.admit_dispatch(&request).await {
        Ok(outcome) => {
            tracing::info!(
                task_run_id = %spec.task_run_id,
                protocol = effective_protocol.as_wire(),
                outcome = ?outcome,
                "supervisor dispatch: launcher quota authority established; dispatch admitted"
            );
            Ok(())
        }
        Err(refusal) => Err(anyhow::anyhow!(
            "supervisor dispatch: task-run {} refused by the resize birth gate \
             (pod_deleted={}): {}",
            spec.task_run_id,
            refusal.pod_deleted,
            refusal.reason
        )),
    }
}

fn validate_resize_birth_gate_inputs(
    protocol: djinn_launcher_protocol::LauncherAuthorityProtocol,
    has_resize_bridge: bool,
    permit_attempt: &BuildPodPermitAttempt,
    job_uid_bound: bool,
    task_run_id: &str,
) -> anyhow::Result<()> {
    if protocol.launcher_owns_leaf_quota() {
        return Ok(());
    }
    if !has_resize_bridge {
        anyhow::bail!(
            "supervisor dispatch: task-run {task_run_id} rendered `{}` but this context composes \
             no resize admission bridge",
            protocol.as_wire()
        );
    }
    match permit_attempt {
        BuildPodPermitAttempt::Failed(cause) => anyhow::bail!(
            "supervisor dispatch: task-run {task_run_id} rendered `{}` but build-pod permit \
             acquisition failed ({cause})",
            protocol.as_wire()
        ),
        BuildPodPermitAttempt::NotComposed => anyhow::bail!(
            "supervisor dispatch: task-run {task_run_id} rendered `{}` but no build-pod permit \
             was requested",
            protocol.as_wire()
        ),
        BuildPodPermitAttempt::Acquired(_) if !job_uid_bound => anyhow::bail!(
            "supervisor dispatch: task-run {task_run_id} rendered `{}` but its build-pod permit \
             carries no bound Job UID",
            protocol.as_wire()
        ),
        BuildPodPermitAttempt::Acquired(_) => Ok(()),
    }
}

/// Outcome of attaching stdio and waiting for the worker's terminal report.
struct TerminalReportAwaitOutcome {
    report_result: anyhow::Result<Option<TaskRunReport>>,
    handshake_timed_out: bool,
    terminal_runtime_observation: Option<TerminalRuntimeObservation>,
    presession_timeout: Option<PreSessionTimeout>,
}

/// Drive the provider runtime execution phase: prepare, cancellation watcher,
/// stdio attach, terminal-report await (with infra-death and pre-session
/// timeout watching), teardown, orphan reaping, and cargo-target cleanup.
pub async fn execute_runtime_report_phase(
    runtime: Arc<dyn SessionRuntime>,
    spec: &TaskRunSpec,
    credentials: &ResolvedCredentials,
    task: &Task,
    model_id: &str,
    app_state: &AgentContext,
    kill: &CancellationToken,
) -> anyhow::Result<RuntimeExecutionOutcome> {
    // The durable `task_runs` row is created BEFORE the permit, because the
    // permit's `task_run_id` is a foreign key onto it and the in-pod supervisor
    // cannot create it until a Pod boot later. See
    // `ensure_durable_task_run_row`.
    ensure_durable_task_run_row(app_state, spec).await?;
    // The durable permit is acquired BEFORE the Job exists: its fencing token is
    // what every later resize write is checked against, and a row minted after
    // the Pod could not fence the window it is supposed to own.
    let permit = acquire_build_pod_permit(app_state, spec).await;
    let handle = match runtime.prepare(spec, credentials).await {
        Ok(handle) => handle,
        Err(error) => {
            // The `task_runs` row above outlives a failed Job POST. Before that
            // row was created on this side of the boot there was nothing here to
            // terminalize, and leaving it `starting` would strand it until the
            // coordinator's stale sweep.
            reap_orphan_task_run(app_state, &spec.task_run_id, TaskRunStatus::Failed).await;
            teardown_cargo_target_run_dir(app_state, &spec.task_run_id).await;
            return Err(anyhow::anyhow!("runtime.prepare failed: {error}"));
        }
    };
    // The JOB uid the create above confirmed. Not a Pod UID — `prepare` never
    // waits for a Pod and never sees one.
    let job_uid_bound = match permit.permit() {
        Some(permit) => bind_build_pod_permit_job_uid(app_state, permit, &handle).await,
        None => false,
    };
    let cancel_task = spawn_runtime_cancel_watcher(
        runtime.clone(),
        handle.clone(),
        kill.clone(),
        task.id.clone(),
        model_id.to_string(),
    );
    // The birth gate. For a `resize-v2` render this waits for the launcher
    // sidecar to be admitted, captures the ceiling the apiserver actually
    // stored, downsizes to the birth limit and confirms it from
    // `status.initContainerStatuses` — all strictly before any stdio attach.
    let admitted = tokio::select! {
        biased;
        () = kill.cancelled() => Err(anyhow::anyhow!(
            "supervisor dispatch: cancelled before the launcher birth limit was confirmed"
        )),
        result = admit_task_run_dispatch(app_state, &permit, spec, &handle, job_uid_bound) => result,
    };
    if let Err(refusal) = admitted {
        tracing::warn!(
            task_id = %task.short_id,
            task_run_id = %spec.task_run_id,
            error = %refusal,
            "supervisor dispatch: refusing to attach stdio; tearing the run down"
        );
        abort_runtime_cancel_watcher(cancel_task).await;
        let teardown = runtime.teardown(handle).await;
        reap_orphan_task_run(app_state, &spec.task_run_id, TaskRunStatus::Failed).await;
        teardown_cargo_target_run_dir(app_state, &spec.task_run_id).await;
        if let Err(error) = &teardown {
            tracing::warn!(
                task_id = %task.short_id,
                error = %error,
                "supervisor dispatch: teardown after a refused dispatch also failed"
            );
        }
        return Err(refusal);
    }
    let await_outcome =
        attach_and_await_terminal_report(runtime.clone(), &handle, app_state, spec, task, kill)
            .await;
    // A no-report terminal observation has crossed its absolute report deadline.
    // Durable settlement must precede log-tail capture and teardown: Kubernetes
    // teardown can legitimately wait far longer than the coordinator's 30s
    // report-delivery bound. This phase owns the write because its caller cannot
    // run until diagnostic cleanup and teardown below have completed.
    if let Some(observation) = await_outcome.terminal_runtime_observation.as_ref() {
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        finalize_terminal_runtime_observation(&task_repo, task, app_state, observation).await;
    }
    abort_runtime_cancel_watcher(cancel_task).await;
    // Best-effort: capture pod log tail before teardown deletes the Job.
    let terminal_runtime_log_tail = if await_outcome.terminal_runtime_observation.is_some() {
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
        terminal_runtime_observation: await_outcome.terminal_runtime_observation,
        terminal_runtime_log_tail,
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

/// Attach stdio and coordinate the authoritative report with terminal runtime
/// evidence. Reports are polled first in every race and receive one bounded
/// grace period after evidence arrives.
async fn attach_and_await_terminal_report(
    runtime: Arc<dyn SessionRuntime>,
    handle: &djinn_runtime::RunHandle,
    app_state: &AgentContext,
    spec: &TaskRunSpec,
    task: &Task,
    kill: &CancellationToken,
) -> TerminalReportAwaitOutcome {
    // The dispatch site, in the literal sense: the next line is where a worker
    // session goes live and the first repository-controlled command becomes
    // possible. Recording it here — unconditionally, before the attach, and
    // *outside* any branch that checks admission — is what makes the gate's
    // absence observable. If the birth gate above is ever removed or softened
    // to log-and-continue, this counter climbs off zero on the first early
    // dispatch whether or not anyone remembered to assert about ordering.
    if let Some(admission) = app_state.resize_admission.as_ref() {
        admission.record_dispatch_started(&spec.task_run_id);
    }
    let bistream_result = runtime.attach_stdio(handle).await;
    let handshake_timed_out = matches!(
        &bistream_result,
        Err(djinn_runtime::RuntimeError::HandshakeTimeout(_))
    );
    let mut terminal_runtime_observation: Option<TerminalRuntimeObservation> = None;
    let mut presession_timeout: Option<PreSessionTimeout> = None;
    let report_result: anyhow::Result<Option<TaskRunReport>> = match bistream_result {
        Ok(mut bistream) => {
            let runtime_watch = runtime.watch_infra_death(handle);
            tokio::pin!(runtime_watch);
            let pre_session = tokio::time::sleep(pre_session_deadline());
            tokio::pin!(pre_session);
            let mut session_reached = false;
            let mut last_step = PRE_SESSION_INITIAL_STEP.to_string();
            let mut initial_report = None;
            let mut events_closed = false;
            let observation = loop {
                tokio::select! {
                    biased;
                    // Observe terminal evidence before consuming an ordinary
                    // frame. The final non-blocking drain below still makes a
                    // simultaneously queued Report authoritative, while a
                    // backlog of other frames cannot postpone the deadline.
                    evidence = &mut runtime_watch => break Some(evidence),
                    // Cancellation and the pre-session deadline are also typed
                    // exits. Keep both ahead of ordinary stream frames so a
                    // continuously ready receiver cannot starve either one.
                    _ = kill.cancelled() => break None,
                    _ = &mut pre_session, if !session_reached => {
                        let session_repo = djinn_db::SessionRepository::new(
                            app_state.db.clone(), djinn_core::events::EventBus::new(|_| {})
                        );
                        if !session_repo.exists_for_task_run(&spec.task_run_id).await.unwrap_or(true) {
                            presession_timeout = Some(PreSessionTimeout {
                                step: last_step.clone(),
                                elapsed_secs: pre_session_deadline().as_secs(),
                            });
                            break None;
                        }
                        session_reached = true;
                    }
                    frame = bistream.events_rx.recv(), if !events_closed => match frame {
                        Some(StreamEvent::Report(report)) => {
                            initial_report = Some(report);
                            break None::<TerminalRuntimeObservation>;
                        },
                        Some(StreamEvent::StageStep { step }) => {
                            session_reached |= step == djinn_runtime::STAGE_STEP_FIRST_TURN;
                            last_step = step;
                        }
                        Some(_) => {}
                        // Closing stdio is not terminal runtime evidence. Keep
                        // the typed watcher alive so completion, disappearance,
                        // and classified failures retain distinct causes.
                        None => events_closed = true,
                    },
                }
            };
            if let Some(mut observation) = observation {
                observation.diagnostic =
                    sanitize_terminal_runtime_diagnostic(&observation.diagnostic);
                // Capture the single absolute deadline *before* draining. The
                // drain is non-blocking and neither it nor later stream frames
                // may buy additional time after runtime evidence arrived.
                let report_deadline =
                    tokio::time::Instant::now() + TERMINAL_RUNTIME_REPORT_DEADLINE;
                let grace = tokio::time::sleep_until(report_deadline);
                tokio::pin!(grace);
                // Final non-blocking drain. Check the absolute deadline on
                // every frame so a deep queued stream cannot evade settlement.
                while tokio::time::Instant::now() < report_deadline {
                    match bistream.events_rx.try_recv() {
                        Ok(StreamEvent::Report(report)) => {
                            return TerminalReportAwaitOutcome {
                                report_result: Ok(Some(report)),
                                handshake_timed_out,
                                terminal_runtime_observation: None,
                                presession_timeout,
                            };
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                loop {
                    // A biased `select!` is not itself a deadline: queued
                    // ordinary frames remain ready and can otherwise starve an
                    // expired timer. At the boundary, give the next queued
                    // frame one final report-precedence check, then settle.
                    if tokio::time::Instant::now() >= report_deadline {
                        match bistream.events_rx.try_recv() {
                            Ok(StreamEvent::Report(report)) => {
                                return TerminalReportAwaitOutcome {
                                    report_result: Ok(Some(report)),
                                    handshake_timed_out,
                                    terminal_runtime_observation: None,
                                    presession_timeout,
                                };
                            }
                            Ok(_) | Err(_) => break,
                        }
                    }
                    tokio::select! {
                        biased;
                        frame = bistream.events_rx.recv(), if !events_closed => match frame {
                            Some(StreamEvent::Report(report)) => return TerminalReportAwaitOutcome {
                                report_result: Ok(Some(report)), handshake_timed_out,
                                terminal_runtime_observation: None, presession_timeout,
                            },
                            Some(_) => {
                                // Re-check after every ordinary frame. This
                                // makes the deadline terminal even if the
                                // receiver is continuously ready.
                                if tokio::time::Instant::now() >= report_deadline {
                                    match bistream.events_rx.try_recv() {
                                        Ok(StreamEvent::Report(report)) => return TerminalReportAwaitOutcome {
                                            report_result: Ok(Some(report)), handshake_timed_out,
                                            terminal_runtime_observation: None, presession_timeout,
                                        },
                                        Ok(_) | Err(_) => break,
                                    }
                                }
                            },
                            None => events_closed = true,
                        },
                        _ = kill.cancelled() => return TerminalReportAwaitOutcome {
                            report_result: Ok(None), handshake_timed_out,
                            terminal_runtime_observation: None, presession_timeout,
                        },
                        _ = &mut grace => {
                            // The report receive is first above for same-turn
                            // readiness. If the timer wins, inspect one final
                            // queued frame without allowing ordinary frames to
                            // buy another grace interval.
                            match bistream.events_rx.try_recv() {
                                Ok(StreamEvent::Report(report)) => return TerminalReportAwaitOutcome {
                                    report_result: Ok(Some(report)), handshake_timed_out,
                                    terminal_runtime_observation: None, presession_timeout,
                                },
                                Ok(_) | Err(_) => break,
                            }
                        },
                    }
                }
                tracing::warn!(task_id = %task.short_id, diagnostic = %observation.diagnostic,
                    evidence_kind = ?observation.kind, runtime = ?runtime_kind(),
                    "supervisor dispatch: terminal runtime observation received no report before grace deadline");
                terminal_runtime_observation = Some(observation);
                Ok(None)
            } else {
                Ok(initial_report)
            }
        }
        Err(e) => Err(anyhow::anyhow!("runtime.attach_stdio failed: {e}")),
    };
    TerminalReportAwaitOutcome {
        report_result,
        handshake_timed_out,
        terminal_runtime_observation,
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
    execution_generation: i64,
    project_id: String,
    trigger: TaskRunTrigger,
    base_branch: String,
    task_branch: String,
    flow: SupervisorFlow,
    model_id_per_role: HashMap<RoleKind, String>,
    read_source_project_ids: Vec<String>,
    knowledge_injection: djinn_core::models::KnowledgeInjectionConfig,
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
        execution_generation: i64,
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
        let created_by_user_id: Option<String> = task_repo.created_by_user_id(&task.id).await.ok();
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
            execution_generation,
            project_id: task.project_id.clone(),
            trigger: trigger_for_flow(flow, ctx.has_conflict),
            base_branch: ctx.base_branch.clone(),
            task_branch: ctx.task_branch.clone(),
            flow: *flow,
            model_id_per_role,
            read_source_project_ids,
            knowledge_injection: app_state.knowledge_injection,
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
            execution_generation: inputs.execution_generation,
            project_id: inputs.project_id,
            trigger: inputs.trigger,
            base_branch: inputs.base_branch,
            task_branch: inputs.task_branch,
            flow: inputs.flow,
            model_id_per_role: inputs.model_id_per_role,
            read_source_project_ids: inputs.read_source_project_ids,
            knowledge_injection: inputs.knowledge_injection,
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
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Wait(..)) => TerminalExtractionOutcome::Failed { classification: "model_turn_admission_wait".to_string(), reason: None },
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Rejected(..)) => TerminalExtractionOutcome::Failed { classification: "model_turn_admission_rejected".to_string(), reason: None },
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::DispatchFenced(..)) => TerminalExtractionOutcome::Failed { classification: "model_turn_admission_dispatch_fenced".to_string(), reason: None },
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
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Wait(..))
        | TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::DispatchFenced(..)) => TaskRunStatus::Interrupted,
        TaskRunOutcome::ModelTurnAdmission(ModelTurnAdmissionTerminalOutcome::Rejected(..)) => TaskRunStatus::Failed,
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
    // No fallback by design: `cargo_target_runs_root` is the *calling process's*
    // mount of the shared cache PVC, and the server pod and the Job pods mount it
    // at different paths. See the field doc on `AgentContext`.
    let root = app_state.cargo_target_runs_root.clone();
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_runtime::RoleKind;

    #[test]
    fn resize_birth_gate_refuses_every_missing_input_while_leaf_v1_continues() {
        use djinn_launcher_protocol::LauncherAuthorityProtocol::{LeafV1, ResizeV2};

        let acquired = BuildPodPermitAttempt::Acquired(AcquiredBuildPodPermit {
            task_run_id: "run".into(),
            permit_id: "permit".into(),
            fencing_token: 1,
        });
        let cases = [
            (false, &acquired, true, "no resize admission bridge"),
            (
                true,
                &BuildPodPermitAttempt::NotComposed,
                false,
                "no build-pod permit was requested",
            ),
            (
                true,
                &BuildPodPermitAttempt::Failed("PoolFull { active_count: 8, limit: 8 }".into()),
                false,
                "PoolFull",
            ),
            (true, &acquired, false, "no bound Job UID"),
        ];
        for (bridge, attempt, bound, expected) in cases {
            let error = validate_resize_birth_gate_inputs(ResizeV2, bridge, attempt, bound, "run")
                .expect_err("resize-v2 must fail closed");
            assert!(error.to_string().contains(expected), "{error:#}");
            validate_resize_birth_gate_inputs(LeafV1, bridge, attempt, bound, "run")
                .expect("leaf-v1 preserves warning-and-continue compatibility");
        }
        validate_resize_birth_gate_inputs(ResizeV2, true, &acquired, true, "run")
            .expect("complete resize-v2 inputs pass the preflight");
    }
    fn report(id: &str, stages: Vec<RoleKind>, outcome: TaskRunOutcome) -> TaskRunReport {
        TaskRunReport {
            task_run_id: id.to_string(),
            outcome,
            stages_completed: stages,
        }
    }

    #[tokio::test]
    async fn settled_supervisor_report_precedes_awaited_routing_and_extraction_dispatch() {
        use std::sync::{Arc, Mutex};

        // This models the host's input boundary: `TaskRunSupervisor::run` has
        // already returned a terminal report, which is only possible after its
        // completed settlement. The report itself maps to Completed, making the
        // settled evidence visible to this host-side test before the helper runs.
        async fn consume_settled_supervisor_report() -> TaskRunReport {
            report(
                "settled-supervisor-run",
                vec![RoleKind::Worker],
                TaskRunOutcome::WorkerSubmitted,
            )
        }

        let operations = Arc::new(Mutex::new(vec!["completed_settlement_evidence"]));
        let settled_report = consume_settled_supervisor_report().await;
        assert_eq!(
            report_to_terminal_status(&settled_report),
            TaskRunStatus::Completed,
            "the consumed supervisor report must carry Completed-settlement evidence"
        );

        let routing_operations = Arc::clone(&operations);
        let extraction_operations = Arc::clone(&operations);
        dispatch_post_settlement_host_operations(
            &settled_report,
            move || async move {
                tokio::task::yield_now().await;
                routing_operations
                    .lock()
                    .expect("operation recorder mutex poisoned")
                    .push("loop_guard_routing_completed");
            },
            move |_| {
                extraction_operations
                    .lock()
                    .expect("operation recorder mutex poisoned")
                    .push("post_session_extraction_spawn_dispatched");
            },
        )
        .await;
        assert_eq!(
            *operations
                .lock()
                .expect("operation recorder mutex poisoned"),
            vec![
                "completed_settlement_evidence",
                "loop_guard_routing_completed",
                "post_session_extraction_spawn_dispatched",
            ],
            "the executed host helper must await routing before dispatching, but not await, extraction"
        );

        let empty_stages = report("no-extraction-run", vec![], TaskRunOutcome::WorkerSubmitted);
        let empty_operations = Arc::new(Mutex::new(Vec::new()));
        let empty_routing_operations = Arc::clone(&empty_operations);
        let empty_extraction_operations = Arc::clone(&empty_operations);
        dispatch_post_settlement_host_operations(
            &empty_stages,
            move || async move {
                empty_routing_operations
                    .lock()
                    .expect("operation recorder mutex poisoned")
                    .push("loop_guard_routing_completed");
            },
            move |_| {
                empty_extraction_operations
                    .lock()
                    .expect("operation recorder mutex poisoned")
                    .push("post_session_extraction_spawn_dispatched");
            },
        )
        .await;
        assert_eq!(
            *empty_operations
                .lock()
                .expect("operation recorder mutex poisoned"),
            vec!["loop_guard_routing_completed"],
            "empty stages must keep routing behavior but skip extraction dispatch"
        );
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
            TaskRunOutcome::Failed {
                stage: "worker".into(),
                reason: "typed report failure".into(),
                provider_failure: Some(ProviderFailureClass::Failure),
                error_class: None,
                hint: None,
                body_excerpt: None,
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
        assert!(matches!(
            chosen.outcome,
            TaskRunOutcome::Failed {
                stage,
                reason,
                provider_failure: Some(ProviderFailureClass::Failure),
                ..
            } if stage == "worker" && reason == "typed report failure"
        ));
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
        terminalize_run_attempt(&app_state, Some(&attempt.id), None, &task, &failed).await;

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
        terminalize_run_attempt(&app_state, Some(&submitted.id), None, &task, &failed).await;
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

        // A successful outcome must ALSO terminalize a leftover pending
        // attempt (as `completed`) — leaving it pending wedges the respawn
        // guard for the task's next dispatch (task pl4n, 2026-07-23).
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
        terminalize_run_attempt(
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
            TaskAttemptOutcome::Completed.as_str(),
            "a successful terminal report must terminalize its leftover pending attempt"
        );
    }

    /// Regression for task pl4n (2026-07-23): a dispatch group holds the
    /// coordinator's `<task>:worker:<uuid>` dispatch-start row AND the
    /// supervisor's exact `task-run:<id>` row. `submit_work` advances only the
    /// newest pending row to `submitted`; on a successful `WorkerSubmitted`
    /// report the sibling `task-run:`-keyed row stayed `pending` forever, so
    /// the respawn guard deferred the later rework dispatch on every tick
    /// until the periodic reaper mislabeled the run `crashed`. A completed
    /// task-run must leave NO pending attempt in its dispatch group, while the
    /// `submitted` row keeps its signal for the PR lifecycle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_submitted_report_terminalizes_leftover_pending_group_row() {
        use crate::test_helpers;
        use djinn_core::models::task_attempt::TaskAttemptOutcome;
        use tokio_util::sync::CancellationToken;

        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

        let attempt_repo = TaskAttemptRepository::new(db.clone());
        let group = uuid::Uuid::now_v7().to_string();
        // Supervisor exact-attempt row (`task-run:`-keyed) — the production
        // row that was left pending.
        let task_run_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: "task-run:019f9040-9baa-7582-af72-aa8354f114d7",
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: Some(&group),
            })
            .await
            .expect("create task-run exact attempt row");
        // Coordinator dispatch-start row of the same group; `submit_work`
        // advanced it (the newest pending row) to `submitted`.
        let coordinator_row = attempt_repo
            .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
                id: &uuid::Uuid::now_v7().to_string(),
                task_id: &task.id,
                role: "worker",
                dispatch_key: &format!("{}:worker:{}", task.id, uuid::Uuid::now_v7()),
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: Some(&group),
            })
            .await
            .expect("create coordinator dispatch-start row");
        attempt_repo
            .advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
                id: &coordinator_row.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("did the work"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .expect("advance coordinator row to submitted");

        let ok = report(
            "run-ok",
            vec![RoleKind::Worker],
            TaskRunOutcome::WorkerSubmitted,
        );
        terminalize_run_attempt(&app_state, Some(&task_run_row.id), Some(&group), &task, &ok).await;

        let after_task_run = attempt_repo
            .get(&task_run_row.id)
            .await
            .expect("read task-run row")
            .expect("task-run row exists");
        assert_eq!(
            after_task_run.outcome,
            TaskAttemptOutcome::Completed.as_str(),
            "the task-run:-keyed row must be terminalized with the run"
        );
        let after_coordinator = attempt_repo
            .get(&coordinator_row.id)
            .await
            .expect("read coordinator row")
            .expect("coordinator row exists");
        assert_eq!(
            after_coordinator.outcome,
            TaskAttemptOutcome::Submitted.as_str(),
            "the submitted row is owned by the review/PR lifecycle and must keep its signal"
        );
        // The wedge condition itself: no pending/submitted... a `pending` row
        // must be gone so the respawn guard's non-terminal check cannot see a
        // phantom pending attempt after the PR reopens for rework.
        let live = attempt_repo
            .latest_pending_or_submitted(&task.id, Some("worker"))
            .await
            .expect("lookup live attempt");
        assert!(
            live.as_ref()
                .is_some_and(|a| a.outcome == TaskAttemptOutcome::Submitted.as_str()),
            "only the submitted row may remain non-terminal after run completion, got {live:?}"
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
            let evidence: serde_json::Value = serde_json::from_str(
                after
                    .summary_json
                    .as_deref()
                    .expect("dispatch failure must retain recovery evidence"),
            )
            .expect("dispatch failure evidence must be JSON");
            assert_eq!(
                evidence["recovery_classifier"], "dispatch_failure_orphan",
                "{label} must retain the ordinary dispatch-failure classifier"
            );
            assert_ne!(
                evidence["recovery_classifier"], "environmental_owner_expired",
                "{label} ordinary dispatch failure must not be reclassified as environmental"
            );
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

    /// The failover-comment dedup guard must recognize a prior "failed over off
    /// model X" comment (insensitive to a per-cycle elapsed-time suffix), so the
    /// same comment is not appended every dispatch cycle — while a failover onto
    /// a DIFFERENT model still surfaces a fresh comment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failover_comment_dedup_suppresses_repeats_per_model() {
        use crate::test_helpers;
        use tokio_util::sync::CancellationToken;

        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        let task_repo = TaskRepository::new(db.clone(), app_state.event_bus.clone());

        let model = "openai/gpt-5.6-sol";

        // No prior comment: the guard allows the write.
        assert!(
            !failover_comment_already_logged(&task_repo, &task.id, model).await,
            "no prior comment ⇒ dedup guard must not suppress"
        );

        // First failover comment, with a cycle-specific elapsed suffix.
        task_repo
            .log_activity(
                Some(&task.id),
                "agent-supervisor",
                "system",
                "stage_init_timeout",
                &serde_json::json!({
                    "body": format!(
                        "Stage init hung ... within 480s. Tore down the Job and \
                         failed over off model {model}."
                    ),
                })
                .to_string(),
            )
            .await
            .expect("log first failover comment");

        // A later cycle for the SAME model is suppressed even though its elapsed
        // suffix would differ.
        assert!(
            failover_comment_already_logged(&task_repo, &task.id, model).await,
            "an existing failover comment for this model must suppress the repeat"
        );

        // A failover onto a DIFFERENT model is still surfaced.
        assert!(
            !failover_comment_already_logged(&task_repo, &task.id, "zai/glm-5.2").await,
            "a different model has no prior comment ⇒ must not be suppressed"
        );
    }

    /// The host teardown backstop must reap under the root the *calling process*
    /// was configured with, and nothing else.
    ///
    /// This asserts the side effect, not the log line: the run dir is really gone
    /// and its sibling really survives. The field carries this process's own mount
    /// of the shared cache PVC — the server pod sees it at `$DJINN_HOME/cache`,
    /// Job pods at `/cache` — so if this function ever resolved a path itself
    /// (e.g. reinstating an `unwrap_or_else(djinn_core::paths::cargo_target_runs_root)`
    /// fallback) it would sweep a directory that does not exist in the other pod
    /// and leak silently. That would leave the run dir below intact and fail here.
    #[tokio::test]
    async fn host_teardown_reaps_only_the_exact_run_dir_under_the_configured_root() {
        let root = tempfile::tempdir().expect("teardown root tempdir");
        let mut app_state = crate::test_helpers::agent_context_from_db(
            djinn_db::Database::open_in_memory().expect("in-memory db"),
            CancellationToken::new(),
        );
        app_state.cargo_target_runs_root = root.path().to_path_buf();

        let target = root.path().join("run-under-test");
        let sibling = root.path().join("run-untouched");
        std::fs::create_dir_all(target.join("debug")).expect("create run dir");
        std::fs::write(target.join("debug/artifact.rlib"), b"bytes").expect("write artifact");
        std::fs::create_dir_all(&sibling).expect("create sibling run dir");

        teardown_cargo_target_run_dir(&app_state, "run-under-test").await;

        assert!(
            !target.exists(),
            "teardown must remove the run dir under the configured root: {}",
            target.display()
        );
        assert!(
            sibling.exists(),
            "teardown must not touch other runs under the same root"
        );
        assert!(root.path().exists(), "teardown must not remove the root");

        // Idempotent: a second pass over an already-absent dir is a no-op, not a
        // failure, and still leaves the sibling alone.
        teardown_cargo_target_run_dir(&app_state, "run-under-test").await;
        assert!(sibling.exists());
    }

    /// Controlled in-process runtime for the production report/evidence race.
    struct ScriptedRuntime {
        bistream: tokio::sync::Mutex<Option<djinn_runtime::BiStream>>,
        observation:
            tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<TerminalRuntimeObservation>>>,
    }
    impl ScriptedRuntime {
        fn new() -> (
            Arc<Self>,
            tokio::sync::mpsc::Sender<StreamEvent>,
            tokio::sync::oneshot::Sender<TerminalRuntimeObservation>,
        ) {
            let (bistream, events_tx, _requests_rx) = djinn_runtime::BiStream::new_in_memory(16);
            let (observation_tx, observation_rx) = tokio::sync::oneshot::channel();
            (
                Arc::new(Self {
                    bistream: tokio::sync::Mutex::new(Some(bistream)),
                    observation: tokio::sync::Mutex::new(Some(observation_rx)),
                }),
                events_tx,
                observation_tx,
            )
        }
    }
    #[async_trait::async_trait]
    impl SessionRuntime for ScriptedRuntime {
        async fn prepare(
            &self,
            spec: &TaskRunSpec,
            _: &ResolvedCredentials,
        ) -> Result<djinn_runtime::RunHandle, djinn_runtime::RuntimeError> {
            Ok(scripted_handle(&spec.task_run_id))
        }
        async fn attach_stdio(
            &self,
            _: &djinn_runtime::RunHandle,
        ) -> Result<djinn_runtime::BiStream, djinn_runtime::RuntimeError> {
            Ok(self
                .bistream
                .lock()
                .await
                .take()
                .expect("stdio attached once"))
        }
        async fn cancel(
            &self,
            _: &djinn_runtime::RunHandle,
        ) -> Result<(), djinn_runtime::RuntimeError> {
            Ok(())
        }
        async fn teardown(
            &self,
            handle: djinn_runtime::RunHandle,
        ) -> Result<TaskRunReport, djinn_runtime::RuntimeError> {
            Ok(report(
                &handle.task_run_id,
                vec![],
                TaskRunOutcome::Interrupted,
            ))
        }
        async fn watch_infra_death(
            &self,
            _: &djinn_runtime::RunHandle,
        ) -> TerminalRuntimeObservation {
            self.observation
                .lock()
                .await
                .take()
                .expect("watch once")
                .await
                .expect("observation sent")
        }
    }
    fn scripted_handle(id: &str) -> djinn_runtime::RunHandle {
        djinn_runtime::RunHandle {
            task_run_id: id.into(),
            container_id: None,
            pod_ref: None,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            job_uid: None,
            launcher_authority_protocol: None,
        }
    }
    fn scripted_spec(task: &Task) -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: uuid::Uuid::now_v7().to_string(),
            task_attempt_id: None,
            task_id: task.id.clone(),
            execution_generation: 0,
            project_id: task.project_id.clone(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/scripted-terminal-report".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: Vec::new(),
            knowledge_injection: Default::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }
    async fn scripted_fixture() -> (AgentContext, Task, TaskRunSpec) {
        use crate::test_helpers;
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let context = test_helpers::agent_context_from_db(db, CancellationToken::new());
        let spec = scripted_spec(&task);
        (context, task, spec)
    }

    #[tokio::test]
    async fn terminal_report_wins_delayed_success_and_simultaneous_evidence() {
        let (context, task, spec) = scripted_fixture().await;
        tokio::time::pause();
        let (runtime, events_tx, observation_tx) = ScriptedRuntime::new();
        let handle = scripted_handle(&spec.task_run_id);
        let kill = CancellationToken::new();
        let awaiting = tokio::spawn(async move {
            attach_and_await_terminal_report(runtime, &handle, &context, &spec, &task, &kill).await
        });
        tokio::task::yield_now().await;
        observation_tx
            .send(TerminalRuntimeObservation::new(
                TerminalRuntimeEvidenceKind::ProtocolNoReport,
                "Job succeeded without report",
            ))
            .expect("watch accepts success");
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        events_tx
            .send(StreamEvent::Report(report(
                "delayed",
                vec![RoleKind::Worker],
                TaskRunOutcome::Failed {
                    stage: "worker".into(),
                    reason: "delayed typed failure".into(),
                    provider_failure: Some(ProviderFailureClass::Failure),
                    error_class: None,
                    hint: None,
                    body_excerpt: None,
                },
            )))
            .await
            .expect("report accepted");
        let delayed = awaiting.await.expect("join");
        assert!(matches!(
            delayed.report_result,
            Ok(Some(TaskRunReport {
                outcome: TaskRunOutcome::Failed {
                    stage,
                    reason,
                    provider_failure: Some(ProviderFailureClass::Failure),
                    ..
                },
                ..
            })) if stage == "worker" && reason == "delayed typed failure"
        ));
        assert!(delayed.terminal_runtime_observation.is_none());

        tokio::time::resume();
        let (context, task, spec) = scripted_fixture().await;
        tokio::time::pause();
        let (runtime, events_tx, observation_tx) = ScriptedRuntime::new();
        events_tx
            .send(StreamEvent::Report(report(
                "simultaneous",
                vec![RoleKind::Worker],
                TaskRunOutcome::Failed {
                    stage: "worker".into(),
                    reason: "simultaneous typed failure".into(),
                    provider_failure: Some(ProviderFailureClass::Failure),
                    error_class: None,
                    hint: None,
                    body_excerpt: None,
                },
            )))
            .await
            .expect("queue report");
        observation_tx
            .send(TerminalRuntimeObservation::new(
                TerminalRuntimeEvidenceKind::Infrastructure,
                "OOMKilled",
            ))
            .expect("queue evidence");
        let handle = scripted_handle(&spec.task_run_id);
        let simultaneous = attach_and_await_terminal_report(
            runtime,
            &handle,
            &context,
            &spec,
            &task,
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            simultaneous.report_result,
            Ok(Some(TaskRunReport {
                outcome: TaskRunOutcome::Failed {
                    stage,
                    reason,
                    provider_failure: Some(ProviderFailureClass::Failure),
                    ..
                },
                ..
            })) if stage == "worker" && reason == "simultaneous typed failure"
        ));
        assert!(simultaneous.terminal_runtime_observation.is_none());
    }

    #[tokio::test]
    async fn no_report_evidence_matrix_uses_the_single_thirty_second_bound() {
        let cases = [
            ("OOMKilled", TerminalRuntimeEvidenceKind::Infrastructure),
            ("Pod Evicted", TerminalRuntimeEvidenceKind::Infrastructure),
            ("Pod NodeLost", TerminalRuntimeEvidenceKind::Infrastructure),
            (
                "worker exited 101",
                TerminalRuntimeEvidenceKind::UnknownFailure,
            ),
            (
                "generic Job failure",
                TerminalRuntimeEvidenceKind::UnknownFailure,
            ),
            (
                "Job disappeared",
                TerminalRuntimeEvidenceKind::ProtocolNoReport,
            ),
            (
                "Pod disappeared",
                TerminalRuntimeEvidenceKind::ProtocolNoReport,
            ),
        ];
        let mut clock_is_paused = false;
        for (diagnostic, kind) in cases {
            if clock_is_paused {
                tokio::time::resume();
            }
            let (context, task, spec) = scripted_fixture().await;
            tokio::time::pause();
            clock_is_paused = true;
            let (runtime, _events_tx, observation_tx) = ScriptedRuntime::new();
            let handle = scripted_handle(&spec.task_run_id);
            let kill = CancellationToken::new();
            let awaiting = tokio::spawn(async move {
                attach_and_await_terminal_report(runtime, &handle, &context, &spec, &task, &kill)
                    .await
            });
            tokio::task::yield_now().await;
            observation_tx
                .send(TerminalRuntimeObservation::new(kind, diagnostic))
                .expect("watch accepts evidence");
            tokio::task::yield_now().await;
            tokio::time::advance(TERMINAL_RUNTIME_REPORT_DEADLINE).await;
            let outcome = awaiting.await.expect("must settle at deadline");
            assert!(matches!(outcome.report_result, Ok(None)), "{diagnostic}");
            assert_eq!(
                outcome
                    .terminal_runtime_observation
                    .expect("evidence retained")
                    .kind,
                kind
            );
        }
    }

    #[tokio::test]
    async fn terminal_evidence_persists_every_runtime_case_after_coordinator_deadline() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let cases = [
            (
                "OOMKilled",
                TerminalRuntimeEvidenceKind::Infrastructure,
                SessionFailureCause::Infrastructure,
            ),
            (
                "Pod Evicted",
                TerminalRuntimeEvidenceKind::Infrastructure,
                SessionFailureCause::Infrastructure,
            ),
            (
                "Pod NodeLost",
                TerminalRuntimeEvidenceKind::Infrastructure,
                SessionFailureCause::Infrastructure,
            ),
            (
                "worker exited 101",
                TerminalRuntimeEvidenceKind::UnknownFailure,
                SessionFailureCause::Unknown,
            ),
            (
                "generic Job failure",
                TerminalRuntimeEvidenceKind::UnknownFailure,
                SessionFailureCause::Unknown,
            ),
            (
                "Job disappeared",
                TerminalRuntimeEvidenceKind::ProtocolNoReport,
                SessionFailureCause::Protocol,
            ),
            (
                "Pod disappeared",
                TerminalRuntimeEvidenceKind::ProtocolNoReport,
                SessionFailureCause::Protocol,
            ),
            (
                "Job succeeded without report",
                TerminalRuntimeEvidenceKind::ProtocolNoReport,
                SessionFailureCause::Protocol,
            ),
        ];
        for (diagnostic, kind, expected) in cases {
            let (context, task, spec) = scripted_fixture().await;
            let sessions = SessionRepository::new(context.db.clone(), context.event_bus.clone());
            let session = sessions
                .create(CreateSessionParams {
                    project_id: &task.project_id,
                    task_id: Some(&task.id),
                    model: "fixture-model",
                    agent_type: "worker",
                    metadata_json: None,
                    task_run_id: None,
                    pricing: None,
                    cost_basis: None,
                })
                .await
                .expect("seed running session");

            tokio::time::pause();
            let (runtime, _events_tx, observation_tx) = ScriptedRuntime::new();
            let handle = scripted_handle(&spec.task_run_id);
            let awaiting_context = context.clone();
            let awaiting_task = task.clone();
            let awaiting_spec = spec.clone();
            let awaiting = tokio::spawn(async move {
                let kill = CancellationToken::new();
                attach_and_await_terminal_report(
                    runtime,
                    &handle,
                    &awaiting_context,
                    &awaiting_spec,
                    &awaiting_task,
                    &kill,
                )
                .await
            });
            tokio::task::yield_now().await;
            observation_tx
                .send(TerminalRuntimeObservation::new(kind, diagnostic))
                .expect("watch accepts evidence");
            tokio::task::yield_now().await;
            tokio::time::advance(TERMINAL_RUNTIME_REPORT_DEADLINE).await;
            let outcome = awaiting.await.expect("settles at report deadline");
            assert!(matches!(outcome.report_result, Ok(None)), "{diagnostic}");
            let observation = outcome
                .terminal_runtime_observation
                .expect("coordinator retains no-report evidence");
            // Database pool acquisition uses Tokio time too; unpause once the
            // coordinator's deadline proof is complete before durable I/O.
            tokio::time::resume();

            // This is the same settlement writer execute_runtime_report_phase
            // invokes immediately after the coordinator returns, before teardown.
            finalize_terminal_runtime_observation(
                &TaskRepository::new(context.db.clone(), context.event_bus.clone()),
                &task,
                &context,
                &observation,
            )
            .await;
            let stored = sessions
                .get(&session.id)
                .await
                .expect("read session")
                .expect("session exists");
            assert_eq!(stored.failure_cause, Some(expected), "{diagnostic}");
        }
    }

    #[tokio::test]
    async fn terminal_runtime_diagnostic_is_sanitized_before_durable_activity() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let (context, task, _spec) = scripted_fixture().await;
        let sessions = SessionRepository::new(context.db.clone(), context.event_bus.clone());
        let session = sessions
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "fixture-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("seed session");
        let raw = "Pod status token=ghp_credential_shaped_secret OOMKilled";
        let observation =
            TerminalRuntimeObservation::new(TerminalRuntimeEvidenceKind::Infrastructure, raw);
        finalize_terminal_runtime_observation(
            &TaskRepository::new(context.db.clone(), context.event_bus.clone()),
            &task,
            &context,
            &observation,
        )
        .await;
        let stored = sessions
            .get(&session.id)
            .await
            .expect("read")
            .expect("session");
        assert_eq!(
            stored.failure_cause,
            Some(SessionFailureCause::Infrastructure)
        );
        assert!(
            !serde_json::to_string(&stored)
                .expect("serialize")
                .contains(raw)
        );
        let activity = TaskRepository::new(context.db, context.event_bus)
            .list_activity(&task.id)
            .await
            .expect("read diagnostic evidence");
        let evidence = activity
            .iter()
            .find(|entry| entry.event_type == "session_error")
            .expect("terminal observation activity");
        assert!(!evidence.payload.contains("ghp_credential_shaped_secret"));
        assert!(evidence.payload.contains("OOMKilled"));
        assert!(evidence.payload.contains("[redacted]"));
    }
}

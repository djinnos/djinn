// djinn:allow-oversize — canonical dispatch orchestration; the orchestrator body
// is long because it handles the full runtime lifecycle (prepare → stream →
// teardown → post-dispatch bookkeeping) but it is the SINGLE copy.
//! Reusable dispatch orchestrator extracted from `djinn-agent`'s
//! `supervisor_runner::dispatch_task_runtime`.
//!
//! This module owns the canonical task-dispatch lifecycle:
//!   load task → resolve context → build spec → resolve credentials →
//!   construct runtime → prepare → stream → teardown → post-dispatch handling.
//!
//! Host-specific operations are abstracted through [`TaskDispatchContext`];
//! `djinn-agent` implements that trait for [`AgentContext`].

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_runtime::{
    ProviderFailureClass, ResolvedCredentials, SessionRuntime,
    TaskRunOutcome, TaskRunReport, TaskRunSpec,
};

use crate::dispatch_utils::{
    await_report_from_stream, is_budget_park_report, loop_guard_kind_label,
    loop_guard_planner_intervention_reason, provider_failure_class_for_report,
    report_to_terminal_status, resume_flow, select_terminal_report,
    supervisor_rpc_span, terminal_report_feeds_model_success,
};

// ─── Host-specific dispatch context ─────────────────────────────────────────

/// Trait for host-specific operations needed by the dispatch orchestrator.
///
/// `djinn-agent` implements this for [`AgentContext`], wiring through
/// `TaskRepository`, `MirrorManager`, `HealthTracker`, runtime construction,
/// credential resolution, and coordinator integration.
pub trait TaskDispatchContext: Send + Sync + 'static {
    /// Resolve the task row by id.
    fn load_task<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<djinn_core::models::Task>> + Send + 'a>,
    >;

    /// Resolve `(has_conflict, has_review_response)` for the task.
    fn resolve_dispatch_context<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = (bool, bool)> + Send + 'a>,
    >;

    /// Resolve the default target (base) branch for the project.
    fn resolve_base_branch<'a>(
        &'a self,
        project_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = String> + Send + 'a>,
    >;

    /// Pick the supervisor flow for a task given dispatch context.
    fn resolve_flow(&self, task: &djinn_core::models::Task, has_conflict: bool, has_review_response: bool)
        -> djinn_runtime::SupervisorFlow;

    /// Resolve per-role model id overrides.
    fn resolve_model_id_per_role<'a>(
        &'a self,
        project_id: &'a str,
        flow: djinn_runtime::SupervisorFlow,
        default_model_id: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = HashMap<djinn_runtime::RoleKind, String>>
                + Send
                + 'a,
        >,
    >;

    /// Check whether the worker's output is durable on the mirror task_branch.
    /// Returns `false` when the mirror is absent, the check fails, or it times
    /// out (>10 s).
    fn check_worker_output_durability<'a>(
        &'a self,
        project_id: &'a str,
        task_branch: &'a str,
        base_branch: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;

    /// Resolve per-role provider credentials for the dispatch spec.
    fn resolve_credentials<'a>(
        &'a self,
        spec: &'a TaskRunSpec,
        default_model_id: &'a str,
        creator_user_id: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<ResolvedCredentials>>
                + Send
                + 'a,
        >,
    >;

    /// Construct the `SessionRuntime` for the dispatch (Kubernetes or Test).
    fn construct_runtime<'a>(
        &'a self,
        task: &'a djinn_core::models::Task,
        spec: &'a TaskRunSpec,
        kill: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<std::sync::Arc<dyn SessionRuntime>>,
                > + Send
                + 'a,
        >,
    >;

    /// Resolve read-only multi-repo sources from the task's epic.
    fn resolve_read_sources<'a>(
        &'a self,
        epic_id: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>,
    >;

    /// Resolve private-dependency credentials (github owner, install token).
    fn resolve_private_deps<'a>(
        &'a self,
        project_id: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = (Option<String>, Option<String>),
                > + Send
                + 'a,
        >,
    >;

    /// Resolve the task creator's user id.
    fn resolve_creator_user_id<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>,
    >;

    /// Resolve the commit-author identity (name, email) for a task creator.
    fn resolve_commit_author<'a>(
        &'a self,
        creator_user_id: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = (Option<String>, Option<String>),
                > + Send
                + 'a,
        >,
    >;

    /// Try a host-side silent OAuth credential refresh after a 401.
    /// Returns `true` if the credential was refreshed successfully.
    fn try_refresh_oauth_after_401<'a>(
        &'a self,
        model_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;

    /// Persist and surface a credential revocation after a 401.
    fn surface_credential_revocation<'a>(
        &'a self,
        owner: Option<&'a str>,
        model_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    // ── Post-dispatch operations ──────────────────────────────────────────

    /// Log an activity entry for the task.
    fn log_agent_activity<'a>(
        &'a self,
        task_id: &'a str,
        agent_type: &'a str,
        actor: &'a str,
        event_type: &'a str,
        payload: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    /// Get a coordinator handle for post-dispatch interactions.
    fn get_coordinator<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Option<Box<dyn CoordinatorOps>>,
                > + Send
                + 'a,
        >,
    >;

    /// Record a model health success.
    fn record_model_success(&self, owner: Option<&str>, model_id: &str);

    /// Record a model health stall.
    fn record_model_stall(&self, owner: Option<&str>, model_id: &str, escalate: bool);

    /// Record a model health failure.
    fn record_model_failure(&self, owner: Option<&str>, model_id: &str);

    /// Note a task-level provider failure signal for the coordinator's streak logic.
    fn note_task_provider_failure(
        &self,
        task_id: &str,
        throttle: bool,
        retry_after_ms: Option<u64>,
    );

    /// Finalize any orphaned `running` session rows for a task after infra death.
    fn interrupt_running_sessions<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    /// Best-effort teardown of a terminal task-run's private Cargo target dir.
    fn teardown_cargo_target_run_dir<'a>(
        &'a self,
        task_run_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    /// Fire-and-forget post-session knowledge extraction.
    fn trigger_session_extraction(
        &self,
        task_id: String,
        task_run_id: String,
    );

    /// Best-effort reap of an orphaned `task_runs` row still in `running` status.
    fn reap_orphan_task_run<'a>(
        &'a self,
        task_id: &'a str,
        terminal_status: TaskRunStatus,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Opaque coordinator operations needed post-dispatch.
#[async_trait::async_trait]
pub trait CoordinatorOps: Send + Sync {
    async fn clear_planned_dispatch_completion(
        &self,
        task_id: &str,
        event: &str,
    ) -> anyhow::Result<()>;

    async fn route_loop_guard_planner_intervention(
        &self,
        task_id: &str,
        role: &str,
        reason: &str,
    ) -> anyhow::Result<()>;
}

// ─── Orchestrator ───────────────────────────────────────────────────────────

/// Canonical dispatch orchestrator — the single implementation of the
/// task-dispatch lifecycle.
///
/// This is the `djinn-slot` equivalent of the former
/// `djinn_agent::actors::slot::supervisor_runner::dispatch_task_runtime`.
/// All host-specific operations are delegated to the [`TaskDispatchContext`]
/// trait.
pub async fn dispatch_task_runtime<C: TaskDispatchContext>(
    ctx: &C,
    task_id: String,
    _project_path: String,
    model_id: String,
    kill: CancellationToken,
    _pause: CancellationToken,
) -> anyhow::Result<()> {
    // ── Load the task ─────────────────────────────────────────────────────
    let task = ctx.load_task(&task_id).await?;

    // ── Resolve dispatch context (conflict / review-response) ─────────────
    let (has_conflict, has_review_response) = ctx.resolve_dispatch_context(&task.id).await;

    // ── Resolve branches from project config ──────────────────────────────
    let base_branch = ctx.resolve_base_branch(&task.project_id).await;
    let task_branch = djinn_db::task_branch_name(&task.short_id);

    // ── Pick the supervisor flow ──────────────────────────────────────────
    let base_flow = ctx.resolve_flow(&task, has_conflict, has_review_response);

    // ── Stage-aware resume ────────────────────────────────────────────────
    let worker_output_durable = matches!(base_flow, djinn_runtime::SupervisorFlow::ReviewResponse)
        && ctx
            .check_worker_output_durability(&task.project_id, &task_branch, &base_branch)
            .await;
    let flow = resume_flow(base_flow, worker_output_durable);
    let loop_guard_intervention_role = flow
        .role_sequence()
        .first()
        .map(|role| role.as_str())
        .unwrap_or("worker");
    if matches!(flow, djinn_runtime::SupervisorFlow::ReviewResume) {
        tracing::info!(
            task_id = %task.short_id,
            branch = %task_branch,
            "supervisor dispatch: worker output durable on task_branch; \
             resuming at reviewer stage (skipping worker redo)"
        );
    }

    // ── Map flow → trigger ────────────────────────────────────────────────
    let trigger = if has_conflict {
        TaskRunTrigger::ConflictRetry
    } else if matches!(
        flow,
        djinn_runtime::SupervisorFlow::ReviewResponse
            | djinn_runtime::SupervisorFlow::ReviewResume
    ) {
        TaskRunTrigger::ReviewResponse
    } else {
        TaskRunTrigger::NewTask
    };

    // ── Resolve per-role model ids ────────────────────────────────────────
    let model_id_per_role = ctx
        .resolve_model_id_per_role(&task.project_id, flow, &model_id)
        .await;

    // ── Build the spec ────────────────────────────────────────────────────
    let task_run_id = uuid::Uuid::now_v7().to_string();

    let read_source_project_ids = ctx
        .resolve_read_sources(task.epic_id.as_deref())
        .await;

    let (github_owner, github_install_token) =
        ctx.resolve_private_deps(&task.project_id).await;

    let created_by_user_id = ctx.resolve_creator_user_id(&task.id).await;
    let (commit_author_name, commit_author_email) = ctx
        .resolve_commit_author(created_by_user_id.as_deref())
        .await;

    let spec = TaskRunSpec {
        task_run_id,
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        trigger,
        base_branch,
        task_branch,
        flow,
        model_id_per_role,
        read_source_project_ids,
        github_owner,
        github_install_token,
        commit_author_name,
        commit_author_email,
    };

    // ── Resolve per-role provider credentials ─────────────────────────────
    let creator_scope = created_by_user_id.clone();
    let credentials = ctx
        .resolve_credentials(&spec, &model_id, created_by_user_id)
        .await?;

    // ── Resolve the runtime ───────────────────────────────────────────────
    let runtime = ctx.construct_runtime(&task, &spec, &kill).await?;

    // ── Drive prepare → (await report) → teardown ─────────────────────────
    let handle = runtime
        .prepare(&spec, &credentials)
        .await
        .map_err(|e| anyhow::anyhow!("runtime.prepare failed: {e}"))?;

    // Kill token fires cancel through the runtime.
    let cancel_runtime = runtime.clone();
    let cancel_handle = handle.clone();
    let cancel_task_id = task.id.clone();
    let cancel_model_id = model_id.clone();
    let cancel_session_id = spec.task_run_id.clone();
    let cancel_task = tokio::spawn({
        let kill = kill.clone();
        async move {
            kill.cancelled().await;
            let span = tracing::info_span!(
                "djinn.slot.kill",
                task_id = %cancel_task_id,
                model_id = %cancel_model_id,
            );
            async move {
                tracing::info!(
                    event = "slot.runtime_cancel",
                    task_id = %cancel_task_id,
                    model_id = %cancel_model_id,
                );
                let rpc_span =
                    supervisor_rpc_span("kill", &cancel_session_id, &cancel_task_id);
                async move {
                    tracing::info!(
                        event = "supervisor.rpc.cancel",
                        op = "kill",
                        session_id = %cancel_session_id,
                        task_id = %cancel_task_id,
                    );
                    let _ = cancel_runtime.cancel(&cancel_handle).await;
                }
                .instrument(rpc_span)
                .await;
            }
            .instrument(span)
            .await;
        }
    });

    // Consume the worker's terminal report off the BiStream.
    let bistream_result = runtime.attach_stdio(&handle).await;
    let handshake_timed_out = matches!(
        &bistream_result,
        Err(djinn_runtime::RuntimeError::HandshakeTimeout(_))
    );

    let mut infra_death: Option<String> = None;
    let report_result: anyhow::Result<Option<TaskRunReport>> = match bistream_result {
        Ok(bistream) => {
            tokio::select! {
                biased;
                res = await_report_from_stream(bistream, &kill, &spec.task_run_id, &spec.task_id) => res,
                reason = runtime.watch_infra_death(&handle) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        %reason,
                        "supervisor dispatch: worker infra died before terminal report \
                         (OOM / eviction / Job failure); finalizing run as interrupted"
                    );
                    infra_death = Some(reason);
                    Ok(None)
                }
            }
        }
        Err(e) => Err(anyhow::anyhow!("runtime.attach_stdio failed: {e}")),
    };

    // Stop the cancel watcher regardless of success path.
    cancel_task.abort();
    let _ = cancel_task.await;

    let teardown = runtime.teardown(handle).await;

    // Best-effort: stamp orphaned `task_runs` row.
    let reap_status = teardown
        .as_ref()
        .ok()
        .map(report_to_terminal_status)
        .unwrap_or(TaskRunStatus::Interrupted);
    reap_orphan_task_run(ctx, &task.id, reap_status).await;

    // Best-effort: teardown of terminal task-run's private Cargo target dir.
    ctx.teardown_cargo_target_run_dir(&spec.task_run_id).await;

    // D2: handshake timeout → stall + failover.
    if handshake_timed_out {
        ctx.record_model_stall(creator_scope.as_deref(), &model_id, true);
        ctx.note_task_provider_failure(&task.id, true, None);
        ctx.log_agent_activity(
            &task.id,
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

    // Infra death → finalize orphaned running session row.
    if let Some(reason) = infra_death.as_deref() {
        let payload = serde_json::json!({
            "error": format!("Worker infrastructure died before completing the run: {reason}"),
            "agent_type": "system",
        })
        .to_string();
        ctx.log_agent_activity(
            &task.id,
            "agent-supervisor",
            "system",
            "session_error",
            &payload,
        )
        .await;
        ctx.interrupt_running_sessions(&task.id).await;
    }

    match (report_result, teardown) {
        (Ok(streamed), Ok(teardown_report)) => {
            let report = select_terminal_report(streamed, teardown_report);
            tracing::info!(
                task_id = %task.short_id,
                task_run_id = %report.task_run_id,
                outcome = ?report.outcome,
                stages_completed = ?report.stages_completed,
                "supervisor dispatch: task-run complete"
            );

            // Persist loop guard activity.
            persist_loop_guard_activity(ctx, &task.id, &report).await;

            // Feed the model circuit-breaker on a productive run.
            if terminal_report_feeds_model_success(&report) {
                ctx.record_model_success(creator_scope.as_deref(), &model_id);
            }

            // Feed breaker on typed provider failures.
            if let Some(class) = provider_failure_class_for_report(&report) {
                let (is_throttle, retry_after_ms) = match class {
                    ProviderFailureClass::Throttle { retry_after_ms } => {
                        ctx.record_model_stall(creator_scope.as_deref(), &model_id, false);
                        (true, retry_after_ms)
                    }
                    ProviderFailureClass::Failure => {
                        ctx.record_model_failure(creator_scope.as_deref(), &model_id);
                        (false, None)
                    }
                    ProviderFailureClass::AuthInvalid => {
                        if ctx.try_refresh_oauth_after_401(&model_id).await {
                            ctx.record_model_stall(creator_scope.as_deref(), &model_id, false);
                            (true, None)
                        } else {
                            ctx.record_model_stall(creator_scope.as_deref(), &model_id, true);
                            ctx.surface_credential_revocation(
                                creator_scope.as_deref(),
                                &model_id,
                            )
                            .await;
                            (true, None)
                        }
                    }
                };
                ctx.note_task_provider_failure(&task.id, is_throttle, retry_after_ms);
            }

            // Budget-park dispatch state clear.
            if is_budget_park_report(&report) {
                if let Some(coordinator) = ctx.get_coordinator().await {
                    if let Err(e) = coordinator
                        .clear_planned_dispatch_completion(
                            &task.id,
                            "budget_park_planned_completion_clear",
                        )
                        .await
                    {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "supervisor dispatch: failed to clear budget-park dispatch state"
                        );
                    }
                }
            }

            // Loop guard → planner intervention.
            if let TaskRunOutcome::LoopGuardTripped {
                kind,
                offending_signature,
                threshold,
                observed,
                turn_span,
                session_id,
            } = &report.outcome
            {
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
                if let Some(coordinator) = ctx.get_coordinator().await {
                    if let Err(e) = coordinator
                        .route_loop_guard_planner_intervention(
                            &task.id,
                            loop_guard_intervention_role,
                            &reason,
                        )
                        .await
                    {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "supervisor dispatch: failed to enqueue loop-guard Planner intervention"
                        );
                    }
                }
            }

            // Post-session knowledge extraction.
            if !report.stages_completed.is_empty() {
                ctx.trigger_session_extraction(
                    task.id.clone(),
                    report.task_run_id.clone(),
                );
            }
            Ok(())
        }
        (Err(e), teardown_result) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                teardown_ok = teardown_result.is_ok(),
                "supervisor dispatch: pre-teardown failure"
            );
            Err(e)
        }
        (Ok(_streamed), Err(e)) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "supervisor dispatch: teardown failure"
            );
            Err(anyhow::anyhow!("runtime.teardown failed: {e}"))
        }
    }
}

/// Best-effort reap of an orphaned `task_runs` row still in `running` status.
async fn reap_orphan_task_run<C: TaskDispatchContext>(
    ctx: &C,
    task_id: &str,
    terminal_status: TaskRunStatus,
) {
    ctx.reap_orphan_task_run(task_id, terminal_status).await;
}

/// Persist loop guard trip details as a task activity entry.
async fn persist_loop_guard_activity<C: TaskDispatchContext>(
    ctx: &C,
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

    ctx.log_agent_activity(
        task_id,
        "agent-supervisor",
        "system",
        "loop_guard_tripped",
        &payload,
    )
    .await;
}

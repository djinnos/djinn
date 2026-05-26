//! Default slot runner: routes dispatch through a
//! [`djinn_runtime::SessionRuntime`] chosen at startup by
//! [`crate::runtime_bridge::runtime_kind`].
//!
//! This is the Phase 2 K8s PR 4 pt2 cutover.  Previously (Phase 1 /
//! PR 4 pt1) this function constructed `Arc<dyn SupervisorServices>` and
//! called `TaskRunSupervisor::new(...).run(spec)` directly in-process.  That
//! path is now relegated to [`djinn_runtime::TestRuntime`] wrapping a
//! [`crate::runtime_bridge::SupervisorTaskRunner`] — which is the path
//! `DJINN_RUNTIME=test` selects and the path the integration tests exercise.
//! The production default (`DJINN_RUNTIME` unset / `"kubernetes"`) constructs
//! a [`djinn_k8s::KubernetesRuntime`] and drives
//! `prepare → await_report → teardown`.
//!
//! The runner receives the same arguments as the legacy runner
//! (`task_id`, `project_path`, `model_id`, `app_state`, `kill`, `pause`) so
//! it drops into the existing `SlotHandle::spawn` seam unchanged.  It
//! translates those into a [`TaskRunSpec`] and drives the runtime; the
//! returned [`djinn_runtime::TaskRunReport`] is collapsed to
//! `anyhow::Result<()>` for the slot actor's `JoinHandle`.
//!
//! `pause` is accepted for signature parity but the supervisor-driven flow
//! owns the whole run and does not release the slot between stages — there
//! is no external pause/resume handoff, so we just drop the token.  `kill`
//! is threaded into [`crate::supervisor::SupervisorServices::cancel`] (for
//! the Test path) and used to drive [`SessionRuntime::cancel`] (for the K8s
//! path).

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{TaskRepository, task_branch_name};
use djinn_runtime::{
    BiStream, ResolvedCredentials, SessionRuntime, StreamEvent, TaskRunOutcome, TaskRunReport,
    TestRuntime,
};

use crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference;
use crate::context::AgentContext;
use crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind};
use crate::supervisor::{RoleKind, SupervisorFlow, TaskRunSpec, services_for_agent_context};

use super::helpers::{
    conflict_context_for_dispatch, default_target_branch, load_provider_credential, parse_model_id,
};

/// Default slot-dispatch runner.
///
/// Resolves `(task, flow, base_branch, task_branch, trigger)` from the task
/// row + ambient dispatch context, builds a [`TaskRunSpec`], then:
///
/// - on [`RuntimeKind::Kubernetes`]: constructs a
///   [`djinn_k8s::KubernetesRuntime`] and drives `prepare → teardown` — the
///   worker Pod connects back to djinn-server's TCP listener (bound at boot)
///   and streams events through `serve_on_tcp`'s dispatch.  The supervisor
///   body runs *inside the Pod*; the final `TaskRunReport` is synthesized
///   from the Job's terminal state during `teardown`.
/// - on [`RuntimeKind::Test`]: constructs a [`TestRuntime`] wrapping a
///   [`SupervisorTaskRunner`] — the supervisor runs in-process and the
///   terminal report rides the in-memory `BiStream`.
///
/// Returns:
/// - `Ok(())` on any terminal runtime outcome.  The slot actor treats that as
///   `SlotEvent::Free`; the supervisor has already written the
///   task_run/session/task rows, so there is nothing else for the slot to do.
/// - `Err(..)` only for infra-level setup failures the runtime cannot
///   express through a `TaskRunReport` (task lookup failed, mirror not
///   configured, runtime construction error).  The slot actor logs the
///   error and still emits `SlotEvent::Free`.
pub(crate) async fn run_supervisor_dispatch(
    task_id: String,
    _project_path: String,
    model_id: String,
    app_state: AgentContext,
    kill: CancellationToken,
    _pause: CancellationToken,
) -> anyhow::Result<()> {
    // ── Load the task ─────────────────────────────────────────────────────
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match task_repo.get(&task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            anyhow::bail!("supervisor dispatch: task {task_id} not found");
        }
        Err(e) => {
            anyhow::bail!("supervisor dispatch: failed to load task {task_id}: {e}");
        }
    };

    // ── Resolve dispatch context (conflict / review-response) ─────────────
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let has_conflict = conflict_ctx.is_some();
    let has_review_response = matches!(
        task.status.as_str(),
        "needs_task_review" | "in_task_review"
    );

    // ── Pick the supervisor flow ──────────────────────────────────────────
    let flow = crate::roles::flow_for_task_dispatch(&task, has_conflict, has_review_response);

    // ── Map flow → trigger ────────────────────────────────────────────────
    let trigger = if has_conflict {
        TaskRunTrigger::ConflictRetry
    } else if matches!(flow, SupervisorFlow::ReviewResponse) {
        TaskRunTrigger::ReviewResponse
    } else {
        TaskRunTrigger::NewTask
    };

    // ── Resolve branches from project config ──────────────────────────────
    let base_branch = default_target_branch(&task.project_id, &app_state).await;
    let task_branch = task_branch_name(&task.short_id);

    // ── Resolve per-role model ids ────────────────────────────────────────
    let mut model_id_per_role: HashMap<RoleKind, String> = HashMap::new();
    for role in flow.role_sequence() {
        let resolved =
            resolve_role_model_preference(&task.project_id, role.as_str(), &app_state)
                .await
                .unwrap_or_else(|| model_id.clone());
        model_id_per_role.insert(*role, resolved);
    }

    // ── Build the spec ────────────────────────────────────────────────────
    let spec = TaskRunSpec {
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        trigger,
        base_branch,
        task_branch,
        flow,
        model_id_per_role,
    };

    // ── Resolve the task's creator for per-user credential scoping ────────
    //
    // Per-user provider credentials (migration 28) resolve against the acting
    // user via the `SESSION_USER_ID` task-local. The worker dispatch path has
    // no inbound HTTP request to inherit that from, so we explicitly set it to
    // the task's `created_by_user_id` — a task uses ITS CREATOR's credential,
    // falling back to the org-shared one when the column is NULL (background /
    // pre-multiuser tasks). The `Task` model doesn't surface this column, so
    // read it directly. A lookup error or missing column is non-fatal: we just
    // resolve org-shared, preserving historical behaviour.
    let created_by_user_id: Option<String> = sqlx::query_scalar!(
        "SELECT created_by_user_id FROM tasks WHERE id = $1",
        task.id
    )
    .fetch_optional(app_state.db.pool())
    .await
    .ok()
    .flatten()
    .flatten();

    // ── Resolve per-role provider credentials (Phase 7a) ──────────────────
    //
    // The host pulls every role's credential from the vault (or OAuth token
    // store) and ships them into the worker Pod via the per-task-run K8s
    // Secret. Fast-fail on resolution errors so the operator sees a clean
    // dispatch-time failure in session logs instead of a Pod that crash-loops
    // because the model client can't authenticate.
    //
    // The whole resolution loop runs under `SESSION_USER_ID = task creator` so
    // every `load_provider_credential` → `get_decrypted` (and the codex/copilot
    // OAuth token loads) resolves that user's private credential first.
    let mut credentials = ResolvedCredentials::default();
    let resolve_creds = async {
        for role in spec.flow.role_sequence() {
            let model_id = spec
                .model_id_per_role
                .get(role)
                .cloned()
                .unwrap_or_else(|| model_id.clone());
            let (provider_id, _model_name) = parse_model_id(&model_id).map_err(|e| {
                anyhow::anyhow!(
                    "supervisor dispatch: cannot parse model id `{model_id}` for role {role:?}: {e}"
                )
            })?;
            let cred = load_provider_credential(&provider_id, &app_state)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "supervisor dispatch: load_provider_credential({provider_id}) for role {role:?}: {e}"
                    )
                })?;
            credentials.insert(*role, cred.to_serializable());
        }
        Ok::<(), anyhow::Error>(())
    };
    djinn_core::auth_context::SESSION_USER_ID
        .scope(created_by_user_id, resolve_creds)
        .await?;

    // ── Resolve the runtime ───────────────────────────────────────────────
    let mirror = match app_state.mirror.as_ref() {
        Some(m) => m.clone(),
        None => {
            anyhow::bail!(
                "supervisor dispatch: AgentContext has no MirrorManager configured — \
                 cannot run supervisor-driven task-run for task {}",
                task.short_id
            );
        }
    };
    let runtime_kind = runtime_kind();

    let runtime: Arc<dyn SessionRuntime> = match runtime_kind {
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
            match djinn_k8s::KubernetesRuntime::with_db(
                config,
                registry,
                app_state.db.clone(),
            )
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

    // ── Drive prepare → (await report) → teardown ─────────────────────────
    let handle = runtime
        .prepare(&spec, &credentials)
        .await
        .map_err(|e| anyhow::anyhow!("runtime.prepare failed: {e}"))?;

    // Kill token fires cancel through the runtime.
    let cancel_runtime = runtime.clone();
    let cancel_handle = handle.clone();
    let cancel_task = tokio::spawn({
        let kill = kill.clone();
        async move {
            kill.cancelled().await;
            let _ = cancel_runtime.cancel(&cancel_handle).await;
        }
    });

    let bistream_result = runtime.attach_stdio(&handle).await;
    let report_result = match runtime_kind {
        RuntimeKind::Test => match bistream_result {
            Ok(bistream) => await_report_from_stream(bistream).await,
            Err(e) => Err(anyhow::anyhow!("runtime.attach_stdio failed: {e}")),
        },
        RuntimeKind::Kubernetes => {
            // PR 4 pt2: the K8s attach_stdio is still a detached placeholder
            // (the real BiStream is fed by the launcher-side TCP dispatch,
            // which `serve_on_tcp` owns at djinn-server boot).  Fall back to
            // synthesizing the terminal TaskRunReport from the Job's
            // terminal state — that's exactly what KubernetesRuntime::teardown
            // already computes.  Formalising the BiStream hand-off between
            // serve_on_tcp and the dispatch loop is the follow-up PR.
            //
            // We still attach for its side effects (object-safety + future
            // compatibility) but ignore the returned stream.
            let _ = bistream_result;
            Ok(())
        }
    };

    // Stop the cancel watcher regardless of success path.
    cancel_task.abort();
    let _ = cancel_task.await;

    let teardown = runtime.teardown(handle).await;

    // Best-effort: if the in-pod supervisor died before sending its terminal
    // `update_task_run_status` RPC (OOM, eviction, SIGKILL past the grace
    // window), the matching `task_runs` row is still 'running' in the host
    // DB. Stamp it now using the terminal status the teardown report
    // synthesized — defaulting to `Interrupted` when the report is missing.
    // Slot serialization means at most one in-flight run per task, so
    // reaping by `task_id` finds exactly the right row.
    let reap_status = teardown
        .as_ref()
        .ok()
        .map(report_to_terminal_status)
        .unwrap_or(TaskRunStatus::Interrupted);
    reap_orphan_task_run(&app_state, &task.id, reap_status).await;

    match (report_result, teardown) {
        (Ok(()), Ok(report)) => {
            tracing::info!(
                task_id = %task.short_id,
                task_run_id = %report.task_run_id,
                outcome = ?report.outcome,
                stages_completed = ?report.stages_completed,
                runtime = ?runtime_kind,
                "supervisor dispatch: task-run complete"
            );
            // Phase 2.2: post-session knowledge extraction. Fire-and-forget on
            // the long-lived server (it owns the embedding model + Qdrant, so
            // notes created here get embedded; worker pods are ephemeral and
            // lack that config). Gated on real work having run — skip
            // interrupted/empty runs so we don't burn an LLM call on nothing.
            // Extraction is fully isolated: any failure is logged and never
            // affects the task-run outcome.
            if !report.stages_completed.is_empty() {
                let app_state_ext = app_state.clone();
                let task_id_ext = task.id.clone();
                let task_run_id_ext = report.task_run_id.clone();
                tokio::spawn(async move {
                    crate::actors::slot::session_extraction::run_post_session_extraction(
                        task_id_ext,
                        task_run_id_ext,
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
        (Ok(()), Err(e)) => {
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

fn report_to_terminal_status(report: &TaskRunReport) -> TaskRunStatus {
    match &report.outcome {
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        | TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
        TaskRunOutcome::Failed { .. } => TaskRunStatus::Failed,
        TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
    }
}

async fn reap_orphan_task_run(
    app_state: &AgentContext,
    task_id: &str,
    terminal_status: TaskRunStatus,
) {
    let repo = TaskRunRepository::new(app_state.db.clone());
    match repo.reap_running_for_task(task_id, terminal_status).await {
        Ok(Some(run_id)) => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %run_id,
                status = %terminal_status,
                "supervisor dispatch: reaped orphan task_run row \
                 (in-pod supervisor never sent terminal RPC)"
            );
        }
        Ok(None) => {
            // Common path: the in-pod supervisor's terminal RPC already
            // flipped the row. Nothing to do.
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "supervisor dispatch: reap_running_for_task failed"
            );
        }
    }
}

/// Drain a [`BiStream`] until we see a [`StreamEvent::Report`] frame.
///
/// Used by the TestRuntime path — `TestRuntime` forwards the
/// [`TaskRunReport`] produced by [`SupervisorTaskRunner`] as a terminal
/// `StreamEvent::Report` on `events_rx` before closing the channel.  We drop
/// non-terminal frames (they're already observed via the event-bus /
/// tracing seams in-process).
async fn await_report_from_stream(mut stream: BiStream) -> anyhow::Result<()> {
    while let Some(frame) = stream.events_rx.recv().await {
        match frame {
            StreamEvent::Report(_report) => {
                // The terminal report is the signal the run is done; the
                // supervisor has already persisted state.  Nothing to do
                // here beyond returning success.
                return Ok(());
            }
            other => {
                tracing::trace!(event = ?other, "supervisor dispatch: dropping non-terminal frame");
            }
        }
    }
    // Channel closed without a terminal report — treat as success; the
    // supervisor path persists state as a side effect, and TestRuntime's
    // `teardown` synthesizes a report from the join handle anyway.
    Ok(())
}


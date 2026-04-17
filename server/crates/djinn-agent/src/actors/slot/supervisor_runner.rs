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

use djinn_core::models::TaskRunTrigger;
use djinn_db::{TaskRepository, task_branch_name};
use djinn_runtime::{BiStream, SessionRuntime, StreamEvent, TestRuntime};

use crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference;
use crate::context::AgentContext;
use crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind};
use crate::supervisor::{RoleKind, SupervisorFlow, TaskRunSpec, services_for_agent_context};

use super::helpers::{conflict_context_for_dispatch, default_target_branch};

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
    let task_runs = Arc::new(djinn_db::repositories::task_run::TaskRunRepository::new(
        app_state.db.clone(),
    ));

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
            match djinn_k8s::KubernetesRuntime::new(config, registry).await {
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
            let runner = SupervisorTaskRunner::new(task_runs.clone(), mirror.clone(), services);
            Arc::new(TestRuntime::new(runner))
        }
    };

    // ── Drive prepare → (await report) → teardown ─────────────────────────
    let handle = runtime
        .prepare(&spec)
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


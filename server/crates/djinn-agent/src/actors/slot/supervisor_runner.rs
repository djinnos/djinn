//! Default slot runner: routes dispatch through
//! [`crate::supervisor::TaskRunSupervisor`] instead of the legacy
//! [`crate::actors::slot::lifecycle::run_task_lifecycle`].
//!
//! This is the task #7 switch — one slot dispatch = one task-run that spans
//! the entire role sequence (planner → worker → reviewer → verifier or the
//! flow-specific sequence), rather than one slot per agent stage.
//!
//! The runner receives the same arguments as the legacy runner
//! (`task_id`, `project_path`, `model_id`, `app_state`, `kill`, `pause`) so
//! it drops into the existing `SlotHandle::spawn` seam unchanged.  It
//! translates those into a [`TaskRunSpec`] and drives the supervisor; the
//! returned [`crate::supervisor::TaskRunReport`] is collapsed to
//! `anyhow::Result<()>` for the slot actor's `JoinHandle`.
//!
//! `pause` is accepted for signature parity but the supervisor-driven flow
//! owns the whole run and does not release the slot between stages — there
//! is no external pause/resume handoff, so we just drop the token.  `kill`
//! is threaded into [`crate::supervisor::SupervisorServices::cancel`].
//!
//! If the deployment lacks a configured `MirrorManager` (tests, off-server
//! contexts) the runner fails loudly — production always wires a mirror via
//! `AppState::agent_context()`, and the legacy path is unused in production
//! as of this commit (task #7).  Tests that need the old lifecycle use
//! [`crate::actors::slot::SlotHandle::spawn_with_test_runner`] directly.

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use djinn_core::models::TaskRunTrigger;
use djinn_db::{TaskRepository, task_branch_name};

use crate::context::AgentContext;
use crate::supervisor::{
    RoleKind, SupervisorFlow, SupervisorServices, TaskRunSpec, TaskRunSupervisor,
};

use super::helpers::{conflict_context_for_dispatch, default_target_branch};

/// Default slot-dispatch runner.
///
/// Resolves `(task, flow, base_branch, task_branch, trigger)` from the task
/// row + ambient dispatch context, builds a [`TaskRunSpec`], and drives
/// [`TaskRunSupervisor::run`] to completion.
///
/// Returns:
/// - `Ok(())` on any terminal supervisor outcome (`PrOpened`, `Closed`,
///   `Escalated`, `Failed`, `Interrupted`).  The slot actor treats that as
///   `SlotEvent::Free`; the supervisor has already written the
///   task_run/session/task rows, so there is nothing else for the slot to do.
/// - `Err(..)` only for infra-level setup failures the supervisor cannot
///   express through a `TaskRunReport` (task lookup failed, mirror not
///   configured, supervisor construction error).  The slot actor logs the
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
    } else if matches!(
        flow,
        SupervisorFlow::ReviewResponse
    ) {
        TaskRunTrigger::ReviewResponse
    } else {
        TaskRunTrigger::NewTask
    };

    // ── Resolve branches from project config ──────────────────────────────
    let base_branch = default_target_branch(&task.project_id, &app_state).await;
    let task_branch = task_branch_name(&task.short_id);

    // ── Map dispatch model to every stage in the flow ─────────────────────
    //
    // Minimum-viable model selection for task #7 (matches the spec note):
    // the coordinator dispatches one slot with one model, chosen for the
    // role resolved by `role_for_task_dispatch`.  The supervisor then runs
    // the whole flow in that slot.  We seed `model_id_per_role` with the
    // dispatched `model_id` for every `RoleKind` in the flow's sequence so
    // every stage has a resolved model without a second coordinator
    // round-trip.  Per-role model priorities (the richer routing the
    // coordinator currently owns via `resolve_role_model_preference`) can
    // be threaded in as a follow-up when stage-level model diversity
    // becomes necessary.
    let mut model_id_per_role: HashMap<RoleKind, String> = HashMap::new();
    for role in flow.role_sequence() {
        model_id_per_role.insert(*role, model_id.clone());
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

    // ── Construct the supervisor ──────────────────────────────────────────
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
    let services = SupervisorServices::new(app_state.clone(), kill);
    let supervisor = TaskRunSupervisor::new(task_runs, mirror, services);

    // ── Run the task-run to terminal state ────────────────────────────────
    match supervisor.run(spec).await {
        Ok(report) => {
            tracing::info!(
                task_id = %task.short_id,
                task_run_id = %report.task_run_id,
                outcome = ?report.outcome,
                stages_completed = ?report.stages_completed,
                "supervisor dispatch: task-run complete"
            );
            Ok(())
        }
        Err(e) => {
            // Infra-level supervisor failure (e.g. mirror clone error, DB
            // write failed).  Surface as an error so the slot actor logs
            // it; the slot still emits SlotEvent::Free afterwards.
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "supervisor dispatch: supervisor run failed"
            );
            Err(anyhow::anyhow!("supervisor run failed: {e}"))
        }
    }
}

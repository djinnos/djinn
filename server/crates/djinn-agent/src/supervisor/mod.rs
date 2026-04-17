//! Task-run supervisor: orchestrates one full execution of a task across its
//! role sequence (planner → worker → reviewer → verifier → open PR, or
//! variants for spikes / conflict retries / review responses).
//!
//! This replaces per-agent-dispatch lifecycle: today's `run_task_lifecycle`
//! runs once per role; a `TaskRunSupervisor` runs once per *task-run* and
//! internally sequences the role phases, reusing the same workspace across
//! them so warm build caches compound naturally between stages.
//!
//! Scaffolding: the `run` method currently creates the `TaskRunRecord`,
//! clones an ephemeral workspace, and returns a stub report. Wiring the
//! per-role execution to the existing `reply_loop` machinery is the next
//! task on the Phase 1 board (see `coordinator` dispatch rewrite).

use std::sync::Arc;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::TaskRunRepository;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_workspace::{MirrorError, MirrorManager, Workspace, WorkspaceError};
use thiserror::Error;
use tracing::{debug, info};

pub use self::flow::{RoleKind, SupervisorFlow};
pub use self::spec::{TaskRunOutcome, TaskRunReport, TaskRunSpec};

mod flow;
mod spec;

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("mirror: {0}")]
    Mirror(#[from] MirrorError),

    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceError),

    #[error("db: {0}")]
    Db(#[from] djinn_db::Error),

    #[error("stage not yet implemented: {0}")]
    StageUnwired(String),
}

pub struct TaskRunSupervisor {
    task_runs: Arc<TaskRunRepository>,
    mirror: Arc<MirrorManager>,
}

impl TaskRunSupervisor {
    pub fn new(task_runs: Arc<TaskRunRepository>, mirror: Arc<MirrorManager>) -> Self {
        Self { task_runs, mirror }
    }

    /// Drive a task-run from start to terminal state.
    ///
    /// Stages are currently stubbed — see `TODO stage execution` below. The
    /// supervisor skeleton already owns the TaskRunRecord lifecycle and the
    /// workspace clone, which are the parts most-coupled to other subsystems
    /// (DB, mirror, TempDir semantics). Per-stage wiring to `reply_loop`
    /// lands in the coordinator-rewrite task.
    pub async fn run(&self, spec: TaskRunSpec) -> Result<TaskRunReport, SupervisorError> {
        let run_id = uuid::Uuid::now_v7().to_string();
        let trigger_str = spec.trigger.as_str().to_string();

        info!(
            task_run_id = %run_id,
            task_id = %spec.task_id,
            flow = ?spec.flow,
            "task-run starting"
        );

        self.task_runs
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &spec.project_id,
                task_id: &spec.task_id,
                trigger_type: &trigger_str,
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await?;

        // Clone once; workspace is shared across stages so cargo target/ and
        // node_modules compound between planner/worker/reviewer/verifier.
        let workspace = self
            .mirror
            .clone_ephemeral(&spec.project_id, &spec.base_branch)
            .await?;
        debug!(task_run_id = %run_id, path = ?workspace.path(), "ephemeral workspace ready");

        let sequence = spec.flow.role_sequence();
        let mut completed: Vec<RoleKind> = Vec::new();
        let outcome = self.run_sequence(&spec, &workspace, sequence, &mut completed).await?;

        let terminal_status = match &outcome {
            TaskRunOutcome::PrOpened { .. } | TaskRunOutcome::Closed { .. } => {
                TaskRunStatus::Completed
            }
            TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
            TaskRunOutcome::Failed { .. } => TaskRunStatus::Failed,
            TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
        };
        self.task_runs
            .update_status(&run_id, terminal_status)
            .await?;

        info!(task_run_id = %run_id, ?outcome, "task-run finished");
        Ok(TaskRunReport {
            task_run_id: run_id,
            outcome,
            stages_completed: completed,
        })
    }

    /// Execute the role sequence against the shared workspace.
    ///
    /// **Scaffold only.** Each stage must eventually:
    ///   - Create a child `SessionRecord` (`sessions.task_run_id = run.id`)
    ///   - Invoke the role's agent loop via `reply_loop`
    ///   - Interpret the role's output (planner's execute/close/escalate,
    ///     reviewer's pass/block, verifier's pass/fail, etc.)
    ///   - Branch or terminate the flow based on outcome
    ///
    /// For now it returns `StageUnwired` so callers can compile-integrate
    /// the supervisor without deadlocking on the full wiring landing.
    async fn run_sequence(
        &self,
        _spec: &TaskRunSpec,
        _workspace: &Workspace,
        sequence: &[RoleKind],
        _completed: &mut Vec<RoleKind>,
    ) -> Result<TaskRunOutcome, SupervisorError> {
        let first = sequence.first().copied().unwrap_or(RoleKind::Worker);
        Err(SupervisorError::StageUnwired(format!(
            "role sequence execution not yet wired (first stage: {first:?})"
        )))
    }
}

/// Convenience helper so the supervisor's trigger vocabulary travels cleanly
/// to the `TaskRunRecord` column. Mirrors `TaskRunTrigger::as_str` but takes
/// ownership-free.
#[inline]
pub fn trigger_as_str(t: TaskRunTrigger) -> &'static str {
    t.as_str()
}

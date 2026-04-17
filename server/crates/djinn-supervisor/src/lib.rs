//! `djinn-supervisor` — task-run orchestration body extracted from
//! `djinn-agent::supervisor` during Phase 2 PR 2 of
//! `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! This crate owns the orchestration skeleton (`TaskRunSupervisor`,
//! `SupervisorServices`, `StageOutcome`, `StageError`, `SupervisorError`) but
//! does **not** depend on `djinn-agent` — that would be a cycle because
//! `djinn-agent` now re-exports this crate under `djinn_agent::supervisor::*`.
//!
//! ## Phase 2 PR 3: SupervisorServices is a trait
//!
//! PR 2 left `SupervisorServices` as a struct-with-callbacks (`Arc<dyn Fn …>`
//! fields for `load_task_fn` / `execute_stage_fn` / `open_pr_fn`). PR 3 swaps
//! that shape for an object-safe trait (see [`services::SupervisorServices`])
//! with two impls:
//!
//! - `djinn_agent::direct_services::DirectServices` — wraps `AgentContext`,
//!   delegates straight into the in-tree lifecycle helpers. Production path
//!   and the `phase1_supervisor` integration test.
//! - [`services::rpc::StubRpcServices`] — a placeholder that pins the trait
//!   layout ahead of PR 4/5's real bincode-over-unix-socket worker wiring.
//!   Every method `unimplemented!()`s today.
//!
//! The supervisor holds the services behind `Arc<dyn SupervisorServices>`
//! (rather than a generic `S: SupervisorServices`) because PR 4/5's dispatch
//! story reuses the same `Arc` plumbing on the host side to hand the
//! supervisor to a `SessionRuntime`.

use std::sync::Arc;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::TaskRunRepository;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_workspace::{MirrorError, MirrorManager, WorkspaceError};
use thiserror::Error;
use tracing::{debug, info};

pub mod services;

pub use services::SupervisorServices;
pub use services::rpc::StubRpcServices;

// Re-export runtime spec types at the crate root so the thin
// `djinn_agent::supervisor` shim preserves every existing import path.
pub use djinn_runtime::spec::{
    RoleKind, SupervisorFlow, TaskRunOutcome, TaskRunReport, TaskRunSpec, role_sequence,
};

// ── Error types ──────────────────────────────────────────────────────────────

/// Failure from [`TaskRunSupervisor::run`] *before* a stage can return a
/// typed [`StageOutcome`]. Errors that occur inside a stage and are
/// recoverable at the supervisor level arrive as a [`StageOutcome::Failed`]
/// variant instead.
#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("mirror: {0}")]
    Mirror(#[from] MirrorError),

    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceError),

    #[error("db: {0}")]
    Db(#[from] djinn_db::Error),

    #[error("load task: {0}")]
    LoadTask(String),

    #[error("stage: {0}")]
    Stage(#[from] StageError),
}

/// Pre-reply-loop failure surfaced by [`SupervisorServices::execute_stage`].
/// Always fatal for the task-run.
#[derive(Debug, Error)]
pub enum StageError {
    #[error("model resolution: {0}")]
    ModelResolution(String),

    #[error("setup/verification: {0}")]
    Setup(String),

    #[error("session create: {0}")]
    SessionCreate(String),
}

/// Outcome of executing one role stage.
#[derive(Clone, Debug)]
pub enum StageOutcome {
    WorkerDone,
    PlannerExecute,
    PlannerClose { reason: String },
    ReviewerApproved,
    ReviewerRejected { feedback: String },
    VerifierPassed,
    VerifierFailed { reason: String },
    ArchitectDone,
    Escalate { reason: String },
    Failed { reason: String },
}

impl StageOutcome {
    /// Whether this outcome should short-circuit the role sequence.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StageOutcome::PlannerClose { .. }
                | StageOutcome::Escalate { .. }
                | StageOutcome::Failed { .. }
                | StageOutcome::ReviewerRejected { .. }
                | StageOutcome::VerifierFailed { .. }
        )
    }
}

// ── TaskRunSupervisor ────────────────────────────────────────────────────────

pub struct TaskRunSupervisor {
    task_runs: Arc<TaskRunRepository>,
    mirror: Arc<MirrorManager>,
    services: Arc<dyn SupervisorServices>,
}

impl TaskRunSupervisor {
    /// Construct a supervisor bound to the given services.
    pub fn new(
        task_runs: Arc<TaskRunRepository>,
        mirror: Arc<MirrorManager>,
        services: Arc<dyn SupervisorServices>,
    ) -> Self {
        Self {
            task_runs,
            mirror,
            services,
        }
    }

    /// Drive a task-run from start to terminal state.
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

        let workspace = self
            .mirror
            .clone_ephemeral(&spec.project_id, &spec.base_branch)
            .await?;
        debug!(task_run_id = %run_id, path = ?workspace.path(), "ephemeral workspace ready");

        let task = self
            .services
            .load_task(spec.task_id.clone())
            .await
            .map_err(SupervisorError::LoadTask)?;

        let sequence = spec.flow.role_sequence();
        let mut completed: Vec<RoleKind> = Vec::new();
        let outcome = {
            let mut last_stage_role: Option<RoleKind> = None;
            let mut result: Option<TaskRunOutcome> = None;
            for &role_kind in sequence {
                if self.services.cancel().is_cancelled() {
                    result = Some(TaskRunOutcome::Interrupted);
                    break;
                }

                let stage_outcome = self
                    .services
                    .execute_stage(&task, &workspace, role_kind, &run_id, &spec)
                    .await?;

                last_stage_role = Some(role_kind);
                completed.push(role_kind);

                match stage_outcome {
                    StageOutcome::WorkerDone
                    | StageOutcome::PlannerExecute
                    | StageOutcome::ReviewerApproved
                    | StageOutcome::VerifierPassed
                    | StageOutcome::ArchitectDone => {
                        // Advance to the next stage.
                    }
                    StageOutcome::PlannerClose { reason } => {
                        result = Some(TaskRunOutcome::Closed { reason });
                        break;
                    }
                    StageOutcome::Escalate { reason } => {
                        result = Some(TaskRunOutcome::Escalated { reason });
                        break;
                    }
                    StageOutcome::ReviewerRejected { feedback } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: "reviewer".into(),
                            reason: format!("review rejected: {feedback}"),
                        });
                        break;
                    }
                    StageOutcome::VerifierFailed { reason } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: "verifier".into(),
                            reason,
                        });
                        break;
                    }
                    StageOutcome::Failed { reason } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: role_kind.as_str().into(),
                            reason,
                        });
                        break;
                    }
                }
            }

            match result {
                Some(r) => r,
                None => {
                    // All stages completed successfully.  Spike / Planning
                    // have no PR semantics; the merge-landing flows go
                    // through `open_pr`.
                    match spec.flow {
                        SupervisorFlow::Planning | SupervisorFlow::Spike => {
                            TaskRunOutcome::Closed {
                                reason: format!(
                                    "{} flow completed (last stage: {:?})",
                                    spec.flow.as_str(),
                                    last_stage_role
                                ),
                            }
                        }
                        SupervisorFlow::NewTask
                        | SupervisorFlow::ReviewResponse
                        | SupervisorFlow::ConflictRetry => {
                            self.services.open_pr(&spec, &task).await
                        }
                    }
                }
            }
        };

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
}

/// Convenience helper so the supervisor's trigger vocabulary travels cleanly
/// to the `TaskRunRecord` column.
#[inline]
pub fn trigger_as_str(t: TaskRunTrigger) -> &'static str {
    t.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time assertion: `SupervisorServices` is object-safe.
    ///
    /// PR 3 dispatches the supervisor through `Arc<dyn SupervisorServices>`,
    /// so the trait must stay object-safe forever. If a new method sneaks in
    /// with a generic parameter or a `Self`-by-value receiver, this function
    /// stops compiling.
    #[allow(dead_code)]
    fn _obj_safe(_: &dyn SupervisorServices) {}

    #[test]
    fn stage_outcome_terminal_classifier() {
        assert!(StageOutcome::Failed { reason: "x".into() }.is_terminal());
        assert!(StageOutcome::PlannerClose { reason: "x".into() }.is_terminal());
        assert!(StageOutcome::Escalate { reason: "x".into() }.is_terminal());
        assert!(StageOutcome::ReviewerRejected { feedback: "x".into() }.is_terminal());
        assert!(StageOutcome::VerifierFailed { reason: "x".into() }.is_terminal());
        assert!(!StageOutcome::WorkerDone.is_terminal());
        assert!(!StageOutcome::PlannerExecute.is_terminal());
        assert!(!StageOutcome::ReviewerApproved.is_terminal());
        assert!(!StageOutcome::VerifierPassed.is_terminal());
        assert!(!StageOutcome::ArchitectDone.is_terminal());
    }
}

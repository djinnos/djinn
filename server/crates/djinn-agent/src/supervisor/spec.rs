//! Supervisor input/output types.

use djinn_core::models::TaskRunTrigger;

use super::flow::{RoleKind, SupervisorFlow};

/// Input to `TaskRunSupervisor::run`.
///
/// All runtime-variable data the supervisor needs to execute one task-run.
/// Provider/model selection per role and MCP config will be added here as
/// per-role execution comes online.
#[derive(Clone, Debug)]
pub struct TaskRunSpec {
    pub task_id: String,
    pub project_id: String,
    pub trigger: TaskRunTrigger,
    /// Existing branch in the mirror to start from (e.g. `main`).
    pub base_branch: String,
    /// Branch the task-run commits onto; created locally from `base_branch`
    /// when needed. Pushed to origin at PR-open time.
    pub task_branch: String,
    pub flow: SupervisorFlow,
}

/// Terminal outcome of a task-run.
#[derive(Clone, Debug)]
pub enum TaskRunOutcome {
    PrOpened {
        url: String,
        sha: String,
    },
    /// Planner decided the task should not execute.
    Closed {
        reason: String,
    },
    /// Planner/architect surfaced a question that blocks automated execution
    /// (e.g. ambiguous scope, missing design decision).
    Escalated {
        reason: String,
    },
    Failed {
        stage: String,
        reason: String,
    },
    Interrupted,
}

/// Return value of `TaskRunSupervisor::run`.
#[derive(Clone, Debug)]
pub struct TaskRunReport {
    pub task_run_id: String,
    pub outcome: TaskRunOutcome,
    pub stages_completed: Vec<RoleKind>,
}

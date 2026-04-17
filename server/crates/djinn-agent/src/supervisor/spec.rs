//! Supervisor input/output types.

use std::collections::HashMap;

use djinn_core::models::TaskRunTrigger;

use super::flow::{RoleKind, SupervisorFlow};

/// Input to `TaskRunSupervisor::run`.
///
/// All runtime-variable data the supervisor needs to execute one task-run.
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
    /// Optional per-role model override.  When a [`RoleKind`] key is present,
    /// [`crate::supervisor::stage::execute_stage`] uses the mapped
    /// `provider/model` id for that stage instead of the catalog-default
    /// fallback.  The coordinator populates this from its per-role model
    /// resolution (dispatch priorities + project `model_preference`) so the
    /// supervisor path keeps parity with the legacy `run_task_lifecycle` model
    /// selection.  Empty = fall back to catalog-default for every stage.
    pub model_id_per_role: HashMap<RoleKind, String>,
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

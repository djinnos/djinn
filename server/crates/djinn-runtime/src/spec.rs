//! Wire-capable task-run spec + outcome types.
//!
//! These types were previously `djinn_agent::supervisor::{spec, flow}`; Phase
//! 2 PR 1 moved them here so that the future in-container supervisor (running
//! inside `djinn-agent-worker`) can share the exact type definitions with the
//! host-side coordinator without re-exporting them across an `AppState`-heavy
//! crate boundary.
//!
//! All types derive `Serialize + Deserialize` so they can ride a
//! `bincode::serialize`/`deserialize` frame between the host and the
//! container (bincode 1.3 is serde-driven, so no separate `Encode`/`Decode`
//! derives are needed).

use std::collections::HashMap;

use djinn_core::models::TaskRunTrigger;
use serde::{Deserialize, Serialize};

/// Which role executes at each stage of a task-run.
///
/// Not the same as `djinn-agent`'s existing `AgentRole` trait objects — this
/// is a lightweight enum suitable for flow templates and telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    Planner,
    Worker,
    Reviewer,
    Verifier,
    Architect,
}

impl RoleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::Planner => "planner",
            RoleKind::Worker => "worker",
            RoleKind::Reviewer => "reviewer",
            RoleKind::Verifier => "verifier",
            RoleKind::Architect => "architect",
        }
    }
}

/// Template for a task-run's role sequence.
///
/// `NewTask` is the canonical "work" flow: plan, execute, review, verify,
/// PR. `ReviewResponse` and `ConflictRetry` re-enter mid-flow when the
/// planner's decision is already implicit in the task's existence. `Spike`
/// routes the architect onto scoped research tasks the planner created
/// during a prior NewTask. `Planning` runs the planner alone — useful for
/// explicit "just re-plan this" invocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorFlow {
    NewTask,
    ReviewResponse,
    ConflictRetry,
    Spike,
    Planning,
}

impl SupervisorFlow {
    pub fn role_sequence(self) -> &'static [RoleKind] {
        role_sequence(self)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SupervisorFlow::NewTask => "new_task",
            SupervisorFlow::ReviewResponse => "review_response",
            SupervisorFlow::ConflictRetry => "conflict_retry",
            SupervisorFlow::Spike => "spike",
            SupervisorFlow::Planning => "planning",
        }
    }
}

/// Free-function form of [`SupervisorFlow::role_sequence`] — exposed at the
/// crate root so call sites that only need the sequence can avoid pulling in
/// the full `SupervisorFlow` enum scope (matches the `lib.rs` re-export).
pub fn role_sequence(flow: SupervisorFlow) -> &'static [RoleKind] {
    use RoleKind::*;
    match flow {
        SupervisorFlow::NewTask => &[Planner, Worker, Reviewer, Verifier],
        SupervisorFlow::ReviewResponse | SupervisorFlow::ConflictRetry => {
            &[Worker, Reviewer, Verifier]
        }
        SupervisorFlow::Spike => &[Architect],
        SupervisorFlow::Planning => &[Planner],
    }
}

/// Input to `TaskRunSupervisor::run`.
///
/// All runtime-variable data the supervisor needs to execute one task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// `execute_stage` uses the mapped `provider/model` id for that stage
    /// instead of the catalog-default fallback.  The coordinator populates
    /// this from its per-role model resolution (dispatch priorities + project
    /// `model_preference`) so the supervisor path keeps parity with the
    /// legacy `run_task_lifecycle` model selection.  Empty = fall back to
    /// catalog-default for every stage.
    pub model_id_per_role: HashMap<RoleKind, String>,
}

/// Terminal outcome of a task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskRunOutcome {
    PrOpened { url: String, sha: String },
    /// Planner decided the task should not execute.
    Closed { reason: String },
    /// Planner/architect surfaced a question that blocks automated execution
    /// (e.g. ambiguous scope, missing design decision).
    Escalated { reason: String },
    Failed { stage: String, reason: String },
    Interrupted,
}

/// Return value of `TaskRunSupervisor::run`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRunReport {
    pub task_run_id: String,
    pub outcome: TaskRunOutcome,
    pub stages_completed: Vec<RoleKind>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_flow_is_four_stages() {
        assert_eq!(
            SupervisorFlow::NewTask.role_sequence(),
            &[
                RoleKind::Planner,
                RoleKind::Worker,
                RoleKind::Reviewer,
                RoleKind::Verifier
            ]
        );
    }

    #[test]
    fn spike_flow_is_architect_only() {
        assert_eq!(
            SupervisorFlow::Spike.role_sequence(),
            &[RoleKind::Architect]
        );
    }

    #[test]
    fn review_response_skips_planner() {
        let seq = SupervisorFlow::ReviewResponse.role_sequence();
        assert!(!seq.contains(&RoleKind::Planner));
        assert!(seq.contains(&RoleKind::Worker));
    }

    #[test]
    fn task_run_spec_bincode_roundtrip() {
        let mut per_role = HashMap::new();
        per_role.insert(RoleKind::Planner, "anthropic/claude-sonnet-4.5".to_string());
        per_role.insert(RoleKind::Worker, "anthropic/claude-opus-4.7".to_string());

        let spec = TaskRunSpec {
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: per_role,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.task_id, spec.task_id);
        assert_eq!(back.project_id, spec.project_id);
        assert_eq!(back.trigger, spec.trigger);
        assert_eq!(back.base_branch, spec.base_branch);
        assert_eq!(back.task_branch, spec.task_branch);
        assert_eq!(back.flow, spec.flow);
        assert_eq!(back.model_id_per_role, spec.model_id_per_role);
    }

    #[test]
    fn task_run_report_bincode_roundtrip() {
        let report = TaskRunReport {
            task_run_id: "run-1".to_string(),
            outcome: TaskRunOutcome::PrOpened {
                url: "https://github.com/o/r/pull/1".to_string(),
                sha: "deadbeef".to_string(),
            },
            stages_completed: vec![RoleKind::Planner, RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.task_run_id, report.task_run_id);
        assert_eq!(back.stages_completed, report.stages_completed);
        match back.outcome {
            TaskRunOutcome::PrOpened { url, sha } => {
                assert_eq!(url, "https://github.com/o/r/pull/1");
                assert_eq!(sha, "deadbeef");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}

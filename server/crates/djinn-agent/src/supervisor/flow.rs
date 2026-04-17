//! Supervisor flow templates: which role sequence runs, keyed off the task
//! type / trigger.
//!
//! Flows are static data — they don't hold state, they're just an enum over
//! role orderings. The supervisor picks a flow per task-run based on
//! `(task.issue_type, trigger)` and drives the chosen sequence in order.

/// Which role executes at each stage of a task-run.
///
/// Not the same as `djinn-agent`'s existing `AgentRole` trait objects — this
/// is a lightweight enum suitable for flow templates and telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorFlow {
    NewTask,
    ReviewResponse,
    ConflictRetry,
    Spike,
    Planning,
}

impl SupervisorFlow {
    pub fn role_sequence(self) -> &'static [RoleKind] {
        use RoleKind::*;
        match self {
            SupervisorFlow::NewTask => {
                &[Planner, Worker, Reviewer, Verifier]
            }
            SupervisorFlow::ReviewResponse | SupervisorFlow::ConflictRetry => {
                &[Worker, Reviewer, Verifier]
            }
            SupervisorFlow::Spike => &[Architect],
            SupervisorFlow::Planning => &[Planner],
        }
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
}

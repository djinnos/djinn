use std::collections::HashMap;

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

pub(super) enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Run an immediate dispatch pass for a specific project.
    TriggerProjectDispatch { project_id: String },
    /// Update runtime dispatch limit from settings reload.
    UpdateDispatchLimit { limit: usize },
    /// Update per-role model priority list from settings reload.
    UpdateModelPriorities {
        priorities: HashMap<String, Vec<String>>,
    },
    /// Run an immediate stuck-task detection pass.
    TriggerStuckScan,
    /// Trigger an immediate Planner patrol dispatch (for testing).
    /// Per ADR-051 §1 the Planner owns the board patrol.
    #[cfg(test)]
    TriggerPlannerPatrol,
    /// Lead requests Planner escalation for a task.
    /// Creates a review task and dispatches Planner to it.
    /// Per ADR-051 §8 the Planner is the escalation ceiling above Lead.
    DispatchPlannerEscalation {
        source_task_id: String,
        reason: String,
        project_id: String,
    },
    /// Increment the Lead escalation count for a task; reply with new count.
    IncrementEscalationCount {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<u32>,
    },
}

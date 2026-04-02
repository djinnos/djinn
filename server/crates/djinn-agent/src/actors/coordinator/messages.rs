use std::collections::HashMap;

// ─── Messages (≤15 variants — AGENT-11) ──────────────────────────────────────

pub(super) enum CoordinatorMessage {
    /// Run an immediate dispatch pass for all ready tasks.
    TriggerDispatch,
    /// Run an immediate dispatch pass for a specific project.
    TriggerProjectDispatch { project_id: String },
    /// Pause dispatch — no new sessions will start until `Resume`.
    Pause {
        /// If true, interrupt all active sessions immediately.
        interrupt_active: bool,
        /// Optional interruption reason passed to the slot pool.
        reason: String,
    },
    /// Resume dispatch and immediately run a dispatch pass.
    Resume,
    /// Resume dispatch for one project.
    ResumeProject { project_id: String },
    /// Pause dispatch for one project, optionally interrupting active sessions.
    PauseProject {
        project_id: String,
        interrupt_active: bool,
        reason: String,
    },
    /// Update runtime dispatch limit from settings reload.
    UpdateDispatchLimit { limit: usize },
    /// Update per-role model priority list from settings reload.
    UpdateModelPriorities {
        priorities: HashMap<String, Vec<String>>,
    },
    /// Run an immediate stuck-task detection pass.
    TriggerStuckScan,
    /// Trigger background health validation for all (or one) project on execution_start.
    ValidateProjectHealth { project_id_filter: Option<String> },
    /// Internal callback: result from a background project health-check task.
    SetProjectHealth {
        project_id: String,
        healthy: bool,
        error: Option<String>,
    },
    /// Trigger an immediate Architect patrol dispatch (for testing).
    #[cfg(test)]
    TriggerArchitectPatrol,
    /// Lead requests Architect escalation for a task.
    /// Creates a review task and dispatches Architect to it.
    DispatchArchitectEscalation {
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

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Serialize;


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileTerminateKind { GenuinelyAbsent, Terminated, DesyncReconciled, TeardownFailed, SettlementFailed, ReconciliationIncomplete }
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileTerminateExecution { pub session_id: String, pub task_run_id: Option<String>, pub teardown_owner: bool, pub teardown_attempted: bool, pub teardown_error: Option<String>, pub settlement_attempted: bool, pub settlement_error: Option<String> }
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileTerminateObservations { pub initial_non_terminal_ids: Vec<String>, pub initial_mapping_slot_id: Option<usize>, pub initial_pending_teardown: bool, pub initial_compacting: bool, pub fenced_generation: Option<i64>, pub initial_capture_error: Option<String>, pub final_non_terminal_ids: Vec<String>, pub final_mapping_slot_id: Option<usize>, pub final_pending_teardown: bool, pub final_reread_error: Option<String>, pub pool_cleanup_error: Option<String>, pub completion_source: String, pub underlying_kind: Option<ReconcileTerminateKind> }
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileTerminateSnapshot { pub ok: bool, pub kind: ReconcileTerminateKind, pub task_id: String, pub executions: Vec<ReconcileTerminateExecution>, pub observations: ReconcileTerminateObservations }

#[derive(Debug, Clone)]
pub struct ModelPoolStatus {
    pub active: u32,
    pub free: u32,
    pub total: u32,
}

#[derive(Debug, Clone)]
pub struct RunningTaskInfo {
    pub task_id: String,
    pub model_id: String,
    pub slot_id: usize,
    pub duration_seconds: u64,
    pub idle_seconds: u64,
    /// Project UUID the task belongs to, tracked by the slot pool so
    /// project-scoped status queries can filter pre-session lifecycles.
    pub project_id: Option<String>,
    /// Live no-progress streak for this session, sourced from the worker's
    /// durable-progress detector. Defaults to 0 when not yet reported.
    pub no_progress_streak: u32,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub active_slots: usize,
    pub total_slots: usize,
    pub per_model: HashMap<String, ModelPoolStatus>,
    pub running_tasks: Vec<RunningTaskInfo>,
}

// ── Slot pool ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SlotPoolOps: Send + Sync {
    async fn get_status(&self) -> Result<PoolStatus, String>;
    async fn kill_session(&self, task_id: &str) -> Result<(), String>;
    async fn terminate_session(&self, task_id: &str) -> Result<(), String>;
    /// Run the pool-owned reconciliation; callers consume this snapshot as the
    /// authoritative result rather than independently querying session state.
    async fn reconcile_terminate(
        &self,
        task_id: &str,
    ) -> Result<ReconcileTerminateSnapshot, String>;
    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String>;
    async fn has_session(&self, task_id: &str) -> Result<bool, String>;
}

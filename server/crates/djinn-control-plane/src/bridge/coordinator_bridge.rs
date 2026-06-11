use std::collections::HashMap;

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Tasks merged per hour per epic in the past hour (epic_id → count).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
}

// ── Coordinator ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait CoordinatorOps: Send + Sync {
    fn get_status(&self) -> Result<CoordinatorStatus, String>;
    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String>;
}

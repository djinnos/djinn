/// Bridge traits that decouple djinn-mcp from the server binary.
///
/// The server implements each trait for its concrete handle types
/// (CoordinatorHandle, SlotPoolHandle, LspManager, SyncManager, AppState).
/// McpState holds Arc<dyn Trait> so the MCP layer never imports server types.
use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use serde::Serialize;

// ── Data types ─────────────────────────────────────────────────────────────────
// Plain data returned by the bridge traits. Defined here so they can be used
// by both djinn-mcp tool handlers and the server bridge implementations.

#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub paused: bool,
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    pub unhealthy_projects: HashMap<String, String>,
}

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
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub active_slots: usize,
    pub total_slots: usize,
    pub per_model: HashMap<String, ModelPoolStatus>,
    pub running_tasks: Vec<RunningTaskInfo>,
}

#[derive(Debug, Clone)]
pub struct LspWarning {
    pub server: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    pub name: String,
    pub branch: String,
    pub enabled: bool,
    /// Sync-enabled project paths.
    pub project_paths: Vec<String>,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub failure_count: u32,
    pub backoff_secs: u64,
    pub needs_attention: bool,
}

#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub channel: String,
    pub ok: bool,
    pub count: Option<usize>,
    pub error: Option<String>,
}

// ── Coordinator ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait CoordinatorOps: Send + Sync {
    async fn resume_project(&self, project_id: &str) -> Result<(), String>;
    async fn resume(&self) -> Result<(), String>;
    async fn pause_project(&self, project_id: &str) -> Result<(), String>;
    async fn pause_project_immediate(&self, project_id: &str, reason: &str)
        -> Result<(), String>;
    async fn pause_immediate(&self, reason: &str) -> Result<(), String>;
    fn get_status(&self) -> Result<CoordinatorStatus, String>;
    fn get_project_status(&self, project_id: &str) -> Result<CoordinatorStatus, String>;
    async fn validate_project_health(&self, project_id: Option<String>) -> Result<(), String>;
    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String>;
    async fn pause(&self) -> Result<(), String>;
}

// ── Slot pool ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SlotPoolOps: Send + Sync {
    async fn get_status(&self) -> Result<PoolStatus, String>;
    async fn kill_session(&self, task_id: &str) -> Result<(), String>;
    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String>;
    async fn has_session(&self, task_id: &str) -> Result<bool, String>;
}

// ── LSP ─────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LspOps: Send + Sync {
    async fn warnings(&self) -> Vec<LspWarning>;
}

// ── Sync ────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SyncOps: Send + Sync {
    async fn enable_project(&self, project_id: &str) -> Result<(), String>;
    async fn disable_project(&self, project_id: &str) -> Result<(), String>;
    async fn delete_remote_branch(&self, channel: &str, project_path: &Path)
        -> Result<(), String>;
    async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult>;
    async fn import_all(&self) -> Vec<SyncResult>;
    async fn status(&self) -> Vec<ChannelStatus>;
}

// ── Runtime ─────────────────────────────────────────────────────────────────────
// Server-level operations that don't fit neatly into the other trait groups.

#[async_trait]
pub trait RuntimeOps: Send + Sync {
    async fn apply_settings(
        &self,
        settings: &djinn_core::models::DjinnSettings,
    ) -> Result<(), String>;
    async fn reset_runtime_settings(&self);
    async fn persist_model_health_state(&self);
    /// Purge stale worktrees from all registered projects.
    async fn purge_worktrees(&self);
}

// ── Git ─────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait GitOps: Send + Sync {
    async fn git_actor(
        &self,
        path: &Path,
    ) -> Result<djinn_git::GitActorHandle, djinn_git::GitError>;
}

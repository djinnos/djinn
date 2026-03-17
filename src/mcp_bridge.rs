/// Bridge trait implementations: connect djinn-mcp's abstract traits to
/// the server's concrete actor handles and managers.
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use djinn_git::{GitActorHandle, GitError};
use djinn_mcp::bridge::{
    ChannelStatus, CoordinatorOps, CoordinatorStatus, GitOps, LspOps, LspWarning,
    ModelPoolStatus, PoolStatus, RunningTaskInfo, RuntimeOps, SlotPoolOps, SyncOps, SyncResult,
};

use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;

use crate::sync::SyncManager;

// ── CoordinatorHandle ─────────────────────────────────────────────────────────

#[async_trait]
impl CoordinatorOps for CoordinatorHandle {
    async fn resume_project(&self, project_id: &str) -> Result<(), String> {
        self.resume_project(project_id).await.map_err(|e| e.to_string())
    }

    async fn resume(&self) -> Result<(), String> {
        self.resume().await.map_err(|e| e.to_string())
    }

    async fn pause_project(&self, project_id: &str) -> Result<(), String> {
        self.pause_project(project_id).await.map_err(|e| e.to_string())
    }

    async fn pause_project_immediate(
        &self,
        project_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        self.pause_project_immediate(project_id, reason)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause_immediate(&self, reason: &str) -> Result<(), String> {
        self.pause_immediate(reason).await.map_err(|e| e.to_string())
    }

    fn get_status(&self) -> Result<CoordinatorStatus, String> {
        let s = self.get_status().map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            paused: s.paused,
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            unhealthy_projects: s.unhealthy_projects,
        })
    }

    fn get_project_status(&self, project_id: &str) -> Result<CoordinatorStatus, String> {
        let s = self.get_project_status(project_id).map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            paused: s.paused,
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            unhealthy_projects: s.unhealthy_projects,
        })
    }

    async fn validate_project_health(&self, project_id: Option<String>) -> Result<(), String> {
        self.validate_project_health(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String> {
        self.trigger_dispatch_for_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause(&self) -> Result<(), String> {
        self.pause().await.map_err(|e| e.to_string())
    }
}

// ── SlotPoolHandle ────────────────────────────────────────────────────────────

#[async_trait]
impl SlotPoolOps for SlotPoolHandle {
    async fn get_status(&self) -> Result<PoolStatus, String> {
        let s = self.get_status().await.map_err(|e| e.to_string())?;
        Ok(PoolStatus {
            active_slots: s.active_slots,
            total_slots: s.total_slots,
            per_model: s
                .per_model
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        ModelPoolStatus {
                            active: v.active,
                            free: v.free,
                            total: v.total,
                        },
                    )
                })
                .collect(),
            running_tasks: s
                .running_tasks
                .into_iter()
                .map(|t| RunningTaskInfo {
                    task_id: t.task_id,
                    model_id: t.model_id,
                    slot_id: t.slot_id,
                    duration_seconds: t.duration_seconds,
                })
                .collect(),
        })
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), String> {
        self.kill_session(task_id).await.map_err(|e| e.to_string())
    }

    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String> {
        let result = self.session_for_task(task_id).await.map_err(|e| e.to_string())?;
        Ok(result.map(|t| RunningTaskInfo {
            task_id: t.task_id,
            model_id: t.model_id,
            slot_id: t.slot_id,
            duration_seconds: t.duration_seconds,
        }))
    }

    async fn has_session(&self, task_id: &str) -> Result<bool, String> {
        self.has_session(task_id).await.map_err(|e| e.to_string())
    }
}

// ── LspManager ────────────────────────────────────────────────────────────────

#[async_trait]
impl LspOps for LspManager {
    async fn warnings(&self) -> Vec<LspWarning> {
        self.warnings()
            .await
            .into_iter()
            .map(|w| LspWarning { server: w.server, message: w.message })
            .collect()
    }
}

// ── SyncManager ───────────────────────────────────────────────────────────────

#[async_trait]
impl SyncOps for SyncManager {
    async fn enable_project(&self, project_id: &str) -> Result<(), String> {
        self.enable_project(project_id).await.map_err(|e| e.to_string())
    }

    async fn disable_project(&self, project_id: &str) -> Result<(), String> {
        self.disable_project(project_id).await.map_err(|e| e.to_string())
    }

    async fn delete_remote_branch(
        &self,
        channel: &str,
        project_path: &Path,
    ) -> Result<(), String> {
        self.delete_remote_branch(channel, project_path)
            .await
            .map_err(|e| e.to_string())
    }

    async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult> {
        self.export_all(user_id)
            .await
            .into_iter()
            .map(|r| SyncResult {
                channel: r.channel,
                ok: r.ok,
                count: r.count,
                error: r.error,
            })
            .collect()
    }

    async fn import_all(&self) -> Vec<SyncResult> {
        self.import_all()
            .await
            .into_iter()
            .map(|r| SyncResult {
                channel: r.channel,
                ok: r.ok,
                count: r.count,
                error: r.error,
            })
            .collect()
    }

    async fn status(&self) -> Vec<ChannelStatus> {
        self.status()
            .await
            .into_iter()
            .map(|s| ChannelStatus {
                name: s.name,
                branch: s.branch,
                enabled: s.enabled,
                project_paths: s.project_paths,
                last_synced_at: s.last_synced_at,
                last_error: s.last_error,
                failure_count: s.failure_count,
                backoff_secs: s.backoff_secs,
                needs_attention: s.needs_attention,
            })
            .collect()
    }
}

// ── AppState: RuntimeOps + GitOps ─────────────────────────────────────────────

use crate::server::AppState;

#[async_trait]
impl RuntimeOps for AppState {
    async fn apply_settings(
        &self,
        settings: &djinn_core::models::DjinnSettings,
    ) -> Result<(), String> {
        AppState::apply_settings(self, settings).await
    }

    async fn reset_runtime_settings(&self) {
        AppState::reset_runtime_settings(self).await;
    }

    async fn persist_model_health_state(&self) {
        AppState::persist_model_health_state(self).await;
    }

    async fn purge_worktrees(&self) {
        djinn_agent::actors::slot::purge_all_worktrees(&self.agent_context()).await;
    }
}

#[async_trait]
impl GitOps for AppState {
    async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        AppState::git_actor(self, path).await
    }
}

// ── AppState::mcp_state() ─────────────────────────────────────────────────────

impl AppState {
    /// Build a `djinn_mcp::McpState` from AppState, wiring all bridge impls.
    pub fn mcp_state(&self) -> djinn_mcp::McpState {
        djinn_mcp::McpState::new(
            self.db().clone(),
            self.event_bus(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            self.sync_user_id().to_string(),
            // Coordinator and pool are set lazily at runtime; start as None.
            // The server calls initialize_agents() after startup.
            None,
            None,
            Arc::new(self.lsp().clone()),
            Arc::new(self.sync_manager().clone()),
            Arc::new(self.clone()),
            Arc::new(self.clone()),
        )
    }
}

/// Bridge trait implementations: connect djinn-mcp's abstract traits to
/// the server's concrete actor handles and managers.
///
/// Newtypes are required for CoordinatorHandle, SlotPoolHandle, LspManager,
/// and SyncManager because both the trait (djinn-mcp) and the implementor
/// (djinn-agent / crate::sync) are external to the server — orphan rule.
/// AppState is a server-local type so it implements RuntimeOps and GitOps directly.
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use djinn_git::{GitActorHandle, GitError};
use djinn_mcp::bridge::{
    ChannelStatus, CoordinatorOps, CoordinatorStatus, GitOps, LspOps, LspWarning, ModelPoolStatus,
    PoolStatus, RunningTaskInfo, RuntimeOps, SlotPoolOps, SyncOps, SyncResult,
};

use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;

use crate::sync::SyncManager;

// ── Newtype wrappers ───────────────────────────────────────────────────────────

pub struct CoordinatorBridge(pub CoordinatorHandle);
pub struct SlotPoolBridge(pub SlotPoolHandle);
pub struct LspBridge(pub LspManager);
pub struct SyncBridge(pub SyncManager);

// ── CoordinatorBridge → CoordinatorOps ───────────────────────────────────────

#[async_trait]
impl CoordinatorOps for CoordinatorBridge {
    async fn resume_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .resume_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resume(&self) -> Result<(), String> {
        self.0.resume().await.map_err(|e| e.to_string())
    }

    async fn pause_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .pause_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause_project_immediate(&self, project_id: &str, reason: &str) -> Result<(), String> {
        self.0
            .pause_project_immediate(project_id, reason)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause_immediate(&self, reason: &str) -> Result<(), String> {
        self.0
            .pause_immediate(reason)
            .await
            .map_err(|e| e.to_string())
    }

    fn get_status(&self) -> Result<CoordinatorStatus, String> {
        let s = self.0.get_status().map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            paused: s.paused,
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            unhealthy_projects: s.unhealthy_projects,
            epic_throughput: s.epic_throughput,
            pr_errors: s.pr_errors,
        })
    }

    fn get_project_status(&self, project_id: &str) -> Result<CoordinatorStatus, String> {
        let s = self
            .0
            .get_project_status(project_id)
            .map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            paused: s.paused,
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            unhealthy_projects: s.unhealthy_projects,
            epic_throughput: s.epic_throughput,
            pr_errors: s.pr_errors,
        })
    }

    async fn validate_project_health(&self, project_id: Option<String>) -> Result<(), String> {
        self.0
            .validate_project_health(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .trigger_dispatch_for_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn pause(&self) -> Result<(), String> {
        self.0.pause().await.map_err(|e| e.to_string())
    }
}

// ── SlotPoolBridge → SlotPoolOps ──────────────────────────────────────────────

#[async_trait]
impl SlotPoolOps for SlotPoolBridge {
    async fn get_status(&self) -> Result<PoolStatus, String> {
        let s = self.0.get_status().await.map_err(|e| e.to_string())?;
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
                    idle_seconds: t.idle_seconds,
                })
                .collect(),
        })
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), String> {
        self.0
            .kill_session(task_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String> {
        let result = self
            .0
            .session_for_task(task_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result.map(|t| RunningTaskInfo {
            task_id: t.task_id,
            model_id: t.model_id,
            slot_id: t.slot_id,
            duration_seconds: t.duration_seconds,
            idle_seconds: t.idle_seconds,
        }))
    }

    async fn has_session(&self, task_id: &str) -> Result<bool, String> {
        self.0.has_session(task_id).await.map_err(|e| e.to_string())
    }
}

// ── LspBridge → LspOps ───────────────────────────────────────────────────────

#[async_trait]
impl LspOps for LspBridge {
    async fn warnings(&self) -> Vec<LspWarning> {
        self.0
            .warnings()
            .await
            .into_iter()
            .map(|w| LspWarning {
                server: w.server,
                message: w.message,
            })
            .collect()
    }
}

// ── SyncBridge → SyncOps ─────────────────────────────────────────────────────

#[async_trait]
impl SyncOps for SyncBridge {
    async fn enable_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .enable_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn disable_project(&self, project_id: &str) -> Result<(), String> {
        self.0
            .disable_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_remote_branch(&self, channel: &str, project_path: &Path) -> Result<(), String> {
        self.0
            .delete_remote_branch(channel, project_path)
            .await
            .map_err(|e| e.to_string())
    }

    async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult> {
        self.0
            .export_all(user_id)
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
        self.0
            .import_all()
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
        self.0
            .status()
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

// ── AppState → RuntimeOps + GitOps + mcp_state() ─────────────────────────────

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

impl AppState {
    /// Build a `djinn_mcp::McpState` from this AppState, wiring all bridge impls.
    ///
    /// Snapshots the current coordinator and pool handles via `try_lock()`.
    /// In production this is called after `initialize_agents()`, so both are
    /// populated. In tests neither is initialised; tools return graceful errors.
    pub fn mcp_state(&self) -> djinn_mcp::McpState {
        let coordinator = self
            .coordinator_sync()
            .map(|c| Arc::new(CoordinatorBridge(c)) as Arc<dyn CoordinatorOps>);
        let pool = self
            .pool_sync()
            .map(|p| Arc::new(SlotPoolBridge(p)) as Arc<dyn SlotPoolOps>);

        djinn_mcp::McpState::new(
            self.db().clone(),
            self.event_bus(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            self.sync_user_id().to_string(),
            coordinator,
            pool,
            Arc::new(LspBridge(self.lsp().clone())),
            Arc::new(SyncBridge(self.sync_manager().clone())),
            Arc::new(self.clone()),
            Arc::new(self.clone()),
        )
    }
}

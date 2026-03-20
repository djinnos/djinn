use std::path::Path;
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_core::models::DjinnSettings;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};

use crate::bridge::{CoordinatorOps, GitOps, LspOps, RuntimeOps, SlotPoolOps, SyncOps};

/// Subset of application state consumed by the MCP layer.
///
/// Holds the database, catalog, and boxed bridge-trait handles for
/// server-specific actors (coordinator, pool, LSP, sync). The server
/// constructs this from AppState; djinn-mcp never depends on AppState or
/// any actor type directly.
#[derive(Clone)]
pub struct McpState {
    db: Database,
    event_bus: EventBus,
    catalog: CatalogService,
    health_tracker: HealthTracker,
    sync_user_id: String,
    coordinator: Option<Arc<dyn CoordinatorOps>>,
    pool: Option<Arc<dyn SlotPoolOps>>,
    lsp: Arc<dyn LspOps>,
    sync: Arc<dyn SyncOps>,
    runtime: Arc<dyn RuntimeOps>,
    git: Arc<dyn GitOps>,
}

impl McpState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        event_bus: EventBus,
        catalog: CatalogService,
        health_tracker: HealthTracker,
        sync_user_id: String,
        coordinator: Option<Arc<dyn CoordinatorOps>>,
        pool: Option<Arc<dyn SlotPoolOps>>,
        lsp: Arc<dyn LspOps>,
        sync: Arc<dyn SyncOps>,
        runtime: Arc<dyn RuntimeOps>,
        git: Arc<dyn GitOps>,
    ) -> Self {
        Self {
            db,
            event_bus,
            catalog,
            health_tracker,
            sync_user_id,
            coordinator,
            pool,
            lsp,
            sync,
            runtime,
            git,
        }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn event_bus(&self) -> EventBus {
        self.event_bus.clone()
    }

    pub fn catalog(&self) -> &CatalogService {
        &self.catalog
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        &self.health_tracker
    }

    pub fn sync_user_id(&self) -> &str {
        &self.sync_user_id
    }

    pub async fn coordinator(&self) -> Option<Arc<dyn CoordinatorOps>> {
        self.coordinator.clone()
    }

    pub async fn pool(&self) -> Option<Arc<dyn SlotPoolOps>> {
        self.pool.clone()
    }

    pub fn lsp(&self) -> &Arc<dyn LspOps> {
        &self.lsp
    }

    pub fn sync_manager(&self) -> &Arc<dyn SyncOps> {
        &self.sync
    }

    pub async fn git_actor(
        &self,
        path: &Path,
    ) -> Result<djinn_git::GitActorHandle, djinn_git::GitError> {
        self.git.git_actor(path).await
    }

    pub async fn apply_settings(&self, settings: &DjinnSettings) -> Result<(), String> {
        self.runtime.apply_settings(settings).await
    }

    pub async fn reset_runtime_settings(&self) {
        self.runtime.reset_runtime_settings().await;
    }

    pub async fn persist_model_health_state(&self) {
        self.runtime.persist_model_health_state().await;
    }

    pub async fn purge_worktrees(&self) {
        self.runtime.purge_worktrees().await;
    }
}

// ── Stub impls for test builds ─────────────────────────────────────────────────
// Provide a no-actor McpState for tests that exercise MCP tool handlers
// directly (without a full Axum server).

#[cfg(test)]
pub(crate) mod stubs {
    #![allow(dead_code, unused_imports)]
    use super::*;
    use crate::bridge::{ChannelStatus, LspWarning, PoolStatus, RunningTaskInfo, SyncResult};
    use async_trait::async_trait;
    use djinn_git::{GitActorHandle, GitError};

    pub struct StubCoordinatorOps;
    #[async_trait]
    impl CoordinatorOps for StubCoordinatorOps {
        async fn resume_project(&self, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn resume(&self) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn pause_project(&self, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn pause_project_immediate(&self, _: &str, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn pause_immediate(&self, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        fn get_status(&self) -> Result<crate::bridge::CoordinatorStatus, String> {
            Err("coordinator not initialized".into())
        }
        fn get_project_status(&self, _: &str) -> Result<crate::bridge::CoordinatorStatus, String> {
            Err("coordinator not initialized".into())
        }
        async fn validate_project_health(&self, _: Option<String>) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_dispatch_for_project(&self, _: &str) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
        async fn pause(&self) -> Result<(), String> {
            Err("coordinator not initialized".into())
        }
    }

    pub struct StubSlotPoolOps;
    #[async_trait]
    impl SlotPoolOps for StubSlotPoolOps {
        async fn get_status(&self) -> Result<PoolStatus, String> {
            Err("slot pool not initialized".into())
        }
        async fn kill_session(&self, _: &str) -> Result<(), String> {
            Err("slot pool not initialized".into())
        }
        async fn session_for_task(&self, _: &str) -> Result<Option<RunningTaskInfo>, String> {
            Err("slot pool not initialized".into())
        }
        async fn has_session(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    pub struct StubLspOps;
    #[async_trait]
    impl LspOps for StubLspOps {
        async fn warnings(&self) -> Vec<LspWarning> {
            vec![]
        }
    }

    pub struct StubSyncOps;
    #[async_trait]
    impl SyncOps for StubSyncOps {
        async fn enable_project(&self, _: &str) -> Result<(), String> {
            Err("sync not enabled".into())
        }
        async fn disable_project(&self, _: &str) -> Result<(), String> {
            Err("sync not enabled".into())
        }
        async fn delete_remote_branch(&self, _: &str, _: &Path) -> Result<(), String> {
            Err("sync not enabled".into())
        }
        async fn export_all(&self, _: Option<&str>) -> Vec<SyncResult> {
            vec![]
        }
        async fn import_all(&self) -> Vec<SyncResult> {
            vec![]
        }
        async fn status(&self) -> Vec<ChannelStatus> {
            vec![ChannelStatus {
                name: "tasks".into(),
                branch: "djinn/tasks".into(),
                enabled: false,
                project_paths: vec![],
                last_synced_at: None,
                last_error: None,
                failure_count: 0,
                backoff_secs: 0,
                needs_attention: false,
            }]
        }
    }

    pub struct StubRuntimeOps;
    #[async_trait]
    impl RuntimeOps for StubRuntimeOps {
        async fn apply_settings(&self, _: &DjinnSettings) -> Result<(), String> {
            Ok(())
        }
        async fn reset_runtime_settings(&self) {}
        async fn persist_model_health_state(&self) {}
        async fn purge_worktrees(&self) {}
    }

    pub struct StubGitOps;
    #[async_trait]
    impl GitOps for StubGitOps {
        async fn git_actor(&self, _: &Path) -> Result<GitActorHandle, GitError> {
            Err(GitError::CommandFailed {
                code: 1,
                command: "rev-parse".into(),
                cwd: ".".into(),
                stdout: String::new(),
                stderr: "no repository found".into(),
            })
        }
    }

    /// Build a McpState backed only by an in-memory database (no live actors).
    /// Useful for direct-invocation tests of MCP tool handlers.
    pub fn test_mcp_state(db: Database) -> McpState {
        McpState::new(
            db,
            EventBus::noop(),
            CatalogService::new(),
            HealthTracker::new(),
            "test-user".into(),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
        )
    }
}

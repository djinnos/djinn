use std::path::Path;

use crate::actors::coordinator::CoordinatorHandle;
use crate::actors::slot::SlotPoolHandle;
use crate::agent::lsp::LspManager;
use crate::db::connection::Database;
use crate::events::EventBus;
use crate::models::DjinnSettings;
use crate::provider::{CatalogService, HealthTracker};
use crate::server::AppState;
use crate::sync::SyncManager;
use djinn_git::{GitActorHandle, GitError};

/// Subset of application state consumed by the MCP layer.
///
/// Wraps `AppState` and exposes only what MCP tools need. `DjinnMcpServer`
/// holds `McpState` rather than `AppState` directly, breaking the structural
/// dependency ahead of extracting MCP into its own crate.
#[derive(Clone)]
pub struct McpState(AppState);

impl McpState {
    pub fn db(&self) -> &Database {
        self.0.db()
    }

    pub fn event_bus(&self) -> EventBus {
        self.0.event_bus()
    }

    pub fn catalog(&self) -> &CatalogService {
        self.0.catalog()
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        self.0.health_tracker()
    }

    pub fn sync_manager(&self) -> &SyncManager {
        self.0.sync_manager()
    }

    pub fn sync_user_id(&self) -> &str {
        self.0.sync_user_id()
    }

    pub fn lsp(&self) -> &LspManager {
        self.0.lsp()
    }

    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.0.coordinator().await
    }

    pub async fn pool(&self) -> Option<SlotPoolHandle> {
        self.0.pool().await
    }

    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        self.0.git_actor(path).await
    }

    pub async fn apply_settings(&self, settings: &DjinnSettings) -> Result<(), String> {
        self.0.apply_settings(settings).await
    }

    pub async fn reset_runtime_settings(&self) {
        self.0.reset_runtime_settings().await;
    }

    pub async fn persist_model_health_state(&self) {
        self.0.persist_model_health_state().await;
    }

    /// Escape hatch for crate-internal functions that still accept `&AppState`.
    pub(crate) fn as_app_state(&self) -> &AppState {
        &self.0
    }
}

impl From<AppState> for McpState {
    fn from(state: AppState) -> Self {
        McpState(state)
    }
}

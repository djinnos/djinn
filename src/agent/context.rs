use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_git::{GitActorHandle, GitError};
use tokio::sync::Mutex;

use crate::actors::coordinator::{CoordinatorHandle, VerificationTracker};
use crate::agent::file_time::FileTime;
use crate::agent::lsp::LspManager;
use crate::agent::roles::RoleRegistry;
use crate::db::connection::Database;
use crate::events::EventBus;
use crate::provider::{CatalogService, HealthTracker};

/// Subset of application state required by agent lifecycle, coordinator, and
/// slot code.  Cheaply cloneable — all fields are either `Clone` or wrapped in
/// `Arc`.
///
/// Construct via [`crate::server::AppState::agent_context()`] at the server
/// boundary, or via [`crate::test_helpers::agent_context_from_db()`] in tests.
#[derive(Clone)]
pub struct AgentContext {
    pub db: Database,
    pub event_bus: EventBus,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    pub verifying_tasks: VerificationTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub health_tracker: HealthTracker,
    pub file_time: Arc<FileTime>,
    pub lsp: LspManager,
    pub catalog: CatalogService,
    pub coordinator: Arc<tokio::sync::Mutex<Option<CoordinatorHandle>>>,
}

impl AgentContext {
    /// Get or spawn a `GitActorHandle` for the given project path.
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut map, path)
    }

    /// Register a task as having an in-flight verification pipeline.
    pub fn register_verification(&self, task_id: &str) {
        self.verifying_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string());
    }

    /// Deregister a task's verification pipeline (completed or crashed).
    pub fn deregister_verification(&self, task_id: &str) {
        self.verifying_tasks
            .lock()
            .expect("poisoned")
            .remove(task_id);
    }

    /// Check whether a task has a live verification pipeline.
    pub fn has_verification(&self, task_id: &str) -> bool {
        self.verifying_tasks
            .lock()
            .expect("poisoned")
            .contains(task_id)
    }

    /// Get the current coordinator handle, if one is running.
    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.coordinator.lock().await.clone()
    }

    /// Persist current model health state to the settings DB.
    pub async fn persist_model_health_state(&self) {
        use crate::db::SettingsRepository;
        const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";
        let repo = SettingsRepository::new(self.db.clone(), self.event_bus.clone());
        let snapshot = self.health_tracker.all_health();
        match serde_json::to_string(&snapshot) {
            Ok(raw) => {
                if let Err(e) = repo.set(MODEL_HEALTH_STATE_KEY, &raw).await {
                    tracing::warn!(error = %e, "failed to persist model health state");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to serialize model health state"),
        }
    }
}

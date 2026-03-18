use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use djinn_git::{GitActorHandle, GitError};
use tokio::sync::Mutex;

use crate::actors::coordinator::{CoordinatorHandle, VerificationTracker};
use crate::file_time::FileTime;
use crate::lsp::LspManager;
use crate::roles::RoleRegistry;
use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};

/// Shared tracker for per-task last-activity timestamps (unix seconds).
/// Used by stall detection to kill sessions that stop producing tokens.
pub type ActivityTracker = Arc<std::sync::Mutex<HashMap<String, Arc<AtomicU64>>>>;

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
    pub active_tasks: ActivityTracker,
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

    /// Register a task as active and return the shared timestamp atomic.
    /// The atomic is initialized to the current unix timestamp.
    pub fn register_activity(&self, task_id: &str) -> Arc<AtomicU64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ts = Arc::new(AtomicU64::new(now));
        self.active_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string(), ts.clone());
        ts
    }

    /// Update the activity timestamp for a task to now.
    pub fn touch_activity(&self, task_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(ts) = self.active_tasks.lock().expect("poisoned").get(task_id) {
            ts.store(now, Ordering::Relaxed);
        }
    }

    /// Return seconds since last activity touch, or `None` if not registered.
    pub fn idle_seconds(&self, task_id: &str) -> Option<u64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let guard = self.active_tasks.lock().expect("poisoned");
        let ts = guard.get(task_id)?;
        let last = ts.load(Ordering::Relaxed);
        Some(now.saturating_sub(last))
    }

    /// Deregister a task's activity tracker.
    pub fn deregister_activity(&self, task_id: &str) {
        self.active_tasks.lock().expect("poisoned").remove(task_id);
    }

    /// Get the current coordinator handle, if one is running.
    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.coordinator.lock().await.clone()
    }

    /// Persist current model health state to the settings DB.
    pub async fn persist_model_health_state(&self) {
        use djinn_db::SettingsRepository;
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::CoordinatorHandle;
use crate::actors::git::{GitActorHandle, GitError};
use crate::actors::slot::{SlotPoolConfig, SlotPoolHandle};
use crate::db::connection::Database;
use crate::db::NoteRepository;
use crate::db::ProjectRepository;
use crate::db::SettingsRepository;
use crate::events::DjinnEvent;
use crate::provider::{CatalogService, HealthTracker};
use crate::sync::SyncManager;

mod settings;

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const SETTINGS_RAW_KEY: &str = "settings.raw";
const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pub db: Database,
    pub cancel: CancellationToken,
    pub events: broadcast::Sender<DjinnEvent>,
    pub git_actors: Mutex<HashMap<PathBuf, GitActorHandle>>,
    /// models.dev catalog + custom providers (in-memory, refreshed on startup).
    pub catalog: CatalogService,
    /// Per-model circuit-breaker health tracker.
    pub health_tracker: HealthTracker,
    /// djinn/ namespace git sync manager.
    pub sync: SyncManager,
    /// Long-running coordinator actor handle.
    pub coordinator: Mutex<Option<CoordinatorHandle>>,
    /// Long-running slot pool actor handle.
    pub pool: Mutex<Option<SlotPoolHandle>>,
    /// User identity for sync (JSONL filename). Single source of truth.
    ///
    /// Resolved once at startup from `git config user.email`. When djinn
    /// authentication is added, update `resolve_sync_user_id()` to return
    /// the authenticated email instead — everything else follows.
    pub sync_user_id: String,
    /// Task IDs with an in-flight verification pipeline (background tokio task).
    /// Used by the coordinator to distinguish genuinely stuck `verifying` tasks
    /// (orphaned after server restart) from ones with a live pipeline.
    pub verifying_tasks: crate::actors::coordinator::VerificationTracker,
}

impl AppState {
    pub fn new(db: Database, cancel: CancellationToken) -> Self {
        Self::new_inner(db, cancel)
    }

    fn new_inner(db: Database, cancel: CancellationToken) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sync = SyncManager::new(db.clone(), events.clone());
        let sync_user_id = resolve_sync_user_id();
        tracing::info!(sync_user_id = %sync_user_id, "resolved sync user identity");
        Self {
            inner: Arc::new(Inner {
                db,
                cancel,
                events,
                git_actors: Mutex::new(HashMap::new()),
                catalog: CatalogService::new(),
                health_tracker: HealthTracker::new(),
                sync,
                coordinator: Mutex::new(None),
                pool: Mutex::new(None),
                sync_user_id,
                verifying_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
            }),
        }
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    pub fn events(&self) -> &broadcast::Sender<DjinnEvent> {
        &self.inner.events
    }

    pub fn sync_user_id(&self) -> &str {
        &self.inner.sync_user_id
    }

    /// Get or spawn a `GitActorHandle` for the given project path (GIT-04).
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.inner.git_actors.lock().await;
        crate::actors::git::get_or_spawn(&mut map, path)
    }

    pub fn catalog(&self) -> &CatalogService {
        &self.inner.catalog
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        &self.inner.health_tracker
    }

    pub fn sync_manager(&self) -> &SyncManager {
        &self.inner.sync
    }

    /// Register a task as having an in-flight verification pipeline.
    pub fn register_verification(&self, task_id: &str) {
        self.inner.verifying_tasks.lock().expect("poisoned").insert(task_id.to_string());
    }

    /// Deregister a task's verification pipeline (completed or crashed).
    pub fn deregister_verification(&self, task_id: &str) {
        self.inner.verifying_tasks.lock().expect("poisoned").remove(task_id);
    }

    /// Check whether a task has a live verification pipeline.
    pub fn has_verification(&self, task_id: &str) -> bool {
        self.inner.verifying_tasks.lock().expect("poisoned").contains(task_id)
    }

    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.inner.coordinator.lock().await.clone()
    }

    pub async fn pool(&self) -> Option<SlotPoolHandle> {
        self.inner.pool.lock().await.clone()
    }

    /// Spawn long-running agent actors once and keep their handles in AppState.
    pub async fn initialize_agents(&self) {
        if self.pool().await.is_some() {
            return;
        }

        let sessions_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".djinn")
            .join("sessions");
        if let Err(e) = std::fs::create_dir_all(&sessions_dir) {
            tracing::warn!(error = %e, path = %sessions_dir.display(), "failed to create sessions directory");
            return;
        }

        let pool = SlotPoolHandle::spawn(
            self.clone(),
            self.cancel().clone(),
            SlotPoolConfig {
                models: Vec::new(),
                role_priorities: std::collections::HashMap::new(),
            },
        );
        let coordinator = CoordinatorHandle::spawn(
            self.events().clone(),
            self.cancel().clone(),
            self.db().clone(),
            pool.clone(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            self.inner.verifying_tasks.clone(),
        );

        *self.inner.pool.lock().await = Some(pool.clone());
        *self.inner.coordinator.lock().await = Some(coordinator.clone());

        self.apply_runtime_settings_from_db().await;

        // Coordinator starts paused — require explicit `execution_start` to begin dispatching.
        tracing::info!("coordinator spawned (paused — awaiting explicit execution_start)");
    }

    /// Load custom providers from DB into the catalog and trigger a background
    /// catalog refresh from models.dev.  Call once after server startup.
    pub async fn initialize(&self) {
        use crate::db::CustomProviderRepository;
        use crate::models::{Model, Provider};

        // Load custom providers from DB → merge into in-memory catalog.
        let repo = CustomProviderRepository::new(self.db().clone(), self.events().clone());
        match repo.list().await {
            Ok(providers) => {
                for cp in providers {
                    let provider = Provider {
                        id: cp.id.clone(),
                        name: cp.name,
                        npm: String::new(),
                        env_vars: vec![cp.env_var],
                        base_url: cp.base_url,
                        docs_url: String::new(),
                        is_openai_compatible: true,
                    };
                    let seed_models: Vec<Model> = cp
                        .seed_models
                        .iter()
                        .map(|s| Model {
                            id: s.id.clone(),
                            provider_id: cp.id.clone(),
                            name: s.name.clone(),
                            tool_call: false,
                            reasoning: false,
                            attachment: false,
                            context_window: 0,
                            output_limit: 0,
                            pricing: crate::models::Pricing::default(),
                        })
                        .collect();
                    self.catalog().add_custom_provider(provider, seed_models);
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to load custom providers from DB"),
        }

        // Inject synthetic catalog entries for built-in providers (e.g.
        // chatgpt_codex, gcp_vertex_ai) that aren't in models.dev.
        use crate::provider::builtin::BUILTIN_PROVIDERS;
        self.catalog().inject_builtin_providers(BUILTIN_PROVIDERS);

        // Kick off background refresh from models.dev.
        let catalog = self.catalog().clone();
        tokio::spawn(async move {
            catalog.refresh().await;
            // Re-inject after refresh so built-in providers survive the replace.
            catalog.inject_builtin_providers(BUILTIN_PROVIDERS);
        });

        // Restore sync state from DB and start background auto-export task.
        let sync = self.sync_manager().clone();
        sync.restore().await;
        sync.spawn_background_task(self.cancel().clone(), self.sync_user_id().to_string());

        self.restore_model_health_state().await;

        // Finalize any sessions left in `running` from a previous process.
        self.interrupt_stale_sessions_on_startup().await;

        self.reindex_all_projects_on_startup().await;

        // Watch .djinn/ directories for KB note changes and auto-reindex.
        crate::watchers::spawn_kb_watchers(
            self.db().clone(),
            self.events().clone(),
            self.cancel().clone(),
        );
    }

    async fn interrupt_stale_sessions_on_startup(&self) {
        use crate::db::SessionRepository;
        let repo = SessionRepository::new(self.db().clone(), self.events().clone());
        match repo.interrupt_all_running().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "interrupted stale sessions from previous run"),
            Err(e) => tracing::warn!(error = %e, "failed to interrupt stale sessions"),
        }
    }

    async fn reindex_all_projects_on_startup(&self) {
        let project_repo = ProjectRepository::new(self.db().clone(), self.events().clone());
        let note_repo = NoteRepository::new(self.db().clone(), self.events().clone());
        let projects = match project_repo.list().await {
            Ok(projects) => projects,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list projects for startup reindex");
                return;
            }
        };

        for project in projects {
            match note_repo
                .reindex_from_disk(&project.id, Path::new(&project.path))
                .await
            {
                Ok(summary) => tracing::info!(
                    project = %project.path,
                    updated = summary.updated,
                    created = summary.created,
                    deleted = summary.deleted,
                    unchanged = summary.unchanged,
                    "startup memory reindex completed"
                ),
                Err(e) => tracing::warn!(
                    project = %project.path,
                    error = %e,
                    "startup memory reindex failed"
                ),
            }
        }
    }

    pub async fn persist_model_health_state(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        let snapshot = self.health_tracker().all_health();
        match serde_json::to_string(&snapshot) {
            Ok(raw) => {
                if let Err(e) = repo.set(MODEL_HEALTH_STATE_KEY, &raw).await {
                    tracing::warn!(error = %e, "failed to persist model health state");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to serialize model health state"),
        }
    }

    async fn restore_model_health_state(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        let raw = repo
            .get(MODEL_HEALTH_STATE_KEY)
            .await
            .ok()
            .flatten()
            .map(|s| s.value);
        let Some(raw) = raw else {
            return;
        };
        match serde_json::from_str::<Vec<crate::provider::health::ModelHealth>>(&raw) {
            Ok(snapshot) => self.health_tracker().restore_all(snapshot),
            Err(e) => tracing::warn!(error = %e, "failed to parse model health state"),
        }
    }
}

/// Resolve the sync user identity.
///
/// **Single point of update:** When djinn authentication is added, change this
/// function to return the authenticated email. Every caller (JSONL filename,
/// commit author, event metadata) flows through `AppState::sync_user_id()`
/// which reads the value set here at startup.
///
/// Current strategy: `git config user.email` → sanitized email.
/// Fallback chain: git email → hostname → "local".
fn resolve_sync_user_id() -> String {
    // Try git config user.email first.
    if let Ok(output) = std::process::Command::new("git")
        .args(["config", "user.email"])
        .output()
        && output.status.success()
    {
        let email = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !email.is_empty() {
            return sanitize_sync_id(&email);
        }
    }

    // Fallback: machine hostname.
    if let Ok(output) = std::process::Command::new("hostname").output()
        && output.status.success()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return sanitize_sync_id(&name);
        }
    }

    "local".to_string()
}

/// Sanitize a string for use as a JSONL filename stem.
/// Replaces characters that are problematic in filenames with underscores.
fn sanitize_sync_id(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' | '@' => c,
            _ => '_',
        })
        .collect()
}

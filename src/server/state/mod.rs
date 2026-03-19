use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::events::DjinnEventEnvelope;
use crate::sync::SyncManager;
use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_db::{Database, NoteRepository, ProjectRepository, SettingsRepository};
use djinn_git::{GitActorHandle, GitError};
use djinn_provider::catalog::{CatalogService, HealthTracker};

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
    pub events: broadcast::Sender<DjinnEventEnvelope>,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    /// models.dev catalog + custom providers (in-memory, refreshed on startup).
    pub catalog: CatalogService,
    /// Per-model circuit-breaker health tracker.
    pub health_tracker: HealthTracker,
    pub role_registry: Arc<RoleRegistry>,
    /// djinn/ namespace git sync manager.
    pub sync: SyncManager,
    /// Long-running coordinator actor handle.
    pub coordinator: Arc<tokio::sync::Mutex<Option<CoordinatorHandle>>>,
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
    pub verifying_tasks: djinn_agent::actors::coordinator::VerificationTracker,
    /// Per-session file read timestamps used to enforce read-before-edit/write.
    pub file_time: Arc<FileTime>,
    pub lsp: LspManager,
    pub active_tasks: djinn_agent::context::ActivityTracker,
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
                git_actors: Arc::new(Mutex::new(HashMap::new())),
                catalog: CatalogService::new(),
                health_tracker: HealthTracker::new(),
                role_registry: Arc::new(RoleRegistry::new()),
                sync,
                coordinator: Arc::new(tokio::sync::Mutex::new(None)),
                pool: Mutex::new(None),
                sync_user_id,
                verifying_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
                file_time: Arc::new(FileTime::new()),
                lsp: LspManager::new(),
                active_tasks: djinn_agent::context::ActivityTracker::default(),
            }),
        }
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    pub fn events(&self) -> &broadcast::Sender<DjinnEventEnvelope> {
        &self.inner.events
    }

    pub fn event_bus(&self) -> crate::events::EventBus {
        crate::events::event_bus_for(&self.inner.events)
    }

    pub fn sync_user_id(&self) -> &str {
        &self.inner.sync_user_id
    }

    /// Get or spawn a `GitActorHandle` for the given project path (GIT-04).
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.inner.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut map, path)
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
        self.inner
            .verifying_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string());
    }

    /// Deregister a task's verification pipeline (completed or crashed).
    pub fn deregister_verification(&self, task_id: &str) {
        self.inner
            .verifying_tasks
            .lock()
            .expect("poisoned")
            .remove(task_id);
    }

    /// Check whether a task has a live verification pipeline.
    pub fn has_verification(&self, task_id: &str) -> bool {
        self.inner
            .verifying_tasks
            .lock()
            .expect("poisoned")
            .contains(task_id)
    }

    pub fn file_time(&self) -> &FileTime {
        &self.inner.file_time
    }

    pub fn agent_context(&self) -> djinn_agent::context::AgentContext {
        djinn_agent::context::AgentContext {
            db: self.inner.db.clone(),
            event_bus: self.event_bus(),
            git_actors: self.inner.git_actors.clone(),
            verifying_tasks: self.inner.verifying_tasks.clone(),
            role_registry: self.inner.role_registry.clone(),
            health_tracker: self.inner.health_tracker.clone(),
            file_time: self.inner.file_time.clone(),
            lsp: self.inner.lsp.clone(),
            catalog: self.inner.catalog.clone(),
            coordinator: self.inner.coordinator.clone(),
            active_tasks: self.inner.active_tasks.clone(),
        }
    }

    pub fn lsp(&self) -> &LspManager {
        &self.inner.lsp
    }

    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.inner.coordinator.lock().await.clone()
    }

    pub async fn pool(&self) -> Option<SlotPoolHandle> {
        self.inner.pool.lock().await.clone()
    }

    /// Non-blocking snapshot of the coordinator handle (for sync contexts).
    /// Returns `None` if the lock is contended or the coordinator is not yet initialized.
    pub fn coordinator_sync(&self) -> Option<CoordinatorHandle> {
        self.inner.coordinator.try_lock().ok()?.clone()
    }

    /// Non-blocking snapshot of the slot-pool handle (for sync contexts).
    /// Returns `None` if the lock is contended or the pool is not yet initialized.
    pub fn pool_sync(&self) -> Option<SlotPoolHandle> {
        self.inner.pool.try_lock().ok()?.clone()
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
            self.agent_context(),
            self.cancel().clone(),
            SlotPoolConfig {
                models: Vec::new(),
                role_priorities: std::collections::HashMap::new(),
            },
        );
        let coordinator =
            CoordinatorHandle::spawn(djinn_agent::actors::coordinator::CoordinatorDeps {
                events_tx: self.events().clone(),
                cancel: self.cancel().clone(),
                db: self.db().clone(),
                pool: pool.clone(),
                catalog: self.catalog().clone(),
                health: self.health_tracker().clone(),
                role_registry: self.inner.role_registry.clone(),
                verification_tracker: self.inner.verifying_tasks.clone(),
            });

        *self.inner.pool.lock().await = Some(pool.clone());
        *self.inner.coordinator.lock().await = Some(coordinator.clone());

        self.apply_runtime_settings_from_db().await;

        // Coordinator starts paused — require explicit `execution_start` to begin dispatching.
        tracing::info!("coordinator spawned (paused — awaiting explicit execution_start)");
    }

    /// Load custom providers from DB into the catalog and trigger a background
    /// catalog refresh from models.dev.  Call once after server startup.
    pub async fn initialize(&self) {
        use djinn_core::models::{Model, Provider};
        use djinn_provider::repos::CustomProviderRepository;

        // Load custom providers from DB → merge into in-memory catalog.
        let repo = CustomProviderRepository::new(self.db().clone(), self.event_bus());
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
                            pricing: djinn_core::models::Pricing::default(),
                        })
                        .collect();
                    self.catalog().add_custom_provider(provider, seed_models);
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to load custom providers from DB"),
        }

        // Inject synthetic catalog entries for built-in providers (e.g.
        // chatgpt_codex, gcp_vertex_ai) that aren't in models.dev.
        use djinn_provider::catalog::builtin::BUILTIN_PROVIDERS;
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

        // Prune stale verification cache entries (>7 days old).
        self.prune_verification_cache_on_startup().await;

        self.reindex_all_projects_on_startup().await;

        // Watch .djinn/ directories for KB note changes and auto-reindex.
        crate::watchers::spawn_kb_watchers(
            self.db().clone(),
            self.events().clone(),
            self.cancel().clone(),
        );

        crate::task_confidence::spawn_task_outcome_listener(self.clone());
    }

    async fn interrupt_stale_sessions_on_startup(&self) {
        use djinn_db::SessionRepository;
        let repo = SessionRepository::new(self.db().clone(), self.event_bus());
        match repo.interrupt_all_running().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "interrupted stale sessions from previous run"),
            Err(e) => tracing::warn!(error = %e, "failed to interrupt stale sessions"),
        }
    }

    async fn prune_verification_cache_on_startup(&self) {
        use djinn_db::VerificationCacheRepository;
        let repo = VerificationCacheRepository::new(self.db().clone());
        match repo.prune_older_than(7).await {
            Ok(()) => tracing::debug!("pruned stale verification cache entries"),
            Err(e) => tracing::warn!(error = %e, "failed to prune verification cache"),
        }
    }

    async fn reindex_all_projects_on_startup(&self) {
        let project_repo = ProjectRepository::new(self.db().clone(), self.event_bus());
        let note_repo = NoteRepository::new(self.db().clone(), self.event_bus());
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
        let repo = SettingsRepository::new(self.db().clone(), self.event_bus());
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
        let repo = SettingsRepository::new(self.db().clone(), self.event_bus());
        let raw = repo
            .get(MODEL_HEALTH_STATE_KEY)
            .await
            .ok()
            .flatten()
            .map(|s| s.value);
        let Some(raw) = raw else {
            return;
        };
        match serde_json::from_str::<Vec<djinn_provider::catalog::health::ModelHealth>>(&raw) {
            Ok(snapshot) => {
                // Filter out health entries whose provider prefix is a merged
                // child (e.g. "chatgpt_codex/…").  Merged children share
                // credentials with their parent and should never appear as
                // standalone model IDs — any such entries are stale artifacts.
                let merged = djinn_provider::catalog::builtin::merged_provider_ids();
                let snapshot: Vec<_> = snapshot
                    .into_iter()
                    .filter(|h| {
                        h.model_id
                            .split_once('/')
                            .is_none_or(|(prefix, _)| !merged.contains(prefix))
                    })
                    .collect();
                self.health_tracker().restore_all(snapshot);
            }
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

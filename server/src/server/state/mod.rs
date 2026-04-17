use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::db::runtime::{DatabaseRuntimeHealth, DatabaseRuntimeManager};
use crate::events::DjinnEventEnvelope;
use crate::semantic_memory::{EmbeddingService, default_embedding_cache_dir};
use crate::sync::SyncManager;
use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::runtime_bridge::{K8sTokenReviewValidator, RuntimeKind, runtime_kind};
use djinn_supervisor::{AllowAllValidator, ServeHandle, serve_on_tcp};
use djinn_db::{
    Database, NoopNoteVectorStore, NoteRepository, NoteVectorStore, ProjectRepository,
    QdrantNoteVectorStore, SettingsRepository,
};
use djinn_git::{GitActorHandle, GitError};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_provider::github_app::AppConfig as GitHubAppConfig;
use djinn_workspace::MirrorManager;

mod canonical_graph_refresh_planner;
mod settings;

use crate::memory_fs::MemoryViewSelection;
use crate::memory_mount::MountedMemoryFilesystem;
use canonical_graph_refresh_planner::{
    CanonicalGraphRefreshPlanner, CanonicalGraphRefreshProbe, RefreshPlan, WarmPlan, WarmPlanInputs,
};

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const SETTINGS_RAW_KEY: &str = "settings.raw";
const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";

/// Resolve the bare-mirror root directory, mirroring `vault_key_path`:
/// `$DJINN_HOME/mirrors` if set, else `$HOME/.djinn/mirrors`. Directory is
/// created on first mirror write (MirrorManager::ensure_mirror).
fn mirrors_root() -> PathBuf {
    if let Ok(djinn_home) = std::env::var("DJINN_HOME")
        && !djinn_home.is_empty()
    {
        return PathBuf::from(djinn_home).join("mirrors");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("mirrors")
}

fn canonical_view_resolution(
    active_task_count: usize,
    fallback: Option<crate::server::MemoryMountViewFallback>,
) -> crate::server::MemoryMountViewResolution {
    let fallback = fallback.or_else(|| {
        (active_task_count > 1).then(|| crate::server::MemoryMountViewFallback {
            reason: crate::server::MemoryMountViewFallbackReason::AmbiguousActiveTasks,
            detail: Some(
                "mounted memory requires exactly one active task before task-scoped selection can be used"
                    .to_string(),
            ),
            active_task_count: Some(active_task_count),
            task_id: None,
            task_short_id: None,
            task_project_id: None,
            mount_project_id: None,
            session_workspace_path: None,
        })
    });

    crate::server::MemoryMountViewResolution {
        selection: MemoryViewSelection::Canonical,
        health: crate::server::MemoryMountViewHealth {
            kind: crate::server::MemoryMountViewKind::Canonical,
            task_short_id: None,
            worktree_root: None,
            fallback,
        },
    }
}

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pub db: Database,
    pub db_runtime: DatabaseRuntimeManager,
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
    pub embedding_service: EmbeddingService,
    /// ADR-050 §3 single-flight gate for SCIP indexer subprocess
    /// invocations.  At most one `run_indexers` call is allowed to spawn
    /// a child process server-wide; additional callers queue on this
    /// mutex.  Combined with the `CARGO_BUILD_JOBS` cap this prevents
    /// the parallel-indexer cc-fanout meltdown.
    pub indexer_lock: Arc<tokio::sync::Mutex<()>>,
    /// Per-chat-session cache of canonical working roots, keyed by
    /// `(session_id, project_id)`.  The first chat completion request for
    /// a session calls `ensure_canonical_graph` and stores the returned
    /// index-tree path here; subsequent requests in the same session reuse
    /// the cached path and skip the warming call entirely (avoiding the
    /// per-request `git fetch` probe and worktree-add probe).  ADR-050
    /// Chunk C cleanup.
    pub chat_warmed_sessions: Arc<std::sync::Mutex<HashMap<(String, String), PathBuf>>>,
    /// Single-flight gate for background canonical-graph warm tasks spawned
    /// by `AppStateCanonicalGraphWarmer::warm`.  Keyed by `project_id`:
    /// membership means a detached warm task is already running for that
    /// project and additional warm requests should be coalesced (return
    /// immediately without spawning a duplicate task).  The entry is removed
    /// by the spawned task in its completion branch.
    pub canonical_warm_inflight: Arc<std::sync::Mutex<HashSet<String>>>,
    pub memory_mount: Mutex<Option<MountedMemoryFilesystem>>,
    /// Active GitHub App configuration (DB row → env fallback). Populated
    /// lazily on first read; hot-swapped by the manifest auto-provision
    /// callback so subsequent requests pick up new credentials without a
    /// process restart.
    pub app_config: tokio::sync::RwLock<Option<Arc<GitHubAppConfig>>>,
    /// Per-project bare git mirrors on disk. Single shared instance so
    /// fetches serialize correctly and clones hit the same hardlink pool.
    /// Path resolution mirrors the vault key: `$DJINN_HOME/mirrors` or
    /// `$HOME/.djinn/mirrors`.
    pub mirror: Arc<MirrorManager>,
    /// TCP listener for worker-pod RPC traffic.  Spawned in `initialize()` on
    /// the `DJINN_RUNTIME=kubernetes` (default) path; `None` on the
    /// `DJINN_RUNTIME=test` path and before boot finishes.  Wrapped in a
    /// `Mutex<Option<ServeHandle>>` rather than `OnceCell` so `shutdown()`
    /// can move the handle out and cancel it cleanly.
    pub rpc_server: tokio::sync::Mutex<Option<ServeHandle>>,
}

impl AppState {
    pub fn new(db: Database, cancel: CancellationToken) -> Self {
        let runtime = DatabaseRuntimeManager::new(crate::db::runtime::DatabaseRuntimeConfig::dolt(
            db.bootstrap_info().target.clone(),
        ));
        Self::new_with_runtime(db, runtime, cancel)
    }

    pub fn new_with_runtime(
        db: Database,
        db_runtime: DatabaseRuntimeManager,
        cancel: CancellationToken,
    ) -> Self {
        Self::new_inner(db, db_runtime, cancel)
    }

    fn new_inner(
        db: Database,
        db_runtime: DatabaseRuntimeManager,
        cancel: CancellationToken,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sync = SyncManager::new(db.clone(), events.clone());
        let sync_user_id = resolve_sync_user_id();
        tracing::info!(sync_user_id = %sync_user_id, "resolved sync user identity");
        Self {
            inner: Arc::new(Inner {
                db,
                db_runtime,
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
                embedding_service: EmbeddingService::new(default_embedding_cache_dir()),
                indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
                chat_warmed_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
                canonical_warm_inflight: Arc::new(std::sync::Mutex::new(HashSet::new())),
                memory_mount: Mutex::new(None),
                app_config: tokio::sync::RwLock::new(None),
                mirror: Arc::new(MirrorManager::new(mirrors_root())),
                rpc_server: tokio::sync::Mutex::new(None),
            }),
        }
    }

    /// Shared MirrorManager. Used by the task-run supervisor for ephemeral
    /// clones, the fetch watcher for periodic refreshes, and `task_merge`
    /// for mirror-direct pushes.
    pub fn mirror(&self) -> Arc<MirrorManager> {
        self.inner.mirror.clone()
    }

    /// Read-only snapshot of the active GitHub App configuration, if any.
    pub async fn app_config(&self) -> Option<Arc<GitHubAppConfig>> {
        self.inner.app_config.read().await.clone()
    }

    /// Hot-swap the in-memory GitHub App configuration. Called after the
    /// manifest auto-provision flow persists fresh credentials.
    pub async fn set_app_config(&self, cfg: Option<Arc<GitHubAppConfig>>) {
        *self.inner.app_config.write().await = cfg;
    }

    /// Initialise the in-memory App config from DB → env on startup.
    /// Called during server bootstrap; safe to call again to refresh.
    pub async fn init_app_config(&self) {
        let cfg = GitHubAppConfig::load(self.db(), self.event_bus()).await;
        if cfg.is_some() {
            tracing::info!("github_app: loaded persisted/env App configuration");
        } else {
            tracing::debug!("github_app: no persisted or env App configuration on startup");
        }
        // Mirror to env so consumers that still read env vars (the JWT
        // minter, the GitHubAppClient install URL helper, etc.) see the
        // same values without a code-wide refactor.
        if let Some(ref c) = cfg {
            c.export_to_env();
        }
        *self.inner.app_config.write().await = cfg.map(Arc::new);
    }

    /// Server-wide single-flight gate for SCIP indexer subprocess
    /// invocations (ADR-050 §3).
    pub fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.inner.indexer_lock.clone()
    }

    /// Attempt to claim the background canonical-graph warm slot for
    /// `project_id`.  Returns `true` if the slot was acquired (caller is
    /// responsible for releasing it via `release_canonical_warm_slot` once
    /// the spawned task finishes).  Returns `false` if another warm task is
    /// already in flight for this project — callers should coalesce and
    /// skip spawning a duplicate.
    pub fn try_claim_canonical_warm_slot(&self, project_id: &str) -> bool {
        self.inner
            .canonical_warm_inflight
            .lock()
            .expect("poisoned")
            .insert(project_id.to_string())
    }

    /// Release a previously-claimed canonical-graph warm slot for
    /// `project_id`.  Must be called by the detached warm task once it has
    /// finished (success or error) so subsequent dispatches on a new
    /// `origin/main` commit can retrigger warming.
    pub fn release_canonical_warm_slot(&self, project_id: &str) {
        self.inner
            .canonical_warm_inflight
            .lock()
            .expect("poisoned")
            .remove(project_id);
    }

    /// Look up a previously cached canonical working root for a chat
    /// session, if one was already warmed this process lifetime.  ADR-050
    /// Chunk C cleanup.
    pub fn chat_session_warmed_root(&self, session_id: &str, project_id: &str) -> Option<PathBuf> {
        self.inner
            .chat_warmed_sessions
            .lock()
            .expect("poisoned")
            .get(&(session_id.to_string(), project_id.to_string()))
            .cloned()
    }

    /// Record the canonical working root for a chat session so subsequent
    /// requests on the same session can skip the warming call entirely.
    /// ADR-050 Chunk C cleanup.
    pub fn chat_session_record_warmed(
        &self,
        session_id: &str,
        project_id: &str,
        working_root: PathBuf,
    ) {
        self.inner
            .chat_warmed_sessions
            .lock()
            .expect("poisoned")
            .insert(
                (session_id.to_string(), project_id.to_string()),
                working_root,
            );
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn db_runtime(&self) -> &DatabaseRuntimeManager {
        &self.inner.db_runtime
    }

    pub fn database_health(&self) -> DatabaseRuntimeHealth {
        self.inner.db_runtime.health_snapshot(self.db())
    }

    pub(crate) async fn memory_mount_health(&self) -> crate::server::MemoryMountHealth {
        let mount = self.inner.memory_mount.lock().await;
        let Some(mount) = mount.as_ref() else {
            return crate::server::MemoryMountHealth {
                enabled: false,
                active: false,
                lifecycle: crate::server::MemoryMountLifecycleState::Disabled,
                configured: false,
                mount_path: None,
                project_id: None,
                detail: None,
                view: crate::server::MemoryMountViewHealth {
                    kind: crate::server::MemoryMountViewKind::Canonical,
                    task_short_id: None,
                    worktree_root: None,
                    fallback: None,
                },
                pending_writes: 0,
                last_error: None,
            };
        };
        let active = mount.is_active();
        let status = mount.status_snapshot().await;
        crate::server::MemoryMountHealth {
            enabled: status.configured,
            active,
            lifecycle: status.lifecycle,
            configured: status.configured,
            mount_path: status.mount_path.map(|path| path.display().to_string()),
            project_id: status.project_id,
            detail: status.detail,
            view: status.view,
            pending_writes: status.pending_writes,
            last_error: status.last_error,
        }
    }

    #[cfg(test)]
    pub(crate) async fn set_memory_mount_for_tests(
        &self,
        mount: Option<crate::memory_mount::MountedMemoryFilesystem>,
    ) {
        *self.inner.memory_mount.lock().await = mount;
    }

    #[cfg_attr(
        not(any(test, all(target_os = "linux", feature = "memory-mount"))),
        allow(dead_code)
    )]
    pub(crate) async fn resolve_memory_mount_view_selection(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> MemoryViewSelection {
        self.resolve_memory_mount_view_resolution(project_id, project_path)
            .await
            .selection
    }

    #[cfg_attr(
        not(any(test, all(target_os = "linux", feature = "memory-mount"))),
        allow(dead_code)
    )]
    pub(crate) async fn resolve_memory_mount_view_resolution(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> crate::server::MemoryMountViewResolution {
        let active_task_ids: Vec<String> = self
            .inner
            .active_tasks
            .lock()
            .expect("poisoned")
            .keys()
            .cloned()
            .collect();

        let [task_id] = active_task_ids.as_slice() else {
            return canonical_view_resolution(active_task_ids.len(), None);
        };

        let task_repo = djinn_db::TaskRepository::new(self.db().clone(), self.event_bus());
        let Some(task) = task_repo.get(task_id).await.ok().flatten() else {
            tracing::debug!(
                task_id,
                "memory mount falling back to main: active task not found"
            );
            return canonical_view_resolution(
                1,
                Some(crate::server::MemoryMountViewFallback {
                    reason: crate::server::MemoryMountViewFallbackReason::ActiveTaskNotFound,
                    detail: Some("active task no longer exists in the database".to_string()),
                    active_task_count: Some(1),
                    task_id: Some(task_id.to_string()),
                    task_short_id: None,
                    task_project_id: None,
                    mount_project_id: Some(project_id.to_string()),
                    session_workspace_path: None,
                }),
            );
        };

        if task.project_id != project_id {
            tracing::debug!(
                task_id = %task.id,
                task_project_id = %task.project_id,
                mount_project_id = %project_id,
                "memory mount falling back to main: active task belongs to another project"
            );
            return canonical_view_resolution(
                1,
                Some(crate::server::MemoryMountViewFallback {
                    reason: crate::server::MemoryMountViewFallbackReason::TaskProjectMismatch,
                    detail: Some("active task belongs to another registered project".to_string()),
                    active_task_count: Some(1),
                    task_id: Some(task.id),
                    task_short_id: Some(task.short_id),
                    task_project_id: Some(task.project_id),
                    mount_project_id: Some(project_id.to_string()),
                    session_workspace_path: None,
                }),
            );
        }

        let session_repo = djinn_db::SessionRepository::new(self.db().clone(), self.event_bus());
        let Some(session) = session_repo.active_for_task(&task.id).await.ok().flatten() else {
            tracing::debug!(
                task_id = %task.id,
                short_id = %task.short_id,
                "memory mount falling back to main: no running session for active task"
            );
            return canonical_view_resolution(
                1,
                Some(crate::server::MemoryMountViewFallback {
                    reason: crate::server::MemoryMountViewFallbackReason::NoActiveSession,
                    detail: Some("no running session is attached to the active task".to_string()),
                    active_task_count: Some(1),
                    task_id: Some(task.id),
                    task_short_id: Some(task.short_id),
                    task_project_id: Some(project_id.to_string()),
                    mount_project_id: Some(project_id.to_string()),
                    session_workspace_path: None,
                }),
            );
        };

        // Prefer the workspace_path owned by the session's task_run (migration
        // 5 model).  Task #8 removed the `sessions.worktree_path` migration-
        // window fallback; task #13 will drop the column.
        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.db().clone());
        let workspace_source: Option<String> = match session.task_run_id.as_deref() {
            Some(run_id) => task_run_repo
                .get(run_id)
                .await
                .ok()
                .flatten()
                .and_then(|run| run.workspace_path),
            None => None,
        };

        let Some(workspace_path) = workspace_source
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            tracing::debug!(
                task_id = %task.id,
                short_id = %task.short_id,
                "memory mount falling back to main: active session has no workspace path"
            );
            return canonical_view_resolution(
                1,
                Some(crate::server::MemoryMountViewFallback {
                    reason: crate::server::MemoryMountViewFallbackReason::MissingSessionWorktree,
                    detail: Some("active session did not publish a workspace path".to_string()),
                    active_task_count: Some(1),
                    task_id: Some(task.id),
                    task_short_id: Some(task.short_id),
                    task_project_id: Some(project_id.to_string()),
                    mount_project_id: Some(project_id.to_string()),
                    session_workspace_path: None,
                }),
            );
        };

        let workspace_root = PathBuf::from(workspace_path);
        if workspace_root == project_path {
            tracing::debug!(
                task_id = %task.id,
                short_id = %task.short_id,
                "memory mount falling back to main: active session is on canonical project root"
            );
            return canonical_view_resolution(
                1,
                Some(crate::server::MemoryMountViewFallback {
                    reason: crate::server::MemoryMountViewFallbackReason::CanonicalProjectRoot,
                    detail: Some(
                        "active session workspace resolves to the canonical project root"
                            .to_string(),
                    ),
                    active_task_count: Some(1),
                    task_id: Some(task.id),
                    task_short_id: Some(task.short_id),
                    task_project_id: Some(project_id.to_string()),
                    mount_project_id: Some(project_id.to_string()),
                    session_workspace_path: Some(workspace_root.display().to_string()),
                }),
            );
        }

        crate::server::MemoryMountViewResolution {
            selection: MemoryViewSelection::Task {
                task_short_id: Some(task.short_id.clone()),
                worktree_root: Some(workspace_root.clone()),
            },
            health: crate::server::MemoryMountViewHealth {
                kind: crate::server::MemoryMountViewKind::TaskScoped,
                task_short_id: Some(task.short_id),
                worktree_root: Some(workspace_root.display().to_string()),
                fallback: None,
            },
        }
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

    pub fn embedding_service(&self) -> &EmbeddingService {
        &self.inner.embedding_service
    }

    pub fn note_vector_store(&self) -> Arc<dyn NoteVectorStore> {
        match std::env::var("DJINN_VECTOR_BACKEND") {
            Ok(value) if value.eq_ignore_ascii_case("qdrant") => {
                Arc::new(QdrantNoteVectorStore::new(Default::default())) as Arc<dyn NoteVectorStore>
            }
            Ok(value) if value.eq_ignore_ascii_case("noop") => {
                Arc::new(NoopNoteVectorStore) as Arc<dyn NoteVectorStore>
            }
            // With sqlite-vec retired, the default falls back to a
            // no-op vector store. Production deployments set
            // DJINN_VECTOR_BACKEND=qdrant.
            _ => Arc::new(NoopNoteVectorStore) as Arc<dyn NoteVectorStore>,
        }
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
            task_ops_project_path_override: None,
            working_root: None,
            canonical_graph_warmer: Some(Arc::new(AppStateCanonicalGraphWarmer {
                state: self.clone(),
            })),
            repo_graph_ops: Some(Arc::new(crate::mcp_bridge::RepoGraphBridge::new(
                self.clone(),
            ))),
            mirror: Some(self.inner.mirror.clone()),
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
        let coordinator = CoordinatorHandle::spawn(
            djinn_agent::actors::coordinator::CoordinatorDeps::new(
                self.events().clone(),
                self.cancel().clone(),
                self.db().clone(),
                pool.clone(),
                self.catalog().clone(),
                self.health_tracker().clone(),
                self.inner.role_registry.clone(),
                self.inner.verifying_tasks.clone(),
                self.inner.lsp.clone(),
            )
            .with_canonical_graph_warmer(Arc::new(AppStateCanonicalGraphWarmer {
                state: self.clone(),
            })
                as Arc<dyn djinn_agent::context::CanonicalGraphWarmer>)
            .with_mirror(self.inner.mirror.clone()),
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

        // Reindex in the background so the HTTP listener can bind immediately.
        // The reindex can be slow (especially with embeddings) and is not
        // required for the server to be functional.
        let reindex_self = self.clone();
        tokio::spawn(async move {
            reindex_self.reindex_all_projects_on_startup().await;
        });

        // Watch .djinn/ directories for KB note changes and auto-reindex.
        crate::watchers::spawn_kb_watchers(
            self.db().clone(),
            self.events().clone(),
            self.cancel().clone(),
            self.embedding_service().clone(),
        );

        // ADR-050 Chunk C: the filesystem-watcher SCIP trigger has been
        // removed.  SCIP indexing now happens lazily via
        // `ensure_canonical_graph` on architect dispatch and chat first
        // use.  Per-worktree skeleton refresh is no longer required.

        crate::task_confidence::spawn_task_outcome_listener(self.clone());

        // Phase 3C: periodic GitHub-org-membership reconciliation.
        // Flips `users.is_member_of_org` and revokes sessions when someone
        // leaves the locked org so their existing `djinn_session` cookie
        // stops working on the next request.
        crate::server::start_org_member_sync(self.clone());

        // Phase 2 K8s PR 4 pt2: spawn the TCP listener that worker Pods dial
        // back into.  Only runs on the `DJINN_RUNTIME=kubernetes` (default)
        // path; the `DJINN_RUNTIME=test` path exercises the supervisor
        // in-process through `TestRuntime` and does not need a TCP listener.
        self.start_rpc_listener_if_needed().await;
    }

    /// Spawn `djinn_supervisor::serve_on_tcp` on the configured RPC address
    /// when running under the Kubernetes runtime.  Idempotent — a second
    /// call finds an existing handle and returns without rebinding.
    ///
    /// Binding is best-effort: on boot inside Docker Compose (or anywhere
    /// without cluster access), `K8sTokenReviewValidator` falls back to
    /// `AllowAllValidator` so tests and dev loops can still exercise the
    /// RPC path.  Production Helm deployments always have cluster access
    /// via the pod's projected SA token.
    async fn start_rpc_listener_if_needed(&self) {
        use std::net::SocketAddr;

        if !matches!(runtime_kind(), RuntimeKind::Kubernetes) {
            tracing::info!(
                "rpc_server: DJINN_RUNTIME is not kubernetes; skipping TCP listener"
            );
            return;
        }

        {
            let existing = self.inner.rpc_server.lock().await;
            if existing.is_some() {
                tracing::debug!("rpc_server: listener already started");
                return;
            }
        }

        let rpc_addr: SocketAddr = match std::env::var("DJINN_RPC_ADDR") {
            Ok(raw) => match raw.parse() {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(
                        value = %raw,
                        error = %e,
                        "rpc_server: invalid DJINN_RPC_ADDR; falling back to 0.0.0.0:8443"
                    );
                    "0.0.0.0:8443".parse().expect("fallback parses")
                }
            },
            Err(_) => "0.0.0.0:8443".parse().expect("default parses"),
        };

        // Build the SupervisorServices the TCP server will dispatch to.  The
        // listener uses the server's long-lived cancellation token as its
        // supervisor-wide cancel — cancelling the server tears down any
        // in-flight RPC cleanly without reaching into individual task-runs.
        let agent_context = self.agent_context();
        let services = djinn_agent::supervisor::services_for_agent_context(
            agent_context,
            self.cancel().clone(),
        );

        // Validator: prefer the real TokenReview path; fall back to
        // AllowAllValidator if no kubeconfig is available (dev / CI).
        let handle_result = match kube::Client::try_default().await {
            Ok(client) => {
                let validator = Arc::new(K8sTokenReviewValidator::new(client, "djinn"));
                tracing::info!(
                    addr = %rpc_addr,
                    "rpc_server: binding TCP listener with K8sTokenReviewValidator"
                );
                serve_on_tcp(rpc_addr, services, validator).await
            }
            Err(e) => {
                tracing::warn!(
                    addr = %rpc_addr,
                    error = %e,
                    "rpc_server: kube::Client::try_default failed; \
                     falling back to AllowAllValidator (dev mode)"
                );
                serve_on_tcp(rpc_addr, services, Arc::new(AllowAllValidator)).await
            }
        };

        match handle_result {
            Ok(handle) => {
                tracing::info!(
                    addr = ?handle.bound_addr,
                    "rpc_server: TCP listener spawned"
                );
                *self.inner.rpc_server.lock().await = Some(handle);
            }
            Err(e) => {
                tracing::warn!(
                    addr = %rpc_addr,
                    error = %e,
                    "rpc_server: failed to bind TCP listener; the K8s dispatch \
                     path will not work until this is resolved"
                );
            }
        }
    }

    /// Cancel and join the RPC TCP listener if it was spawned.
    ///
    // TODO(phase 2.1): wire this into the process-wide graceful-shutdown
    // path (see `server::run` / `server::embedded`). Today the listener is
    // aborted implicitly when the tokio runtime shuts down — that's fine
    // for SIGTERM but doesn't drain in-flight RPC calls cleanly.
    pub async fn shutdown_rpc_listener(&self) {
        let handle = self.inner.rpc_server.lock().await.take();
        if let Some(handle) = handle {
            handle.cancel();
            let _ = handle.join.await;
            tracing::info!("rpc_server: TCP listener stopped");
        }
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
        let note_repo = NoteRepository::new(self.db().clone(), self.event_bus())
            .with_embedding_provider(Some(Arc::new(self.embedding_service().clone())))
            .with_vector_store(Some(self.note_vector_store()));
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

/// `CanonicalGraphWarmer` impl that bridges the agent lifecycle into
/// `crate::mcp_bridge::ensure_canonical_graph`.  ADR-050 Chunk C cold-start
/// fix: every dispatched task (any role) calls `warm` before its session
/// starts, which builds (or fetches from cache) the canonical-main graph and
/// persists the rendered skeleton as a `repo_map` note.  Workers then pick
/// the note up via the existing note-loading machinery without needing
/// per-worktree SCIP indexing.
struct AppStateCanonicalGraphWarmer {
    state: AppState,
}

struct AppStateCanonicalGraphRefreshProbe;

#[async_trait::async_trait]
impl CanonicalGraphRefreshProbe for AppStateCanonicalGraphRefreshProbe {
    async fn cache_has_entry_for(&self, index_tree_path: &Path) -> bool {
        crate::canonical_graph::canonical_graph_cache_has_entry_for(index_tree_path).await
    }

    async fn pinned_commit_for(&self, index_tree_path: &Path) -> Option<String> {
        crate::canonical_graph::canonical_graph_cache_pinned_commit_for(index_tree_path).await
    }

    async fn commits_since(&self, project_root: &Path, pinned_commit: &str) -> Option<u64> {
        crate::canonical_graph::canonical_graph_count_commits_since(project_root, pinned_commit)
            .await
    }
}

#[async_trait::async_trait]
impl djinn_agent::context::CanonicalGraphWarmer for AppStateCanonicalGraphWarmer {
    /// Detached-warm semantics (ADR-050 Chunk C follow-up, 2026-04-07):
    ///
    /// Previously this impl awaited `ensure_canonical_graph` inline, which
    /// meant the lifecycle's outer `tokio::time::timeout(45s, warmer.warm(..))`
    /// would drop the inner future on expiry.  On a cold cache the timeout
    /// routinely fired while `run_indexers_already_locked` was still running
    /// rust-analyzer — dropping that future killed the subprocess, the
    /// post-SCIP `spawn_blocking` CPU pipeline never started, and
    /// `GRAPH_CACHE` stayed empty forever.  Every architect dispatch then
    /// rehit the same timeout.
    ///
    /// The fix:
    ///   1. **Fast path**: if the in-memory `GRAPH_CACHE` already has an
    ///      entry for this project's `_index` worktree path, return
    ///      `Ok(())` instantly.  (We do not verify commit SHA here; the
    ///      detached task handles freshness when it does eventually run.)
    ///   2. **Single-flight**: try to claim a per-`project_id` slot.  If
    ///      another warm task is already running, coalesce and return
    ///      `Ok(())` instantly.
    ///   3. **Spawn detached**: `tokio::spawn` a background task that
    ///      `.await`s `ensure_canonical_graph` to completion on its own —
    ///      independent of any lifecycle timeout.  The post-SCIP CPU work
    ///      still happens in `spawn_blocking` inside `ensure_canonical_graph`;
    ///      detaching at this layer just prevents the lifecycle's 45s
    ///      timeout from cancelling the outer future.
    ///   4. Always return `Ok(())` — the lifecycle treats the result as
    ///      informational anyway and proceeds with whatever skeleton
    ///      currently lives in the DB `repo_map` note pipeline.
    async fn warm(&self, project_id: &str, project_root: &Path) -> Result<(), String> {
        let index_tree_path = project_root.join(".djinn").join("worktrees").join("_index");
        let planner = CanonicalGraphRefreshPlanner;
        let warm_plan = planner.plan_warm(WarmPlanInputs {
            cache_has_entry: crate::canonical_graph::canonical_graph_cache_has_entry_for(
                &index_tree_path,
            )
            .await,
            warm_slot_claimed: self.state.try_claim_canonical_warm_slot(project_id),
        });

        match warm_plan {
            WarmPlan::SkipHotCache => {
                self.state.release_canonical_warm_slot(project_id);
                tracing::debug!(
                    project_id = %project_id,
                    "AppStateCanonicalGraphWarmer: cache already hot, skipping warm"
                );
                return Ok(());
            }
            WarmPlan::CoalesceInflight => {
                tracing::info!(
                    project_id = %project_id,
                    "AppStateCanonicalGraphWarmer: warm already in flight, coalescing"
                );
                return Ok(());
            }
            WarmPlan::KickDetachedWarm => {}
        }

        // Detach the warm onto a background task so the lifecycle's outer
        // `tokio::time::timeout` cannot cancel it mid-flight.  The task
        // owns its own clones of every resource it needs.
        let state = self.state.clone();
        let project_id_owned = project_id.to_string();
        let project_root_owned = project_root.to_path_buf();
        tracing::info!(
            project_id = %project_id,
            project_root = %project_root_owned.display(),
            "AppStateCanonicalGraphWarmer: spawning background warm task"
        );
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = crate::canonical_graph::ensure_canonical_graph(
                &state,
                &project_id_owned,
                &project_root_owned,
            )
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok((handle, graph)) => {
                    tracing::info!(
                        project_id = %project_id_owned,
                        elapsed_ms,
                        commit_sha = %handle.commit_sha(),
                        node_count = graph.node_count(),
                        edge_count = graph.edge_count(),
                        "AppStateCanonicalGraphWarmer: background warm task complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        project_id = %project_id_owned,
                        elapsed_ms,
                        error = %e,
                        "AppStateCanonicalGraphWarmer: background warm task failed"
                    );
                }
            }
            state.release_canonical_warm_slot(&project_id_owned);
        });

        Ok(())
    }

    /// ADR-051 §3 proactive staleness refresh, called from the coordinator
    /// tick loop on a 10-minute cadence (see `GRAPH_REFRESH_INTERVAL`).
    ///
    /// Decision tree (always returns `Ok(())`):
    ///   1. Cold cache → no-op.  The cold path is owned by
    ///      `mcp_bridge::maybe_kick_background_warm`; kicking from here
    ///      would thrash on every tick for projects nobody is reading.
    ///   2. Warm cache but the pinned commit is missing (race window) →
    ///      no-op.
    ///   3. `git rev-list pinned..origin/main` returns `0` → cache is
    ///      already current, no-op.
    ///   4. `>= 1` commits behind → delegate to [`Self::warm`], which is
    ///      single-flight via `try_claim_canonical_warm_slot` and detached
    ///      onto a background task, so this call returns instantly.
    ///
    /// Errors at every step are logged at `debug!`/`warn!` and converted to
    /// `Ok(())` — the caller is fire-and-forget and scheduling churn is not
    /// worth surfacing transient git/fetch failures.
    async fn maybe_refresh_if_stale(
        &self,
        project_id: &str,
        project_root: &Path,
    ) -> Result<(), String> {
        let planner = CanonicalGraphRefreshPlanner;
        let probe = AppStateCanonicalGraphRefreshProbe;

        match planner.plan_refresh(&probe, project_root).await {
            RefreshPlan::SkipColdCache => {
                tracing::debug!(
                    project_id = %project_id,
                    "AppStateCanonicalGraphWarmer: cache cold, skipping proactive refresh (cold path is owned by maybe_kick_background_warm)"
                );
                Ok(())
            }
            RefreshPlan::SkipPinnedCommitUnavailable => {
                tracing::debug!(
                    project_id = %project_id,
                    "AppStateCanonicalGraphWarmer: cache pinned commit unavailable (race), skipping proactive refresh"
                );
                Ok(())
            }
            RefreshPlan::SkipCurrent { pinned_commit } => {
                tracing::debug!(
                    project_id = %project_id,
                    pinned_commit = %pinned_commit,
                    "AppStateCanonicalGraphWarmer: graph cache current, skipping refresh"
                );
                Ok(())
            }
            RefreshPlan::RefreshStale {
                pinned_commit,
                commits_behind,
            } => {
                tracing::info!(
                    project_id = %project_id,
                    pinned_commit = %pinned_commit,
                    commits_behind,
                    "AppStateCanonicalGraphWarmer: graph cache stale, kicking warm"
                );
                if let Err(e) = self.warm(project_id, project_root).await {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %e,
                        "AppStateCanonicalGraphWarmer: proactive warm dispatch reported error (swallowed)"
                    );
                }
                Ok(())
            }
            RefreshPlan::SkipCommitCheckFailed { pinned_commit } => {
                tracing::debug!(
                    project_id = %project_id,
                    pinned_commit = %pinned_commit,
                    "AppStateCanonicalGraphWarmer: count_commits_since failed (e.g. fetch error), skipping refresh"
                );
                Ok(())
            }
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

#[cfg(test)]
mod chat_warmed_session_tests {
    use super::*;
    use crate::test_helpers::create_test_db;

    /// Recording a warmed working_root for a `(session_id, project_id)`
    /// pair makes subsequent lookups return the cached path, and a
    /// different session_id misses.  This is the storage primitive the
    /// chat first-use hook relies on to call `ensure_canonical_graph`
    /// exactly once per chat session (ADR-050 Chunk C cleanup).
    #[tokio::test]
    async fn record_then_lookup_per_session() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let state = AppState::new(db, cancel);

        let session_a = "session-a";
        let session_b = "session-b";
        let project = "proj-1";

        assert!(state.chat_session_warmed_root(session_a, project).is_none());

        let root = PathBuf::from("/tmp/canonical-index");
        state.chat_session_record_warmed(session_a, project, root.clone());

        // Same session: hit.
        assert_eq!(
            state.chat_session_warmed_root(session_a, project),
            Some(root.clone())
        );
        // Same session twice: still a hit, value unchanged.
        assert_eq!(
            state.chat_session_warmed_root(session_a, project),
            Some(root)
        );
        // Different session: miss — would trigger a fresh warming call.
        assert!(state.chat_session_warmed_root(session_b, project).is_none());
    }
}

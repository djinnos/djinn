use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::db::runtime::{DatabaseRuntimeHealth, DatabaseRuntimeManager};
use crate::events::DjinnEventEnvelope;
use crate::semantic_memory::{EmbeddingService, default_embedding_cache_dir};
use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::runtime_bridge::{K8sTokenReviewValidator, RuntimeKind, runtime_kind};
use djinn_supervisor::{AllowAllValidator, ConnectionRegistry, ServeHandle, serve_on_tcp};
use djinn_db::{
    Database, NoopNoteVectorStore, NoteRepository, NoteVectorStore, ProjectRepository,
    QdrantConfig, QdrantNoteVectorStore, SettingsRepository,
};
use djinn_git::{GitActorHandle, GitError};
use djinn_image_controller::{ImageBuildWatcher, ImageController, ImageControllerConfig};
use djinn_k8s::{K8sGraphWarmer, KubernetesConfig};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_provider::github_app::AppConfig as GitHubAppConfig;
use djinn_runtime::GraphWarmerService;
use djinn_workspace::{MirrorManager, mirrors_root};

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

/// Report which `GITHUB_APP_*` env vars are unset or empty, so `init_app_config`
/// can surface a useful diagnosis when `GitHubAppConfig::load()` returns `None`.
fn missing_github_app_env_vars() -> Vec<&'static str> {
    fn empty(key: &str) -> bool {
        std::env::var(key).ok().filter(|v| !v.is_empty()).is_none()
    }
    let mut missing = Vec::new();
    for k in [
        "GITHUB_APP_ID",
        "GITHUB_APP_CLIENT_ID",
        "GITHUB_APP_CLIENT_SECRET",
    ] {
        if empty(k) {
            missing.push(k);
        }
    }
    if empty("GITHUB_APP_PRIVATE_KEY") && empty("GITHUB_APP_PRIVATE_KEY_PATH") {
        missing.push("GITHUB_APP_PRIVATE_KEY");
    }
    missing
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
    /// Long-running coordinator actor handle.
    pub coordinator: Arc<tokio::sync::Mutex<Option<CoordinatorHandle>>>,
    /// Long-running slot pool actor handle.
    pub pool: Mutex<Option<SlotPoolHandle>>,
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
    /// by the in-process graph warmer (`build_in_process_graph_warmer`).
    /// Keyed by `project_id`: membership means a detached warm task is
    /// already running for that project and additional warm requests should
    /// be coalesced (return immediately without spawning a duplicate task).
    /// The entry is removed by the spawned task in its completion branch.
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
    /// Process-wide [`ConnectionRegistry`] shared with the TCP listener and
    /// every [`djinn_k8s::KubernetesRuntime`] the slot runner constructs.
    /// Always present (allocated eagerly in `new_inner`) so callers never
    /// race against listener boot order — the registry is cheap to hold
    /// around when the `DJINN_RUNTIME=test` path doesn't exercise it.
    pub rpc_registry: Arc<ConnectionRegistry>,
    /// Phase 3 PR 5 — per-project devcontainer image controller.
    ///
    /// Populated during [`AppState::initialize`] when a `kube::Client` can
    /// be constructed from the ambient environment (in-cluster SA token or
    /// local `$KUBECONFIG`). Remains `None` on dev boxes without a cluster
    /// — the mirror-fetcher reads this via [`AppState::image_controller`]
    /// and silently skips the enqueue step when absent.
    pub image_controller: tokio::sync::RwLock<Option<Arc<ImageController>>>,
    /// Phase 3 PR 5.5 — background task that watches build `Job`s to
    /// terminal state and flips `projects.image_status`.  Spawned
    /// alongside the controller when a `kube::Client` is available;
    /// `None` on dev boxes without a cluster. `shutdown_image_watcher`
    /// aborts + awaits the task on graceful shutdown.
    pub image_build_watcher: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Phase 3 PR 8 — production canonical-graph warmer.  Populated during
    /// [`AppState::initialize`]: prefers [`K8sGraphWarmer`] when running
    /// under `DJINN_RUNTIME=kubernetes` with a reachable `kube::Client`;
    /// falls back to [`build_in_process_graph_warmer`] otherwise so dev
    /// boxes and `TestRuntime` stay operational.  Read via
    /// [`AppState::graph_warmer`]; mirror-fetcher + agent dispatch paths
    /// dispatch through this handle rather than constructing a warmer
    /// per-call.
    pub graph_warmer: tokio::sync::RwLock<Option<Arc<dyn GraphWarmerService>>>,
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
                coordinator: Arc::new(tokio::sync::Mutex::new(None)),
                pool: Mutex::new(None),
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
                rpc_registry: Arc::new(ConnectionRegistry::new()),
                image_controller: tokio::sync::RwLock::new(None),
                image_build_watcher: tokio::sync::Mutex::new(None),
                graph_warmer: tokio::sync::RwLock::new(None),
            }),
        }
    }

    /// Shared MirrorManager. Used by the task-run supervisor for ephemeral
    /// clones, the fetch watcher for periodic refreshes, and `task_merge`
    /// for mirror-direct pushes.
    pub fn mirror(&self) -> Arc<MirrorManager> {
        self.inner.mirror.clone()
    }

    /// Shared process-wide [`ConnectionRegistry`].  The same `Arc` is handed
    /// to `serve_on_tcp` on boot and to every `KubernetesRuntime` constructed
    /// by the slot runner, so workers dialling the TCP listener and the
    /// runtime awaiting their handshake share a single bridge.
    pub fn rpc_registry(&self) -> Arc<ConnectionRegistry> {
        self.inner.rpc_registry.clone()
    }

    /// Per-project devcontainer image controller (Phase 3 PR 5).
    ///
    /// `None` on dev boxes without a reachable cluster. The mirror-fetcher
    /// threads this through — an absent controller means the enqueue hook
    /// is silently skipped, which is the correct local-dev behaviour.
    pub async fn image_controller(&self) -> Option<Arc<ImageController>> {
        self.inner.image_controller.read().await.clone()
    }

    /// Construct the image controller once a `kube::Client` is available.
    ///
    /// Called from [`Self::initialize`]. Idempotent — a second call that
    /// finds an existing controller is a no-op.
    async fn initialize_image_controller(&self) {
        {
            let existing = self.inner.image_controller.read().await;
            if existing.is_some() {
                return;
            }
        }

        if !matches!(runtime_kind(), RuntimeKind::Kubernetes) {
            tracing::debug!(
                "image_controller: DJINN_RUNTIME is not kubernetes; skipping controller construction"
            );
            return;
        }

        let client = match kube::Client::try_default().await {
            Ok(c) => c,
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "image_controller: no kube::Client available; controller disabled \
                     (dev/local mode — per-project builds skipped)"
                );
                return;
            }
        };

        let config = ImageControllerConfig::from_env();
        let controller = Arc::new(ImageController::new(
            client.clone(),
            config.clone(),
            self.db().clone(),
        ));
        {
            let mut guard = self.inner.image_controller.write().await;
            *guard = Some(controller);
        }
        tracing::info!("image_controller: initialized");

        // Phase 3 PR 5.5: spawn the companion Job-completion watcher so
        // `projects.image_status` flips from `building` → `ready`/`failed`
        // without operator intervention. Uses the same `kube::Client`
        // and config; observes `self.cancel()` for graceful shutdown.
        //
        // Inject the graph warmer so a successful build transition kicks
        // the canonical-graph warm without waiting for the next mirror-fetch
        // tick — this closes the last gap before the coordinator's dispatch
        // gate can clear on first setup.
        let warmer = self.graph_warmer().await;
        let handle = ImageBuildWatcher::spawn(
            client,
            config,
            self.db().clone(),
            self.event_bus(),
            Some(warmer),
            self.cancel().clone(),
        );
        *self.inner.image_build_watcher.lock().await = Some(handle);
        tracing::info!("image_build_watcher: spawned");
    }

    /// Abort + await the image-build watcher task if it was spawned.
    ///
    /// Called from the process-wide graceful-shutdown path alongside
    /// [`Self::shutdown_rpc_listener`] so the background task exits
    /// cleanly rather than being dropped implicitly with the runtime.
    pub async fn shutdown_image_watcher(&self) {
        let handle = self.inner.image_build_watcher.lock().await.take();
        if let Some(handle) = handle {
            // The watcher exits on its own when `self.cancel()` fires;
            // abort is belt-and-braces in case cancellation was already
            // observed but the task is still winding down.
            handle.abort();
            let _ = handle.await;
            tracing::info!("image_build_watcher: stopped");
        }
    }

    /// Return the process-wide canonical-graph warmer (Phase 3 PR 8).
    ///
    /// Prefers the cluster-backed [`K8sGraphWarmer`] when
    /// [`AppState::initialize_graph_warmer`] managed to construct one;
    /// otherwise falls back on-demand to the in-process implementation so
    /// mirror-fetcher and agent-dispatch call sites never have to branch
    /// on "is a warmer configured". The returned `Arc` is cheaply cloned.
    pub async fn graph_warmer(&self) -> Arc<dyn GraphWarmerService> {
        if let Some(warmer) = self.inner.graph_warmer.read().await.clone() {
            return warmer;
        }
        // Fallback: build an in-process warmer lazily. Kept identical to
        // the production shape so `TestRuntime` and dev boxes that never
        // ran `initialize()` still get correct semantics.
        Arc::new(build_in_process_graph_warmer(self.clone())) as Arc<dyn GraphWarmerService>
    }

    /// Pick the best available [`GraphWarmerService`] implementation and
    /// cache it on [`AppState`]. Idempotent.
    ///
    /// Policy:
    /// * If `DJINN_RUNTIME=kubernetes` (or unset — default) AND a
    ///   `kube::Client` can be constructed → [`K8sGraphWarmer`].
    /// * Otherwise (explicit `DJINN_RUNTIME=test`, local dev without a
    ///   cluster) → in-process warmer via [`build_in_process_graph_warmer`].
    async fn initialize_graph_warmer(&self) {
        {
            let existing = self.inner.graph_warmer.read().await;
            if existing.is_some() {
                return;
            }
        }

        let prefer_k8s = matches!(runtime_kind(), RuntimeKind::Kubernetes);
        let warmer: Arc<dyn GraphWarmerService> = if prefer_k8s {
            match kube::Client::try_default().await {
                Ok(client) => {
                    let config = KubernetesConfig::from_env();
                    tracing::info!(
                        namespace = %config.namespace,
                        "graph_warmer: wiring K8sGraphWarmer"
                    );
                    Arc::new(K8sGraphWarmer::new(client, config, self.db().clone()))
                        as Arc<dyn GraphWarmerService>
                }
                Err(e) => {
                    tracing::info!(
                        error = %e,
                        "graph_warmer: no kube::Client available; falling back to in-process warmer"
                    );
                    Arc::new(build_in_process_graph_warmer(self.clone()))
                        as Arc<dyn GraphWarmerService>
                }
            }
        } else {
            tracing::debug!(
                "graph_warmer: DJINN_RUNTIME is not kubernetes; using in-process warmer"
            );
            Arc::new(build_in_process_graph_warmer(self.clone())) as Arc<dyn GraphWarmerService>
        };

        let mut guard = self.inner.graph_warmer.write().await;
        *guard = Some(warmer);
        tracing::info!("graph_warmer: initialized");
    }

    /// Minimal constructor used by out-of-process test callers that need an
    /// `AppState` without the full bootstrap (originally used by
    /// `djinn-server --warm-graph`, now retained for tests only — the warm
    /// path lives in `djinn-agent-worker warm-graph`, which bootstraps its
    /// own `djinn_graph::WarmContext` implementation).
    ///
    /// Boots ONLY the subsystems [`djinn_graph::canonical_graph::ensure_canonical_graph`]
    /// needs — DB + mirror + event bus — and leaves every other service
    /// (HTTP listener, MCP server, coordinator, RPC listener, agent
    /// actors) uninitialised.  The warm Pod is short-lived, has no
    /// inbound traffic, and exits after a single warm run, so the
    /// fat-server bootstrap penalty (≈2–3s) is unnecessary.
    ///
    /// The returned state is wired to the normal Dolt-MySQL pool via the
    /// environment-driven [`crate::db::runtime::DatabaseRuntimeConfig`]
    /// so the warm Pod reads/writes the same `repo_graph_cache` rows the
    /// full server consumes.
    pub async fn minimal_for_warm_only() -> anyhow::Result<Self> {
        let cancel = CancellationToken::new();
        let db_runtime = DatabaseRuntimeManager::new(
            crate::db::runtime::DatabaseRuntimeConfig::from_cli_and_env(None, None, None, None)
                .map_err(|e| anyhow::anyhow!("invalid database runtime configuration: {e}"))?,
        );
        db_runtime
            .ensure_runtime_available()
            .map_err(|e| anyhow::anyhow!("ensure database runtime: {e}"))?;
        let db = db_runtime
            .bootstrap()
            .map_err(|e| anyhow::anyhow!("open database runtime: {e}"))?;
        Ok(Self::new_with_runtime(db, db_runtime, cancel))
    }

    /// Read-only snapshot of the active GitHub App configuration, if any.
    pub async fn app_config(&self) -> Option<Arc<GitHubAppConfig>> {
        self.inner.app_config.read().await.clone()
    }

    /// Hot-swap the in-memory GitHub App configuration. Retained for tests
    /// that seed an in-memory state — the production path only writes once
    /// from `init_app_config` (env vars require a Pod restart to change).
    pub async fn set_app_config(&self, cfg: Option<Arc<GitHubAppConfig>>) {
        *self.inner.app_config.write().await = cfg;
    }

    /// Initialise the in-memory App config from environment variables on
    /// startup. Called during server bootstrap.
    pub async fn init_app_config(&self) {
        let cfg = GitHubAppConfig::load();
        if cfg.is_some() {
            tracing::info!("github_app: loaded App configuration from env");
        } else {
            // Previously logged at `debug!` which is invisible at the default
            // log level, producing a silent "GitHub App not configured"
            // outcome for operators who thought they wired the Secret
            // correctly. Log at `warn!` with the specific unset vars so
            // operators get a single-line diagnosis in `kubectl logs`.
            let missing = missing_github_app_env_vars();
            tracing::warn!(
                missing = missing.join(",").as_str(),
                "github_app: App configuration not loaded — \
                 mount the djinn-github-app Secret or set the listed \
                 GITHUB_APP_* env vars to enable GitHub integration"
            );
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

    pub fn embedding_service(&self) -> &EmbeddingService {
        &self.inner.embedding_service
    }

    pub fn note_vector_store(&self) -> Arc<dyn NoteVectorStore> {
        match std::env::var("DJINN_VECTOR_BACKEND") {
            Ok(value) if value.eq_ignore_ascii_case("qdrant") => {
                let mut config = QdrantConfig::default();
                if let Ok(url) = std::env::var("QDRANT_URL")
                    && !url.is_empty()
                {
                    config.url = url;
                }
                Arc::new(QdrantNoteVectorStore::new(config)) as Arc<dyn NoteVectorStore>
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
        // Prefer the cached warmer (K8s or in-process per
        // `initialize_graph_warmer`); fall back to a fresh in-process
        // warmer when the cache is cold (test paths + dev boxes that
        // skip `initialize()`).  `try_read` stays on the sync path so
        // `agent_context()` keeps its non-async signature.
        let graph_warmer = self
            .inner
            .graph_warmer
            .try_read()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| {
                Arc::new(build_in_process_graph_warmer(self.clone()))
                    as Arc<dyn djinn_runtime::GraphWarmerService>
            });

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
            graph_warmer: Some(graph_warmer),
            repo_graph_ops: Some(Arc::new(crate::mcp_bridge::RepoGraphBridge::new(
                self.clone(),
            ))),
            mirror: Some(self.inner.mirror.clone()),
            rpc_registry: Some(self.inner.rpc_registry.clone()),
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
            .with_graph_warmer(self.graph_warmer().await)
            .with_mirror(self.inner.mirror.clone()),
        );

        *self.inner.pool.lock().await = Some(pool.clone());
        *self.inner.coordinator.lock().await = Some(coordinator.clone());

        self.apply_runtime_settings_from_db().await;

        // Coordinator is always-on in K8s mode; dispatch is gated per-project
        // by the image-ready + graph-warmed readiness check in `dispatch.rs`.
        tracing::info!("coordinator spawned (always dispatching; gated by project readiness)");
    }

    /// Load custom providers from DB into the catalog and trigger a background
    /// catalog refresh from models.dev.  Call once after server startup.
    pub async fn initialize(&self) {
        use djinn_core::models::{Model, Provider};
        use djinn_provider::repos::{CredentialRepository, CustomProviderRepository};

        // Bootstrap provider API keys from deployment-provided env vars
        // (ANTHROPIC_API_KEY, OPENAI_API_KEY, …) into the encrypted vault
        // before anything else reads from it. Idempotent upsert — a Helm
        // upgrade takes effect on the next pod restart.
        let credential_repo = CredentialRepository::new(self.db().clone(), self.event_bus());
        if let Err(e) = djinn_provider::bootstrap::bootstrap_env_credentials(&credential_repo).await
        {
            tracing::warn!(error = %e, "failed to bootstrap provider env credentials");
        }

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

        djinn_agent::verification::task_confidence::spawn_task_outcome_listener(
            self.db().clone(),
            self.event_bus(),
            self.events(),
        );

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

        // Phase 3 PR 5: per-project devcontainer image controller.  Best-
        // effort: absent on dev boxes without a kube::Client; the mirror
        // fetcher silently skips the enqueue step when `image_controller()`
        // returns `None`.
        self.initialize_image_controller().await;

        // Phase 3 PR 8: pick the canonical-graph warmer impl (K8s or
        // in-process) and cache it.  Call order: after
        // `image_controller` because the K8s warmer reads the same
        // `kube::Client` path and we want the info log to surface even
        // if the controller short-circuited (e.g. env disabled).
        self.initialize_graph_warmer().await;
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
        //
        // Threads the process-wide `ConnectionRegistry` into the accept
        // loop so per-task-run `PendingConnection` slots reserved by
        // `KubernetesRuntime::prepare` pick up the worker's inbound
        // `FramePayload::Event` frames once the handshake lands.
        let registry = self.inner.rpc_registry.clone();
        let handle_result = match kube::Client::try_default().await {
            Ok(client) => {
                let validator = Arc::new(K8sTokenReviewValidator::new(client, "djinn"));
                tracing::info!(
                    addr = %rpc_addr,
                    "rpc_server: binding TCP listener with K8sTokenReviewValidator"
                );
                serve_on_tcp(rpc_addr, services, validator, Some(registry)).await
            }
            Err(e) => {
                tracing::warn!(
                    addr = %rpc_addr,
                    error = %e,
                    "rpc_server: kube::Client::try_default failed; \
                     falling back to AllowAllValidator (dev mode)"
                );
                serve_on_tcp(
                    rpc_addr,
                    services,
                    Arc::new(AllowAllValidator),
                    Some(registry),
                )
                .await
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
    /// Called from the process-wide graceful-shutdown path (`djinn-server`
    /// binary's `async_main` runs `server::run(...).await` and then calls
    /// this before dropping the `AppState`), so in-flight RPC connections
    /// get a clean cancel + join instead of being torn down implicitly
    /// when the tokio runtime exits.
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

/// Bridge the server's `AppState` onto the `djinn_graph::WarmContext`
/// seam.  All three accessors already exist on `AppState` — we just
/// delegate through the trait so `djinn_graph::canonical_graph::*`
/// functions can drive the pipeline without taking `&AppState` directly.
impl djinn_graph::WarmContext for AppState {
    fn db(&self) -> &djinn_db::Database {
        AppState::db(self)
    }

    fn event_bus(&self) -> djinn_core::events::EventBus {
        AppState::event_bus(self)
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        AppState::indexer_lock(self)
    }
}

/// Refresh probe used by the `CanonicalGraphRefreshPlanner`.  Unchanged from
/// the pre-PR-7 shape — the planner stays in place to drive the decision
/// tree for "cold / pinned-commit-unavailable / current / stale" before we
/// hand off to the heavy warm pipeline.
struct AppStateCanonicalGraphRefreshProbe;

#[async_trait::async_trait]
impl CanonicalGraphRefreshProbe for AppStateCanonicalGraphRefreshProbe {
    async fn cache_has_entry_for(&self, index_tree_path: &Path) -> bool {
        djinn_graph::canonical_graph::canonical_graph_cache_has_entry_for(index_tree_path).await
    }

    async fn pinned_commit_for(&self, index_tree_path: &Path) -> Option<String> {
        djinn_graph::canonical_graph::canonical_graph_cache_pinned_commit_for(index_tree_path).await
    }

    async fn commits_since(&self, project_root: &Path, pinned_commit: &str) -> Option<u64> {
        djinn_graph::canonical_graph::canonical_graph_count_commits_since(project_root, pinned_commit)
            .await
    }
}

/// Build the production [`djinn_agent::warmer::InProcessGraphWarmer`] backed
/// by this `AppState`.
///
/// The warmer is the sole in-process implementation of
/// [`djinn_runtime::GraphWarmerService`] — it wraps the server's
/// `ensure_canonical_graph` pipeline behind three callbacks so djinn-agent
/// stays free of any server-crate dependency.
///
/// * `warm` — fires the existing single-flight + detached-spawn pipeline.
///   The closure returns `Ok(())` immediately after claiming the slot and
///   spawning the background task; the heavy pipeline runs independently of
///   the caller's future.
/// * `project_root` — resolves a `project_id` to the on-disk project root
///   via `ProjectRepository::get`.  Returns `None` when the project has
///   been deleted.
/// * `is_fresh` — delegates to the `CanonicalGraphRefreshPlanner` to decide
///   whether the in-memory `GRAPH_CACHE` is current for the project's
///   `_index` worktree.  `SkipColdCache` and `SkipPinnedCommitUnavailable`
///   are treated as not-fresh; everything else (the cache contains an entry
///   whose pinned commit is either known-current or
///   commit-check-failed) is treated as fresh so `await_fresh` does not spin.
fn build_in_process_graph_warmer(
    state: AppState,
) -> djinn_agent::warmer::InProcessGraphWarmer {
    use djinn_agent::warmer::{InProcessGraphWarmer, InProcessWarmerDeps};
    use djinn_db::ProjectRepository;

    let warm_state = state.clone();
    let warm: djinn_agent::warmer::WarmCallback = Arc::new(move |project_id, project_root| {
        let state = warm_state.clone();
        Box::pin(async move {
            let index_tree_path = project_root.join(".djinn").join("worktrees").join("_index");
            let planner = CanonicalGraphRefreshPlanner;
            let warm_plan = planner.plan_warm(WarmPlanInputs {
                cache_has_entry: djinn_graph::canonical_graph::canonical_graph_cache_has_entry_for(
                    &index_tree_path,
                )
                .await,
                warm_slot_claimed: state.try_claim_canonical_warm_slot(&project_id),
            });

            match warm_plan {
                WarmPlan::SkipHotCache => {
                    state.release_canonical_warm_slot(&project_id);
                    tracing::debug!(
                        project_id = %project_id,
                        "AppStateGraphWarmer: cache already hot, skipping warm"
                    );
                    return Ok(());
                }
                WarmPlan::CoalesceInflight => {
                    tracing::info!(
                        project_id = %project_id,
                        "AppStateGraphWarmer: warm already in flight, coalescing"
                    );
                    return Ok(());
                }
                WarmPlan::KickDetachedWarm => {}
            }

            // Detach the warm onto a background task so the caller's future
            // cannot cancel it mid-flight.  The task owns its own clones of
            // every resource it needs.
            let state = state.clone();
            let project_id_owned = project_id.clone();
            let project_root_owned = project_root;
            tracing::info!(
                project_id = %project_id,
                project_root = %project_root_owned.display(),
                "AppStateGraphWarmer: spawning background warm task"
            );
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                // Architect-only warm path: this closure is only wired in
                // via `GraphWarmerService::trigger`, which dispatch.rs gates
                // on `role == "architect"` (plus the mirror-fetcher tick,
                // which is the scheduled-refresh sibling of the architect
                // dispatch path).  See `djinn_graph::architect` for the
                // invariant.
                let result = djinn_graph::canonical_graph::ensure_canonical_graph(
                    &state,
                    &project_id_owned,
                    &project_root_owned,
                    djinn_graph::architect::ArchitectWarmToken::new(),
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
                            "AppStateGraphWarmer: background warm task complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            project_id = %project_id_owned,
                            elapsed_ms,
                            error = %e,
                            "AppStateGraphWarmer: background warm task failed"
                        );
                    }
                }
                state.release_canonical_warm_slot(&project_id_owned);
            });

            Ok(())
        })
    });

    let project_root_state = state.clone();
    let project_root: djinn_agent::warmer::ProjectRootResolver =
        Arc::new(move |project_id| {
            let state = project_root_state.clone();
            Box::pin(async move {
                let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
                match repo.get(&project_id).await {
                    Ok(Some(project)) => Some(PathBuf::from(project.path)),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(
                            project_id = %project_id,
                            error = %e,
                            "AppStateGraphWarmer: project lookup failed"
                        );
                        None
                    }
                }
            })
        });

    let is_fresh: djinn_agent::warmer::FreshnessProbe =
        Arc::new(move |_project_id, project_root, _ttl| {
            Box::pin(async move {
                // Freshness model: the graph is considered fresh when the
                // planner's refresh decision is anything other than
                // "cold cache" or "pinned commit unavailable".  That covers
                // both the "cache current" and "commit-check failed"
                // branches — the latter being a transient git/fetch error
                // where we would rather proceed with a slightly-stale graph
                // than spin waiting for the network to recover.
                let planner = CanonicalGraphRefreshPlanner;
                let probe = AppStateCanonicalGraphRefreshProbe;
                match planner.plan_refresh(&probe, &project_root).await {
                    RefreshPlan::SkipColdCache
                    | RefreshPlan::SkipPinnedCommitUnavailable
                    | RefreshPlan::RefreshStale { .. } => false,
                    RefreshPlan::SkipCurrent { .. }
                    | RefreshPlan::SkipCommitCheckFailed { .. } => true,
                }
            })
        });

    InProcessGraphWarmer::new(InProcessWarmerDeps {
        warm,
        project_root,
        is_fresh,
    })
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

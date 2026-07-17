// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::db::runtime::{DatabaseRuntimeHealth, DatabaseRuntimeManager};
use crate::events::DjinnEventEnvelope;
use djinn_agent::actors::coordinator::{
    BuildAdmissionController, BuildAdmissionMode, CoordinatorHandle,
};
use djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::runtime_bridge::{K8sTokenReviewValidator, RuntimeKind, runtime_kind};
use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};
use djinn_db::{
    Database, NoopNoteVectorStore, NoteVectorStore, ProjectRepository, QdrantCodeChunkConfig,
    QdrantCodeChunkVectorStore, QdrantConfig, QdrantNoteVectorStore, SettingsRepository,
};
use djinn_git::{GitActorHandle, GitError};
use djinn_image_controller::{
    ImageBuildWatcher, ImageController, ImageControllerConfig, PreflightError, PreflightOutcome,
    PreflightOutcomeKind, RetentionPreflightConfig, ZotHttpAuth, ZotHttpConfig, ZotHttpStateSource,
    ZotRepository, ZotStateError, ZotStateSource, adapt_selected_images, run_preflight,
};
use djinn_k8s::{K8sGraphWarmer, KubernetesConfig, TokenReviewer, WarmCompletionSink};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_provider::embeddings::{EmbeddingService, default_embedding_cache_dir};
use djinn_provider::github_app::AppConfig as GitHubAppConfig;
use djinn_provider::github_app::CredentialSourceState;
use djinn_provider::repos::CredentialRepository;
use djinn_runtime::GraphWarmerService;
use djinn_supervisor::{AllowAllValidator, ConnectionRegistry, ServeHandle, serve_on_tcp};
use djinn_workspace::{MirrorManager, WorkspaceStore, mirrors_root, workspaces_root};

mod canonical_graph_refresh_planner;
mod provider_catalog_refresh;
mod settings;

use canonical_graph_refresh_planner::{
    CanonicalGraphRefreshPlanner, CanonicalGraphRefreshProbe, RefreshPlan, WarmPlan, WarmPlanInputs,
};

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const SETTINGS_RAW_KEY: &str = "settings.raw";
const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";

const BUILD_ADMISSION_MODE_ENV: &str = "DJINN_BUILD_ADMISSION_MODE";
const MAX_BUILD_TASKRUNS_ENV: &str = "DJINN_MAX_BUILD_TASKRUNS";

/// Immutable build-admission startup policy. It is parsed before composition,
/// so an environment change only takes effect after a process restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BuildAdmissionConfig {
    mode: BuildAdmissionMode,
    cap: i64,
}

impl BuildAdmissionConfig {
    const DEFAULT_CAP: i64 = 3;
    const MAX_CAP: i64 = 64;

    fn from_env() -> Result<Self, String> {
        Self::parse(
            std::env::var(BUILD_ADMISSION_MODE_ENV).ok().as_deref(),
            std::env::var(MAX_BUILD_TASKRUNS_ENV).ok().as_deref(),
        )
    }

    fn parse(mode: Option<&str>, cap: Option<&str>) -> Result<Self, String> {
        let explicit_mode = match mode {
            None => None,
            Some("off") => Some(BuildAdmissionMode::Off),
            Some("observe") => Some(BuildAdmissionMode::Observe),
            Some("enforce") => Some(BuildAdmissionMode::Enforce),
            Some("") => return Err(format!("{BUILD_ADMISSION_MODE_ENV} must not be empty")),
            Some(value) => {
                return Err(format!(
                    "{BUILD_ADMISSION_MODE_ENV} must be exactly off, observe, or enforce (got {value:?})"
                ));
            }
        };
        let parsed_cap = match cap {
            None => None,
            Some("") => return Err(format!("{MAX_BUILD_TASKRUNS_ENV} must not be empty")),
            Some(value) => Some(value.parse::<i64>().map_err(|_| {
                format!(
                    "{MAX_BUILD_TASKRUNS_ENV} must be an integer from 1 through 64 (got {value:?})"
                )
            })?),
        };
        if parsed_cap == Some(0) {
            return match explicit_mode {
                Some(BuildAdmissionMode::Observe | BuildAdmissionMode::Enforce) => Err(format!(
                    "{MAX_BUILD_TASKRUNS_ENV}=0 conflicts with explicit {BUILD_ADMISSION_MODE_ENV}"
                )),
                _ => Ok(Self {
                    mode: BuildAdmissionMode::Off,
                    cap: 0,
                }),
            };
        }
        let cap = parsed_cap.unwrap_or(Self::DEFAULT_CAP);
        if !(1..=Self::MAX_CAP).contains(&cap) {
            return Err(format!(
                "{MAX_BUILD_TASKRUNS_ENV} must be an integer from 1 through 64 (got {cap})"
            ));
        }
        Ok(Self {
            mode: explicit_mode.unwrap_or(BuildAdmissionMode::Observe),
            cap,
        })
    }
}

/// Production [`WarmCompletionSink`]: converge the server's in-memory
/// canonical-graph slot after an *out-of-pod* warm Job succeeds.
///
/// The K8s warm Job writes a fresh blob to `repo_graph_cache` from a separate
/// pod and cannot reach this process's `djinn_graph::canonical_graph::GRAPH_CACHE`
/// slot, so `code_graph` queries would keep serving the pre-warm graph until a
/// restart. Clearing the slot here makes the next query reload the fresh blob
/// within seconds of the warm completing. Wired at this composition root
/// because it is the only place both `djinn-k8s` (the warmer) and `djinn-graph`
/// (the cache) are in scope — `djinn-k8s` deliberately does not depend on
/// `djinn-graph`.
struct CanonicalGraphInvalidationSink;

#[async_trait::async_trait]
impl WarmCompletionSink for CanonicalGraphInvalidationSink {
    async fn on_warm_succeeded(&self, project_id: &str) {
        djinn_graph::canonical_graph::invalidate_canonical_graph_cache().await;
        tracing::info!(
            project_id,
            "graph_warmer: warm Job succeeded; invalidated in-memory canonical-graph slot for reload"
        );
    }
}

/// Default startup grace window (ms) used for the reconnectability probe.
/// During this window the measurement checks whether a running session's
/// worker RPC identity reconnects, allowing a connected worker to survive
/// the startup blanket-interruption path.
pub(crate) const STARTUP_GRACE_WINDOW_MS: u64 = 10_000;

/// Result of the startup reconnectability measurement emitted before
/// `interrupt_stale_sessions_on_startup` mutates any session status.
///
/// Extracted as a struct so tests can assert the measurement deterministically
/// without depending on log capture.  Proposal `phif` AC 7/8.
#[derive(Debug, Clone)]
pub struct StartupReconnectabilityMeasurement {
    /// Total number of sessions currently marked `running`.
    pub running_sessions: usize,
    /// Sessions whose `task_run_id` is connected in the `ConnectionRegistry`
    /// (or would reconnect during the startup grace probe).
    pub connected_or_reconnectable_sessions: usize,
    /// Duration of the startup grace probe in milliseconds.
    pub grace_window_ms: u64,
    /// Opaque identifier for this startup instance (UUID v7).
    pub startup_instance_id: String,
    /// Unique `task_run_id`s observed as connected or reconnectable during
    /// the startup probe.
    reconnectable_task_run_ids: HashSet<String>,
}

impl StartupReconnectabilityMeasurement {
    /// Return the exact measured reconnectable task-run identities.
    ///
    /// Kept as a narrow read-only accessor so the startup mutation can consume
    /// the same identity set in the follow-up selective-interruption slice
    /// without re-deriving it separately from the count emitted in tracing.
    pub(crate) fn reconnectable_task_run_ids(&self) -> &HashSet<String> {
        &self.reconnectable_task_run_ids
    }
}

/// Build a `QdrantConfig` from `QDRANT_URL` (and friends), falling back to
/// the library default (`http://127.0.0.1:6334`, no API key, collection
/// `notes`). Centralized so the per-call `note_vector_store()` and the
/// startup-time `initialize_vector_store()` agree on configuration.
fn qdrant_config_from_env() -> QdrantConfig {
    let mut config = QdrantConfig::default();
    if let Ok(url) = std::env::var("QDRANT_URL")
        && !url.is_empty()
    {
        config.url = url;
    }
    if let Ok(key) = std::env::var("QDRANT_API_KEY")
        && !key.is_empty()
    {
        config.api_key = Some(key);
    }
    config
}

/// `code_chunks`-collection variant of [`qdrant_config_from_env`]. Same
/// URL/API-key surface; the collection name is fixed to `code_chunks` to
/// keep it disjoint from the notes collection.
fn qdrant_code_chunk_config_from_env() -> QdrantCodeChunkConfig {
    let mut config = QdrantCodeChunkConfig::default();
    if let Ok(url) = std::env::var("QDRANT_URL")
        && !url.is_empty()
    {
        config.url = url;
    }
    if let Ok(key) = std::env::var("QDRANT_API_KEY")
        && !key.is_empty()
    {
        config.api_key = Some(key);
    }
    config
}

/// Report which `GITHUB_APP_*` env vars are unset or empty, so `init_app_config`
/// can surface a useful diagnosis when the credential source state is `Unconfigured`.
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

/// Keep provider-level GitHub App consumers (JWT minting, installation-token
/// exchange, background OAuth refresh) aligned with AppState's retained
/// credential-source snapshot.
fn sync_provider_runtime_config(state: &CredentialSourceState) {
    match state.app_config() {
        Some(config) => djinn_provider::github_app::install_runtime_config(config.clone()),
        None => djinn_provider::github_app::clear_runtime_config(),
    }
}

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

/// Server-side half of the browser-only setup-launch capability.
///
/// The matching random token is carried in an HttpOnly, SameSite=Strict
/// cookie. Keeping the expiry here (rather than trusting the browser's
/// `Max-Age`) makes a copied or manually replayed cookie expire as well.
struct SetupLaunchCapability {
    digest: Vec<u8>,
    expires_at: Instant,
}

const MAX_SETUP_LAUNCH_CAPABILITIES: usize = 8;
const SETUP_SESSION_TTL: Duration = Duration::from_secs(60 * 15);

struct SetupSession {
    digest: Vec<u8>,
    expires_at: Instant,
}

fn setup_authority_digest(raw: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    Sha256::digest(raw.as_bytes()).to_vec()
}

struct Inner {
    pub db: Database,
    pub db_runtime: DatabaseRuntimeManager,
    pub cancel: CancellationToken,
    pub events: broadcast::Sender<DjinnEventEnvelope>,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    /// models.dev catalog + custom providers (in-memory, refreshed on startup).
    pub catalog: CatalogService,
    /// Parsed once at startup so the catalog refresh loop and health endpoint
    /// share the exact same cadence contract.
    pub provider_catalog_refresh_interval: std::time::Duration,
    /// Ensures repeated startup hooks cannot create concurrent refresh loops.
    pub provider_catalog_refresh_started: AtomicBool,
    /// Per-model circuit-breaker health tracker.
    pub health_tracker: HealthTracker,
    /// Immutable retrieval-health config parsed once at startup.
    pub retrieval_config: djinn_core::doctor::RetrievalHealthConfig,
    /// Process-owned retrieval telemetry shared by every MCP session.
    pub retrieval_metrics: Arc<djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics>,
    pub role_registry: Arc<RoleRegistry>,
    /// Long-running coordinator actor handle.
    pub coordinator: Arc<tokio::sync::Mutex<Option<CoordinatorHandle>>>,
    /// Long-running slot pool actor handle.
    pub pool: Mutex<Option<SlotPoolHandle>>,
    /// Task IDs with in-flight post-session background work (merge/transition
    /// for non-worker roles, knowledge extraction). Used by the coordinator's
    /// stuck-task recovery to avoid releasing a task while its background work
    /// is still running.
    pub background_work_tasks: djinn_agent::actors::coordinator::BackgroundWorkTracker,
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
    /// Persistent per-project read-only workspace store for the chat
    /// subsystem (commit 7 of chat-user-global).  Owns a single
    /// `git clone --local --shared` per project under
    /// `{DJINN_HOME}/workspaces/{project_id}/`.  Acquired per tool
    /// call by the [`ProjectResolver`]; refreshed by the
    /// mirror-fetcher after each successful ref-advancing fetch.
    pub workspace_store: Arc<WorkspaceStore>,
    /// Single-flight gate for background canonical-graph warm tasks spawned
    /// by the in-process graph warmer (`build_in_process_graph_warmer`).
    /// Keyed by `project_id`: membership means a detached warm task is
    /// already running for that project and additional warm requests should
    /// be coalesced (return immediately without spawning a duplicate task).
    /// The entry is removed by the spawned task in its completion branch.
    pub canonical_warm_inflight: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Retained GitHub App credential-source state. This is the single source
    /// of truth for both the active configuration and operator-facing recovery
    /// state: `app_config()` derives its value from this enum instead of
    /// discarding the distinction between unconfigured, invalid Secret, and
    /// undecryptable persisted credentials.
    pub app_credential_state: tokio::sync::RwLock<CredentialSourceState>,
    /// One-time boot token for the self-setup flow. Generated at startup
    /// when `DJINN_ENABLE_SELF_SETUP=true` and no usable credentials exist.
    /// `None` when the gate is disabled, credentials are present, or the
    /// token was already consumed.
    pub boot_token: tokio::sync::RwLock<Option<crate::server::auth::boot_token::BootToken>>,
    /// Valid setup session from the most recent boot-token exchange or local
    /// browser launch. The server enforces the same 15-minute lifetime as the
    /// cookie so manually replaying an expired cookie cannot revive setup.
    /// `None` means no exchange occurred, the session expired, or it was
    /// cleared after credential persistence.
    setup_session: tokio::sync::RwLock<Option<SetupSession>>,
    /// Short-lived, single-use capability issued only to a same-origin
    /// browser that fetched `/auth/config`. It authorizes the explicit
    /// `POST /auth/github/setup-start` UI action without exposing the boot
    /// token in JSON or markup.
    setup_launch_capabilities: tokio::sync::RwLock<Vec<SetupLaunchCapability>>,
    /// Pending install-continuation nonce generated after manifest credential
    /// persistence. When present, `/auth/github/app-setup-callback` requires
    /// the caller to include a matching `continuation_state` query parameter.
    /// Consumed (set to `None`) on successful validation. This ties the
    /// post-manifest install redirect to the app-setup-callback endpoint so
    /// an unsolicited direct hit on `/auth/github/app-setup-callback` is
    /// rejected when a manifest flow just completed.
    pub(crate) pending_install_continuation: tokio::sync::RwLock<Option<String>>,
    /// Test-only flag: when `true`, `persist_and_reload_app_config` skips the
    /// real database persistence and just hot-swaps the in-memory config.
    /// This allows the full callback flow to be tested without a Postgres
    /// database.
    #[cfg(test)]
    test_bypass_persist: tokio::sync::RwLock<bool>,
    /// Test-only credential-resolution result injected after real persistence.
    #[cfg(test)]
    test_reload_state_override: tokio::sync::RwLock<Option<CredentialSourceState>>,
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
    /// Populated during [`AppState::initialize`] when a
    /// [`djinn_k8s::KubeClient`] can be constructed from the ambient
    /// environment (in-cluster SA token or local `$KUBECONFIG`). Remains
    /// `None` on dev boxes without a cluster — the mirror-fetcher reads
    /// this via [`AppState::image_controller`] and silently skips the
    /// enqueue step when absent.
    pub image_controller: tokio::sync::RwLock<Option<Arc<ImageController>>>,
    /// Phase 3 PR 5.5 — background task that watches build `Job`s to
    /// terminal state and flips `projects.image_status`.  Spawned
    /// alongside the controller when a [`djinn_k8s::KubeClient`] is
    /// available; `None` on dev boxes without a cluster.
    /// `shutdown_image_watcher` aborts + awaits the task on graceful
    /// shutdown.
    pub image_build_watcher: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Phase 3 PR 8 — production canonical-graph warmer.  Populated during
    /// [`AppState::initialize`]: prefers [`K8sGraphWarmer`] when running
    /// under `DJINN_RUNTIME=kubernetes` with a reachable
    /// [`djinn_k8s::KubeClient`]; falls back to
    /// [`build_in_process_graph_warmer`] otherwise so dev boxes and
    /// `TestRuntime` stay operational.  Read via
    /// [`AppState::graph_warmer`]; mirror-fetcher + agent dispatch paths
    /// dispatch through this handle rather than constructing a warmer
    /// per-call.
    pub graph_warmer: tokio::sync::RwLock<Option<Arc<dyn GraphWarmerService>>>,
    /// The admission controller for this server process when admission is enabled.
    pub build_admission: Option<Arc<BuildAdmissionController>>,
}

/// Result of a boot token exchange attempt.
pub enum BootTokenExchangeResult {
    /// Token was valid and consumed; the inner value is the session token.
    Ok(String),
    /// No boot token exists (setup not enabled or already consumed).
    NotAvailable,
    /// The provided token was invalid or already used.
    InvalidOrUsed,
}

impl AppState {
    pub fn new(db: Database, cancel: CancellationToken) -> Self {
        let runtime = DatabaseRuntimeManager::new(
            crate::db::runtime::DatabaseRuntimeConfig::postgres(db.bootstrap_info().target.clone()),
        );
        Self::new_inner(
            db,
            runtime,
            cancel,
            djinn_core::doctor::RetrievalHealthConfig::default(),
            Arc::new(djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics::new()),
            BuildAdmissionConfig {
                mode: BuildAdmissionMode::Observe,
                cap: BuildAdmissionConfig::DEFAULT_CAP,
            },
        )
    }

    pub fn new_with_runtime(
        db: Database,
        db_runtime: DatabaseRuntimeManager,
        cancel: CancellationToken,
        retrieval_config: djinn_core::doctor::RetrievalHealthConfig,
        retrieval_metrics: Arc<djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics>,
    ) -> Result<Self, String> {
        let admission_config = BuildAdmissionConfig::from_env()?;
        Ok(Self::new_inner(
            db,
            db_runtime,
            cancel,
            retrieval_config,
            retrieval_metrics,
            admission_config,
        ))
    }

    fn new_inner(
        db: Database,
        db_runtime: DatabaseRuntimeManager,
        cancel: CancellationToken,
        retrieval_config: djinn_core::doctor::RetrievalHealthConfig,
        retrieval_metrics: Arc<djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics>,
        admission_config: BuildAdmissionConfig,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mirror = Arc::new(MirrorManager::new(mirrors_root()));
        let workspace_store = Arc::new(WorkspaceStore::new(workspaces_root(), Arc::clone(&mirror)));
        // Each server process allocates a unique epoch so recovery can
        // distinguish rows created by a predecessor from rows created here.
        // The hostname is retained as a human-readable prefix; the UUIDv7
        // suffix guarantees uniqueness across restarts and replicas.
        let server_epoch = format!(
            "{}:{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "coordinator".to_owned()),
            djinn_agent::actors::coordinator::allocate_server_epoch()
        );
        let build_admission = match admission_config.mode {
            BuildAdmissionMode::Off => None,
            BuildAdmissionMode::Observe => Some(Arc::new(BuildAdmissionController::new(
                Arc::new(djinn_db::AdmissionJournalRepository::new(db.clone())),
                BuildAdmissionMode::Observe,
                admission_config.cap,
                server_epoch,
            ))),
            // Enforce starts fail-closed (JournalRecoveryIncomplete) until the
            // durable journal is recovered and seeded before any inventory or
            // task/warm create can run.
            BuildAdmissionMode::Enforce => Some(Arc::new(BuildAdmissionController::new_closed(
                Arc::new(djinn_db::AdmissionJournalRepository::new(db.clone())),
                admission_config.cap,
                server_epoch,
            ))),
        };
        Self {
            inner: Arc::new(Inner {
                db,
                db_runtime,
                cancel,
                events,
                git_actors: Arc::new(Mutex::new(HashMap::new())),
                catalog: CatalogService::new(),
                provider_catalog_refresh_interval:
                    provider_catalog_refresh::refresh_interval_from_env(),
                provider_catalog_refresh_started: AtomicBool::new(false),
                health_tracker: HealthTracker::new(),
                retrieval_config,
                retrieval_metrics,
                role_registry: Arc::new(RoleRegistry::new()),
                coordinator: Arc::new(tokio::sync::Mutex::new(None)),
                pool: Mutex::new(None),
                background_work_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
                file_time: Arc::new(FileTime::new()),
                lsp: LspManager::new(),
                active_tasks: djinn_agent::context::ActivityTracker::default(),
                embedding_service: EmbeddingService::new(default_embedding_cache_dir()),
                indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
                workspace_store,
                canonical_warm_inflight: Arc::new(std::sync::Mutex::new(HashSet::new())),
                app_credential_state: tokio::sync::RwLock::new(CredentialSourceState::Unconfigured),
                boot_token: tokio::sync::RwLock::new(None),
                setup_session: tokio::sync::RwLock::new(None),
                setup_launch_capabilities: tokio::sync::RwLock::new(Vec::new()),
                pending_install_continuation: tokio::sync::RwLock::new(None),
                #[cfg(test)]
                test_bypass_persist: tokio::sync::RwLock::new(false),
                #[cfg(test)]
                test_reload_state_override: tokio::sync::RwLock::new(None),
                mirror,
                rpc_server: tokio::sync::Mutex::new(None),
                rpc_registry: Arc::new(ConnectionRegistry::new()),
                image_controller: tokio::sync::RwLock::new(None),
                image_build_watcher: tokio::sync::Mutex::new(None),
                graph_warmer: tokio::sync::RwLock::new(None),
                build_admission,
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

    /// Construct the image controller once a [`djinn_k8s::KubeClient`] is
    /// available.
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

        let client = match djinn_k8s::try_default_client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "image_controller: no Kubernetes client available; controller disabled \
                     (dev/local mode — per-project builds skipped)"
                );
                return;
            }
        };

        // P5 boot reseed — seed `environment_config` for every project
        // whose column is still the migration-10 default. Runs before
        // the controller starts taking enqueue calls so the first
        // reconcile tick sees populated configs instead of the sentinel.
        let stats = djinn_image_controller::reseed_empty_configs(self.db()).await;
        tracing::info!(
            inspected = stats.inspected,
            reseeded = stats.reseeded,
            skipped_seeded = stats.skipped_already_seeded,
            skipped_no_stack = stats.skipped_stack_missing,
            errors = stats.errors,
            "environment_config: boot reseed complete"
        );

        let config = ImageControllerConfig::from_env();
        tracing::info!(
            buildkitd_host = %config.buildkitd_host,
            registry_host = %config.registry_host,
            builder_image = %config.builder_image,
            agent_worker_image = %config.agent_worker_image,
            namespace = %config.namespace,
            "image_controller: config loaded"
        );
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
        // without operator intervention. Uses the same Kubernetes client
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

    async fn run_zot_retention_preflight(
        &self,
        config: &ImageControllerConfig,
    ) -> anyhow::Result<()> {
        let retention = &config.zot_retention;
        let preflight_cfg = RetentionPreflightConfig {
            enabled: retention.enabled,
            destructive_enabled: retention.enabled && !retention.dry_run,
            newest_tags: retention.newest_tags,
        };
        // When disabled, run_preflight returns a bounded disabled outcome
        // without contacting Zot or the DB — no early return needed.
        if !retention.enabled {
            let outcome = run_preflight(&DisabledZotStateSource, &[], &preflight_cfg)
                .await
                .map_err(|error| anyhow::anyhow!("Zot retention preflight failed: {error}"))?;
            tracing::info!(
                mode = outcome.mode.label(),
                outcome = outcome.outcome_kind.label(),
                "Zot catalog retention preflight: disabled"
            );
            return Ok(());
        }
        let selected_rows = djinn_db::ImageRepository::new(self.db().clone())
            .list_selected_catalog_images()
            .await
            .map_err(|error: djinn_db::Error| {
                anyhow::anyhow!("load selected catalog images for Zot retention preflight: {error}")
            })?;
        let auth = match (&retention.username, &retention.password) {
            (Some(username), Some(password)) => ZotHttpAuth::Basic {
                username: username.clone(),
                password: password.clone(),
            },
            _ => ZotHttpAuth::None,
        };
        let source = ZotHttpStateSource::new(ZotHttpConfig::new(&retention.endpoint, auth))
            .map_err(|error| anyhow::anyhow!("construct Zot retention state source: {error}"))?;
        run_retention_preflight_orchestration(&source, &selected_rows, &preflight_cfg)
            .await
            .map(|_| ())
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

    /// Recover the durable build-admission journal and seed the controller
    /// before any Kubernetes inventory or task/warm create can run.
    ///
    /// Enforce admission remains fail-closed until recovery completes. Observe
    /// runs lifecycle/telemetry but never denies; Off has no admission coupling.
    /// This is idempotent: a second call re-seeds from the journal without harm.
    async fn initialize_build_admission_recovery(&self) {
        let Some(admission) = self.inner.build_admission.clone() else {
            // Off: no journal, no controller, no readiness coupling.
            return;
        };
        match admission.recover_all_predecessors_and_seed().await {
            Ok(report) => {
                tracing::info!(
                    mode = ?admission.mode(),
                    retired_reserved = report.retired_reserved,
                    marked_create_unknown = report.marked_create_unknown,
                    seeded_rows = report.seeded_rows,
                    readiness = ?report.readiness,
                    "build_admission: recovered durable journal and seeded controller"
                );
            }
            Err(error) => {
                // Enforce fails closed: the journal gate records the unhealthy
                // journal and every later gate stays pending, so admission
                // keeps denying. Observe continues without denial (it never
                // gated on readiness). A durable journal outage during
                // recovery is a fail-closed condition for Enforce.
                admission.mark_journal_unhealthy();
                tracing::error!(
                    %error,
                    mode = ?admission.mode(),
                    "build_admission: journal recovery failed; Enforce remains fail-closed"
                );
            }
        }
    }

    /// Run the broad Kubernetes build-capable inventory and advance the
    /// Enforce readiness gate.
    ///
    /// This MUST run after [`Self::initialize_build_admission_recovery`] (the
    /// durable journal loads before any Kubernetes inventory) and after
    /// [`Self::initialize_graph_warmer`] (the inventory rides the same client
    /// selection). The gate fails closed: any inventory LIST error leaves
    /// `InventoryPending` set so Enforce admission keeps denying until a real
    /// inventory succeeds. The in-process runtime has no Kubernetes objects
    /// to inventory, so its (empty) listing trivially completes the gate.
    /// Observe records the same degradation but never denies.
    async fn initialize_build_admission_inventory(&self) {
        let Some(admission) = self.inner.build_admission.clone() else {
            // Off: no controller, no readiness coupling.
            return;
        };
        let warmer = self.inner.graph_warmer.read().await.clone();
        let Some(warmer) = warmer else {
            admission.mark_inventory_pending();
            tracing::error!(
                mode = ?admission.mode(),
                "build_admission: no graph warmer available for inventory; Enforce remains fail-closed"
            );
            return;
        };
        match warmer.list_taskrun_jobs().await {
            Ok(jobs) => {
                admission.mark_inventory_ready();
                tracing::info!(
                    mode = ?admission.mode(),
                    task_run_jobs = jobs.len(),
                    readiness = ?admission.readiness(),
                    "build_admission: Kubernetes inventory complete"
                );
            }
            Err(error) => {
                admission.mark_inventory_pending();
                tracing::error!(
                    %error,
                    mode = ?admission.mode(),
                    "build_admission: Kubernetes inventory failed; Enforce remains fail-closed"
                );
            }
        }
    }

    /// Confirm the single-active topology gate and advance Enforce readiness.
    ///
    /// Called from [`Self::become_leader`], which runs only after this process
    /// wins the coordinator advisory lock — the lock IS the single-active
    /// topology gate for build admission. A standby pod never runs this, so
    /// its Enforce admission stays fail-closed with `TopologyPending`.
    fn confirm_build_admission_topology(&self) {
        if let Some(admission) = self.inner.build_admission.clone() {
            admission.mark_topology_ready();
            tracing::info!(
                mode = ?admission.mode(),
                readiness = ?admission.readiness(),
                "build_admission: single-active topology confirmed by coordinator leadership"
            );
        }
    }

    /// Begin graceful build-admission draining.
    ///
    /// New Enforce reservations are blocked while draining; in-flight permits
    /// may still transition to terminal. Observe and Off are unaffected because
    /// they never gated admission on readiness. Safety under forced process
    /// loss does not depend on this cooperative shutdown: the durable journal
    /// records pre-POST state, so a replacement recovers the same occupancy.
    pub async fn begin_build_admission_draining(&self) {
        if let Some(admission) = self.inner.build_admission.clone() {
            admission.begin_draining();
            tracing::info!(
                mode = ?admission.mode(),
                "build_admission: draining begun; new Enforce reservations blocked"
            );
        }
    }

    /// Pick the best available [`GraphWarmerService`] implementation and
    /// cache it on [`AppState`]. Idempotent.
    ///
    /// Policy:
    /// * If `DJINN_RUNTIME=kubernetes` (or unset — default) AND a
    ///   [`djinn_k8s::KubeClient`] can be constructed → [`K8sGraphWarmer`].
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
            match djinn_k8s::try_default_client().await {
                Ok(client) => {
                    let config = KubernetesConfig::from_env();
                    tracing::info!(
                        namespace = %config.namespace,
                        "graph_warmer: wiring K8sGraphWarmer"
                    );
                    let warmer = K8sGraphWarmer::new(client, config, self.db().clone())
                        .with_completion_sink(Arc::new(CanonicalGraphInvalidationSink));
                    let warmer = match self.inner.build_admission.clone() {
                        Some(admission) => warmer.with_warm_admission(admission),
                        None => warmer,
                    };
                    Arc::new(warmer) as Arc<dyn GraphWarmerService>
                }
                Err(e) => {
                    tracing::info!(
                        error = %e,
                        "graph_warmer: no Kubernetes client available; falling back to in-process warmer"
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
            crate::db::runtime::DatabaseRuntimeConfig::from_cli_and_env(None, None)
                .map_err(|e| anyhow::anyhow!("invalid database runtime configuration: {e}"))?,
        );
        db_runtime
            .ensure_runtime_available()
            .map_err(|e| anyhow::anyhow!("ensure database runtime: {e}"))?;
        let db = db_runtime
            .bootstrap()
            .map_err(|e| anyhow::anyhow!("open database runtime: {e}"))?;
        Self::new_with_runtime(
            db,
            db_runtime,
            cancel,
            djinn_core::doctor::RetrievalHealthConfig::default(),
            Arc::new(djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics::new()),
        )
        .map_err(anyhow::Error::msg)
    }

    /// Read-only snapshot of the active GitHub App configuration, if any.
    pub async fn app_config(&self) -> Option<Arc<GitHubAppConfig>> {
        self.inner
            .app_credential_state
            .read()
            .await
            .app_config()
            .cloned()
    }

    /// Read-only snapshot of the retained GitHub App credential-source state.
    pub async fn app_credential_state(&self) -> CredentialSourceState {
        self.inner.app_credential_state.read().await.clone()
    }

    /// Hot-swap the in-memory GitHub App configuration. Tests use this helper
    /// to seed a usable Secret-shaped state without reaching into the retained
    /// credential-state lock directly.
    pub async fn set_app_config(&self, cfg: Option<Arc<GitHubAppConfig>>) {
        let state = match cfg {
            Some(cfg) => CredentialSourceState::ValidSecret(cfg),
            None => CredentialSourceState::Unconfigured,
        };
        *self.inner.app_credential_state.write().await = state;
    }

    /// Inject a specific credential-source state for focused auth/recovery
    /// tests. Production code updates this only through initialization and
    /// manifest persistence/reload.
    #[cfg(test)]
    pub(crate) async fn set_app_credential_state_for_tests(&self, state: CredentialSourceState) {
        *self.inner.app_credential_state.write().await = state;
    }

    /// Inject a boot token for testing. Not used in production code.
    #[cfg(test)]
    pub(crate) async fn set_boot_token_for_tests(
        &self,
        token: Option<crate::server::auth::boot_token::BootToken>,
    ) {
        *self.inner.boot_token.write().await = token;
    }

    /// Inject a known setup session token for testing.
    #[cfg(test)]
    pub(crate) async fn set_setup_session_token_for_tests(&self, token: Option<String>) {
        self.set_setup_session_token_with_ttl_for_tests(token, SETUP_SESSION_TTL)
            .await;
    }

    /// Inject a setup session with a deterministic lifetime for expiry tests.
    #[cfg(test)]
    pub(crate) async fn set_setup_session_token_with_ttl_for_tests(
        &self,
        token: Option<String>,
        ttl: Duration,
    ) {
        *self.inner.setup_session.write().await = token.map(|token| SetupSession {
            digest: setup_authority_digest(&token),
            expires_at: SystemClockTrait::new().now_instant() + ttl,
        });
    }

    /// Issue a short-lived browser setup-launch capability.
    ///
    /// Only its SHA-256 digest is retained. A small bounded set prevents
    /// concurrent `/auth/config` responses or multiple local tabs from
    /// invalidating one another when their Set-Cookie headers arrive out of
    /// order; each individual capability is still single-use.
    pub(crate) async fn issue_setup_launch_capability(&self, ttl: Duration) -> String {
        let now = SystemClockTrait::new().now_instant();
        let token = crate::server::auth::random_token_b64();
        let digest = setup_authority_digest(&token);
        let mut guard = self.inner.setup_launch_capabilities.write().await;
        guard.retain(|capability| capability.expires_at > now);
        if guard.len() >= MAX_SETUP_LAUNCH_CAPABILITIES {
            guard.remove(0);
        }
        guard.push(SetupLaunchCapability {
            digest,
            expires_at: now + ttl,
        });
        token
    }

    /// Atomically validate and consume a setup-launch capability.
    ///
    /// Invalid guesses do not burn a legitimate browser's token. Expired
    /// capabilities are cleared server-side even if a client manually keeps
    /// sending the cookie after its `Max-Age` elapsed.
    pub(crate) async fn consume_setup_launch_capability(&self, candidate: &str) -> bool {
        let candidate_digest = setup_authority_digest(candidate);
        let now = SystemClockTrait::new().now_instant();
        let mut guard = self.inner.setup_launch_capabilities.write().await;
        guard.retain(|capability| capability.expires_at > now);
        let Some(index) = guard.iter().position(|capability| {
            crate::server::auth::constant_time_eq(&candidate_digest, &capability.digest)
        }) else {
            return false;
        };
        guard.remove(index);
        true
    }

    /// Establish (or recover) the setup session used across the GitHub
    /// manifest round-trip and retire the boot-token fallback once the UI
    /// path has actually started.
    ///
    /// Reusing an existing session keeps a config refetch or second local tab
    /// from invalidating the first tab's callback cookie.
    pub(crate) async fn begin_browser_setup_session(
        &self,
        existing_cookie: Option<&str>,
    ) -> String {
        // Match `exchange_boot_token`'s lock order (boot token, then setup
        // session) so the two one-time entry paths cannot deadlock or race.
        let mut boot_token = self.inner.boot_token.write().await;
        *boot_token = None;

        let now = SystemClockTrait::new().now_instant();
        let mut setup_session = self.inner.setup_session.write().await;
        if let Some(session) = setup_session.as_ref()
            && session.expires_at > now
            && let Some(candidate) = existing_cookie
            && crate::server::auth::constant_time_eq(
                &setup_authority_digest(candidate),
                &session.digest,
            )
        {
            return candidate.to_string();
        }
        let token = crate::server::auth::random_token_b64();
        *setup_session = Some(SetupSession {
            digest: setup_authority_digest(&token),
            expires_at: now + SETUP_SESSION_TTL,
        });
        token
    }

    /// Enable or disable the test bypass for persistence. When enabled,
    /// `persist_and_reload_app_config` skips the real DB write and just
    /// hot-swaps the in-memory config.
    #[cfg(test)]
    pub(crate) async fn set_test_bypass_persist(&self, bypass: bool) {
        *self.inner.test_bypass_persist.write().await = bypass;
    }

    /// Override post-persistence credential resolution for failure-path tests.
    #[cfg(test)]
    pub(crate) async fn set_test_reload_state_override(
        &self,
        state: Option<CredentialSourceState>,
    ) {
        *self.inner.test_reload_state_override.write().await = state;
    }

    /// Validate a candidate setup session token against the stored token.
    ///
    /// Returns `true` when a stored token exists and `candidate` matches it
    /// (constant-time comparison). Returns `false` when no token is stored
    /// or the values don't match.
    pub(crate) async fn validate_setup_session_token(&self, candidate: &str) -> bool {
        let mut stored = self.inner.setup_session.write().await;
        let Some(session) = stored.as_ref() else {
            return false;
        };
        if session.expires_at <= SystemClockTrait::new().now_instant() {
            *stored = None;
            return false;
        }
        crate::server::auth::constant_time_eq(&setup_authority_digest(candidate), &session.digest)
    }

    /// Invalidate the currently stored setup session token.
    ///
    /// Called after manifest credential persistence and hot-reload succeeds so
    /// the one-time setup session cannot be replayed in this process after the
    /// terminal setup step completes.
    pub(crate) async fn clear_setup_session_token(&self) {
        *self.inner.setup_session.write().await = None;
        self.inner.setup_launch_capabilities.write().await.clear();
    }

    /// Store a pending install-continuation nonce after manifest credential
    /// persistence. The nonce is embedded in the install URL and validated
    /// when GitHub redirects back to `/auth/github/app-setup-callback`.
    pub(crate) async fn set_pending_install_continuation(&self, nonce: String) {
        *self.inner.pending_install_continuation.write().await = Some(nonce);
    }

    /// Whether a manifest-install continuation is waiting to be completed.
    pub(crate) async fn has_pending_install_continuation(&self) -> bool {
        self.inner
            .pending_install_continuation
            .read()
            .await
            .is_some()
    }

    /// Validate a continuation candidate without consuming it. Callers use
    /// this before fallible GitHub API and database work so a transient error
    /// does not destroy the operator's retry path.
    pub(crate) async fn validate_pending_install_continuation(&self, candidate: &str) -> bool {
        self.inner
            .pending_install_continuation
            .read()
            .await
            .as_deref()
            .is_some_and(|expected| {
                crate::server::auth::constant_time_eq(candidate.as_bytes(), expected.as_bytes())
            })
    }

    /// Atomically validate and consume a continuation after binding succeeds
    /// or the flow safely transitions to the installation picker.
    pub(crate) async fn consume_pending_install_continuation(&self, candidate: &str) -> bool {
        let mut guard = self.inner.pending_install_continuation.write().await;
        let Some(expected) = guard.as_deref() else {
            return false;
        };
        if !crate::server::auth::constant_time_eq(candidate.as_bytes(), expected.as_bytes()) {
            return false;
        }
        *guard = None;
        true
    }

    /// Inject a pending install-continuation nonce for testing.
    #[cfg(test)]
    pub(crate) async fn set_pending_install_continuation_for_tests(&self, nonce: Option<String>) {
        *self.inner.pending_install_continuation.write().await = nonce;
    }

    /// Exchange a raw boot token for a setup session.
    ///
    /// Validates the token, atomically marks it consumed, stores the generated
    /// session token for later validation by `extract_setup_session`, and
    /// returns a result the auth handler can map to HTTP responses.
    pub async fn exchange_boot_token(&self, raw_token: &str) -> BootTokenExchangeResult {
        let mut guard = self.inner.boot_token.write().await;
        let Some(bt) = guard.as_mut() else {
            return BootTokenExchangeResult::NotAvailable;
        };

        if !bt.verify(raw_token) {
            return BootTokenExchangeResult::InvalidOrUsed;
        }

        if !bt.mark_used() {
            return BootTokenExchangeResult::InvalidOrUsed;
        }

        let session_token = crate::server::auth::random_token_b64();
        // Store so `extract_setup_session` can validate the cookie against it.
        *self.inner.setup_session.write().await = Some(SetupSession {
            digest: setup_authority_digest(&session_token),
            expires_at: SystemClockTrait::new().now_instant() + SETUP_SESSION_TTL,
        });
        BootTokenExchangeResult::Ok(session_token)
    }

    /// Initialise the in-memory App config using the deterministic credential
    /// source state machine.
    ///
    /// Checks Secret/env credentials first (highest priority); if absent,
    /// falls back to persisted credentials from the encrypted store. An
    /// invalid/incomplete Secret produces a fatal state and does NOT silently
    /// fall through to persisted credentials.
    ///
    /// Called during server bootstrap.
    pub async fn init_app_config(&self) {
        let credential_repo = CredentialRepository::new(self.db().clone(), self.event_bus());
        #[cfg(test)]
        let override_state = self.inner.test_reload_state_override.read().await.clone();
        #[cfg(test)]
        let state = match override_state {
            Some(state) => state,
            None => {
                djinn_provider::github_app::resolve_credential_source(Some(&credential_repo)).await
            }
        };
        #[cfg(not(test))]
        let state =
            djinn_provider::github_app::resolve_credential_source(Some(&credential_repo)).await;

        match &state {
            CredentialSourceState::ValidSecret(cfg) => {
                tracing::info!(
                    source = "secret",
                    app_id = cfg.app_id,
                    "github_app: loaded App configuration from env/Secret"
                );
            }
            CredentialSourceState::InvalidSecret(detail) => {
                tracing::error!(
                    issues = ?detail.issues,
                    "github_app: env/Secret credentials are present but invalid — \
                     FIX the listed env vars; NOT falling back to persisted credentials"
                );
            }
            CredentialSourceState::ValidPersisted(cfg) => {
                tracing::info!(
                    source = "persisted",
                    app_id = cfg.app_id,
                    "github_app: loaded App configuration from encrypted persistence store"
                );
            }
            CredentialSourceState::UndecryptablePersisted => {
                tracing::error!(
                    "github_app: persisted credentials exist but cannot be decrypted — \
                     re-provision via the setup flow or fix the encryption key"
                );
            }
            CredentialSourceState::Unconfigured => {
                let missing = missing_github_app_env_vars();
                tracing::warn!(
                    missing = missing.join(",").as_str(),
                    "github_app: App configuration not loaded from any source — \
                     mount the djinn-github-app Secret, set GITHUB_APP_* env vars, \
                     or complete the self-setup flow"
                );
            }
        }

        *self.inner.app_credential_state.write().await = state.clone();
        sync_provider_runtime_config(&state);

        // Re-initialization must not leave a token from an earlier
        // unconfigured snapshot alive after credentials become invalid or
        // configured.
        *self.inner.boot_token.write().await = None;
        self.inner.setup_launch_capabilities.write().await.clear();

        // Generate a one-time boot token when self-setup is enabled and no
        // credentials exist at all. Invalid Secret and undecryptable persisted
        // states require explicit operator recovery and must never silently
        // fall through to self-setup.
        if crate::server::auth::self_setup_enabled()
            && matches!(state, CredentialSourceState::Unconfigured)
        {
            let (raw, bt) = crate::server::auth::boot_token::BootToken::generate();
            let public_url = crate::server::auth::public_url();
            let setup_url = format!("{public_url}/auth/github/create-app?setup_token={raw}");
            tracing::info!(
                setup_url = %setup_url,
                "self-setup: generated one-time boot token — \
                 use this URL to begin GitHub App creation"
            );
            *self.inner.boot_token.write().await = Some(bt);
        }
    }

    /// Persist new GitHub App credentials (e.g., after a manifest exchange)
    /// and hot-reload the in-memory config without a process restart.
    ///
    /// Returns the new credential source state so callers can handle errors.
    pub async fn persist_and_reload_app_config(
        &self,
        config: &GitHubAppConfig,
    ) -> Result<CredentialSourceState, String> {
        // Test bypass: when set, skip the real DB persistence and just
        // hot-reload from the provided config.
        #[cfg(test)]
        if *self.inner.test_bypass_persist.read().await {
            let cfg = Arc::new(config.clone());
            let state = CredentialSourceState::ValidPersisted(cfg);
            *self.inner.app_credential_state.write().await = state.clone();
            sync_provider_runtime_config(&state);
            tracing::info!(
                app_id = config.app_id,
                "github_app: (test) simulated persistence and hot-reload"
            );
            return Ok(state);
        }

        let credential_repo = CredentialRepository::new(self.db().clone(), self.event_bus());
        djinn_provider::github_app::persist_app_config(&credential_repo, config).await?;

        // Hot-reload: re-resolve to pick up the persisted credentials.
        #[cfg(test)]
        let override_state = self.inner.test_reload_state_override.read().await.clone();
        #[cfg(test)]
        let state = match override_state {
            Some(state) => state,
            None => {
                djinn_provider::github_app::resolve_credential_source(Some(&credential_repo)).await
            }
        };
        #[cfg(not(test))]
        let state =
            djinn_provider::github_app::resolve_credential_source(Some(&credential_repo)).await;
        *self.inner.app_credential_state.write().await = state.clone();
        sync_provider_runtime_config(&state);

        if !state.is_usable() {
            let detail = match &state {
                CredentialSourceState::InvalidSecret(detail) => format!(
                    "invalid Secret/env credentials: {}",
                    detail.issues.join(", ")
                ),
                CredentialSourceState::UndecryptablePersisted => {
                    "persisted credentials are undecryptable".to_string()
                }
                CredentialSourceState::Unconfigured => {
                    "no usable credential source after persistence".to_string()
                }
                CredentialSourceState::ValidSecret(_)
                | CredentialSourceState::ValidPersisted(_) => unreachable!(),
            };
            return Err(format!(
                "GitHub App credentials were persisted but hot-reload failed: {detail}"
            ));
        }

        tracing::info!(
            source = ?state.source(),
            app_id = config.app_id,
            "github_app: persisted and hot-reloaded App configuration"
        );
        Ok(state)
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

    /// Shared persistent workspace store for chat.  The resolver
    /// (`ProjectResolver`) calls `ensure_workspace` per tool call; the
    /// mirror-fetcher calls `sync_workspace` after every ref-advancing
    /// mirror fetch so the tree stays current.
    pub fn workspace_store(&self) -> Arc<WorkspaceStore> {
        self.inner.workspace_store.clone()
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

    /// Configured provider catalog refresh cadence, parsed once from
    /// `DJINN_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS` at state construction.
    pub fn provider_catalog_refresh_interval(&self) -> std::time::Duration {
        self.inner.provider_catalog_refresh_interval
    }

    pub fn provider_catalog_refresh_interval_secs(&self) -> u64 {
        self.provider_catalog_refresh_interval().as_secs()
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        &self.inner.health_tracker
    }

    /// Immutable retrieval-health config parsed once at startup.
    pub fn retrieval_config(&self) -> djinn_core::doctor::RetrievalHealthConfig {
        self.inner.retrieval_config
    }

    /// Process-owned retrieval telemetry shared by every MCP session.
    pub fn retrieval_metrics(
        &self,
    ) -> Arc<djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics> {
        self.inner.retrieval_metrics.clone()
    }

    pub fn embedding_service(&self) -> &EmbeddingService {
        &self.inner.embedding_service
    }

    pub fn note_vector_store(&self) -> Arc<dyn NoteVectorStore> {
        match std::env::var("DJINN_VECTOR_BACKEND") {
            Ok(value) if value.eq_ignore_ascii_case("qdrant") => {
                Arc::new(QdrantNoteVectorStore::new(qdrant_config_from_env()))
                    as Arc<dyn NoteVectorStore>
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

    /// `code_chunks` analog of [`Self::note_vector_store`]. Gated on
    /// `DJINN_CODE_CHUNKS_BACKEND=qdrant`; defaults to the no-op store
    /// so the chunk-and-embed pipeline still records SQL rows but never
    /// attempts a Qdrant upsert until the operator opts in.
    pub fn code_chunk_vector_store(&self) -> Arc<dyn djinn_db::CodeChunkVectorStore> {
        let backend = std::env::var("DJINN_CODE_CHUNKS_BACKEND").unwrap_or_default();
        if backend.eq_ignore_ascii_case("qdrant") {
            Arc::new(QdrantCodeChunkVectorStore::new(
                qdrant_code_chunk_config_from_env(),
            )) as Arc<dyn djinn_db::CodeChunkVectorStore>
        } else {
            Arc::new(djinn_db::NoopCodeChunkVectorStore) as Arc<dyn djinn_db::CodeChunkVectorStore>
        }
    }

    /// Bootstrap-time call: ensure the configured Qdrant collection exists
    /// with the expected vector dimensions before the server starts taking
    /// embed-upsert traffic. No-op when `DJINN_VECTOR_BACKEND` isn't `qdrant`.
    pub async fn initialize_vector_store(&self) -> Result<(), String> {
        let backend = std::env::var("DJINN_VECTOR_BACKEND").unwrap_or_default();
        if !backend.eq_ignore_ascii_case("qdrant") {
            return Ok(());
        }
        let store = QdrantNoteVectorStore::new(qdrant_config_from_env());
        let dim = djinn_provider::embeddings::DEFAULT_EMBEDDING_DIMENSION as u64;
        store.ensure_collection(dim).await
    }

    /// `code_chunks` analog of [`Self::initialize_vector_store`]. Gated on
    /// `DJINN_CODE_CHUNKS_BACKEND=qdrant` (PR B1 of the code-graph + RAG
    /// overhaul) so the collection only materializes once the chunker
    /// pipeline (B2/B3) is rolled in. Reuses the same embedding dimension
    /// as notes since both sit on `nomic-embed-text-v1.5`.
    pub async fn initialize_code_vector_store(&self) -> Result<(), String> {
        let backend = std::env::var("DJINN_CODE_CHUNKS_BACKEND").unwrap_or_default();
        if !backend.eq_ignore_ascii_case("qdrant") {
            return Ok(());
        }
        let store = QdrantCodeChunkVectorStore::new(qdrant_code_chunk_config_from_env());
        let dim = djinn_provider::embeddings::DEFAULT_EMBEDDING_DIMENSION as u64;
        store.ensure_collection(dim).await
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
            background_work_tasks: self.inner.background_work_tasks.clone(),
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
            runtime_ops: Some(Arc::new(self.clone())),
            // Host-side runs root for the coordinator sweep + teardown backstop.
            // Resolves to `$DJINN_HOME/cache/cargo-target-runs` (the server pod's
            // mount of the shared cache PVC), not the Job-pod `/cache` path.
            cargo_target_runs_root: Some(djinn_core::paths::cargo_target_runs_root()),
            mirror: Some(self.inner.mirror.clone()),
            rpc_registry: Some(self.inner.rpc_registry.clone()),
            // Host-side AgentContext serves multiple projects (chat surface
            // + dispatcher); caller MUST disambiguate via the `project`
            // tool arg. Only the K8s worker (one-project-per-Pod) sets
            // this in build_worker_agent_context.
            default_project_id: None,
            reconciliation_sweep: djinn_agent::context::ReconciliationSweepConfig::from_env(),
            memory_intent_planner: djinn_agent::context::MemoryIntentPlannerConfig::from_env(),
            compaction_cs: djinn_slot::reply_loop::CompactionCriticalSection::default(),
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

    #[cfg(test)]
    pub(crate) async fn initialize_agent_handles_for_tests(&self) {
        if self.pool().await.is_some() {
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
        let coordinator = djinn_agent::actors::coordinator::spawn_coordinator(
            djinn_agent::actors::coordinator::CoordinatorDeps::new(
                self.events().clone(),
                self.cancel().clone(),
                self.db().clone(),
                pool.clone(),
                self.catalog().clone(),
                self.health_tracker().clone(),
                self.inner.role_registry.clone(),
                self.inner.background_work_tasks.clone(),
                self.inner.lsp.clone(),
            ),
        );

        self.set_agent_handles_for_tests(pool, coordinator).await;
    }

    #[cfg(test)]
    pub(crate) async fn set_agent_handles_for_tests(
        &self,
        pool: SlotPoolHandle,
        coordinator: CoordinatorHandle,
    ) {
        *self.inner.pool.lock().await = Some(pool);
        *self.inner.coordinator.lock().await = Some(coordinator);
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
        let deps = djinn_agent::actors::coordinator::CoordinatorDeps::new(
            self.events().clone(),
            self.cancel().clone(),
            self.db().clone(),
            pool.clone(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            self.inner.role_registry.clone(),
            self.inner.background_work_tasks.clone(),
            self.inner.lsp.clone(),
        )
        .with_graph_warmer(self.graph_warmer().await)
        .with_mirror(self.inner.mirror.clone())
        .with_runtime_ops(Arc::new(self.clone()))
        .with_rpc_registry(self.inner.rpc_registry.clone());
        let deps = match self.inner.build_admission.clone() {
            Some(admission) => deps.with_build_admission(admission),
            None => deps,
        };
        let coordinator = djinn_agent::actors::coordinator::spawn_coordinator(deps);

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

        // Reconcile custom providers from DB into the in-memory catalog in one
        // deterministic call.  This replaces any previously retained custom
        // providers so deleted DB rows do not survive across restarts.
        let repo = CustomProviderRepository::new(self.db().clone(), self.event_bus());
        match repo.list().await {
            Ok(db_providers) => {
                let entries: Vec<(Provider, Vec<Model>)> = db_providers
                    .into_iter()
                    .map(|cp| {
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
                        (provider, seed_models)
                    })
                    .collect();
                self.catalog().set_custom_providers(entries);
            }
            Err(e) => tracing::warn!(error = %e, "failed to load custom providers from DB"),
        }

        // Inject synthetic catalog entries for built-in providers (e.g.
        // chatgpt_codex, gcp_vertex_ai) that aren't in models.dev.
        use djinn_provider::catalog::builtin::BUILTIN_PROVIDERS;
        self.catalog().inject_builtin_providers(BUILTIN_PROVIDERS);

        // One task owns both retrying boot refresh and steady periodic refresh.
        // It starts only after custom-provider reconciliation and builtin
        // injection, and `refresh` preserves both overlays on every live swap.
        if self
            .inner
            .provider_catalog_refresh_started
            .swap(true, Ordering::AcqRel)
        {
            tracing::debug!("provider catalog refresh loop already started");
        } else {
            let catalog = self.catalog().clone();
            let interval = self.provider_catalog_refresh_interval();
            let cancel = self.cancel().clone();
            tokio::spawn(async move {
                provider_catalog_refresh::run_provider_catalog_refresh_loop(
                    catalog,
                    interval,
                    cancel,
                    djinn_provider::catalog::ProviderCatalogRefreshTicks::Interval,
                )
                .await;
            });
        }

        self.restore_model_health_state().await;

        // NOTE: stale-session finalization is a mutating sweep and now runs in
        // `become_leader()` — only the active (lock-holding) pod may touch
        // `running` sessions, otherwise a standby pod would interrupt the
        // leader's freshly-dispatched work.

        // KB note storage is db-only: there is no on-disk reindex on startup
        // and no .djinn/ filesystem watcher. Notes are written directly to
        // Dolt by the MCP write path; embeddings catch up asynchronously.

        // Bootstrap the Qdrant `notes` collection so the very first embed
        // upsert doesn't tombstone with `Collection 'notes' doesn't exist!`.
        // Idempotent: a second boot finds the collection and only verifies
        // its dimensions match. If they don't, surface a loud startup error
        // rather than silently writing to a wrongly-shaped collection.
        if let Err(error) = self.initialize_vector_store().await {
            tracing::error!(%error, "failed to bootstrap qdrant collection on startup");
        }

        // Same idempotent bootstrap for the `code_chunks` collection (PR B1
        // of the code-graph + RAG overhaul). Gated on the
        // `DJINN_CODE_CHUNKS_BACKEND` flag — empty default makes this a
        // no-op until B2/B3 ship the chunker + embed pipeline.
        if let Err(error) = self.initialize_code_vector_store().await {
            tracing::error!(%error, "failed to bootstrap qdrant code_chunks collection on startup");
        }

        // ADR-050 Chunk C: the filesystem-watcher SCIP trigger has been
        // removed.  SCIP indexing now happens lazily via
        // `ensure_canonical_graph` on architect dispatch and chat first
        // use.  Per-worktree skeleton refresh is no longer required.

        // NOTE: the mutating/dispatch subsystems that used to spawn here
        // (mirror backfill, workspace warming, the task-outcome extraction
        // listener, org-membership sync, the worker RPC listener, and the
        // image controller) now start in `become_leader()` so only the
        // single lock-holding pod runs them. See `crate::leadership`.

        // Build-admission journal recovery. The durable journal is loaded and
        // recovered BEFORE any Kubernetes inventory or task/warm create can run.
        // Enforce admission remains fail-closed until recovery seeds the
        // controller from all active recovered rows and confirms occupancy is
        // within the cap. Observe records degradation but never denies; Off has
        // no admission coupling. This must precede `initialize_graph_warmer`
        // because the warmer can create warm jobs under the shared cap.
        self.initialize_build_admission_recovery().await;

        // Phase 3 PR 8: pick the canonical-graph warmer impl (K8s or
        // in-process) and cache it. This is just a cached handle (the actual
        // warm is single-flight-gated elsewhere), so it is safe on every pod
        // and the serving/chat path needs it regardless of leadership.
        self.initialize_graph_warmer().await;

        // Build-admission Kubernetes inventory. This must follow BOTH the
        // journal recovery above (the durable journal loads before any
        // Kubernetes inventory) and the graph-warmer selection (the inventory
        // rides the same client). Enforce stays fail-closed with
        // `InventoryPending` until this LIST succeeds.
        self.initialize_build_admission_inventory().await;
    }

    /// Start the subsystems that must run on **exactly one** pod: the
    /// coordinator + slot pool, the worker RPC listener, the image controller,
    /// the periodic housekeeping/mirror-fetch loops, and the one-shot startup
    /// sweeps (stale-session finalization, cache pruning, mirror backfill,
    /// workspace warming). Called once by `crate::leadership::run_with_leadership`
    /// when this process wins the coordinator advisory lock.
    ///
    /// Everything here either dispatches cluster work, reaps/mutates shared
    /// rows, or writes to shared PVCs — running it on a standby pod during a
    /// rolling deploy would double-dispatch or corrupt shared state. The HTTP
    /// plane (which `initialize()` sets up) serves on every pod regardless.
    pub async fn become_leader(&self) {
        tracing::info!("become_leader: starting active coordinator subsystems");

        // Build-admission single-active topology gate. Winning the coordinator
        // advisory lock is the topology check: only this lock-holding pod may
        // open Enforce admission, so standby pods stay fail-closed with
        // `TopologyPending` (and their dispatch subsystems never start).
        self.confirm_build_admission_topology();

        let retention_config = ImageControllerConfig::from_env();
        if let Err(error) = self.run_zot_retention_preflight(&retention_config).await {
            tracing::error!(%error, "Zot retention preflight failed; refusing leader startup");
            std::process::exit(1);
        }

        // Finalize any sessions left in `running` from a previous leader. Safe
        // now (and only now): we hold the lock, so the previous leader is gone
        // and any `running` row is genuinely orphaned.
        //
        // This intentionally runs before spawning the coordinator: the
        // coordinator's startup task-run Job backstop immediately reconciles K8s
        // Jobs against these interrupted rows, so boot cleanup does not have to
        // wait for the long periodic stale-resource sweep. The backstop remains
        // idempotent if this ordering changes and observes a still-running row.
        self.interrupt_stale_sessions_on_startup().await;

        // Best-effort backfill of pricing snapshot columns for pre-existing
        // sessions that were created before snapshot capture was added.  Uses
        // the *current* catalog pricing — an approximation, not exact historical
        // rates.  Only sessions whose snapshot columns are all NULL are touched;
        // sessions that already have snapshots are preserved.  Idempotent.
        self.backfill_session_pricing_on_startup().await;

        // Coordinator + slot pool (the dispatch engine) + runtime settings.
        self.initialize_agents().await;

        // One-shot backfill of pre-existing blobless mirrors to full mirrors.
        // Idempotent + serialized per-project by the mirror lock.
        let backfill_self = self.clone();
        tokio::spawn(async move {
            backfill_self.backfill_full_mirrors_on_startup().await;
        });

        // Warm the persistent per-project workspaces so the first chat tool
        // call against a project doesn't pay the clone latency. Idempotent.
        let warm_workspaces_self = self.clone();
        tokio::spawn(async move {
            warm_workspaces_self.warm_workspaces_on_startup().await;
        });

        // Post-session knowledge-extraction listener (reacts to task outcomes).
        djinn_agent::task_confidence::spawn_task_outcome_listener(
            self.db().clone(),
            self.event_bus(),
            self.events(),
        );

        // Phase 3C: periodic GitHub-org-membership reconciliation. Flips
        // `users.is_member_of_org` and revokes sessions when someone leaves
        // the locked org.
        crate::server::start_org_member_sync(self.clone());

        // Phase 2 K8s PR 4 pt2: the TCP listener that worker Pods dial back
        // into. Only the leader dispatches workers, so only the leader needs
        // to accept their reverse-RPC. (No-op outside the Kubernetes runtime.)
        self.start_rpc_listener_if_needed().await;

        // Phase 3 PR 5: per-project devcontainer image controller. Dispatches
        // build Jobs — must be a singleton.
        self.initialize_image_controller().await;

        // Periodic DB housekeeping + mirror-fetch loops. Both mutate shared
        // state (reaping rows / writing the mirrors PVC) and trigger dispatch,
        // so they belong to the leader.
        djinn_db::background::housekeeping::spawn(
            self.db().clone(),
            self.event_bus(),
            self.cancel().clone(),
        );
        crate::mirror_fetcher::spawn(self.clone());

        // Periodic `git gc` over every project's mirror + working clone. Both
        // fetch with `--prune` but never reclaim the objects behind deleted
        // task branches, so the on-disk stores grow without bound — this is
        // the missing `git gc`. Leader-only, slow cadence; never competes with
        // dispatch.
        crate::git_maintenance::spawn(self.clone());

        // Periodic Codex OAuth keep-alive. ChatGPT/Codex refresh tokens rotate
        // single-use on a sliding window, so a connected-but-idle plan silently
        // dies once OpenAI expires its unused refresh token. This leader-only
        // loop refreshes idle Codex credentials to keep the rotation chain warm
        // and proactively flags genuinely-dead ones for reconnect.
        crate::codex_keepalive::spawn(self.clone());

        // One-time recovery sweep: backfill post-session knowledge extraction
        // over completed task-runs whose sessions were never extracted. Opt-in
        // via `DJINN_BACKFILL_EXTRACTION`; idempotent (skips already-extracted
        // sessions). Runs in the background so the leader keeps serving.
        if std::env::var("DJINN_BACKFILL_EXTRACTION")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            tracing::info!("DJINN_BACKFILL_EXTRACTION set — spawning one-time extraction backfill");
            let ctx = self.agent_context();
            tokio::spawn(async move {
                djinn_agent::run_extraction_backfill(ctx).await;
            });
        }

        tracing::info!("become_leader: active subsystems started");
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
            tracing::info!("rpc_server: DJINN_RUNTIME is not kubernetes; skipping TCP listener");
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

        // Validator: prefer the real TokenReview path via djinn-k8s owner-crate
        // wrapper; fall back to AllowAllValidator if no kubeconfig is available
        // (dev / CI).  This listener deliberately does not construct a raw
        // Kubernetes client — the client is owned and configured inside
        // djinn_k8s::token_review::TokenReviewer.
        //
        // Threads the process-wide `ConnectionRegistry` into the accept
        // loop so per-task-run `PendingConnection` slots reserved by
        // `KubernetesRuntime::prepare` pick up the worker's inbound
        // `FramePayload::Event` frames once the handshake lands.
        let registry = self.inner.rpc_registry.clone();
        let handle_result = match TokenReviewer::try_default("djinn").await {
            Ok(reviewer) => {
                let validator = Arc::new(K8sTokenReviewValidator::new(reviewer));
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
                    "rpc_server: TokenReviewer::try_default failed; \
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

    /// On startup, reconcile then interrupt stale sessions.
    ///
    /// If the reconnectability measurement is zero, we keep the legacy blanket
    /// `interrupt_all_running()` behavior. If one or more sessions are
    /// reconnectable, we interrupt running sessions *except* the measured
    /// reconnectable `task_run_id` set, so reconnectable workers survive
    /// rolling restart (proposal `phif` AC 8).
    ///
    /// The test build uses `pub(crate)` visibility so the same logic can be
    /// exercised from crate-internal tests without requiring the public API to
    /// expose the startup interruption seam.
    #[cfg(not(test))]
    async fn interrupt_stale_sessions_on_startup(&self) {
        self.interrupt_stale_sessions_on_startup_impl().await
    }

    #[cfg(test)]
    pub(crate) async fn interrupt_stale_sessions_on_startup(&self) {
        self.interrupt_stale_sessions_on_startup_impl().await
    }

    /// Shared implementation for [`Self::interrupt_stale_sessions_on_startup`].
    async fn interrupt_stale_sessions_on_startup_impl(&self) {
        use djinn_db::SessionRepository;
        let repo = SessionRepository::new(self.db().clone(), self.event_bus());

        // ── Measurement (proposal phif AC 7/8) ─────────────────────────────
        // Observe reconnectability *before* the selective/blanket interruption
        // mutation so the structured event reflects the pre-mutation state.
        let measurement = self.measure_startup_reconnectability(&repo).await;

        tracing::info!(
            target: "djinn_startup_running_session_reconnectability",
            running_sessions = measurement.running_sessions,
            connected_or_reconnectable_sessions = measurement.connected_or_reconnectable_sessions,
            grace_window_ms = measurement.grace_window_ms,
            startup_instance_id = %measurement.startup_instance_id,
            "startup reconnectability measurement"
        );

        // ── Mutation (proposal phif AC 8) ─────────────────────────────────────
        // Use the exact reconnectable identity set to preserve reconnectable
        // running sessions while still interrupting stale ones (NULL task_run_id
        // and disconnected identities are not preserved).
        let reconnectable_ids = measurement.reconnectable_task_run_ids();
        let result = if reconnectable_ids.is_empty() {
            repo.interrupt_all_running().await
        } else {
            repo.interrupt_running_except_task_run_ids(reconnectable_ids)
                .await
        };

        match result {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "interrupted stale sessions from previous run"),
            Err(e) => tracing::warn!(error = %e, "failed to interrupt stale sessions"),
        }
    }

    /// Measure running-session reconnectability at startup.
    ///
    /// Counts how many sessions are currently `running` and how many of those
    /// have a live RPC connection registered in [`ConnectionRegistry`].  The
    /// resulting [`StartupReconnectabilityMeasurement`] is emitted as a
    /// structured tracing event *before* any session status mutation, giving
    /// the proposal `phif` decision rule a deterministic pre-mutation signal.
    ///
    /// The grace probe (`STARTUP_GRACE_WINDOW_MS`) is only performed when
    /// at least one running session exists; zero-session startups return
    /// immediately.
    pub async fn measure_startup_reconnectability(
        &self,
        repo: &djinn_db::SessionRepository,
    ) -> StartupReconnectabilityMeasurement {
        let startup_instance_id = uuid::Uuid::now_v7().to_string();

        let running_sessions = match repo.list_active().await {
            Ok(sessions) => sessions,
            Err(e) => {
                tracing::warn!(error = %e, "failed to enumerate running sessions for measurement");
                return StartupReconnectabilityMeasurement {
                    running_sessions: 0,
                    connected_or_reconnectable_sessions: 0,
                    grace_window_ms: STARTUP_GRACE_WINDOW_MS,
                    startup_instance_id,
                    reconnectable_task_run_ids: HashSet::new(),
                };
            }
        };

        if running_sessions.is_empty() {
            return StartupReconnectabilityMeasurement {
                running_sessions: 0,
                connected_or_reconnectable_sessions: 0,
                grace_window_ms: STARTUP_GRACE_WINDOW_MS,
                startup_instance_id,
                reconnectable_task_run_ids: HashSet::new(),
            };
        }

        let registry = self.inner.rpc_registry.clone();
        let measured_task_run_ids: HashSet<String> = running_sessions
            .iter()
            .filter_map(|session| session.task_run_id.clone())
            .collect();

        // Immediate connectivity check: record unique worker RPC identities
        // that are already connected.
        let mut reconnectable_task_run_ids = HashSet::new();
        for task_run_id in &measured_task_run_ids {
            if registry.is_connected(task_run_id).await {
                reconnectable_task_run_ids.insert(task_run_id.clone());
            }
        }

        // Grace probe: wait up to STARTUP_GRACE_WINDOW_MS for workers that
        // might reconnect after a rolling deploy.  The probe is a coarse
        // poll — production would use a watch/notify path, but for the
        // measurement slice a single post-grace re-check suffices.
        if reconnectable_task_run_ids.len() < measured_task_run_ids.len() {
            tokio::time::sleep(std::time::Duration::from_millis(STARTUP_GRACE_WINDOW_MS)).await;
            for task_run_id in &measured_task_run_ids {
                if registry.is_connected(task_run_id).await {
                    reconnectable_task_run_ids.insert(task_run_id.clone());
                }
            }
        }

        let connected_or_reconnectable_sessions = reconnectable_task_run_ids.len();
        StartupReconnectabilityMeasurement {
            running_sessions: running_sessions.len(),
            connected_or_reconnectable_sessions,
            grace_window_ms: STARTUP_GRACE_WINDOW_MS,
            startup_instance_id,
            reconnectable_task_run_ids,
        }
    }

    /// Best-effort backfill of pricing snapshot columns for sessions that were
    /// created before snapshot capture existed.
    ///
    /// Uses the *current* catalog pricing — this is an approximation that does
    /// not reflect what the model actually cost when the session ran.  Only
    /// sessions whose four snapshot columns are all `NULL` are touched; sessions
    /// that already captured a start-time snapshot are left alone.
    ///
    /// Sessions whose `model_id` is not found in the current catalog, or whose
    /// catalog entry is unpriced/default-priced, remain all-NULL (never treated
    /// as free).  Idempotent — safe to rerun.
    async fn backfill_session_pricing_on_startup(&self) {
        use djinn_db::SessionRepository;

        let pricing_map = self.catalog().pricing_for_all_models();
        if pricing_map.is_empty() {
            tracing::debug!("pricing backfill: no models in catalog; skipping");
            return;
        }

        let pricing_vec: Vec<(String, djinn_core::models::Pricing)> =
            pricing_map.into_iter().collect();
        let model_count = pricing_vec.len();

        let repo = SessionRepository::new(self.db().clone(), self.event_bus());
        match repo.backfill_pricing_snapshots(&pricing_vec).await {
            Ok(0) => {
                tracing::debug!(model_count, "pricing backfill: no sessions needed updating");
            }
            Ok(n) => {
                tracing::info!(
                    rows_updated = n,
                    model_count,
                    "pricing backfill: approximate historical pricing applied from current catalog"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "pricing backfill failed — sessions with NULL snapshots remain"
                );
            }
        }
    }

    /// Iterate every project in the DB and upgrade its bare mirror
    /// from blobless (pre-cut-over) to full on boot.
    ///
    /// Serialized per-project via the existing `MirrorManager` lock,
    /// so a concurrent fetch from the normal fetch-watcher queues
    /// behind the backfill for that project but other projects keep
    /// progressing. Missing mirrors (project row exists but no clone
    /// on disk yet) are skipped with a debug log — `ensure_mirror`
    /// will create them with the new full-clone semantics on first
    /// use. Projects whose mirrors are already full short-circuit
    /// in `ensure_full_mirror` with no git process spawned.
    async fn backfill_full_mirrors_on_startup(&self) {
        let project_repo = ProjectRepository::new(self.db().clone(), self.event_bus());
        let projects = match project_repo.list().await {
            Ok(projects) => projects,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list projects for mirror backfill");
                return;
            }
        };

        tracing::info!(
            project_count = projects.len(),
            "starting mirror full-history backfill"
        );

        let mirror = self.mirror();
        let mut upgraded = 0usize;
        let mut skipped_missing = 0usize;
        let mut failed = 0usize;
        for project in &projects {
            match mirror.ensure_full_mirror(&project.id).await {
                Ok(()) => upgraded += 1,
                Err(djinn_workspace::MirrorError::Missing(_)) => {
                    skipped_missing += 1;
                    tracing::debug!(
                        project = %project.slug(),
                        "mirror backfill skipped: no mirror on disk yet"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        project = %project.slug(),
                        error = %e,
                        "mirror backfill failed"
                    );
                }
            }
        }

        tracing::info!(
            upgraded,
            skipped_missing,
            failed,
            total = projects.len(),
            "mirror full-history backfill complete"
        );
    }

    /// Warm every project's persistent chat workspace on boot.
    ///
    /// Iterates [`ProjectRepository::list`] and calls
    /// [`WorkspaceStore::ensure_workspace`] for each row. Missing
    /// mirrors (projects that haven't been fetched yet) are skipped
    /// with a debug log; genuine errors log a warn! and keep going.
    /// The store's per-project lock serialises `ensure_workspace`
    /// with any concurrent sync-from-mirror-fetch, so this is safe
    /// to run alongside normal server traffic.
    async fn warm_workspaces_on_startup(&self) {
        let project_repo = ProjectRepository::new(self.db().clone(), self.event_bus());
        let projects = match project_repo.list().await {
            Ok(projects) => projects,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list projects for workspace warm");
                return;
            }
        };

        tracing::info!(
            project_count = projects.len(),
            "starting chat workspace warm"
        );

        let store = self.workspace_store();
        let mut warmed = 0usize;
        let mut skipped_missing = 0usize;
        let mut failed = 0usize;
        for project in &projects {
            let branch = match project_repo.get_default_branch(&project.id).await {
                Ok(Some(b)) => b,
                Ok(None) if !project.target_branch.trim().is_empty() => {
                    project.target_branch.clone()
                }
                Ok(None) => "HEAD".to_owned(),
                Err(e) => {
                    tracing::warn!(
                        project = %project.slug(),
                        error = %e,
                        "workspace warm: default-branch lookup failed"
                    );
                    failed += 1;
                    continue;
                }
            };
            match store.ensure_workspace(&project.id, &branch).await {
                Ok(_) => warmed += 1,
                Err(djinn_workspace::WorkspaceError::MirrorMissing(_)) => {
                    skipped_missing += 1;
                    tracing::debug!(
                        project = %project.slug(),
                        "workspace warm skipped: mirror not yet on disk"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        project = %project.slug(),
                        error = %e,
                        "workspace warm failed"
                    );
                }
            }
        }

        tracing::info!(
            warmed,
            skipped_missing,
            failed,
            total = projects.len(),
            "chat workspace warm complete"
        );
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

    fn code_chunk_embeddings(&self) -> Option<Arc<dyn djinn_db::CodeChunkEmbeddingProvider>> {
        // The shared `EmbeddingService` impls both `NoteEmbeddingProvider`
        // and `CodeChunkEmbeddingProvider` (PR B3) — same model, same
        // version stamp. Cloning is cheap (Arc-internal).
        Some(Arc::new(self.inner.embedding_service.clone())
            as Arc<dyn djinn_db::CodeChunkEmbeddingProvider>)
    }

    fn code_chunk_vector_store(&self) -> Option<Arc<dyn djinn_db::CodeChunkVectorStore>> {
        Some(AppState::code_chunk_vector_store(self))
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
        djinn_graph::canonical_graph::canonical_graph_count_commits_since(
            project_root,
            pinned_commit,
        )
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
///
/// Orchestrate the Zot retention preflight over injected state sources.
///
/// This is the real orchestration seam: it adapts DB-selected image rows into
/// the pure planner shape, runs [`run_preflight`] against a [`ZotStateSource`],
/// logs the deterministic report, and returns the outcome. The production
/// caller ([`AppState::run_zot_retention_preflight`]) wires the live
/// `ZotHttpStateSource` and DB rows; tests inject mock implementations to
/// exercise mode/error semantics without a live registry or Kubernetes.
///
/// Returns `Err` for Zot fetch failures, Zot state errors, and unsafe
/// destructive selected images — fail-closed at the startup/enablement
/// boundary.
pub(crate) async fn run_retention_preflight_orchestration(
    zot_source: &dyn ZotStateSource,
    selected_rows: &[djinn_db::SelectedCatalogImage],
    cfg: &RetentionPreflightConfig,
) -> anyhow::Result<PreflightOutcome> {
    let outcome = run_preflight(zot_source, &adapt_selected_images(selected_rows), cfg)
        .await
        .map_err(|error| {
            // Surface the deterministic report from the blocked-destructive
            // error so operators can inspect it in the startup log.
            if let PreflightError::UnsafeSelectedImages { ref report, .. } = error {
                tracing::error!(
                    mode = cfg.mode().label(),
                    outcome = PreflightOutcomeKind::DestructiveBlocked.label(),
                    report = %report,
                    "Zot catalog retention preflight: destructive blocked"
                );
            }
            anyhow::anyhow!("Zot retention preflight failed: {error}")
        })?;
    tracing::info!(
        mode = outcome.mode.label(),
        outcome = outcome.outcome_kind.label(),
        blocks_rollout = outcome.blocks_rollout,
        newest_tags = cfg.newest_tags,
        report = %outcome.report,
        "Zot catalog retention preflight report"
    );
    Ok(outcome)
}

/// A no-op [`ZotStateSource`] used when retention is disabled — `run_preflight`
/// never calls `fetch_repositories` in disabled mode, but the trait is still
/// required by the function signature.
struct DisabledZotStateSource;

#[async_trait::async_trait]
impl ZotStateSource for DisabledZotStateSource {
    async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError> {
        Err(ZotStateError::Fetch(
            "disabled preflight should not contact Zot".into(),
        ))
    }
}

fn build_in_process_graph_warmer(state: AppState) -> djinn_agent::warmer::InProcessGraphWarmer {
    use djinn_agent::warmer::{InProcessGraphWarmer, InProcessWarmerDeps};
    use djinn_db::ProjectRepository;

    let warm_state = state.clone();
    let warm: djinn_agent::warmer::WarmCallback = Arc::new(move |project_id, project_root| {
        let state = warm_state.clone();
        Box::pin(async move {
            let index_tree_path = djinn_core::index_tree::index_tree_path(&project_root);
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
                let started = SystemClockTrait::new().now_instant();
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
    let project_root: djinn_agent::warmer::ProjectRootResolver = Arc::new(move |project_id| {
        let state = project_root_state.clone();
        Box::pin(async move {
            let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
            match repo.get(&project_id).await {
                Ok(Some(project)) => Some(djinn_core::paths::project_dir(
                    &project.github_owner,
                    &project.github_repo,
                )),
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
                    RefreshPlan::SkipCurrent { .. } | RefreshPlan::SkipCommitCheckFailed { .. } => {
                        true
                    }
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
mod retention_preflight_tests {
    //! Injected scenario tests for the Zot retention startup preflight
    //! orchestration seam ([`super::run_retention_preflight_orchestration`]).
    //!
    //! These tests exercise the *real* production orchestration path (DB-row
    //! adaptation → pure planner → preflight mode/error semantics → report)
    //! without a live Zot registry or Kubernetes cluster. State is injected via
    //! mock [`ZotStateSource`] implementations and canned
    //! [`djinn_db::SelectedCatalogImage`] rows.

    use super::*;
    use djinn_image_controller::{PreflightMode, ZotRepository, ZotStateError, ZotTag};

    /// Join an error's full cause chain into a single lowercase string for
    /// substring assertions.
    fn error_chain(err: &anyhow::Error) -> String {
        err.chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" → ")
            .to_lowercase()
    }

    // ── Mock Zot state sources ────────────────────────────────────────────

    /// A mock Zot state source that returns canned repository state.
    struct MockZot {
        repos: Vec<ZotRepository>,
    }

    #[async_trait::async_trait]
    impl ZotStateSource for MockZot {
        async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError> {
            Ok(self.repos.clone())
        }
    }

    /// A mock Zot state source that always fails — proves fail-closed on Zot
    /// fetch/state errors.
    struct FailingZot;

    #[async_trait::async_trait]
    impl ZotStateSource for FailingZot {
        async fn fetch_repositories(&self) -> Result<Vec<ZotRepository>, ZotStateError> {
            Err(ZotStateError::Fetch("injected: connection refused".into()))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn ztag(name: &str, digest: &str, size: u64, pushed: &str) -> ZotTag {
        ZotTag {
            tag: name.into(),
            digest: digest.into(),
            size_bytes: size,
            pushed_at: pushed.into(),
        }
    }

    fn zrepo(name: &str, tags: Vec<ZotTag>) -> ZotRepository {
        ZotRepository {
            name: name.into(),
            tags,
        }
    }

    /// Build a DB selected-catalog-image row in the same shape the
    /// production `ImageRepository::list_selected_catalog_images` returns.
    /// `tag` is a full registry ref like `reg/djinn-image-i1:hash`, matching
    /// how `adapt_selected_images` extracts the bare tag suffix.
    fn db_selected(
        image_id: &str,
        name: &str,
        tag: Option<&str>,
        digest: Option<&str>,
        status: &str,
    ) -> djinn_db::SelectedCatalogImage {
        djinn_db::SelectedCatalogImage {
            image_id: image_id.into(),
            name: name.into(),
            tag: tag.map(String::from),
            registry_digest: digest.map(String::from),
            status: status.into(),
            last_error: None,
            selected_project_ids: vec!["p1".into()],
        }
    }

    /// A two-tag repo fixture: a "keep" tag (newer, larger) and a "drop" tag
    /// (older, smaller). With newest_tags=1, "keep" is retained and "drop" is
    /// deleted.
    fn two_tag_repo(image_id: &str) -> ZotRepository {
        zrepo(
            &format!("djinn-image-{image_id}"),
            vec![
                ztag("keep", "sha256:keep", 500, "2024-01-02T00:00:00Z"),
                ztag("drop", "sha256:drop", 300, "2024-01-01T00:00:00Z"),
            ],
        )
    }

    const fn dry_run_cfg(newest: usize) -> RetentionPreflightConfig {
        RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: false,
            newest_tags: newest,
        }
    }

    const fn destructive_cfg(newest: usize) -> RetentionPreflightConfig {
        RetentionPreflightConfig {
            enabled: true,
            destructive_enabled: true,
            newest_tags: newest,
        }
    }

    const fn disabled_cfg() -> RetentionPreflightConfig {
        RetentionPreflightConfig {
            enabled: false,
            destructive_enabled: false,
            newest_tags: 5,
        }
    }

    // ── Advisory dry-run report ────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_report_contains_all_sections_and_deterministic_byte_counts() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:keep"),
            Some("sha256:keep"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(1))
            .await
            .expect("dry-run must succeed");

        // Mode: never blocks.
        assert!(!outcome.blocks_rollout, "dry-run must never block rollout");
        assert!(outcome.plan.is_safe, "selected tag 'keep' is retained");

        // Deterministic report content.
        assert!(outcome.report.contains("Retained tags"));
        assert!(outcome.report.contains("Deleted tags"));
        assert!(outcome.report.contains("Selected-image pins"));
        // "drop" (300B) is reclaimed; "keep" (500B) is retained.
        assert!(
            outcome.report.contains("Projected reclaimed bytes: 300"),
            "report must show projected reclaimed bytes: {report}",
            report = outcome.report
        );
        assert!(
            outcome.report.contains("Projected retained bytes: 500"),
            "report must show projected retained bytes: {report}",
            report = outcome.report
        );
        assert!(outcome.report.contains("Safe: yes"));
    }

    #[tokio::test]
    async fn dry_run_emits_advisory_report_even_when_unsafe() {
        // Selected image uses the "drop" tag which will be deleted.
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:drop"),
            Some("sha256:drop"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(1))
            .await
            .expect("dry-run must not fail even if images are unsafe");

        assert!(!outcome.blocks_rollout, "dry-run must never block");
        assert!(
            !outcome.plan.is_safe,
            "plan should flag the unsafe image in the report"
        );
        assert!(outcome.report.contains("UNSAFE"), "report must flag unsafe");
        assert!(outcome.report.contains("Safe: NO"));
    }

    // ── Safe destructive progression ───────────────────────────────────────

    #[tokio::test]
    async fn safe_destructive_progresses_when_all_images_pullable() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:keep"),
            Some("sha256:keep"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .expect("destructive must succeed when all images are safe");

        assert!(!outcome.blocks_rollout);
        assert!(outcome.plan.is_safe);
        assert_eq!(outcome.plan.unsafe_images.len(), 0);
    }

    #[tokio::test]
    async fn destructive_allows_digest_alias_when_tag_deleted() {
        // The selected "v1" tag will be deleted (newest=1 keeps "v2"), but
        // both tags share the same digest — an alias — so the image remains
        // pullable by the retained "v2" tag.
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-i1",
                vec![
                    ztag("v1", "sha256:shared", 100, "2024-01-01T00:00:00Z"),
                    ztag("v2", "sha256:shared", 200, "2024-01-02T00:00:00Z"),
                ],
            )],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:v1"),
            Some("sha256:shared"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .expect("alias via shared digest must be safe");
        assert!(outcome.plan.is_safe);
        assert!(!outcome.blocks_rollout);
    }

    // ── Unsafe selected-image blocking ─────────────────────────────────────

    #[tokio::test]
    async fn destructive_blocks_when_selected_image_tag_is_deleted() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:drop"),
            Some("sha256:drop"),
            "ready",
        )];
        let err = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .expect_err("destructive must fail-closed when an image is unsafe");

        let chain = error_chain(&err);
        assert!(
            chain.contains("blocked") || chain.contains("destructive"),
            "error should mention destructive block: {chain}"
        );
    }

    #[tokio::test]
    async fn destructive_blocks_when_any_of_multiple_images_is_unsafe() {
        let zot = MockZot {
            repos: vec![two_tag_repo("safe"), two_tag_repo("risky")],
        };
        let selected = vec![
            db_selected(
                "safe",
                "Rust",
                Some("reg/djinn-image-safe:keep"),
                Some("sha256:keep"),
                "ready",
            ),
            // "risky" image uses the "drop" tag → unsafe.
            db_selected(
                "risky",
                "Node",
                Some("reg/djinn-image-risky:drop"),
                Some("sha256:drop"),
                "ready",
            ),
        ];
        let err = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .expect_err("one unsafe image out of two must still block");
        let chain = error_chain(&err);
        assert!(
            chain.contains("1 selected image"),
            "error should report 1 blocked image: {chain}"
        );
    }

    // ── Selected-image DB-error fail-closed semantic ───────────────────────
    //
    // The DB-error fail-closed path lives at the AppState layer: if
    // list_selected_catalog_images fails, the error propagates to the startup
    // boundary. At this seam (which receives already-loaded rows), an empty
    // selected list means no images to protect, so retention is safe.
    // The fail-closed semantic that matters at this seam is: even with zero
    // selected images, a Zot fetch error must still fail (see zot_error tests).

    #[tokio::test]
    async fn empty_selected_list_is_trivially_safe_in_destructive() {
        let zot = MockZot {
            repos: vec![two_tag_repo("orphan")],
        };
        let outcome = run_retention_preflight_orchestration(&zot, &[], &destructive_cfg(1))
            .await
            .expect("no selected images = nothing to protect = safe");
        assert!(outcome.plan.is_safe);
        assert!(outcome.plan.unsafe_images.is_empty());
        assert!(!outcome.blocks_rollout);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_state_blocks_when_selected_image_db_query_fails() {
        // The DB-error fail-closed path lives at the AppState layer: if
        // list_selected_catalog_images fails, the error propagates to the
        // startup/enablement boundary (become_leader exits). Here we construct
        // a real AppState with a test DB, drop the `images` table so the
        // query fails, and assert run_zot_retention_preflight returns Err —
        // proving fail-closed on DB failure without a live Zot or Kubernetes.
        //
        // Guard: if no test Postgres is reachable (e.g. DJINN_TEST_DATABASE_URL
        // is empty and the default fallback port isn't listening), skip the
        // test rather than fail-spuriously. The fail-closed semantic is
        // structurally a `?` propagation; the other tests cover the Zot and
        // unsafe-image error paths at the seam level.
        let db = crate::test_helpers::create_test_db();
        match db.ensure_initialized().await {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, "skipping DB-error test — test database unavailable");
                return;
            }
        }
        // Drop the images table so list_selected_catalog_images SQL fails.
        // Use the djinn-db test helper so this test file stays outside the raw
        // SQL boundary (djinn-db is the allowed home for sqlx::query).
        djinn_db::test_support::drop_table_cascade_for_test(&db, "images").await;

        let state = AppState::new(db, tokio_util::sync::CancellationToken::new());
        // Enabled retention config — forces the DB query to execute.
        let mut config = ImageControllerConfig::for_testing();
        config.zot_retention.enabled = true;

        let err = state
            .run_zot_retention_preflight(&config)
            .await
            .expect_err("DB query failure must fail-closed at the startup boundary");
        let chain = error_chain(&err);
        assert!(
            chain.contains("load selected catalog images"),
            "error should mention the DB load failure: {chain}"
        );
    }

    // ── Zot error blocking ─────────────────────────────────────────────────

    #[tokio::test]
    async fn zot_state_error_fails_destructive_preflight() {
        let zot = FailingZot;
        let selected: Vec<djinn_db::SelectedCatalogImage> = vec![];
        let err = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .expect_err("Zot fetch failure must fail-closed in destructive mode");
        let chain = error_chain(&err);
        assert!(
            chain.contains("zot"),
            "error should mention Zot failure: {chain}"
        );
    }

    #[tokio::test]
    async fn zot_state_error_fails_even_in_dry_run() {
        let zot = FailingZot;
        let selected: Vec<djinn_db::SelectedCatalogImage> = vec![];
        let err = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(1))
            .await
            .expect_err("Zot fetch failure must surface even in dry-run");
        let chain = error_chain(&err);
        assert!(
            chain.contains("zot"),
            "fetch error must propagate in dry-run: {chain}"
        );
    }

    // ── Deterministic end-to-end report assertions ─────────────────────────
    //
    // One comprehensive fixture that asserts retained/deleted tags, selected
    // pins/aliases, projected reclaimed bytes, and projected post-GC retained
    // bytes in a single end-to-end orchestration run.

    #[tokio::test]
    async fn end_to_end_report_has_deterministic_tags_pins_and_bytes() {
        let zot = MockZot {
            repos: vec![
                zrepo(
                    "djinn-image-a1",
                    vec![
                        ztag("v3", "sha256:a-v3", 900, "2024-03-01T00:00:00Z"),
                        ztag("v2", "sha256:a-v2", 800, "2024-02-01T00:00:00Z"),
                        ztag("v1", "sha256:a-v1", 700, "2024-01-01T00:00:00Z"),
                    ],
                ),
                zrepo(
                    "djinn-image-b2",
                    vec![
                        ztag("latest", "sha256:b-lat", 400, "2024-03-01T00:00:00Z"),
                        ztag("old", "sha256:b-old", 200, "2024-01-01T00:00:00Z"),
                    ],
                ),
            ],
        };
        // Two selected images, both pointing at retained tags.
        let selected = vec![
            db_selected(
                "a1",
                "Rust",
                Some("reg/djinn-image-a1:v2"),
                Some("sha256:a-v2"),
                "ready",
            ),
            db_selected(
                "b2",
                "Node",
                Some("reg/djinn-image-b2:latest"),
                Some("sha256:b-lat"),
                "ready",
            ),
        ];
        // newest=2: retains top-2 by pushed_at per repo.
        //   a1: retains v3(900) + v2(800), deletes v1(700) → reclaim 700.
        //   b2: retains latest(400) + old(200) → no deletion.
        // Both selected tags are retained → safe.
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(2))
            .await
            .expect("dry-run with all-safe images must succeed");

        // Retained/deleted tag assertions.
        assert_eq!(outcome.plan.repos.len(), 2);
        let a1_plan = &outcome.plan.repos[0];
        assert_eq!(a1_plan.repo, "djinn-image-a1");
        assert_eq!(
            a1_plan
                .retained
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["v2", "v3"],
            "retained tags sorted alphabetically"
        );
        assert_eq!(
            a1_plan
                .deleted
                .iter()
                .map(|t| t.tag.as_str())
                .collect::<Vec<_>>(),
            vec!["v1"]
        );
        let b2_plan = &outcome.plan.repos[1];
        assert_eq!(b2_plan.repo, "djinn-image-b2");
        assert_eq!(b2_plan.deleted.len(), 0);

        // Selected pins/aliases.
        assert_eq!(
            outcome.plan.safe_pins.len(),
            2,
            "both selected images should be safe"
        );
        // safe_pins sorted by image_id.
        assert_eq!(outcome.plan.safe_pins[0].image_id, "a1");
        assert_eq!(outcome.plan.safe_pins[1].image_id, "b2");

        // Projected bytes.
        // Reclaimed: only a1's v1 (700). b2 deletes nothing.
        assert_eq!(outcome.plan.projected_reclaimed_bytes, 700);
        // Retained: a1 (900+800) + b2 (400+200) = 2300.
        assert_eq!(outcome.plan.projected_retained_bytes, 2300);

        // Report string contains the same byte projections.
        assert!(outcome.report.contains("Projected reclaimed bytes: 700"));
        assert!(outcome.report.contains("Projected retained bytes: 2300"));

        // The plan-level fields must match the report sections.
        assert!(outcome.report.contains("Retained tags (2)"));
        assert!(outcome.report.contains("Deleted tags (1)"));
        assert!(outcome.report.contains("Selected-image pins (2)"));
    }

    #[tokio::test]
    async fn end_to_end_report_shows_alias_pin_when_tag_deleted() {
        // a1's selected tag "v1" is deleted (newest=1 keeps "v3"), but "v3"
        // shares the same digest as "v1" — an alias. The report should show
        // an AliasRetained pin, not an unsafe image.
        let zot = MockZot {
            repos: vec![zrepo(
                "djinn-image-a1",
                vec![
                    ztag("v3", "sha256:shared", 900, "2024-03-01T00:00:00Z"),
                    ztag("v1", "sha256:shared", 700, "2024-01-01T00:00:00Z"),
                ],
            )],
        };
        let selected = vec![db_selected(
            "a1",
            "Rust",
            Some("reg/djinn-image-a1:v1"),
            Some("sha256:shared"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(1))
            .await
            .unwrap();

        assert!(outcome.plan.is_safe);
        assert_eq!(outcome.plan.safe_pins.len(), 1);
        // The pin reason should be an alias (the retained tag differs from
        // the selected tag but shares the digest).
        assert!(
            outcome.report.contains("aliased by retained tag"),
            "report should describe alias pin: {report}",
            report = outcome.report
        );
        // Retained: v3 (900). Deleted: v1 (700).
        assert_eq!(outcome.plan.projected_retained_bytes, 900);
        assert_eq!(outcome.plan.projected_reclaimed_bytes, 700);
    }

    // ── Bounded mode/outcome contract at the orchestration seam ───────────

    #[tokio::test]
    async fn disabled_cfg_produces_disabled_outcome_without_zot_contact() {
        // FailingZot proves the source is never contacted in disabled mode.
        let zot = FailingZot;
        let selected: Vec<djinn_db::SelectedCatalogImage> = vec![];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &disabled_cfg())
            .await
            .expect("disabled preflight must succeed without Zot contact");
        assert_eq!(outcome.mode, PreflightMode::Disabled);
        assert_eq!(outcome.outcome_kind, PreflightOutcomeKind::Disabled);
        assert!(!outcome.blocks_rollout);
        assert!(outcome.report.contains("Mode: disabled"));
        assert!(outcome.report.contains("Outcome: disabled"));
    }

    #[tokio::test]
    async fn dry_run_outcome_is_advisory_with_mode_label() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:keep"),
            Some("sha256:keep"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(1))
            .await
            .unwrap();
        assert_eq!(outcome.mode, PreflightMode::DryRun);
        assert_eq!(outcome.outcome_kind, PreflightOutcomeKind::Advisory);
        assert!(outcome.report.contains("Mode: dry_run"));
        assert!(outcome.report.contains("Outcome: advisory"));
        // Aggregate candidate counts.
        assert!(outcome.report.contains("Candidate tags: 2"));
        assert!(outcome.report.contains("Retained tags: 1"));
        assert!(outcome.report.contains("Deleted tags: 1"));
    }

    #[tokio::test]
    async fn destructive_safe_outcome_has_destructive_safe_label() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:keep"),
            Some("sha256:keep"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .unwrap();
        assert_eq!(outcome.mode, PreflightMode::Destructive);
        assert_eq!(outcome.outcome_kind, PreflightOutcomeKind::DestructiveSafe);
        assert!(outcome.report.contains("Mode: destructive"));
        assert!(outcome.report.contains("Outcome: destructive_safe"));
    }

    #[tokio::test]
    async fn destructive_blocked_error_report_contains_mode_and_outcome() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:drop"),
            Some("sha256:drop"),
            "ready",
        )];
        let err = run_retention_preflight_orchestration(&zot, &selected, &destructive_cfg(1))
            .await
            .expect_err("must fail-closed");
        let chain = error_chain(&err);
        // The error chain carries the PreflightError message.
        assert!(chain.contains("blocked"));
    }

    #[tokio::test]
    async fn report_contains_gc_observation_guidance_and_operator_owned() {
        let zot = MockZot {
            repos: vec![two_tag_repo("i1")],
        };
        let selected = vec![db_selected(
            "i1",
            "Rust",
            Some("reg/djinn-image-i1:keep"),
            Some("sha256:keep"),
            "ready",
        )];
        let outcome = run_retention_preflight_orchestration(&zot, &selected, &dry_run_cfg(1))
            .await
            .unwrap();
        assert!(
            outcome
                .report
                .contains("Post-enable Zot GC observation guidance"),
            "report must contain GC observation guidance: {report}",
            report = outcome.report
        );
        assert!(
            outcome.report.contains("operator-owned"),
            "report must state production execution is operator-owned"
        );
    }
}

#[cfg(test)]
mod build_admission_config_tests {
    use super::*;

    static BUILD_ADMISSION_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn state_for_admission_config(config: BuildAdmissionConfig) -> AppState {
        let db = Database::open_in_memory().expect("test database");
        let runtime = DatabaseRuntimeManager::new(
            crate::db::runtime::DatabaseRuntimeConfig::postgres(db.bootstrap_info().target.clone()),
        );
        AppState::new_inner(
            db,
            runtime,
            CancellationToken::new(),
            djinn_core::doctor::RetrievalHealthConfig::default(),
            Arc::new(djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics::new()),
            config,
        )
    }

    #[test]
    fn build_admission_defaults_and_legacy_zero_are_deterministic() {
        assert_eq!(
            BuildAdmissionConfig::parse(None, None).unwrap(),
            BuildAdmissionConfig {
                mode: BuildAdmissionMode::Observe,
                cap: 3
            }
        );
        assert_eq!(
            BuildAdmissionConfig::parse(None, Some("0")).unwrap(),
            BuildAdmissionConfig {
                mode: BuildAdmissionMode::Off,
                cap: 0
            }
        );
        assert!(BuildAdmissionConfig::parse(Some("observe"), Some("0")).is_err());
        assert!(BuildAdmissionConfig::parse(Some("enforce"), Some("0")).is_err());
    }

    #[test]
    fn build_admission_rejects_invalid_startup_values() {
        for cap in ["", "-1", "not-a-number", "65"] {
            assert!(
                BuildAdmissionConfig::parse(None, Some(cap)).is_err(),
                "{cap}"
            );
        }
        for mode in ["", "Observe", "unknown"] {
            assert!(
                BuildAdmissionConfig::parse(Some(mode), None).is_err(),
                "{mode:?}"
            );
        }
    }

    #[tokio::test]
    async fn off_state_composition_has_no_admission_dependency_to_inject() {
        let state = state_for_admission_config(BuildAdmissionConfig {
            mode: BuildAdmissionMode::Off,
            cap: 3,
        });

        // This is the actual AppState composition root used by both the
        // coordinator and K8s graph-warmer injection branches. No controller
        // means no journal repository or warm-admission object exists to pass
        // to either `.with_build_admission` or `.with_warm_admission`.
        assert!(state.inner.build_admission.is_none());
        assert!(
            state
                .inner
                .coordinator
                .try_lock()
                .expect("unstarted")
                .is_none()
        );
        assert!(
            state
                .inner
                .graph_warmer
                .try_read()
                .expect("unstarted")
                .is_none()
        );
    }

    #[tokio::test]
    async fn environment_rollback_requires_a_new_app_state() {
        let _guard = BUILD_ADMISSION_ENV_LOCK.lock().expect("environment lock");
        let old_mode = std::env::var_os(BUILD_ADMISSION_MODE_ENV);
        let old_cap = std::env::var_os(MAX_BUILD_TASKRUNS_ENV);

        // SAFETY: this test serializes its admission-environment mutations and
        // restores both variables before returning.
        unsafe {
            std::env::set_var(BUILD_ADMISSION_MODE_ENV, "enforce");
            std::env::set_var(MAX_BUILD_TASKRUNS_ENV, "4");
        }
        let running = state_for_admission_config(BuildAdmissionConfig::from_env().unwrap());
        let running_admission = running
            .inner
            .build_admission
            .as_ref()
            .expect("enforce composes a controller");
        assert!(!running_admission.is_ready(), "enforce begins closed");

        // A rollback changes the process environment but cannot replace the
        // controller retained by the already constructed process state.
        unsafe {
            std::env::set_var(BUILD_ADMISSION_MODE_ENV, "off");
            std::env::set_var(MAX_BUILD_TASKRUNS_ENV, "3");
        }
        assert!(running.inner.build_admission.is_some());
        let restarted = state_for_admission_config(BuildAdmissionConfig::from_env().unwrap());
        assert!(restarted.inner.build_admission.is_none());

        // SAFETY: restore the inherited process environment before releasing
        // the serialization lock.
        unsafe {
            match old_mode {
                Some(value) => std::env::set_var(BUILD_ADMISSION_MODE_ENV, value),
                None => std::env::remove_var(BUILD_ADMISSION_MODE_ENV),
            }
            match old_cap {
                Some(value) => std::env::set_var(MAX_BUILD_TASKRUNS_ENV, value),
                None => std::env::remove_var(MAX_BUILD_TASKRUNS_ENV),
            }
        }
    }

    #[tokio::test]
    async fn enforce_state_allocates_fresh_epoch_and_starts_journal_recovery_incomplete() {
        let first = state_for_admission_config(BuildAdmissionConfig {
            mode: BuildAdmissionMode::Enforce,
            cap: 3,
        });
        let first_admission = first
            .inner
            .build_admission
            .as_ref()
            .expect("enforce composes a controller");
        assert!(
            !first_admission.is_ready(),
            "Enforce starts fail-closed before recovery"
        );
        assert_eq!(
            first_admission.readiness(),
            djinn_agent::actors::coordinator::BuildAdmissionReadiness::JournalRecoveryIncomplete,
        );

        // A second AppState allocation produces a distinct, unique epoch.
        let second = state_for_admission_config(BuildAdmissionConfig {
            mode: BuildAdmissionMode::Enforce,
            cap: 3,
        });
        let second_admission = second
            .inner
            .build_admission
            .as_ref()
            .expect("enforce composes a controller");
        assert_ne!(
            first_admission.server_epoch(),
            second_admission.server_epoch(),
            "each Enforce startup allocates a fresh, unique server epoch"
        );
    }

    #[tokio::test]
    async fn begin_draining_blocks_enforce_admission_via_app_state() {
        let state = state_for_admission_config(BuildAdmissionConfig {
            mode: BuildAdmissionMode::Enforce,
            cap: 1,
        });
        let admission = state
            .inner
            .build_admission
            .as_ref()
            .expect("enforce composes a controller");
        assert!(!admission.is_draining());
        state.begin_build_admission_draining().await;
        assert!(
            admission.is_draining(),
            "graceful shutdown begins draining before permit release"
        );
        assert_eq!(
            admission.readiness(),
            djinn_agent::actors::coordinator::BuildAdmissionReadiness::ShutdownDraining,
        );
    }

    #[tokio::test]
    async fn enforce_startup_walks_journal_inventory_topology_gates_in_order() {
        use djinn_agent::actors::coordinator::BuildAdmissionReadiness;

        let state = state_for_admission_config(BuildAdmissionConfig {
            mode: BuildAdmissionMode::Enforce,
            cap: 3,
        });
        let admission = state
            .inner
            .build_admission
            .as_ref()
            .expect("enforce composes a controller")
            .clone();
        assert_eq!(
            admission.readiness(),
            BuildAdmissionReadiness::JournalRecoveryIncomplete,
            "Enforce starts fail-closed before journal recovery"
        );

        // Journal recovery runs first and, on an empty journal, must NOT mark
        // the controller healthy: the inventory gate keeps admission closed.
        state.initialize_build_admission_recovery().await;
        assert_eq!(
            admission.readiness(),
            BuildAdmissionReadiness::InventoryPending,
            "journal recovery alone advances only to inventory-pending"
        );
        assert!(!admission.is_ready());

        // Without a graph warmer there is no inventory to trust, so the gate
        // stays pending (fail-closed) rather than opening optimistically.
        state.initialize_build_admission_inventory().await;
        assert_eq!(
            admission.readiness(),
            BuildAdmissionReadiness::InventoryPending,
            "a missing inventory keeps Enforce fail-closed"
        );

        // The in-process runtime's warmer returns an empty Kubernetes
        // inventory successfully, completing the inventory gate.
        let warmer = build_in_process_graph_warmer(state.clone());
        *state.inner.graph_warmer.write().await = Some(Arc::new(warmer));
        state.initialize_build_admission_inventory().await;
        assert_eq!(
            admission.readiness(),
            BuildAdmissionReadiness::TopologyPending,
            "a successful inventory LIST advances the gate to topology-pending"
        );
        assert!(!admission.is_ready());

        // Winning the coordinator advisory lock (single-active topology) is
        // the final gate; only then does Enforce admission open.
        state.confirm_build_admission_topology();
        assert_eq!(admission.readiness(), BuildAdmissionReadiness::Healthy);
        assert!(admission.is_ready());
    }
}

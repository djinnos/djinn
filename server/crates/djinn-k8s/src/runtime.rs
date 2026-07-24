// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
//! `KubernetesRuntime` — dispatches per-task-run work as K8s `Job`s.
//!
//! Phase 2 K8s PR 3 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! `prepare` writes a per-task-run `Secret` carrying the bincode-encoded
//! [`djinn_runtime::TaskRunSpec`], creates the worker `Job`, and then back-
//! fills an `OwnerReference` so the Secret GCs when the Job does. The
//! launcher's TCP listener is NOT bound here — it's process-wide, bound at
//! djinn-server boot in PR 4 pt2. `KubernetesConfig::server_addr` carries
//! the pre-bound address the worker dials from inside the pod.
//!
//! `cancel` deletes the Job with a `Foreground` propagation policy so the
//! API server blocks the Job's completion on its Pod being fully cleaned
//! up. A 404 from the apiserver is treated as success — the call is
//! idempotent.
//!
//! `teardown` polls the Job status for completion with a five-minute cap,
//! best-effort deletes the Secret (the OwnerReference also GCs it), and
//! foreground-deletes the Job so Pods cascade-clean. Returns a minimal
//! [`TaskRunReport`] with `outcome: TaskRunOutcome::Interrupted` — real
//! terminal reports flow over the launcher's TCP connection in a later PR.
//!
//! `attach_stdio` is Phase 2.1's real BiStream hand-off: it awaits the
//! [`PendingConnection`] that `prepare` reserved on the shared
//! [`ConnectionRegistry`], consumes it via `into_parts`, and spawns a pair
//! of forwarder / translator tasks that bridge the TCP frame channel and
//! the returned [`BiStream`].  `cancel` pushes a
//! `FramePayload::Control(Cancel)` at the live worker before deleting the
//! Job.  `teardown` drains any remaining `events_rx` for a
//! [`StreamEvent::Report`]; if `attach_stdio` already consumed the slot
//! (the forwarder owns `events_rx` for the BiStream's lifetime), teardown
//! falls back to the Job-status polling path immediately.
//!
//! End-to-end `prepare`/`cancel`/`teardown` against a live kind cluster is
//! covered by `tests/kind_smoke.rs` (DJINN_TEST_KIND-gated). The unit tests
//! in this file exercise both the builder-parity invariants and the
//! forwarder topology in `attach_stdio` via an in-memory registry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_core::clock::{Clock, SystemClock};
use djinn_db::{Database, ProjectRepository, TaskRepository};
use djinn_runtime::wire::ControlMsg;
use djinn_runtime::{
    BiStream, InfraDeathLogTailCapture, ResolvedCredentials, RoleKind, RunHandle, RuntimeError,
    SessionRuntime, StreamEvent, StreamFrame, TaskRunOutcome, TaskRunReport, TaskRunSpec,
};
use djinn_supervisor::{ConnectionRegistry, Frame, FramePayload, PendingConnection};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::KubernetesConfig;
use crate::job::{build_task_run_job_with_read_sources, taskrun_job_ref_from_job};
use crate::secret::{TaskRunSecretBuilder, job_owner_reference, task_run_resource_name};
use crate::sidecar::ImageServiceResolution;

#[async_trait]
trait ReadSourcePreparation: Send + Sync {
    async fn github_coords(&self, project_id: &str) -> Result<Option<(String, String)>, String>;

    async fn materialize(&self, request: djinn_workspace::ReadSourceRequest) -> Result<(), String>;
}

struct HostReadSourcePreparation {
    projects: ProjectRepository,
}

impl HostReadSourcePreparation {
    fn new(db: &Database) -> Self {
        Self {
            projects: ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop()),
        }
    }
}

#[async_trait]
impl ReadSourcePreparation for HostReadSourcePreparation {
    async fn github_coords(&self, project_id: &str) -> Result<Option<(String, String)>, String> {
        self.projects
            .get_github_coords(project_id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn materialize(&self, request: djinn_workspace::ReadSourceRequest) -> Result<(), String> {
        djinn_workspace::ReadSourceMaterializer::ensure(request)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Populate the owner-scoped cache from its authoritative mirror before any
/// task-run resource exists. Every materialization error defers dispatch.
async fn pre_materialize_read_sources_with(
    preparation: &dyn ReadSourcePreparation,
    spec: &TaskRunSpec,
) -> Result<Option<String>, RuntimeError> {
    if spec.read_source_project_ids.is_empty() {
        return Ok(None);
    }

    let (owner, repo) = preparation
        .github_coords(&spec.project_id)
        .await
        .map_err(|error| {
            RuntimeError::Prepare(format!(
                "read-source authorization lookup for owner {} is uncertain: {error}",
                spec.project_id
            ))
        })?
        .ok_or_else(|| {
            RuntimeError::Prepare(format!(
                "read-source authorization owner {} does not exist",
                spec.project_id
            ))
        })?;
    let owner_root = djinn_core::paths::project_dir(&owner, &repo);

    for target_project_id in &spec.read_source_project_ids {
        // The immutable spec grant is the authorization. This lookup only
        // fails closed for a deleted granted project or uncertain DB state.
        preparation
            .github_coords(target_project_id)
            .await
            .map_err(|error| RuntimeError::Prepare(format!(
                "read-source authorization lookup for target {target_project_id} is uncertain: {error}"
            )))?
            .ok_or_else(|| RuntimeError::Prepare(format!(
                "authorized read-source project {target_project_id} does not exist"
            )))?;
        let request = djinn_workspace::ReadSourceRequest::new(
            spec.project_id.clone(),
            target_project_id.clone(),
            owner_root.clone(),
            djinn_workspace::mirror_path_for(target_project_id),
        );
        preparation.materialize(request).await.map_err(|error| {
            RuntimeError::Prepare(format!(
                "read-source pre-materialization for owner {} target {target_project_id} deferred: {error}",
                spec.project_id
            ))
        })?;
    }
    // The projects PVC is rooted at `projects_root`; the migrator writes under
    // `project_dir(owner, repo)`. Use that exact relative cache directory,
    // never the database project UUID, for the restricted Pod subPath.
    Ok(Some(format!("{owner}/{repo}/.task-runtime/read-sources")))
}

async fn pre_materialize_read_sources(
    db: &Database,
    spec: &TaskRunSpec,
) -> Result<Option<String>, RuntimeError> {
    pre_materialize_read_sources_with(&HostReadSourcePreparation::new(db), spec).await
}

/// Bound on the [`ConnectionRegistry::register_pending`] buffer used by
/// `prepare`.  Large enough that a busy worker doesn't back-pressure on
/// frame-rate, small enough that we don't hoard memory if the launcher
/// stalls between prepares.
const PENDING_CONNECTION_BUFFER: usize = 64;

/// How long [`KubernetesRuntime::teardown`] will drain an un-consumed
/// `events_rx` looking for a terminal [`StreamEvent::Report`] before
/// falling back to the Job-status poll path.  Short because the worker
/// always emits the terminal report as its last frame before exiting, so
/// any observable delay here is purely network latency; when the events
/// stream is closed without a report we want to fall through to the
/// Job-status poll quickly.
const TEARDOWN_EVENTS_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// D2: deadline on the worker startup handshake — how long `attach_stdio` waits
/// for the Pod to dial back and complete `AuthHello` before declaring it dead.
/// Generous enough to cover image pull + scheduling + container start on a cold
/// node (Karpenter scale-up can take minutes), but bounded so a Pod that never
/// starts (image-pull error, unschedulable, crash-loop) can't hang host dispatch
/// indefinitely. On expiry `attach_stdio` returns [`RuntimeError::HandshakeTimeout`].
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(600);

/// Grace period observed while polling `teardown` for job completion.
///
/// Per the Phase 2 K8s plan, we bound this at five minutes: worker tasks
/// typically finish in well under 60s, but the supervisor occasionally
/// ships tasks that post-process large diffs and we'd rather surface a
/// clean timeout than an indeterminate hang.
// Long enough to cover a full dev session (plan → code → cargo test → submit).
// The supervisor calls teardown immediately after `attach_stdio` for K8s and then
// polls the Job for terminal state; if this fires before the worker exits, the
// task-run is declared a failure and the planner is re-dispatched on a fresh
// workspace, losing in-progress code changes. Must exceed the Job's
// `activeDeadlineSeconds` (default 10800s) plus the termination grace window so
// the host keeps polling until the kubelet's own deadline resolves the Pod — a
// 3h-budget run must not be declared failed by the host an hour in.
const TEARDOWN_POLL_TIMEOUT: Duration = Duration::from_secs(11_400);
/// Poll interval used inside [`poll_job_terminal_state`].
const TEARDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn service_resolution_activity_payload(
    task_run_id: &str,
    project_id: &str,
    resolution: &ImageServiceResolution,
) -> serde_json::Value {
    json!({
        "task_run_id": task_run_id,
        "project_id": project_id,
        "image": resolution.image,
        "requested": resolution.requested_preset_ids,
        "injected": resolution.injected,
        "skipped": resolution.skipped,
        "errors": resolution.lookup_error.as_ref().map(|error| vec![error]).unwrap_or_default(),
    })
}

/// Poll interval for the in-flight infra-death watch
/// ([`KubernetesRuntime::watch_infra_death`]). Coarser than the teardown poll:
/// this loop runs for the *entire* lifetime of every in-flight run (racing the
/// worker's report stream), so it must be cheap on the apiserver — a dead Job
/// detected ~15s late is still ~two orders of magnitude faster than the 30-min
/// idle stall reaper it replaces, and well inside the worker's termination
/// grace + report-flush window for a clean exit.
const INFRA_DEATH_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Kubernetes-backed `SessionRuntime`.
///
/// Owns the cluster-side configuration plus a `kube::Client` acquired from
/// the ambient kubeconfig / in-cluster ServiceAccount, plus a shared
/// [`ConnectionRegistry`] the launcher-side `serve_on_tcp` routes worker
/// event frames through.  The registry is process-wide (one `Arc` lives in
/// `server::AppState`) and threaded into every `KubernetesRuntime`
/// instance so multiple parallel task-runs share a single TCP listener.
pub struct KubernetesRuntime {
    client: kube::Client,
    config: KubernetesConfig,
    registry: Arc<ConnectionRegistry>,
    /// Database handle used by [`Self::prepare`] to look up the per-project
    /// devcontainer image tag before building the task-run Job manifest
    /// (Phase 3 PR 5). `None` in tests that construct the runtime via the
    /// legacy `new`/`from_client` surface — those callers never reach the
    /// `prepare` code path (they exercise pure-builder unit tests).
    db: Option<Database>,
    /// Injectable host-side read-source gate. Production constructs the DB-backed
    /// implementation lazily; tests inject a recorder while still driving the
    /// real `SessionRuntime::prepare` orchestration.
    read_source_preparation: Option<Arc<dyn ReadSourcePreparation>>,
    /// Test-only dispatch-image bypass used to reach the actual resource POSTs
    /// without coupling orchestration regressions to a live database.
    #[cfg(test)]
    dispatch_image_override: Option<String>,
    /// Per-task-run [`PendingConnection`] handles reserved during `prepare`
    /// and drained by `attach_stdio` / `teardown`.  Keyed by
    /// `task_run_id`.  Entries stay present until whichever method lands
    /// first: if `attach_stdio` runs, it consumes the handle via
    /// `into_parts` and stores nothing back; if `teardown` runs without a
    /// matching attach (e.g. the worker never dialled), it drains the
    /// handle's `events_rx` for a short window before falling back to the
    /// Job-status poll.
    pending: Arc<Mutex<HashMap<String, PendingConnection>>>,
}

impl KubernetesRuntime {
    /// Construct a new runtime by discovering a `kube::Client` from the
    /// ambient environment (in-cluster ServiceAccount when running in a Pod,
    /// `$KUBECONFIG` otherwise).
    ///
    /// The returned runtime has no database handle bound; callers that need
    /// to dispatch task-run Jobs (the production path) must prefer
    /// [`Self::with_db`] so `prepare` can resolve the per-project
    /// devcontainer image tag.
    pub async fn new(
        config: KubernetesConfig,
        registry: Arc<ConnectionRegistry>,
    ) -> Result<Self, kube::Error> {
        let client = kube::Client::try_default().await?;
        Ok(Self {
            client,
            config,
            registry,
            db: None,
            read_source_preparation: None,
            #[cfg(test)]
            dispatch_image_override: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Construct a new runtime with a bound database handle (production
    /// path — `prepare` uses the DB to resolve `projects.image_tag`).
    pub async fn with_db(
        config: KubernetesConfig,
        registry: Arc<ConnectionRegistry>,
        db: Database,
    ) -> Result<Self, kube::Error> {
        let client = kube::Client::try_default().await?;
        Ok(Self {
            client,
            config,
            registry,
            db: Some(db),
            read_source_preparation: None,
            #[cfg(test)]
            dispatch_image_override: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Construct a runtime from an already-built client — handy for tests and
    /// for call sites that share a client across multiple consumers.
    pub fn from_client(
        client: kube::Client,
        config: KubernetesConfig,
        registry: Arc<ConnectionRegistry>,
    ) -> Self {
        Self {
            client,
            config,
            registry,
            db: None,
            read_source_preparation: None,
            #[cfg(test)]
            dispatch_image_override: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Construct a runtime from an already-built client + DB (the supervisor
    /// production path uses this so it can also share the DB pool).
    pub fn from_client_with_db(
        client: kube::Client,
        config: KubernetesConfig,
        registry: Arc<ConnectionRegistry>,
        db: Database,
    ) -> Self {
        Self {
            client,
            config,
            registry,
            db: Some(db),
            read_source_preparation: None,
            #[cfg(test)]
            dispatch_image_override: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reference to the active config (used by tests + the kind smoke suite).
    pub fn config(&self) -> &KubernetesConfig {
        &self.config
    }

    /// Reference to the underlying `kube::Client`.
    pub fn client(&self) -> &kube::Client {
        &self.client
    }

    /// Reference to the shared [`ConnectionRegistry`].  Exposed so call
    /// sites that boot their own runtime can wire the same registry into
    /// a concurrent `serve_on_tcp` spawn.
    pub fn registry(&self) -> &Arc<ConnectionRegistry> {
        &self.registry
    }

    async fn prepare_read_sources(
        &self,
        db: &Database,
        spec: &TaskRunSpec,
    ) -> Result<Option<String>, RuntimeError> {
        match &self.read_source_preparation {
            Some(preparation) => {
                pre_materialize_read_sources_with(preparation.as_ref(), spec).await
            }
            None => pre_materialize_read_sources(db, spec).await,
        }
    }

    /// Foreground-delete the canonical task-run Job for `task_run_id`.
    ///
    /// This is the layering-safe primitive exposed through server/runtime
    /// bridges for lifecycle code that knows only the task-run id. It reuses
    /// the same `delete_job_foreground` helper as `cancel`/`teardown`, so
    /// Kubernetes 404/not-found is idempotent success.
    pub async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), kube::Error> {
        delete_taskrun_job_foreground(&self.client, &self.config.namespace, task_run_id).await
    }

    /// List Djinn task-run Jobs in the configured namespace.
    pub async fn list_taskrun_jobs(
        &self,
    ) -> Result<Vec<djinn_runtime::TaskrunJobRef>, kube::Error> {
        list_taskrun_jobs(&self.client, &self.config.namespace).await
    }
}

#[async_trait]
impl SessionRuntime for KubernetesRuntime {
    /// Materialise the per-task-run K8s objects.
    ///
    /// 1. Allocate a new task-run id (uuid v7).
    /// 2. Build + create the `Secret` carrying the bincode-encoded
    ///    [`TaskRunSpec`] at key `spec.bin`.
    /// 3. Build + create the `Job` manifest pointing at that Secret.
    /// 4. Patch the Secret with an `OwnerReference` to the freshly-created
    ///    Job so kubernetes GCs the Secret together with its Job.
    ///
    /// Does NOT bind any listener — the launcher owns the TCP listener and
    /// advertises its address through `config.server_addr`.
    async fn prepare(
        &self,
        spec: &TaskRunSpec,
        credentials: &ResolvedCredentials,
    ) -> Result<RunHandle, RuntimeError> {
        // The canonical id is minted by the host coordinator and carried on the
        // spec — `prepare` no longer mints its own. Parse it back to a `Uuid`
        // for the resource-name / label derivations; a non-UUID id is a
        // programmer error in the dispatch path.
        let task_run_id = Uuid::parse_str(&spec.task_run_id).map_err(|e| {
            RuntimeError::Prepare(format!(
                "spec.task_run_id `{}` is not a valid UUID: {e}",
                spec.task_run_id
            ))
        })?;
        let task_run_id_str = task_run_id.to_string();
        let ns = &self.config.namespace;
        let resource_name = task_run_resource_name(&task_run_id);

        debug!(
            task_run_id = %task_run_id_str,
            namespace = %ns,
            project_id = %spec.project_id,
            "kubernetes_runtime: preparing task-run resources"
        );

        // Phase 3 PR 5: resolve the per-project devcontainer image BEFORE
        // doing any cluster work.  The dispatch path is hard-failed if the
        // image controller hasn't produced a ready build — no silent
        // fallback to `config.image`.
        let db = self.db.as_ref().ok_or_else(|| {
            RuntimeError::Prepare(
                "KubernetesRuntime constructed without a database handle; \
                 `with_db` / `from_client_with_db` is required to dispatch \
                 task-run Jobs"
                    .into(),
            )
        })?;

        // Safety gate: every authorized cache is materialized before any
        // Kubernetes API request can be issued.
        let read_source_cache_sub_path = self.prepare_read_sources(db, spec).await?;

        let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Catalog-image precedence (migration 46): a project on a shared
        // catalog image dispatches against that image's pull ref; otherwise
        // it uses its own per-project build. The resolver is the single
        // source of truth — no silent fallback if the resolved image isn't
        // ready yet (hard-fail, exactly as the per-project path always did).
        #[cfg(test)]
        let project_image_tag = self.dispatch_image_override.clone();
        #[cfg(not(test))]
        let project_image_tag: Option<String> = None;
        let project_image_tag = match project_image_tag {
            Some(tag) => tag,
            None => {
                let dispatch_image = repo
                    .resolve_dispatch_image(&spec.project_id)
                    .await
                    .map_err(|e| {
                        RuntimeError::Prepare(format!(
                            "resolve_dispatch_image({}): {e}",
                            spec.project_id
                        ))
                    })?;
                match dispatch_image.as_ref().and_then(|d| d.pull_ref()) {
                    Some(pull_ref) => pull_ref,
                    None => return Err(RuntimeError::DevcontainerMissing(spec.project_id.clone())),
                }
            }
        };

        // Load the project's effective EnvironmentConfig once for the
        // entire prepare flow.  Fail-open: absent DB config, lookup
        // errors, or parse failures yield `EnvironmentConfig::empty()`
        // so dispatch continues with no pre-task config and no cargo
        // cache policy — preserving rolling compatibility with old/no
        // environment_config rows.
        let (effective_env_config, cargo_cache_policy): (
            djinn_stack::environment::EnvironmentConfig,
            Option<djinn_stack::environment::CargoCachePolicy>,
        ) = {
            let env_repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
            match env_repo.get_environment_config(&spec.project_id).await {
                Ok(Some(raw)) => {
                    match serde_json::from_str::<djinn_stack::environment::EnvironmentConfig>(&raw)
                    {
                        Ok(cfg) => {
                            let policy = cfg.cargo_cache_policy.clone();
                            (cfg, policy)
                        }
                        Err(e) => {
                            warn!(
                                task_run_id = %task_run_id_str,
                                project_id = %spec.project_id,
                                error = %e,
                                "kubernetes_runtime: environment config parse failed; \
                                 using empty config (fail-open)"
                            );
                            (djinn_stack::environment::EnvironmentConfig::empty(), None)
                        }
                    }
                }
                Ok(None) => (djinn_stack::environment::EnvironmentConfig::empty(), None),
                Err(e) => {
                    warn!(
                        task_run_id = %task_run_id_str,
                        project_id = %spec.project_id,
                        error = %e,
                        "kubernetes_runtime: environment config DB lookup failed; \
                         using empty config (fail-open)"
                    );
                    (djinn_stack::environment::EnvironmentConfig::empty(), None)
                }
            }
        };

        // 0. Reserve the registry slot BEFORE creating the Job.  This closes
        //    the race where the Pod starts up and completes the AuthHello
        //    handshake faster than `prepare` returns — without a reservation
        //    the serve_on_tcp accept loop would drop the worker's event
        //    frames as "unrecognised task_run_id".  The handle is stashed in
        //    `self.pending` for `attach_stdio` / `teardown` to consume.
        let pending = self
            .registry
            .register_pending(task_run_id_str.clone(), PENDING_CONNECTION_BUFFER)
            .await
            .map_err(|e| RuntimeError::Prepare(format!("register pending: {e}")))?;
        self.pending
            .lock()
            .await
            .insert(task_run_id_str.clone(), pending);

        // 1. Resolve backing services and build the per-task-run Secret.
        //    Service resolution is computed before the Secret so the worker
        //    receives enough metadata to wait after sidecar readiness.
        //    The existing `task_run_services_resolved` activity event is
        //    logged after Secret creation (see step 1b below).
        let service_resolution =
            crate::sidecar::resolve_image_services_with_metadata(db, &spec.project_id).await;

        // Build + create the per-task-run Secret.  Carries `spec.bin`,
        // `credentials.bin`, the effective `EnvironmentConfig` JSON,
        // and the resolved service metadata JSON (hgd0 Wave 1).
        let secret = {
            let builder = TaskRunSecretBuilder::new(ns, &task_run_id, spec, credentials)
                .environment_config(&effective_env_config);
            match builder.service_metadata(&service_resolution) {
                Ok(b) => match b.build() {
                    Ok(s) => s,
                    Err(e) => {
                        self.drop_pending(&task_run_id_str).await;
                        return Err(RuntimeError::Prepare(format!("build secret: {e}")));
                    }
                },
                Err(e) => {
                    self.drop_pending(&task_run_id_str).await;
                    return Err(RuntimeError::Prepare(format!(
                        "serialize service metadata: {e}"
                    )));
                }
            }
        };

        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), ns);
        if let Err(e) = secrets.create(&PostParams::default(), &secret).await {
            self.drop_pending(&task_run_id_str).await;
            return Err(RuntimeError::Prepare(format!(
                "create secret {resource_name}: {e}"
            )));
        }

        // 1b. Log the `task_run_services_resolved` activity event.
        //     Service resolution was computed earlier (step 1) so both the
        //     Secret payload and this event carry identical metadata.
        //     Existing consumers of this event name and
        //     requested/injected/skipped semantics remain unchanged.
        let services = &service_resolution.services;
        let service_payload = service_resolution_activity_payload(
            &task_run_id_str,
            &spec.project_id,
            &service_resolution,
        );
        let requested_service_count = service_resolution.requested_preset_ids.len();
        let injected_service_count = service_resolution.injected.len();
        let skipped_service_count = service_resolution.skipped.len();
        let requested_preset_ids = service_resolution.requested_preset_ids.join(",");
        let injected_service_types = service_resolution
            .injected
            .iter()
            .map(|service| service.service_type.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let skipped_preset_ids = service_resolution
            .skipped
            .iter()
            .map(|skipped| format!("{}:{}", skipped.preset_id, skipped.reason))
            .collect::<Vec<_>>()
            .join(",");
        let lookup_error = service_resolution.lookup_error.as_deref().unwrap_or("");
        if service_resolution.lookup_error.is_some()
            || requested_service_count != injected_service_count
        {
            warn!(
                task_run_id = %task_run_id_str,
                project_id = %spec.project_id,
                requested_service_count,
                injected_service_count,
                skipped_service_count,
                requested_preset_ids = %requested_preset_ids,
                injected_service_types = %injected_service_types,
                skipped_preset_ids = %skipped_preset_ids,
                lookup_error = %lookup_error,
                "kubernetes_runtime: task-run backing service resolution incomplete"
            );
        } else {
            info!(
                task_run_id = %task_run_id_str,
                project_id = %spec.project_id,
                requested_service_count,
                injected_service_count,
                requested_preset_ids = %requested_preset_ids,
                injected_service_types = %injected_service_types,
                "kubernetes_runtime: task-run backing services resolved"
            );
        }
        match TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .log_activity(
                Some(&spec.task_id),
                "system",
                "system",
                "task_run_services_resolved",
                &service_payload.to_string(),
            )
            .await
        {
            Ok(_) => {}
            Err(error) => warn!(
                task_run_id = %task_run_id_str,
                project_id = %spec.project_id,
                %error,
                "kubernetes_runtime: failed to log task-run service resolution activity"
            ),
        }

        // 2a. Fail-closed enforcement render validation BEFORE the Job is
        //     submitted — i.e. before any user code can execute. Rejects
        //     unsupported cgroup-v2 delegation profiles, an out-of-bounds broker
        //     quota, or an incompatible volume-ownership mode. Grounded in the
        //     launcher crate's own `Readiness::validate`.
        if let Err(error) = crate::launcher::validate_enforcement_render(&self.config) {
            self.drop_pending(&task_run_id_str).await;
            return Err(RuntimeError::Prepare(format!(
                "enforcement render validation failed: {error}"
            )));
        }

        // 2b. Build + create the Job manifest.  The `cargo_cache_policy`
        //     was extracted from the effective EnvironmentConfig earlier
        //     (step 1) and is passed through as before.  The role that executes
        //     this run is the primary role of its supervisor flow; it drives the
        //     role-classed CPU request (light vs build-capable). `None` (an
        //     empty sequence) fails safe to build-capable in the renderer.
        let role = spec.flow.role_sequence().first().copied();
        let job = build_task_run_job_with_read_sources(
            &self.config,
            &task_run_id,
            &spec.project_id,
            &resource_name,
            &project_image_tag,
            services,
            cargo_cache_policy.as_ref(),
            spec.is_evidence_spike,
            role,
            read_source_cache_sub_path.as_deref(),
        );
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let created_job = match jobs.create(&PostParams::default(), &job).await {
            Ok(j) => j,
            Err(e) => {
                // Best-effort cleanup of the orphan Secret — don't shadow the
                // original error if cleanup also fails.
                let secrets_bg = secrets.clone();
                let name = resource_name.clone();
                tokio::spawn(async move {
                    let _ = secrets_bg.delete(&name, &DeleteParams::default()).await;
                });
                self.drop_pending(&task_run_id_str).await;
                return Err(RuntimeError::Prepare(format!(
                    "create job {resource_name}: {e}"
                )));
            }
        };

        // 3. Attach an OwnerReference so the Secret GCs with the Job.
        let job_uid = match created_job.metadata.uid.clone() {
            Some(uid) => uid,
            None => {
                self.drop_pending(&task_run_id_str).await;
                return Err(RuntimeError::Prepare(
                    "created Job missing metadata.uid".into(),
                ));
            }
        };
        let owner = job_owner_reference(&resource_name, &job_uid);
        let patch = json!({
            "metadata": {
                "ownerReferences": [owner],
            }
        });
        // Owner-ref patch is best-effort: the Job's `ttlSecondsAfterFinished`
        // already guarantees cleanup, so patch failure shouldn't block the
        // task-run starting. Log at warn level and continue.
        if let Err(e) = secrets
            .patch(
                &resource_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
        {
            warn!(
                task_run_id = %task_run_id_str,
                namespace = %ns,
                secret = %resource_name,
                error = %e,
                "kubernetes_runtime: owner-ref patch failed (continuing; TTL-based GC still applies)"
            );
        }

        info!(
            task_run_id = %task_run_id_str,
            namespace = %ns,
            job = %resource_name,
            "kubernetes_runtime: task-run resources created"
        );

        Ok(RunHandle {
            task_run_id: task_run_id_str,
            container_id: None,
            pod_ref: Some(resource_name),
            started_at: SystemClock::new().now(),
        })
    }

    /// Await the worker Pod's TCP handshake and wire a [`BiStream`] onto
    /// its event + control channels.
    ///
    /// Flow:
    ///   1. Pull the [`PendingConnection`] reserved by `prepare` out of
    ///      `self.pending` — error out if `prepare` never ran for this
    ///      `task_run_id` or `attach_stdio` was already called once.
    ///   2. Consume the handle via [`PendingConnection::into_parts`],
    ///      bypassing the handle's [`Drop`] auto-deregister.  The
    ///      Kubernetes runtime now owns the registry slot for the rest of
    ///      the run; `teardown` deregisters explicitly after cleanup.
    ///   3. Await `connected_rx` so we don't spawn forwarder tasks before
    ///      the worker actually finishes `AuthHello` — the outbound
    ///      sender isn't populated until that point.
    ///   4. Spawn a forwarder: `events_rx` (TCP → registry) → the
    ///      `BiStream::events_rx` the caller reads.  Terminates naturally
    ///      when `events_rx` closes (the worker exits and
    ///      `serve_on_tcp`'s dispatch loop drops its side).
    ///   5. Spawn a translator: the `BiStream::requests_tx` the caller
    ///      writes → outbound `Frame`s pushed back down the worker TCP
    ///      connection.  `StreamFrame::Cancel` maps onto
    ///      `FramePayload::Control(ControlMsg::Cancel)`; `RpcResponse`
    ///      frames are logged — they belong to a future PR (they'd carry
    ///      correlated worker-originated RPC replies).
    ///
    /// Both spawned tasks are fully detached — they own their ends of the
    /// channels and drop cleanly when either side closes.  Returning the
    /// `BiStream` here hands live event ownership back to the supervisor
    /// runner; nothing in the runtime continues to hold the consumed
    /// `events_rx`, so a later `teardown` falls straight through to the
    /// Job-status poll path.
    async fn attach_stdio(&self, handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        let task_run_id = handle.task_run_id.clone();

        let pending = {
            let mut pending_map = self.pending.lock().await;
            pending_map.remove(&task_run_id)
        };
        let pending = pending.ok_or_else(|| {
            RuntimeError::Attach(format!(
                "no pending connection reserved for task_run_id={task_run_id} \
                 (prepare not called, or attach_stdio already consumed it)"
            ))
        })?;

        bridge_pending_to_bistream(&task_run_id, pending).await
    }

    /// Request graceful cancellation by first nudging the worker with a
    /// `FramePayload::Control(Cancel)` over its outbound sender (best-
    /// effort — if the worker never dialled, or the sender has already
    /// closed, we skip it) and then deleting the Job with `Foreground`
    /// propagation and the configured grace period.  Idempotent: a 404
    /// from the apiserver is mapped to success.
    ///
    /// Sending the Cancel frame *before* the Job delete gives the
    /// supervisor inside the worker Pod a chance to flush the terminal
    /// report and cleanly close — otherwise the Pod delete races the
    /// supervisor's final `TaskRunReport` write and we lose it.
    async fn cancel(&self, handle: &RunHandle) -> Result<(), RuntimeError> {
        let job_name = handle
            .pod_ref
            .as_deref()
            .ok_or_else(|| RuntimeError::Cancel("RunHandle.pod_ref missing".into()))?;

        // Best-effort cancel-frame delivery.  Errors here are never
        // propagated: the worker may already be dead, the handshake may
        // never have landed, or the outbound writer may have closed.
        if let Some(outbound) = self.registry.outbound_sender_for(&handle.task_run_id).await {
            let cancel_frame = Frame {
                correlation_id: 0,
                payload: FramePayload::Control(ControlMsg::Cancel),
            };
            if let Err(e) = outbound.send(cancel_frame).await {
                debug!(
                    task_run_id = %handle.task_run_id,
                    error = %e,
                    "kubernetes_runtime: cancel-frame send failed (continuing)"
                );
            } else {
                debug!(
                    task_run_id = %handle.task_run_id,
                    "kubernetes_runtime: cancel-frame sent to worker"
                );
            }
        } else {
            debug!(
                task_run_id = %handle.task_run_id,
                "kubernetes_runtime: no outbound sender registered; skipping cancel-frame"
            );
        }

        delete_job_foreground(&self.client, &self.config.namespace, job_name, 30)
            .await
            .map_err(|e| RuntimeError::Cancel(format!("delete job {job_name}: {e}")))
    }

    /// Drain any remaining `events_rx` for a terminal
    /// [`StreamEvent::Report`], best-effort delete the Secret, foreground-
    /// delete the Job so its Pods cascade-clean, and return the
    /// [`TaskRunReport`].
    ///
    /// Decision tree for the terminal report:
    ///
    /// 1. `self.pending` still holds the [`PendingConnection`] for this
    ///    task_run_id ⇒ `attach_stdio` never ran (the Kubernetes path
    ///    currently ignores the `BiStream` in `supervisor_runner`).  We
    ///    drain `events_rx` for a bounded
    ///    [`TEARDOWN_EVENTS_DRAIN_TIMEOUT`] window; the terminal report,
    ///    if it landed, becomes the returned `TaskRunReport`.
    /// 2. On drain timeout / channel close without a Report, or when
    ///    `attach_stdio` already consumed the slot (the forwarder owns
    ///    `events_rx` and has already delivered the report to the
    ///    BiStream — teardown sees an empty `pending` map), we fall
    ///    through to the Job-status poll path and synthesise a
    ///    minimal [`TaskRunOutcome::Interrupted`] report the way PR 3
    ///    always did.
    ///
    /// Polls for at most [`TEARDOWN_POLL_TIMEOUT`]; on poll timeout,
    /// cleanup is still attempted and then an `Err(RuntimeError::Teardown)`
    /// is returned.
    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        let job_name = handle
            .pod_ref
            .as_deref()
            .ok_or_else(|| RuntimeError::Teardown("RunHandle.pod_ref missing".into()))?
            .to_string();
        let ns = self.config.namespace.clone();
        // Secret shares the Job's name — both produced via
        // `task_run_resource_name(&task_run_id)` in `prepare`.
        let secret_name = job_name.clone();

        // Drain an un-consumed `events_rx` if `attach_stdio` never ran.
        let mut report_from_events: Option<TaskRunReport> = None;
        let pending = {
            let mut pending_map = self.pending.lock().await;
            pending_map.remove(&handle.task_run_id)
        };
        if let Some(pending) = pending {
            let mut parts = pending.into_parts();
            let drain = tokio::time::timeout(TEARDOWN_EVENTS_DRAIN_TIMEOUT, async {
                while let Some(event) = parts.events_rx.recv().await {
                    if let StreamEvent::Report(report) = event {
                        return Some(report);
                    }
                }
                None
            })
            .await;
            report_from_events = match drain {
                Ok(Some(report)) => {
                    debug!(
                        task_run_id = %handle.task_run_id,
                        "kubernetes_runtime: teardown drained terminal report from events_rx"
                    );
                    Some(report)
                }
                Ok(None) => {
                    debug!(
                        task_run_id = %handle.task_run_id,
                        "kubernetes_runtime: teardown events_rx closed without Report (falling back to Job poll)"
                    );
                    None
                }
                Err(_) => {
                    debug!(
                        task_run_id = %handle.task_run_id,
                        timeout_ms = TEARDOWN_EVENTS_DRAIN_TIMEOUT.as_millis(),
                        "kubernetes_runtime: teardown events_rx drain timed out (falling back to Job poll)"
                    );
                    None
                }
            };
        } else {
            debug!(
                task_run_id = %handle.task_run_id,
                "kubernetes_runtime: teardown has no pending entry — attach_stdio already consumed events_rx; falling back to Job poll"
            );
        }

        let terminal = poll_job_terminal_state(&self.client, &ns, &job_name).await;

        // Best-effort Secret delete. The OwnerReference from `prepare` also
        // GCs it, but deleting explicitly tightens the window. 404 is fine.
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &ns);
        match secrets
            .delete(&secret_name, &DeleteParams::background())
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(resp)) if resp.code == 404 => {
                debug!(
                    secret = %secret_name,
                    namespace = %ns,
                    "kubernetes_runtime: teardown secret already gone (404)"
                );
            }
            Err(e) => {
                warn!(
                    secret = %secret_name,
                    namespace = %ns,
                    error = %e,
                    "kubernetes_runtime: teardown secret-delete failed (ignored)"
                );
            }
        }

        // Foreground-delete the Job so Pods cascade-clean. 404 is fine.
        if let Err(e) = delete_job_foreground(&self.client, &ns, &job_name, 30 /* seconds */).await
        {
            warn!(
                job = %job_name,
                namespace = %ns,
                error = %e,
                "kubernetes_runtime: teardown job-delete failed (ignored)"
            );
        }

        // Always release the registry slot, regardless of the report path
        // (drain-from-events, forwarder-consumed, or Job-poll fallback).
        self.registry.deregister(&handle.task_run_id).await;

        // On timeout, surface the error AFTER cleanup so the caller knows
        // the Job is still being torn down but didn't complete in-window.
        if matches!(terminal, JobTerminal::TimedOut) {
            warn!(
                job = %job_name,
                namespace = %ns,
                timeout_secs = TEARDOWN_POLL_TIMEOUT.as_secs(),
                "kubernetes_runtime: teardown poll timed out; cleanup attempted"
            );
            return Err(RuntimeError::Teardown(format!(
                "timeout waiting for Job {job_name} to complete"
            )));
        }

        Ok(report_from_events.unwrap_or(TaskRunReport {
            task_run_id: handle.task_run_id.clone(),
            outcome: TaskRunOutcome::Interrupted,
            stages_completed: Vec::<RoleKind>::new(),
        }))
    }

    /// Host-side liveness watch: resolve the moment the run's backing Job/Pod
    /// is *terminally dead*, returning the captured death reason.
    ///
    /// Why this exists: when the worker container is SIGKILLed mid-stage (a
    /// memory-cgroup OOM kill of rust-analyzer + rustc + rust-lld, a node
    /// eviction), the host's RPC connection to the Pod can be left half-open —
    /// the kernel reaped the worker's child build processes and wedged the
    /// supervisor without a clean TCP FIN, so the report stream
    /// (`await_report_from_stream`) never closes. The dispatch runner would
    /// then block on that stream until the generic 30-minute idle stall reaper
    /// finally collected the session, mis-attributing an OOM to a "stall" and
    /// pinning the slot the whole time. Racing this watch against the report
    /// stream lets the runner finalize the run with the *real* reason
    /// (`OOMKilled (exit 137)`, `BackoffLimitExceeded`) and free the slot
    /// within ~15s of the Job dying.
    ///
    /// Termination semantics (conservative — only declares death on a
    /// genuinely terminal condition, never on a scheduling/restart blip):
    /// - The Pod's `worker` container terminated with a non-zero exit (or an
    ///   explicit `OOMKilled` reason) → captured BEFORE the Pod is GC'd, since
    ///   the container exit code / reason is the richest signal and disappears
    ///   when the Pod object is deleted by the Job's `ttlSecondsAfterFinished`.
    /// - The Job reports a `Failed` condition / `status.failed > 0`
    ///   (`backoffLimit: 0` ⇒ a single Pod failure trips `BackoffLimitExceeded`).
    /// - The Pod was OBSERVED running and is now gone while the Job is also
    ///   gone (TTL-GC'd after finishing) → the run finished out-of-band and we
    ///   never saw a report; treat as a terminal disappearance.
    ///
    /// A `Succeeded` Job is NOT treated as death — a clean run delivers its
    /// terminal report over the stream, which the runner prefers; resolving
    /// here on success would race that and risk a spurious "interrupted".
    async fn watch_infra_death(&self, handle: &RunHandle) -> String {
        let Some(job_name) = handle.pod_ref.as_deref() else {
            // No Job reference (shouldn't happen on the K8s path) — never
            // resolve, so the runner relies purely on the report stream.
            return std::future::pending().await;
        };
        let ns = &self.config.namespace;
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let label_selector = format!("{}={}", crate::job::LABEL_TASK_RUN_ID, handle.task_run_id);

        // Tracks whether we ever observed the worker Pod present. Only a
        // pod that WAS seen and then disappeared (alongside a gone Job)
        // counts as a terminal out-of-band death — a pod that simply hasn't
        // been created yet (scheduling lag) must never trip the watch.
        let mut pod_seen = false;

        loop {
            // 1. Richest signal first: the Pod's container terminated state,
            //    captured before TTL-GC removes the Pod object.
            match pods
                .list(&ListParams::default().labels(&label_selector))
                .await
            {
                Ok(list) => {
                    if let Some(pod) = list.items.first() {
                        pod_seen = true;
                        if let Some(reason) = pod_container_death_reason(pod) {
                            debug!(
                                task_run_id = %handle.task_run_id,
                                job = %job_name,
                                %reason,
                                "kubernetes_runtime: infra-death watch — worker container terminated"
                            );
                            return reason;
                        }
                    } else if pod_seen {
                        // The Pod was here and is now gone. If the Job is also
                        // gone (TTL-GC after finishing), the run ended
                        // out-of-band without a report — terminal.
                        match jobs.get_opt(job_name).await {
                            Ok(None) => {
                                debug!(
                                    task_run_id = %handle.task_run_id,
                                    job = %job_name,
                                    "kubernetes_runtime: infra-death watch — pod and job both gone"
                                );
                                return "worker Pod and Job disappeared (TTL-GC after \
                                        out-of-band termination)"
                                    .to_string();
                            }
                            Ok(Some(_)) | Err(_) => {
                                // Job still present (or a transient apiserver
                                // error) — fall through to the Job-status
                                // check, which decides terminal-ness.
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        task_run_id = %handle.task_run_id,
                        job = %job_name,
                        error = %e,
                        "kubernetes_runtime: infra-death watch — pod list failed (continuing)"
                    );
                }
            }

            // 2. Job-condition fallback (covers the case where the Pod object
            //    was already GC'd before we could read its container state).
            match jobs.get_opt(job_name).await {
                Ok(Some(job)) => {
                    if let Some(reason) = job_failed_reason(&job) {
                        debug!(
                            task_run_id = %handle.task_run_id,
                            job = %job_name,
                            %reason,
                            "kubernetes_runtime: infra-death watch — job failed"
                        );
                        return reason;
                    }
                    // Succeeded / still running: NOT a death. A success is
                    // delivered over the report stream; keep watching.
                }
                Ok(None) => {
                    // Job is gone. Only terminal if we had previously seen the
                    // Pod (the run actually started and then disappeared). A
                    // never-observed Job here is a pre-creation race — keep
                    // watching rather than declaring a phantom death.
                    if pod_seen {
                        debug!(
                            task_run_id = %handle.task_run_id,
                            job = %job_name,
                            "kubernetes_runtime: infra-death watch — job gone after pod observed"
                        );
                        return "worker Job disappeared after the Pod was observed \
                                (TTL-GC after out-of-band termination)"
                            .to_string();
                    }
                }
                Err(e) => {
                    warn!(
                        task_run_id = %handle.task_run_id,
                        job = %job_name,
                        error = %e,
                        "kubernetes_runtime: infra-death watch — job get failed (continuing)"
                    );
                }
            }

            tokio::time::sleep(INFRA_DEATH_POLL_INTERVAL).await;
        }
    }

    async fn capture_infra_death_log_tail(
        &self,
        handle: &RunHandle,
    ) -> Option<InfraDeathLogTailCapture> {
        crate::infra_death_log_tail::capture_infra_death_log_tail(
            &self.client,
            &self.config.namespace,
            &handle.task_run_id,
        )
        .await
    }
}

impl KubernetesRuntime {
    /// Drop a reserved pending-connection slot — used on `prepare` failure
    /// paths so we don't leak registry entries when Job / Secret creation
    /// errors out after the slot was reserved.  Best-effort; the caller
    /// logs the primary error.
    async fn drop_pending(&self, task_run_id: &str) {
        self.pending.lock().await.remove(task_run_id);
        self.registry.deregister(task_run_id).await;
    }
}

/// Terminal state discovered by [`poll_job_terminal_state`].
///
/// `Failed` carries the apiserver's condition message for future use (log
/// enrichment, richer reports in PR 4 pt2) — the PR 3 teardown path flattens
/// all non-timeout terminal states to `TaskRunOutcome::Interrupted`.
enum JobTerminal {
    Succeeded,
    Failed(#[allow(dead_code)] String),
    TimedOut,
}

/// Inspect a Pod for a *terminal worker-container failure* and, if present,
/// return a human-readable death reason. `None` means the Pod is still
/// running, succeeded cleanly, or hasn't terminated the worker container yet
/// — i.e. NOT a death the infra-watch should trip on.
///
/// Pure over the Pod object so the OOMKilled / non-zero-exit classification is
/// unit-testable without a live cluster. Reads the `worker` container's
/// `state.terminated` (falling back to the first container if the worker name
/// isn't matched, for forward-compat): an explicit `OOMKilled` reason or any
/// non-zero exit code is a death; a zero exit (clean success) is not — that
/// run's terminal report rides the stream and the runner prefers it.
fn pod_container_death_reason(pod: &Pod) -> Option<String> {
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    let worker = statuses
        .iter()
        .find(|c| c.name == "worker")
        .or_else(|| statuses.first())?;
    let terminated = worker.state.as_ref()?.terminated.as_ref()?;
    let exit_code = terminated.exit_code;
    let reason = terminated.reason.as_deref();
    if reason == Some("OOMKilled") {
        // Exit code for an OOM kill is conventionally 137 (128 + SIGKILL).
        return Some(format!("OOMKilled (exit {exit_code})"));
    }
    if exit_code != 0 {
        return Some(match reason {
            Some(r) => format!("{r} (exit {exit_code})"),
            None => format!("worker container exited with code {exit_code}"),
        });
    }
    None
}

/// Inspect a `Job` for a terminal `Failed` condition and return its reason.
/// `None` means the Job is still running or succeeded cleanly.
///
/// Pure over the Job object so the condition→reason mapping is unit-testable.
/// Prefers the `Failed` condition's `reason` (e.g. `BackoffLimitExceeded`,
/// `DeadlineExceeded`) over its free-text `message`; falls back to
/// `status.failed > 0` with a generic reason when no condition is populated.
fn job_failed_reason(job: &Job) -> Option<String> {
    let status = job.status.as_ref()?;
    let failed_condition = status.conditions.as_ref().and_then(|cs| {
        cs.iter()
            .find(|c| c.type_ == "Failed" && c.status == "True")
    });
    if let Some(c) = failed_condition {
        // `reason` is the machine-stable enum (BackoffLimitExceeded, …);
        // append the human message when it adds detail.
        return Some(match (c.reason.as_deref(), c.message.as_deref()) {
            (Some(r), Some(m)) if !m.is_empty() => format!("{r}: {m}"),
            (Some(r), _) => r.to_string(),
            (None, Some(m)) if !m.is_empty() => m.to_string(),
            _ => "job failed".to_string(),
        });
    }
    if status.failed.unwrap_or(0) > 0 {
        return Some("job failed".to_string());
    }
    None
}

/// Poll a `Job` until its `.status.succeeded` or `.status.failed` condition
/// is non-zero, or [`TEARDOWN_POLL_TIMEOUT`] elapses.
async fn poll_job_terminal_state(
    client: &kube::Client,
    namespace: &str,
    job_name: &str,
) -> JobTerminal {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let deadline = SystemClock::new().now_instant() + TEARDOWN_POLL_TIMEOUT;

    loop {
        match jobs.get(job_name).await {
            Ok(job) => {
                if let Some(status) = job.status.as_ref() {
                    if status.succeeded.unwrap_or(0) > 0 {
                        return JobTerminal::Succeeded;
                    }
                    if status.failed.unwrap_or(0) > 0 {
                        let reason = status
                            .conditions
                            .as_ref()
                            .and_then(|cs| cs.iter().find(|c| c.type_ == "Failed"))
                            .and_then(|c| c.message.clone())
                            .unwrap_or_else(|| "job failed".into());
                        return JobTerminal::Failed(reason);
                    }
                }
            }
            Err(kube::Error::Api(resp)) if resp.code == 404 => {
                // Job is gone — treat as already-torn-down success.
                return JobTerminal::Succeeded;
            }
            Err(e) => {
                warn!(
                    job = %job_name,
                    namespace = %namespace,
                    error = %e,
                    "kubernetes_runtime: poll_job_terminal_state get failed (continuing)"
                );
            }
        }

        if SystemClock::new().now_instant() >= deadline {
            return JobTerminal::TimedOut;
        }
        tokio::time::sleep(TEARDOWN_POLL_INTERVAL).await;
    }
}

/// Consume a [`PendingConnection`] and return a live [`BiStream`] wired to
/// its event channel + outbound control sender.
///
/// Extracted into a free function so unit tests can exercise the forwarder
/// / translator topology without constructing a full [`KubernetesRuntime`]
/// (which needs a real `kube::Client`).  The production [`SessionRuntime::
/// attach_stdio`] impl is a thin wrapper that pulls the `PendingConnection`
/// out of `self.pending` and defers to this helper.
///
/// Topology (see `attach_stdio` doc for the decision tree):
/// - Forwarder: `events_rx` (TCP → registry) → `BiStream::events_rx`
/// - Translator: `BiStream::requests_tx` → outbound `Frame`s down the
///   worker TCP connection.
/// - Both spawned tasks terminate when either end of their channel closes.
pub(crate) async fn bridge_pending_to_bistream(
    task_run_id: &str,
    pending: PendingConnection,
) -> Result<BiStream, RuntimeError> {
    // `into_parts` bypasses `PendingConnection::Drop`'s auto-deregister so
    // the registry slot stays alive for the rest of the run.  The caller
    // deregisters explicitly from `teardown`.
    let mut parts = pending.into_parts();

    // Wait for the worker to complete the handshake so the outbound sender is
    // live before we spawn the translator. D2: bound this wait — it used to be
    // unbounded, so a Pod that never dialled back (image-pull failure,
    // unschedulable, crash-loop) hung the host dispatch forever. The kube Job's
    // `activeDeadlineSeconds` only bounds the *container*, not this host-side
    // await. On timeout we surface a typed `HandshakeTimeout` the dispatch layer
    // turns into a teardown + breaker failover.
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, parts.wait_for_connection()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(RuntimeError::Attach(format!(
                "wait_for_connection for task_run_id={task_run_id}: {e}"
            )));
        }
        Err(_elapsed) => {
            return Err(RuntimeError::HandshakeTimeout(task_run_id.to_string()));
        }
    }

    let outbound_tx = parts.outbound_sender().await.ok_or_else(|| {
        RuntimeError::Attach(format!(
            "outbound sender unavailable after handshake for task_run_id={task_run_id} — \
             registry slot missing or already deregistered"
        ))
    })?;

    // Build the BiStream the caller reads/writes on.  `events_tx` and
    // `requests_rx` are kept on this side; the forwarder + translator
    // tasks own them for the rest of the run.
    let (bistream, bistream_events_tx, mut bistream_requests_rx) =
        BiStream::new_in_memory(PENDING_CONNECTION_BUFFER);

    // Forwarder: registry events → BiStream.events_rx.
    let forwarder_task_run_id = task_run_id.to_string();
    let mut events_rx = parts.events_rx;
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            if bistream_events_tx.send(event).await.is_err() {
                debug!(
                    task_run_id = %forwarder_task_run_id,
                    "attach_stdio forwarder: BiStream consumer dropped; terminating"
                );
                return;
            }
        }
        debug!(
            task_run_id = %forwarder_task_run_id,
            "attach_stdio forwarder: upstream events channel closed"
        );
    });

    // Translator: BiStream.requests_tx → outbound control frames.
    let translator_task_run_id = task_run_id.to_string();
    let outbound = outbound_tx;
    tokio::spawn(async move {
        while let Some(frame) = bistream_requests_rx.recv().await {
            match frame {
                StreamFrame::Cancel => {
                    let control = Frame {
                        correlation_id: 0,
                        payload: FramePayload::Control(ControlMsg::Cancel),
                    };
                    if outbound.send(control).await.is_err() {
                        debug!(
                            task_run_id = %translator_task_run_id,
                            "attach_stdio translator: outbound dropped during Cancel; terminating"
                        );
                        return;
                    }
                }
                StreamFrame::RpcResponse { correlation_id, .. } => {
                    // Worker-originated RPC (e.g. `mcp_tool_call`) isn't
                    // wired through the BiStream on the Kubernetes path
                    // yet — the TCP dispatch loop owns those correlation
                    // ids directly.  Log at debug so the gap is visible
                    // but the translator keeps running.
                    debug!(
                        task_run_id = %translator_task_run_id,
                        correlation_id,
                        "attach_stdio translator: RpcResponse frame ignored (not wired)"
                    );
                }
            }
        }
        debug!(
            task_run_id = %translator_task_run_id,
            "attach_stdio translator: downstream requests channel closed"
        );
    });

    Ok(bistream)
}

/// Canonical Kubernetes Job name for a task-run id.
pub fn taskrun_job_name(task_run_id: &str) -> String {
    format!("{}{task_run_id}", crate::job::TASKRUN_JOB_NAME_PREFIX)
}

/// Delete the canonical task-run Job with foreground propagation, treating 404
/// as success for idempotency. Public for server-side components that own a
/// kube client but do not hold a `KubernetesRuntime` instance.
pub async fn delete_taskrun_job_foreground(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
) -> Result<(), kube::Error> {
    let job_name = taskrun_job_name(task_run_id);
    delete_job_foreground(client, namespace, &job_name, 30).await
}

pub async fn terminate_taskrun_pod_exact(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
    pod_uid: &str,
) -> Result<(), String> {
    if task_run_id.trim().is_empty() || pod_uid.trim().is_empty() {
        return Err("exact-pod termination requires non-empty task-run ID and pod UID".into());
    }
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let Some(job) = jobs
        .get_opt(&taskrun_job_name(task_run_id))
        .await
        .map_err(|e| format!("get task-run Job: {e}"))?
    else {
        return Err("exact-pod watchdog termination task-run Job is unavailable".into());
    };
    if job.metadata.deletion_timestamp.is_some() {
        return Err("exact-pod watchdog termination task-run Job is not confirmable".into());
    }
    let job_uid = job
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| "task-run Job has no immutable UID".to_string())?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let selector = format!("{}={task_run_id}", crate::job::LABEL_TASK_RUN_ID);
    let listed_pods = pods
        .list(&ListParams::default().labels(&selector))
        .await
        .map_err(|e| format!("list task-run Pods: {e}"))?
        .items;

    // An empty list is an authorized retry only when Kubernetes durably
    // records that this exact Pod UID was previously verified. A live Job
    // without this marker may simply not have created its first Pod yet.
    if listed_pods.is_empty() {
        if job
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(EXACT_POD_TERMINATION_ANNOTATION))
            .map(String::as_str)
            != Some(pod_uid)
        {
            return Err("exact pod UID deletion is not confirmed by the task-run Job".into());
        }
        return delete_taskrun_job_orphaned(&jobs, task_run_id, job_uid).await;
    }

    // Reject the entire observation if any labelled Pod is not the recorded
    // immutable object. Finding the old Pod must not authorize teardown while
    // a replacement or foreign Pod is also present.
    if listed_pods.len() != 1 {
        return Err(
            "exact pod UID is unavailable or does not belong to the recorded task-run Job".into(),
        );
    }
    let pod_name = exact_taskrun_pod_name(&listed_pods[0], pod_uid, job_uid).ok_or_else(|| {
        "exact pod UID is unavailable or does not belong to the recorded task-run Job".to_string()
    })?;

    // The first destructive operation is fenced by the recorded Pod UID.
    let params = exact_pod_delete_params(pod_uid);
    match pods.delete(&pod_name, &params).await {
        Ok(_) => {}
        Err(kube::Error::Api(response)) if response.code == 404 => {}
        Err(e) => return Err(format!("delete exact task-run Pod: {e}")),
    }

    // Only a confirmed UID-fenced Pod DELETE (including an accepted 404 race)
    // authorizes an empty-list retry. Binding this subsequent confirmation
    // patch to the observed Job UID and resourceVersion prevents a same-name
    // replacement Job from inheriting authorization. In particular, a failed
    // Pod DELETE must never leave intent that a later retry mistakes for
    // confirmation.
    persist_exact_pod_termination_marker(&jobs, &job, pod_uid).await?;

    // Remove the controller after confirming the exact Pod deletion. Orphan
    // propagation is deliberate: unlike a cascade, it cannot delete a
    // different-UID Pod that appears between the list and this request. The
    // Job operation is independently fenced by the immutable Job UID.
    delete_taskrun_job_orphaned(&jobs, task_run_id, job_uid).await
}

const EXACT_POD_TERMINATION_ANNOTATION: &str = "djinn.dev/exact-pod-termination-uid";

async fn persist_exact_pod_termination_marker(
    jobs: &Api<Job>,
    job: &Job,
    pod_uid: &str,
) -> Result<(), String> {
    let job_name = job
        .metadata
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "task-run Job has no name".to_string())?;
    let job_uid = job
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| "task-run Job has no immutable UID".to_string())?;
    let resource_version = job
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| "task-run Job has no resource version".to_string())?;
    let patch = serde_json::json!({
        "metadata": {
            "uid": job_uid,
            "resourceVersion": resource_version,
            "annotations": { (EXACT_POD_TERMINATION_ANNOTATION): pod_uid }
        }
    });
    jobs.patch(job_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|e| format!("persist exact-Pod termination marker: {e}"))?;
    Ok(())
}

async fn delete_taskrun_job_orphaned(
    jobs: &Api<Job>,
    task_run_id: &str,
    job_uid: &str,
) -> Result<(), String> {
    let params = DeleteParams {
        propagation_policy: Some(kube::api::PropagationPolicy::Orphan),
        preconditions: Some(Preconditions {
            uid: Some(job_uid.to_owned()),
            ..Preconditions::default()
        }),
        ..DeleteParams::default()
    };
    jobs.delete(&taskrun_job_name(task_run_id), &params)
        .await
        .map_err(|e| format!("delete confirmed task-run Job: {e}"))?;
    Ok(())
}

/// Return a mutable Pod name only after binding it to both immutable resource
/// identities. The subsequent delete repeats the Pod UID binding as a
/// Kubernetes precondition, so the name alone never authorizes deletion.
fn exact_taskrun_pod_name(pod: &Pod, pod_uid: &str, job_uid: &str) -> Option<String> {
    (pod.metadata.uid.as_deref() == Some(pod_uid)
        && pod
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|owners| {
                owners
                    .iter()
                    .any(|owner| owner.kind == "Job" && owner.uid == job_uid)
            }))
    .then(|| pod.metadata.name.clone())
    .flatten()
    .filter(|name| !name.trim().is_empty())
}

/// Parameters for the sole destructive exact-Pod watchdog operation.
fn exact_pod_delete_params(pod_uid: &str) -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(30),
        preconditions: Some(Preconditions {
            uid: Some(pod_uid.to_owned()),
            ..Preconditions::default()
        }),
        ..DeleteParams::default()
    }
}

/// List task-run Jobs in a namespace and return primitive inventory rows.
pub async fn list_taskrun_jobs(
    client: &kube::Client,
    namespace: &str,
) -> Result<Vec<djinn_runtime::TaskrunJobRef>, kube::Error> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let mut refs = jobs
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter_map(|job| taskrun_job_ref_from_job(&job))
        .collect::<Vec<_>>();
    refs.sort_by(|a, b| a.job_name.cmp(&b.job_name));
    Ok(refs)
}

/// Delete a Job with `Foreground` propagation and the given grace period,
/// treating 404 as success for idempotency.
async fn delete_job_foreground(
    client: &kube::Client,
    namespace: &str,
    job_name: &str,
    grace_seconds: u32,
) -> Result<(), kube::Error> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let dp = DeleteParams::foreground().grace_period(grace_seconds);
    match jobs.delete(job_name, &dp).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(resp)) if resp.code == 404 => {
            debug!(
                job = %job_name,
                namespace = %namespace,
                "kubernetes_runtime: delete_job_foreground — already gone (404)"
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::models::TaskRunTrigger;
    use djinn_runtime::SupervisorFlow;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    fn owned_pod(name: &str, pod_uid: &str, owner_uid: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                uid: Some(pod_uid.into()),
                owner_references: Some(vec![OwnerReference {
                    api_version: "batch/v1".into(),
                    block_owner_deletion: Some(true),
                    controller: Some(true),
                    kind: "Job".into(),
                    name: "djinn-taskrun-run-1".into(),
                    uid: owner_uid.into(),
                }]),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        }
    }

    #[test]
    fn exact_pod_delete_is_fenced_to_the_requested_pod_uid() {
        let params = exact_pod_delete_params("pod-recorded");
        assert_eq!(params.grace_period_seconds, Some(30));
        assert_eq!(params.propagation_policy, None);
        assert_eq!(
            params
                .preconditions
                .and_then(|preconditions| preconditions.uid),
            Some("pod-recorded".into())
        );
    }

    #[test]
    fn exact_pod_selection_rejects_stale_and_wrong_owner_identities() {
        let recorded = owned_pod("taskrun-pod", "pod-recorded", "job-recorded");
        let replacement = owned_pod("taskrun-pod", "pod-replacement", "job-recorded");
        let foreign = owned_pod("foreign-pod", "pod-recorded", "job-foreign");

        assert_eq!(
            exact_taskrun_pod_name(&recorded, "pod-recorded", "job-recorded"),
            Some("taskrun-pod".into())
        );
        assert_eq!(
            exact_taskrun_pod_name(&replacement, "pod-recorded", "job-recorded"),
            None,
            "a stale UID cannot select a replacement with the same name"
        );
        assert_eq!(
            exact_taskrun_pod_name(&foreign, "pod-recorded", "job-recorded"),
            None,
            "a matching Pod UID owned by another Job is not a task-run Pod"
        );
    }

    #[derive(Clone)]
    struct ExactKubeReplies {
        jobs: Arc<StdMutex<Vec<(u16, serde_json::Value)>>>,
        pod_lists: Arc<StdMutex<Vec<(u16, serde_json::Value)>>>,
        patch_status: u16,
        pod_delete_status: u16,
        job_delete_status: u16,
    }

    /// Captured (method, path?query, body) tuples for every request the mocked
    /// kube service observed, in issue order.
    type CapturedKubeRequests = Arc<StdMutex<Vec<(String, String, String)>>>;

    fn exact_kube_client(replies: ExactKubeReplies) -> (kube::Client, CapturedKubeRequests) {
        use http::Response;
        use http_body_util::BodyExt;
        use kube::client::Body;
        use tower::service_fn;

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured = requests.clone();
        let client = kube::Client::new(
            service_fn(move |request: http::Request<Body>| {
                let replies = replies.clone();
                let captured = captured.clone();
                async move {
                    let method = request.method().to_string();
                    let path = request.uri().path().to_string();
                    // Split the URI string manually: the raw-SQL boundary guard
                    // rejects the bare sqlx-style call token outside djinn-db,
                    // which the direct Uri accessor for the query string trips.
                    let uri_text = request.uri().to_string();
                    let query = uri_text
                        .split_once('?')
                        .map(|(_, tail)| tail.to_string())
                        .unwrap_or_default();
                    let body = request
                        .into_body()
                        .collect()
                        .await
                        .expect("collect kube request")
                        .to_bytes();
                    captured.lock().unwrap().push((
                        method.clone(),
                        format!("{path}?{query}"),
                        String::from_utf8(body.to_vec()).expect("JSON request body"),
                    ));
                    let (status, value) = if method == "GET" && path.contains("/jobs/") {
                        replies.jobs.lock().unwrap().remove(0)
                    } else if method == "GET" && path.ends_with("/pods") {
                        replies.pod_lists.lock().unwrap().remove(0)
                    } else if method == "PATCH" && path.contains("/jobs/") {
                        let status = replies.patch_status;
                        if status < 400 {
                            (
                                status,
                                job_json(Some("job-recorded"), false, Some("pod-recorded")),
                            )
                        } else {
                            (status, api_error(status, "mock patch failed"))
                        }
                    } else if method == "DELETE" && path.contains("/pods/") {
                        let status = replies.pod_delete_status;
                        (status, delete_response(status))
                    } else if method == "DELETE" && path.contains("/jobs/") {
                        let status = replies.job_delete_status;
                        (status, delete_response(status))
                    } else {
                        panic!("unexpected kube request {method} {path}");
                    };
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Body::from(value.to_string().into_bytes()))
                            .unwrap(),
                    )
                }
            }),
            "djinn",
        );
        (client, requests)
    }

    fn api_error(code: u16, message: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion":"v1", "kind":"Status", "status":"Failure",
            "reason":"InternalError", "message":message, "code":code
        })
    }

    fn delete_response(status: u16) -> serde_json::Value {
        let failed = status >= 400;
        serde_json::json!({
            "apiVersion": "v1", "kind": "Status",
            "status": if failed { "Failure" } else { "Success" },
            "reason": if failed { "InternalError" } else { "" },
            "message": if failed { "mock delete failed" } else { "" },
            "code": status
        })
    }

    fn job_json(uid: Option<&str>, deleting: bool, marker: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {
                "name": "djinn-taskrun-run-1", "uid": uid, "resourceVersion": "7",
                "annotations": marker.map(|uid| serde_json::json!({(EXACT_POD_TERMINATION_ANNOTATION): uid})),
                "deletionTimestamp": deleting.then_some("2026-01-01T00:00:00Z")
            },
            "spec": {"template": {"spec": {"containers": [], "restartPolicy": "Never"}}}
        })
    }

    fn pod_list_json(pods: Vec<Pod>) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": pods
        })
    }

    fn exact_replies(
        jobs: Vec<(u16, serde_json::Value)>,
        pod_lists: Vec<(u16, serde_json::Value)>,
    ) -> ExactKubeReplies {
        ExactKubeReplies {
            jobs: Arc::new(StdMutex::new(jobs)),
            pod_lists: Arc::new(StdMutex::new(pod_lists)),
            patch_status: 200,
            pod_delete_status: 200,
            job_delete_status: 200,
        }
    }

    #[tokio::test]
    async fn exact_termination_deletes_then_persists_uid_for_empty_retry() {
        let first_job = job_json(Some("job-recorded"), false, None);
        let retry_job = job_json(Some("job-recorded"), false, Some("pod-recorded"));
        let mut replies = exact_replies(
            vec![(200, first_job), (200, retry_job)],
            vec![
                (
                    200,
                    pod_list_json(vec![owned_pod(
                        "taskrun-pod",
                        "pod-recorded",
                        "job-recorded",
                    )]),
                ),
                (200, pod_list_json(vec![])),
            ],
        );
        replies.job_delete_status = 200;
        let (client, requests) = exact_kube_client(replies);

        terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
            .await
            .expect("first exact termination");
        terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
            .await
            .expect("marker-authorized already-gone retry");

        let requests = requests.lock().unwrap();
        let mutation_order = requests
            .iter()
            .filter(|(method, _, _)| method == "DELETE" || method == "PATCH")
            .map(|(method, path, _)| {
                if path.contains("/pods/") {
                    format!("{method} pods")
                } else {
                    format!("{method} jobs")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            &mutation_order[..3],
            ["DELETE pods", "PATCH jobs", "DELETE jobs"],
            "confirmation must follow the Pod delete and precede Job teardown"
        );
        let patch = requests
            .iter()
            .find(|(method, _, _)| method == "PATCH")
            .unwrap();
        let patch_body: serde_json::Value = serde_json::from_str(&patch.2).unwrap();
        assert_eq!(patch_body["metadata"]["uid"], "job-recorded");
        assert_eq!(patch_body["metadata"]["resourceVersion"], "7");
        assert_eq!(
            patch_body["metadata"]["annotations"][EXACT_POD_TERMINATION_ANNOTATION],
            "pod-recorded"
        );

        let deletes = requests
            .iter()
            .filter(|(method, _, _)| method == "DELETE")
            .collect::<Vec<_>>();
        assert_eq!(deletes.len(), 3, "one Pod and two UID-fenced Job deletes");
        let pod_delete: serde_json::Value = serde_json::from_str(&deletes[0].2).unwrap();
        assert!(deletes[0].1.contains("/pods/taskrun-pod?"));
        assert_eq!(pod_delete["preconditions"]["uid"], "pod-recorded");
        for delete in &deletes[1..] {
            let body: serde_json::Value = serde_json::from_str(&delete.2).unwrap();
            assert!(delete.1.contains("/jobs/djinn-taskrun-run-1?"));
            assert_eq!(body["preconditions"]["uid"], "job-recorded");
            assert_eq!(body["propagationPolicy"], "Orphan");
        }
    }

    #[tokio::test]
    async fn exact_termination_rejects_every_unconfirmed_boundary_without_delete() {
        let cases = vec![
            ("absent Job", (404, api_error(404, "NotFound")), vec![]),
            ("get failure", (500, api_error(500, "get failed")), vec![]),
            (
                "deleting Job",
                (200, job_json(Some("job-recorded"), true, None)),
                vec![],
            ),
            (
                "unidentifiable Job",
                (200, job_json(None, false, None)),
                vec![],
            ),
            (
                "empty list without marker",
                (200, job_json(Some("job-recorded"), false, None)),
                vec![(200, pod_list_json(vec![]))],
            ),
            (
                "empty list with other marker",
                (
                    200,
                    job_json(Some("job-recorded"), false, Some("pod-other")),
                ),
                vec![(200, pod_list_json(vec![]))],
            ),
            (
                "replacement UID",
                (200, job_json(Some("job-recorded"), false, None)),
                vec![(
                    200,
                    pod_list_json(vec![owned_pod(
                        "taskrun-pod",
                        "pod-replacement",
                        "job-recorded",
                    )]),
                )],
            ),
            (
                "foreign owner",
                (200, job_json(Some("job-recorded"), false, None)),
                vec![(
                    200,
                    pod_list_json(vec![owned_pod(
                        "taskrun-pod",
                        "pod-recorded",
                        "job-foreign",
                    )]),
                )],
            ),
        ];
        for (name, job, lists) in cases {
            let (client, requests) = exact_kube_client(exact_replies(vec![job], lists));
            assert!(
                terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                    .await
                    .is_err(),
                "{name} must be unavailable/unconfirmed"
            );
            let requests = requests.lock().unwrap();
            assert!(
                requests.iter().all(|(method, _, _)| method != "DELETE"),
                "{name} issued a destructive call"
            );
            if name.contains("empty list") {
                assert!(
                    requests.iter().all(|(method, _, _)| method != "PATCH"),
                    "{name} attempted to create retry authorization"
                );
            }
        }
    }

    #[tokio::test]
    async fn exact_termination_propagates_list_patch_and_independent_delete_failures() {
        let exact_pods = || {
            vec![(
                200,
                pod_list_json(vec![owned_pod(
                    "taskrun-pod",
                    "pod-recorded",
                    "job-recorded",
                )]),
            )]
        };

        let (client, requests) = exact_kube_client(exact_replies(
            vec![(200, job_json(Some("job-recorded"), false, None))],
            vec![(500, api_error(500, "list failed"))],
        ));
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err()
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _, _)| method != "DELETE")
        );

        let mut replies = exact_replies(
            vec![(200, job_json(Some("job-recorded"), false, None))],
            exact_pods(),
        );
        replies.patch_status = 500;
        let (client, requests) = exact_kube_client(replies);
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err()
        );
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|(method, path, _)| method == "DELETE" && path.contains("/pods/"))
                    .count(),
                1,
                "confirmation PATCH failure happens only after the Pod DELETE"
            );
            assert!(
                requests
                    .iter()
                    .all(|(method, path, _)| method != "DELETE" || !path.contains("/jobs/")),
                "failed confirmation must prevent Job teardown"
            );
        }

        let mut replies = exact_replies(
            vec![(200, job_json(Some("job-recorded"), false, None))],
            exact_pods(),
        );
        replies.pod_delete_status = 500;
        let (client, requests) = exact_kube_client(replies);
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err()
        );
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|(method, _, _)| method == "DELETE")
                    .count(),
                1
            );
            assert!(
                requests
                    .iter()
                    .all(|(method, path, _)| method != "DELETE" || !path.contains("/jobs/"))
            );
        }

        let mut replies = exact_replies(
            vec![(200, job_json(Some("job-recorded"), false, None))],
            exact_pods(),
        );
        replies.job_delete_status = 500;
        let (client, requests) = exact_kube_client(replies);
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err(),
            "successful Pod DELETE plus failed Job DELETE must not claim success"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(method, _, _)| method == "DELETE")
                .count(),
            2
        );
        assert!(
            requests
                .iter()
                .any(|(method, path, _)| method == "DELETE" && path.contains("/pods/"))
        );
        assert!(
            requests
                .iter()
                .any(|(method, path, _)| method == "DELETE" && path.contains("/jobs/"))
        );
    }

    #[tokio::test]
    async fn failed_pod_delete_does_not_authorize_an_empty_list_retry() {
        let mut replies = exact_replies(
            vec![
                (200, job_json(Some("job-recorded"), false, None)),
                (200, job_json(Some("job-recorded"), false, None)),
            ],
            vec![
                (
                    200,
                    pod_list_json(vec![owned_pod(
                        "taskrun-pod",
                        "pod-recorded",
                        "job-recorded",
                    )]),
                ),
                (200, pod_list_json(vec![])),
            ],
        );
        replies.pod_delete_status = 500;
        let (client, requests) = exact_kube_client(replies);

        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err(),
            "failed exact Pod DELETE must be unavailable"
        );
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err(),
            "an empty list without confirmed deletion must remain unavailable"
        );

        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(method, path, _)| method == "DELETE" && path.contains("/pods/"))
                .count(),
            1
        );
        assert!(
            requests.iter().all(|(method, _, _)| method != "PATCH"),
            "failed Pod DELETE must not persist a confirmation marker"
        );
        assert!(
            requests
                .iter()
                .all(|(method, path, _)| method != "DELETE" || !path.contains("/jobs/")),
            "neither failed call may claim success by tearing down the Job"
        );
    }

    #[derive(Default)]
    struct FakeReadSourcePreparation {
        coords: HashMap<String, Result<Option<(String, String)>, String>>,
        materialization_error: Option<String>,
        events: Arc<StdMutex<Vec<String>>>,
        requests: StdMutex<Vec<djinn_workspace::ReadSourceRequest>>,
    }

    #[async_trait]
    impl ReadSourcePreparation for FakeReadSourcePreparation {
        async fn github_coords(
            &self,
            project_id: &str,
        ) -> Result<Option<(String, String)>, String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("lookup:{project_id}"));
            self.coords
                .get(project_id)
                .cloned()
                .unwrap_or_else(|| Ok(None))
        }

        async fn materialize(
            &self,
            request: djinn_workspace::ReadSourceRequest,
        ) -> Result<(), String> {
            self.events.lock().unwrap().push(format!(
                "materialize:{}:{}",
                request.owner_project_id, request.target_project_id
            ));
            self.requests.lock().unwrap().push(request);
            match &self.materialization_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    fn read_source_spec(targets: &[&str]) -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: "019f72b5-a92a-7501-8b41-b0ffe68cdda5".into(),
            task_attempt_id: None,
            task_id: "task-read-source".into(),
            project_id: "owner-project-id".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "task/read-source".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: targets.iter().map(|target| (*target).into()).collect(),
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    fn successful_preparation(targets: &[&str]) -> FakeReadSourcePreparation {
        let mut coords = HashMap::from([(
            "owner-project-id".into(),
            Ok(Some(("canonical-owner".into(), "canonical-repo".into()))),
        )]);
        for target in targets {
            coords.insert(
                (*target).into(),
                Ok(Some((
                    format!("target-owner-{target}"),
                    format!("target-repo-{target}"),
                ))),
            );
        }
        FakeReadSourcePreparation {
            coords,
            ..Default::default()
        }
    }

    async fn prepare_through_runtime(
        preparation: Arc<FakeReadSourcePreparation>,
        spec: &TaskRunSpec,
        seed_dispatch_image: bool,
    ) -> Result<RunHandle, RuntimeError> {
        use http::Response;
        use kube::client::Body;
        use tower::service_fn;

        let events = preparation.events.clone();
        let client = kube::Client::new(
            service_fn(move |request: http::Request<Body>| {
                let events = events.clone();
                async move {
                    let path = request.uri().path();
                    let (event, body) = if path.contains("/secrets") {
                        (
                            "POST:Secret",
                            serde_json::json!({
                                "apiVersion": "v1", "kind": "Secret",
                                "metadata": {"name": "task-secret"}
                            }),
                        )
                    } else {
                        (
                            "POST:Job",
                            serde_json::json!({
                                "apiVersion": "batch/v1", "kind": "Job",
                                "metadata": {"name": "task-job", "uid": "job-uid"}
                            }),
                        )
                    };
                    if request.method() == http::Method::POST {
                        events.lock().unwrap().push(event.into());
                    }
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string().into_bytes()))
                            .unwrap(),
                    )
                }
            }),
            "djinn",
        );
        let db = Database::open_in_memory().expect("in-memory runtime database");
        let runtime = KubernetesRuntime {
            client,
            config: KubernetesConfig::for_testing(),
            registry: Arc::new(ConnectionRegistry::new()),
            db: Some(db),
            read_source_preparation: Some(preparation),
            dispatch_image_override: seed_dispatch_image.then(|| "registry/test:test".into()),
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        runtime.prepare(spec, &ResolvedCredentials::default()).await
    }

    #[tokio::test]
    async fn pre_materialization_covers_zero_one_and_multiple_immutable_grants() {
        for targets in [&[][..], &["target-a"][..], &["target-a", "target-b"][..]] {
            let preparation = successful_preparation(targets);
            let spec = read_source_spec(targets);
            let sub_path = pre_materialize_read_sources_with(&preparation, &spec)
                .await
                .expect("authorized sources materialize");

            assert_eq!(
                sub_path.as_deref(),
                (!targets.is_empty())
                    .then_some("canonical-owner/canonical-repo/.task-runtime/read-sources")
            );
            let requests = preparation.requests.lock().unwrap();
            assert_eq!(requests.len(), targets.len());
            for (request, target) in requests.iter().zip(targets) {
                assert_eq!(request.owner_project_id, "owner-project-id");
                assert_eq!(request.target_project_id, *target);
                assert_eq!(
                    request.owner_root,
                    djinn_core::paths::project_dir("canonical-owner", "canonical-repo")
                );
                assert_eq!(
                    djinn_workspace::ReadSourceMaterializer::destination_for(
                        &request.owner_root,
                        &request.target_project_id
                    ),
                    request
                        .owner_root
                        .join(".task-runtime/read-sources")
                        .join(*target)
                );
                assert_ne!(request.owner_project_id, request.target_project_id);
            }
        }
    }

    #[tokio::test]
    async fn prepare_defers_before_secret_or_job_post_for_uncertain_and_unsafe_sources() {
        let cases = [
            ("database unavailable", true),
            ("cache unavailable", false),
            ("cache invalid", false),
            ("invalid disposable cache", false),
        ];
        for (error, lookup_failure) in cases {
            let mut preparation = successful_preparation(&["target-a"]);
            if lookup_failure {
                preparation
                    .coords
                    .insert("target-a".into(), Err(error.into()));
            } else {
                preparation.materialization_error = Some(error.into());
            }
            let preparation = Arc::new(preparation);
            let result = prepare_through_runtime(
                preparation.clone(),
                &read_source_spec(&["target-a"]),
                false,
            )
            .await;
            assert!(result.is_err(), "{error} must defer preparation");
            let events = preparation.events.lock().unwrap();
            assert!(
                !events.iter().any(|event| event.starts_with("POST:")),
                "resource POST after {error}: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn prepare_materializes_every_target_before_secret_and_job_posts() {
        let preparation = Arc::new(successful_preparation(&["target-a", "target-b"]));
        prepare_through_runtime(
            preparation.clone(),
            &read_source_spec(&["target-a", "target-b"]),
            true,
        )
        .await
        .expect("prepare succeeds");
        assert_eq!(
            *preparation.events.lock().unwrap(),
            vec![
                "lookup:owner-project-id",
                "lookup:target-a",
                "materialize:owner-project-id:target-a",
                "lookup:target-b",
                "materialize:owner-project-id:target-b",
                "POST:Secret",
                "POST:Job",
            ]
        );
    }

    /// Object-safety: `dyn SessionRuntime` must accept a reference to
    /// `KubernetesRuntime`. This is a compile-only check.
    #[allow(dead_code)]
    fn _obj_safe(_: &dyn SessionRuntime) {}

    #[test]
    fn kubernetes_runtime_is_object_safe() {
        // Compile-only: `dyn SessionRuntime` is constructible from
        // `&KubernetesRuntime`. A full constructor call requires a live
        // `kube::Client`, so we gate that work into PR 3's integration tests.
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn SessionRuntime>();
    }

    #[test]
    fn service_resolution_activity_payload_records_requested_injected_and_skipped() {
        let resolution = ImageServiceResolution {
            image: Some(crate::sidecar::ResolvedImageMetadata {
                id: "image-djinn".into(),
                name: "djinn".into(),
                tag: Some("registry.local/djinn:task".into()),
            }),
            requested_preset_ids: vec!["preset-postgres-18".into(), "preset-does-not-exist".into()],
            injected: vec![crate::sidecar::InjectedServiceMetadata {
                preset_id: "preset-postgres-18".into(),
                service_type: "postgres".into(),
                port: 5432,
                conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL".into(),
            }],
            skipped: vec![crate::sidecar::SkippedServicePreset {
                preset_id: "preset-does-not-exist".into(),
                reason: "unknown service preset".into(),
            }],
            lookup_error: Some("image_service_presets lookup failed".into()),
            services: Vec::new(),
        };

        let payload = service_resolution_activity_payload("run-123", "project-123", &resolution);

        assert_eq!(payload["task_run_id"], "run-123");
        assert_eq!(payload["project_id"], "project-123");
        assert_eq!(payload["image"]["id"], "image-djinn");
        assert_eq!(
            payload["requested"],
            serde_json::json!(["preset-postgres-18", "preset-does-not-exist"])
        );
        assert_eq!(payload["injected"][0]["preset_id"], "preset-postgres-18");
        assert_eq!(payload["injected"][0]["service_type"], "postgres");
        assert_eq!(payload["injected"][0]["port"], 5432);
        assert_eq!(payload["skipped"][0]["preset_id"], "preset-does-not-exist");
        assert_eq!(payload["skipped"][0]["reason"], "unknown service preset");
        assert_eq!(
            payload["errors"],
            serde_json::json!(["image_service_presets lookup failed"])
        );
    }

    // ── ld18 "kill actually kills" teardown coverage ─────────────────────
    //
    // The slot-pool / coordinator lifecycle code calls
    // `KubernetesRuntime::teardown_taskrun_job` (or the free function
    // `delete_taskrun_job_foreground`) to actually delete the canonical
    // `djinn-taskrun-{task_run_id}` Job when a session is killed. These
    // tests pin the two layers of idempotency that make the contract
    // safe:
    //
    // 1. The job name is the canonical prefix + task_run_id (no shadow
    //    names that a later redispatch would not match).
    // 2. A 404 from the kube apiserver is treated as success — a
    //    double-call (race between slot kill and the zombie backstop,
    //    or the slot event handler firing after `kill_session` already
    //    deleted the Job) must NOT bubble an error.

    #[test]
    fn taskrun_job_name_is_canonical_prefix_plus_task_run_id() {
        assert_eq!(taskrun_job_name("abc-123"), "djinn-taskrun-abc-123");
        // The exact UUID format used by the host coordinator
        // (Uuid::now_v7().to_string()).
        let uuid = Uuid::now_v7();
        assert_eq!(
            taskrun_job_name(&uuid.to_string()),
            format!("djinn-taskrun-{uuid}")
        );
        // A second invocation with the same id produces the same name
        // — the teardown/delete side of the bridge depends on this
        // for idempotency.
        assert_eq!(taskrun_job_name("x"), taskrun_job_name("x"));
    }

    /// Drive `delete_taskrun_job_foreground` with a mocked kube
    /// client that returns a 404 Status (the apiserver's response
    /// shape for a missing object). The 404 must be treated as
    /// success — this is the layered idempotency guarantee that makes
    /// every server-side interrupt path safe to call twice (e.g.
    /// `pool.kill_session` deletes the Job, then the
    /// `SlotEvent::Killed` handler in `handle_slot_event` runs and
    /// tries to delete the same Job; the second call sees a 404 and
    /// must return Ok(()) instead of bubbling an error).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_taskrun_job_foreground_treats_404_as_success() {
        use http::Response;
        use kube::client::Body;
        use tower_test::mock::{Handle, Mock};

        type Req = http::Request<Body>;
        type Resp = http::Response<Body>;

        let (mock_service, mut handle): (Mock<Req, Resp>, Handle<Req, Resp>) =
            tower_test::mock::pair();
        let client = kube::Client::new(mock_service, "djinn");

        // Spawn the server side: respond to the first request with a
        // 404 Status (the apiserver's standard "not found" body
        // shape), then close so the test can complete without
        // blocking on a second request the client never sends.
        let server = tokio::spawn(async move {
            // The client issues exactly one delete; respond with
            // 404 and then drop the handle to signal "no more
            // responses".
            let (req, send) = handle
                .next_request()
                .await
                .expect("apiserver should receive the Job delete request");
            // Sanity: the URL path includes the canonical job name.
            // The slot pool builds the same name from the
            // task_run_id via `taskrun_job_name`.
            assert!(
                req.uri().path().contains("djinn-taskrun-019e-missing"),
                "delete request should target the canonical job name; got {}",
                req.uri().path()
            );
            assert_eq!(req.method(), http::Method::DELETE, "must be a DELETE");
            let status_body = serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "metadata": {},
                "status": "Failure",
                "message": "jobs.batch \"djinn-taskrun-019e-missing\" not found",
                "reason": "NotFound",
                "code": 404,
            })
            .to_string();
            let response = Response::builder()
                .status(404)
                .header("content-type", "application/json")
                .body(Body::from(status_body.into_bytes()))
                .expect("build 404 response");
            send.send_response(response);
        });

        // 404 must be Ok(()) per the `delete_job_foreground` contract.
        let result = delete_taskrun_job_foreground(&client, "djinn", "019e-missing").await;
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("mock apiserver should complete within 2s")
            .expect("mock apiserver task should not panic");
        assert!(
            result.is_ok(),
            "delete_taskrun_job_foreground must treat 404 as success, got: {result:?}"
        );
    }

    /// A non-404 apiserver error (e.g. 500, 403) must still bubble
    /// through. The contract is "404 is success", NOT "all errors are
    /// success" — a real permission failure or apiserver outage must
    /// surface to the caller (the slot pool / coordinator then logs
    /// and continues, but the failure is observable for ops
    /// dashboards).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_taskrun_job_foreground_propagates_non_404_errors() {
        use http::Response;
        use kube::client::Body;
        use tower_test::mock::{Handle, Mock};

        type Req = http::Request<Body>;
        type Resp = http::Response<Body>;

        let (mock_service, mut handle): (Mock<Req, Resp>, Handle<Req, Resp>) =
            tower_test::mock::pair();
        let client = kube::Client::new(mock_service, "djinn");

        let server = tokio::spawn(async move {
            let (_req, send) = handle
                .next_request()
                .await
                .expect("apiserver should receive the Job delete request");
            let status_body = serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "metadata": {},
                "status": "Failure",
                "message": "forbidden: User cannot delete jobs.batch",
                "reason": "Forbidden",
                "code": 403,
            })
            .to_string();
            let response = Response::builder()
                .status(403)
                .header("content-type", "application/json")
                .body(Body::from(status_body.into_bytes()))
                .expect("build 403 response");
            send.send_response(response);
        });

        let result = delete_taskrun_job_foreground(&client, "djinn", "019e-forbidden").await;
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("mock apiserver should complete within 2s")
            .expect("mock apiserver task should not panic");
        assert!(
            result.is_err(),
            "delete_taskrun_job_foreground must propagate non-404 apiserver errors (got: {result:?})"
        );
    }

    /// The teardown poll must cover a full dev session: too short and the
    /// supervisor reaps in-flight worker pods (bug #18 regression). It must
    /// exceed the Job's `activeDeadlineSeconds` (default 10800s) plus the
    /// termination grace window so the host keeps polling until the kubelet's
    /// own deadline resolves a long-running Pod; allow up to ~3.5h headroom.
    #[test]
    fn teardown_timeout_is_bounded() {
        assert!(TEARDOWN_POLL_TIMEOUT >= Duration::from_secs(600));
        // Must outlast the 3h deadline backstop (+ grace) so a 3h-budget run
        // is not declared failed by the host while still legitimately running.
        assert!(TEARDOWN_POLL_TIMEOUT >= Duration::from_secs(10_800));
        assert!(TEARDOWN_POLL_TIMEOUT <= Duration::from_secs(12_600));
        assert!(TEARDOWN_POLL_INTERVAL < TEARDOWN_POLL_TIMEOUT);
    }

    /// D2: the startup-handshake deadline must be long enough to cover a cold
    /// node (image pull + Karpenter scale-up can take minutes) yet bounded so a
    /// Pod that never starts can't hang host dispatch indefinitely.
    #[test]
    fn handshake_timeout_is_bounded() {
        assert!(HANDSHAKE_TIMEOUT >= Duration::from_secs(120));
        assert!(HANDSHAKE_TIMEOUT <= Duration::from_secs(900));
    }

    /// D2: a worker that never dials back makes `bridge_pending_to_bistream`
    /// return `HandshakeTimeout` (not hang) once the deadline elapses. Uses
    /// paused time so the test is instant and deterministic.
    #[tokio::test(start_paused = true)]
    async fn bridge_times_out_when_worker_never_connects() {
        let registry = std::sync::Arc::new(ConnectionRegistry::new());
        let task_run_id = "tr-never-connects";
        // Reserve a pending connection but never `attach` (no worker dials).
        let pending = registry
            .register_pending(task_run_id, PENDING_CONNECTION_BUFFER)
            .await
            .expect("register pending");

        let handle = tokio::spawn(bridge_pending_to_bistream(task_run_id, pending));

        // Advance past the deadline; the unbounded wait now resolves to a timeout.
        tokio::time::advance(HANDSHAKE_TIMEOUT + Duration::from_secs(1)).await;

        match handle.await.expect("bridge task joins") {
            Err(RuntimeError::HandshakeTimeout(id)) => assert_eq!(id, task_run_id),
            Ok(_) => panic!("expected HandshakeTimeout, got Ok(BiStream)"),
            Err(other) => panic!("expected HandshakeTimeout, got {other:?}"),
        }
    }

    /// Smoke-check that our terminal-state enum covers the cases the caller
    /// relies on — purely a compile-time safeguard against future pruning.
    #[test]
    fn job_terminal_variants_are_exhaustive() {
        let variants = [
            JobTerminal::Succeeded,
            JobTerminal::Failed("x".into()),
            JobTerminal::TimedOut,
        ];
        for v in variants {
            match v {
                JobTerminal::Succeeded | JobTerminal::Failed(_) | JobTerminal::TimedOut => {}
            }
        }
    }

    // ── Infra-death decision logic (the predicate the watch trips on) ─────────

    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
    };

    fn pod_with_worker_terminated(
        exit_code: i32,
        reason: Option<&str>,
        container_name: &str,
    ) -> Pod {
        Pod {
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: container_name.to_string(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code,
                            reason: reason.map(str::to_string),
                            ..ContainerStateTerminated::default()
                        }),
                        ..ContainerState::default()
                    }),
                    image: String::new(),
                    image_id: String::new(),
                    ready: false,
                    restart_count: 0,
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
            ..Pod::default()
        }
    }

    /// An OOM-killed worker container (the production failure: memory-cgroup
    /// SIGKILL, exit 137, reason OOMKilled) is a death — and the reason string
    /// names OOM so operators see it in the session_error event.
    #[test]
    fn pod_oomkilled_is_a_death() {
        let pod = pod_with_worker_terminated(137, Some("OOMKilled"), "worker");
        let reason = pod_container_death_reason(&pod).expect("OOMKilled must be a death");
        assert!(
            reason.contains("OOMKilled"),
            "reason should name OOM: {reason}"
        );
        assert!(
            reason.contains("137"),
            "reason should carry the exit code: {reason}"
        );
    }

    /// Any non-zero exit (crash, SIGKILL past grace, generic Error) is a death.
    #[test]
    fn pod_nonzero_exit_is_a_death() {
        let pod = pod_with_worker_terminated(1, Some("Error"), "worker");
        let reason = pod_container_death_reason(&pod).expect("non-zero exit must be a death");
        assert!(reason.contains("Error"));
        assert!(
            reason.contains("exit 1"),
            "reason should carry exit code: {reason}"
        );
    }

    /// A clean exit (code 0) is NOT a death — that run's terminal report rides
    /// the stream and the runner prefers it; tripping the watch here would race
    /// the real outcome and spuriously mark a healthy run interrupted.
    #[test]
    fn pod_clean_exit_is_not_a_death() {
        let pod = pod_with_worker_terminated(0, Some("Completed"), "worker");
        assert!(pod_container_death_reason(&pod).is_none());
    }

    /// A still-running Pod (no terminated state) is not a death — the predicate
    /// must keep waiting, never declare a phantom death on a live worker.
    #[test]
    fn pod_still_running_is_not_a_death() {
        let pod = Pod {
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "worker".to_string(),
                    state: Some(ContainerState {
                        running: Some(k8s_openapi::api::core::v1::ContainerStateRunning::default()),
                        ..ContainerState::default()
                    }),
                    image: String::new(),
                    image_id: String::new(),
                    ready: true,
                    restart_count: 0,
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
            ..Pod::default()
        };
        assert!(pod_container_death_reason(&pod).is_none());
    }

    /// A Pod with no status at all (just scheduled, not yet started) is not a
    /// death.
    #[test]
    fn pod_no_status_is_not_a_death() {
        assert!(pod_container_death_reason(&Pod::default()).is_none());
    }

    /// When the worker container can't be matched by name (forward-compat for a
    /// renamed container), fall back to the first container's terminated state.
    #[test]
    fn pod_falls_back_to_first_container_when_worker_name_absent() {
        let pod = pod_with_worker_terminated(137, Some("OOMKilled"), "main");
        let reason = pod_container_death_reason(&pod).expect("fallback to first container");
        assert!(reason.contains("OOMKilled"));
    }

    fn job_with_failed_condition(reason: Option<&str>, message: Option<&str>) -> Job {
        Job {
            status: Some(JobStatus {
                failed: Some(1),
                conditions: Some(vec![JobCondition {
                    type_: "Failed".to_string(),
                    status: "True".to_string(),
                    reason: reason.map(str::to_string),
                    message: message.map(str::to_string),
                    ..JobCondition::default()
                }]),
                ..JobStatus::default()
            }),
            ..Job::default()
        }
    }

    /// `backoffLimit: 0` ⇒ a single Pod failure trips a `Failed` condition with
    /// reason `BackoffLimitExceeded`; the death reason must surface it (this is
    /// the production Job-level signal when the Pod object was already GC'd).
    #[test]
    fn job_backoff_limit_exceeded_is_a_failure() {
        let job = job_with_failed_condition(
            Some("BackoffLimitExceeded"),
            Some("Job has reached the specified backoff limit"),
        );
        let reason = job_failed_reason(&job).expect("failed job must be a failure");
        assert!(reason.contains("BackoffLimitExceeded"), "reason: {reason}");
    }

    /// A `Failed` condition whose `status` is not `True` is NOT a failure — only
    /// an asserted failure trips the predicate.
    #[test]
    fn job_failed_condition_false_status_is_not_a_failure() {
        let job = Job {
            status: Some(JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: "Failed".to_string(),
                    status: "False".to_string(),
                    ..JobCondition::default()
                }]),
                ..JobStatus::default()
            }),
            ..Job::default()
        };
        assert!(job_failed_reason(&job).is_none());
    }

    /// A succeeded Job (no Failed condition, failed count 0) is NOT a failure —
    /// success is delivered over the report stream, never via the death watch.
    #[test]
    fn job_succeeded_is_not_a_failure() {
        let job = Job {
            status: Some(JobStatus {
                succeeded: Some(1),
                ..JobStatus::default()
            }),
            ..Job::default()
        };
        assert!(job_failed_reason(&job).is_none());
    }

    /// `status.failed > 0` with no populated condition still counts as a failure
    /// (generic fallback) so an unusual apiserver shape can't hide a dead Job.
    #[test]
    fn job_failed_count_without_condition_is_a_failure() {
        let job = Job {
            status: Some(JobStatus {
                failed: Some(1),
                ..JobStatus::default()
            }),
            ..Job::default()
        };
        assert!(job_failed_reason(&job).is_some());
    }

    /// A Job with no status (just created) is not a failure — never declare a
    /// phantom death before the run even starts.
    #[test]
    fn job_no_status_is_not_a_failure() {
        assert!(job_failed_reason(&Job::default()).is_none());
    }

    /// Builder-parity invariant: the Secret built by `build_task_run_secret`
    /// and the Job built by `build_task_run_job` share the resource name
    /// that `prepare` threads between them. This is the load-bearing
    /// coupling `prepare` relies on — assert it so a future refactor of
    /// either builder can't silently break the Job↔Secret link without
    /// failing here first.
    ///
    /// This test does NOT exercise `prepare` itself (that requires a live
    /// cluster — see `tests/kind_smoke.rs` gated by `DJINN_TEST_KIND=1`).
    #[test]
    fn prepare_builds_expected_job_and_secret_via_builders() {
        use std::collections::HashMap;

        use djinn_core::models::TaskRunTrigger;
        use djinn_runtime::{ResolvedCredentials, SupervisorFlow, TaskRunSpec};

        use crate::secret::task_run_resource_name;

        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();
        let resource_name = task_run_resource_name(&task_run_id);

        let spec = TaskRunSpec {
            task_run_id: task_run_id.to_string(),
            task_attempt_id: None,
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: Vec::new(),
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };
        let credentials = ResolvedCredentials::default();

        let secret =
            crate::secret::build_task_run_secret(&cfg.namespace, &task_run_id, &spec, &credentials)
                .expect("build per-task-run Secret");
        let job = crate::job::build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            &resource_name,
            "reg.test:5000/djinn-project-proj-xyz:deadbeefcafe",
            &[],
            None,
            false,
            Some(djinn_runtime::RoleKind::Worker),
        );

        // The Secret and Job share the same resource name.
        assert_eq!(
            secret.metadata.name.as_deref(),
            Some(resource_name.as_str()),
            "Secret name must equal task_run_resource_name(task_run_id)"
        );
        assert_eq!(
            job.metadata.name.as_deref(),
            Some(resource_name.as_str()),
            "Job name must equal task_run_resource_name(task_run_id)"
        );

        // Both live in the same namespace.
        assert_eq!(
            secret.metadata.namespace.as_deref(),
            Some(cfg.namespace.as_str())
        );
        assert_eq!(
            job.metadata.namespace.as_deref(),
            Some(cfg.namespace.as_str())
        );

        // The Job's spec volume references the Secret by the name we just
        // asserted is shared. This is the handshake `prepare` depends on.
        let pod_spec = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .expect("job.spec.template.spec present");
        let spec_volume = pod_spec
            .volumes
            .as_ref()
            .and_then(|vs| vs.iter().find(|v| v.name == "spec"))
            .expect("spec volume present");
        let secret_src = spec_volume
            .secret
            .as_ref()
            .expect("spec volume must be backed by a Secret");
        assert_eq!(
            secret_src.secret_name.as_deref(),
            Some(resource_name.as_str()),
            "spec volume must reference the per-task-run Secret by name"
        );
    }

    /// Drive the forwarder + translator topology that
    /// [`SessionRuntime::attach_stdio`] spawns — without a live
    /// `kube::Client`.  Reserves a `PendingConnection` on an in-memory
    /// `ConnectionRegistry`, simulates the `serve_on_tcp` handshake by
    /// populating the outbound sender via `attach`, hands the pending
    /// connection to [`bridge_pending_to_bistream`], and asserts:
    ///
    /// 1. `StreamEvent`s delivered on the registry's inbound event channel
    ///    surface on the returned `BiStream::events_rx`.
    /// 2. A `StreamFrame::Cancel` written into `BiStream::requests_tx`
    ///    lands as a `FramePayload::Control(ControlMsg::Cancel)` on the
    ///    outbound sender the registry published.
    ///
    /// This is the minimum guarantee `cancel()` and the supervisor
    /// runner's event-drain loop rely on.
    #[tokio::test]
    async fn attach_stdio_forwards_events_and_translates_cancel() {
        use djinn_runtime::spec::TaskRunOutcome;
        use djinn_runtime::{RoleKind, StreamEvent, StreamFrame, TaskRunReport};
        use djinn_supervisor::{ConnectionRegistry, FramePayload};
        use djinn_supervisor::{Frame as SupFrame, services::server::serve_on_tcp};
        use std::net::SocketAddr;

        // We need the accept loop to publish the outbound sender into the
        // registry, so we spin up a real `serve_on_tcp` + dial handshake
        // just like the supervisor test.  `FakeServices` from the
        // supervisor test isn't exported, so we roll a minimal one inline.
        use async_trait::async_trait;
        use djinn_core::models::Task;
        use djinn_supervisor::{
            AllowAllValidator, AuthHelloMsg, AuthResultMsg, RoleKind as SupRoleKind, StageError,
            StageOutcome, SupervisorServices, TaskRunOutcome as SupTaskRunOutcome, TaskRunSpec,
        };
        use djinn_workspace::Workspace;
        use tokio::net::TcpStream;
        use tokio_util::sync::CancellationToken;

        struct NoopServices {
            cancel: CancellationToken,
        }
        #[async_trait]
        impl SupervisorServices for NoopServices {
            fn cancel(&self) -> &CancellationToken {
                &self.cancel
            }
            async fn load_task(&self, _: String) -> Result<Task, String> {
                Err("not used".into())
            }
            async fn execute_stage(
                &self,
                _: &Task,
                _: &Workspace,
                _: SupRoleKind,
                _: &str,
                _: &TaskRunSpec,
            ) -> Result<StageOutcome, StageError> {
                unimplemented!()
            }
            async fn open_pr(&self, _: &TaskRunSpec, _: &Task) -> SupTaskRunOutcome {
                unimplemented!()
            }
            async fn create_task_run(
                &self,
                _: djinn_supervisor::SerializableCreateTaskRunParams,
            ) -> Result<(), String> {
                unimplemented!()
            }
            async fn update_task_run_status(
                &self,
                _: String,
                _: djinn_core::models::TaskRunStatus,
            ) -> Result<(), String> {
                unimplemented!()
            }
            async fn get_model_context_window(&self, _: String) -> Result<i64, String> {
                unimplemented!()
            }
            async fn get_provider_base_url(&self, _: String) -> Result<String, String> {
                unimplemented!()
            }
            async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
                unimplemented!()
            }
            async fn create_session(
                &self,
                _: djinn_supervisor::services::SerializableCreateSessionParams,
            ) -> Result<djinn_core::models::SessionRecord, String> {
                unimplemented!()
            }
            async fn publish_session_message(
                &self,
                _: String,
                _: String,
                _: String,
                _: serde_json::Value,
            ) -> Result<(), String> {
                unimplemented!()
            }
            async fn get_environment_config(
                &self,
                _: String,
            ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
                unimplemented!()
            }
            async fn invoke_llm(
                &self,
                _: String,
                _: djinn_provider::message::Conversation,
                _: Vec<serde_json::Value>,
                _: Option<djinn_provider::provider::ToolChoice>,
            ) -> Result<djinn_provider::provider::LlmResponse, String> {
                unimplemented!()
            }
            async fn update_session_status(
                &self,
                _: String,
                _: djinn_core::models::SessionStatus,
                _: i64,
                _: i64,
                _: i64,
                _: i64,
                _: Option<String>,
            ) -> Result<(), String> {
                unimplemented!()
            }
            async fn emit_djinn_event(
                &self,
                _: djinn_supervisor::services::SerializableDjinnEvent,
            ) -> Result<(), String> {
                unimplemented!()
            }
            async fn tool_github_search(
                &self,
                _: Option<String>,
                _: serde_json::Map<String, serde_json::Value>,
            ) -> Result<serde_json::Value, String> {
                unimplemented!()
            }
            async fn tool_github_fetch_file(
                &self,
                _: Option<String>,
                _: serde_json::Map<String, serde_json::Value>,
            ) -> Result<serde_json::Value, String> {
                unimplemented!()
            }
            async fn tool_ci_job_log(
                &self,
                _: Option<String>,
                _: serde_json::Map<String, serde_json::Value>,
            ) -> Result<serde_json::Value, String> {
                unimplemented!()
            }
            async fn touch_activity(&self, _: String) -> Result<(), String> {
                unimplemented!()
            }
            async fn transition_task(
                &self,
                _: String,
                _: String,
                _: Option<String>,
            ) -> Result<(), String> {
                unimplemented!()
            }
            async fn record_arbiter_decision(
                &self,
                _task_id: String,
                _decision: String,
                _evidence_json: String,
            ) -> Result<(), String> {
                unimplemented!()
            }

            async fn start_monitored_reopen(
                &self,
                _task_id: String,
                _directive: String,
                _verification_command: String,
                _exclude_models: Vec<String>,
            ) -> Result<(), String> {
                unimplemented!()
            }

            async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
                unimplemented!()
            }

            async fn record_arbiter_session_termination(
                &self,
                _task_id: String,
                _is_infra_failure: bool,
            ) -> Result<bool, String> {
                unimplemented!()
            }
        }

        let services: Arc<dyn SupervisorServices> = Arc::new(NoopServices {
            cancel: CancellationToken::new(),
        });
        let validator = Arc::new(AllowAllValidator);
        let registry = Arc::new(ConnectionRegistry::new());
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = serve_on_tcp(addr, services, validator, Some(registry.clone()))
            .await
            .expect("bind tcp");
        let bound = server.bound_addr.expect("bound addr");

        let task_run_id = "attach-test-run".to_string();
        let pending = registry
            .register_pending(task_run_id.clone(), 8)
            .await
            .expect("register_pending");

        // Dial + handshake so the registry publishes the outbound sender.
        let mut stream = TcpStream::connect(bound).await.expect("connect");
        let hello = SupFrame {
            correlation_id: 1,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                task_run_id: task_run_id.clone(),
                token: "any".into(),
            }),
        };
        djinn_runtime::wire::write_frame(&mut stream, &hello)
            .await
            .expect("write hello");
        let reply: SupFrame = djinn_runtime::wire::read_frame(&mut stream)
            .await
            .expect("read ack");
        match reply.payload {
            FramePayload::AuthResult(AuthResultMsg { accepted: true, .. }) => {}
            other => panic!("unexpected: {other:?}"),
        }

        // Hand the pending connection to the bridge.  This is the exact
        // call `KubernetesRuntime::attach_stdio` makes post-dequeue.
        let mut bistream = bridge_pending_to_bistream(&task_run_id, pending)
            .await
            .expect("bridge_pending_to_bistream");

        // Worker emits a terminal report → should surface on BiStream.
        let report = TaskRunReport {
            task_run_id: task_run_id.clone(),
            outcome: TaskRunOutcome::Closed {
                reason: "bridge-test".into(),
            },
            stages_completed: vec![RoleKind::Planner],
        };
        let event_frame = SupFrame {
            correlation_id: 0,
            payload: FramePayload::Event(djinn_runtime::wire::WorkerEvent::TerminalReport(
                report.clone(),
            )),
        };
        djinn_runtime::wire::write_frame(&mut stream, &event_frame)
            .await
            .expect("write event");

        let got = tokio::time::timeout(Duration::from_secs(2), bistream.events_rx.recv())
            .await
            .expect("BiStream event within 2s")
            .expect("BiStream events channel open");
        match got {
            StreamEvent::Report(r) => assert_eq!(r.task_run_id, task_run_id),
            other => panic!("expected StreamEvent::Report, got {other:?}"),
        }

        // Consumer pushes `Cancel` on BiStream.requests_tx → translator
        // writes a `FramePayload::Control(ControlMsg::Cancel)` back on
        // the TCP connection (reads from the worker's POV).
        bistream
            .requests_tx
            .send(StreamFrame::Cancel)
            .await
            .expect("send Cancel on BiStream");

        let cancel_frame: SupFrame = tokio::time::timeout(
            Duration::from_secs(2),
            djinn_runtime::wire::read_frame(&mut stream),
        )
        .await
        .expect("inbound cancel frame within 2s")
        .expect("read cancel frame");
        match cancel_frame.payload {
            FramePayload::Control(ControlMsg::Cancel) => {}
            other => panic!("expected Control(Cancel), got {other:?}"),
        }

        // Teardown.  Drop the BiStream + TCP stream first so the server's
        // per-connection task observes a clean EOF; then cancel the accept
        // loop and join.  `drop(stream)` is implicit at scope end but the
        // server writer races against it — cancelling the server token is
        // what actually tears the writer down.
        drop(bistream);
        drop(stream);
        server.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), server.join).await;
    }
}

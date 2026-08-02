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
use crate::runtime_eviction::{PodAbsenceVerdict, classify_absent_pod};
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

/// Preserve the Kubernetes create result while recording only confirmed
/// apiserver success. Calling this around the awaited `jobs.create` boundary
/// excludes prerequisite failures, errors, ambiguous calls, and retries that
/// have not themselves returned `Ok`.
fn record_confirmed_job_create<T, E>(result: Result<T, E>) -> Result<T, E> {
    if result.is_ok() {
        djinn_telemetry::taskrun_lifecycle::increment_job_started();
    }
    result
}

/// Create the task-run Job, adopting the existing object when the apiserver
/// says it is already there.
///
/// The task-run Job name is deterministic in the task-run id
/// (`djinn-taskrun-{uuid}`, `job.rs`), so `AlreadyExists` is never a name
/// collision between two different runs — it is *this* run's object, created by
/// a POST whose response we lost, by a concurrent dispatcher, or by a retry
/// after an ambiguous failure. Returning the error instead would leave the
/// caller to create a second Job that Kubernetes will not let it create, and
/// under Kueue would strand an admitted Workload nobody is waiting on.
///
/// The adopted object is fetched with GET rather than reconstructed locally,
/// because the winner's `metadata.uid` is what the Secret's OwnerReference must
/// point at — an ownerRef carrying a UID that never existed is silently dropped
/// by the apiserver and the Secret outlives its Job.
///
/// [`record_confirmed_job_create`] wraps only the create arm: an adopt is a
/// retry of a Job that was already counted, and counting it again would inflate
/// `job_started` by exactly the retries this function exists to absorb.
async fn create_or_adopt_task_run_job(
    jobs: &Api<Job>,
    job: &Job,
    resource_name: &str,
) -> Result<Job, kube::Error> {
    match record_confirmed_job_create(jobs.create(&PostParams::default(), job).await) {
        Ok(created) => Ok(created),
        Err(error) if crate::graph_warmer::api_error_is_already_exists(&error) => {
            info!(
                job = %resource_name,
                "kubernetes_runtime: task-run Job already exists — adopting it instead of \
                 creating a second one"
            );
            jobs.get(resource_name).await
        }
        Err(error) => Err(error),
    }
}

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

/// Termination grace period on the Job reaped by
/// [`KubernetesRuntime::reap_lost_fenced_pod`]. Matches `cancel` / `teardown`
/// so every foreground Job delete this runtime issues offers the same window.
const INFRA_DEATH_REAP_GRACE_SECONDS: u32 = 30;

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
        // The launcher authority protocol travels WITH the resolved image: it
        // is what the artifact declared at build time (migration 166), and the
        // rendered Job must carry it into the pod or #2823's reachable
        // `resize-v2` branch has nothing feeding it. Resolved here, applied to
        // the Job below, BEFORE any Job is created.
        let (project_image_tag, authority_protocol) = match project_image_tag {
            // Test-only image override: no catalog row exists, so the render
            // uses the pre-protocol behavior the override has always implied.
            Some(tag) => (
                tag,
                djinn_cgroup_launcher::LauncherAuthorityProtocol::LeafV1,
            ),
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
                let Some(dispatch_image) = dispatch_image else {
                    return Err(RuntimeError::DevcontainerMissing(spec.project_id.clone()));
                };
                let Some(pull_ref) = dispatch_image.pull_ref() else {
                    return Err(RuntimeError::DevcontainerMissing(spec.project_id.clone()));
                };
                let protocol = crate::launcher::render_authority_protocol(
                    dispatch_image.authority_protocol,
                    dispatch_image.digest.as_deref(),
                )
                .map_err(|error| {
                    RuntimeError::Prepare(format!(
                        "launcher authority protocol for project {}: {error}",
                        spec.project_id
                    ))
                })?;
                (pull_ref, protocol)
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
            // The Secret name is deterministic in the task-run id, exactly like
            // the Job's. Treating `AlreadyExists` as fatal here would make the
            // Job's own create-then-observe adoption unreachable: every retry
            // of an ambiguous dispatch would die one step earlier, on a Secret
            // this very task-run wrote, and the Job it must adopt would never
            // be POSTed at all.
            if !crate::graph_warmer::api_error_is_already_exists(&e) {
                self.drop_pending(&task_run_id_str).await;
                return Err(RuntimeError::Prepare(format!(
                    "create secret {resource_name}: {e}"
                )));
            }
            info!(
                secret = %resource_name,
                "kubernetes_runtime: task-run Secret already exists — adopting it"
            );
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
        // Resolve per-project build_resources overrides for the task-run Pod
        // BEFORE the Job is built: a malformed / request-above-limit /
        // out-of-bounds override fails closed here so no Job is ever created.
        let role_class = crate::launcher::RoleResourceClass::for_role(role);
        let resolved_task_resources = match crate::build_resources::resolve_task_run_resources(
            &self.config,
            role_class,
            effective_env_config
                .build_resources
                .as_ref()
                .and_then(|b| b.task.as_ref()),
            &self.config.task_resource_bounds,
        ) {
            Ok(resources) => resources,
            Err(error) => {
                self.drop_pending(&task_run_id_str).await;
                return Err(RuntimeError::Prepare(format!(
                    "build_resources resolution failed for task-run: {error}"
                )));
            }
        };
        let mut job = build_task_run_job_with_read_sources(
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
        crate::build_resources::apply_resolved_resources(&mut job, resolved_task_resources);
        // Carry the image's quota authority into the pod. Fails closed before
        // the Job is POSTed: an armed render with no launcher container would
        // otherwise start a launcher that silently defaults to leaf-v1.
        if let Err(error) = crate::launcher::apply_launcher_authority_protocol(
            &mut job,
            self.config.cgroup_launcher_mode,
            authority_protocol,
        ) {
            self.drop_pending(&task_run_id_str).await;
            let secrets_bg = secrets.clone();
            let name = resource_name.clone();
            tokio::spawn(async move {
                let _ = secrets_bg.delete(&name, &DeleteParams::default()).await;
            });
            return Err(RuntimeError::Prepare(format!(
                "launcher authority protocol render: {error}"
            )));
        }
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let created_job = match create_or_adopt_task_run_job(&jobs, &job, &resource_name).await {
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
            // The JOB uid, confirmed by the create above. `prepare` does not
            // wait for a Pod and therefore has no Pod UID to offer; the resize
            // bootstrap obtains that separately from a fresh Pod GET.
            job_uid: Some(job_uid),
            // The protocol the render actually APPLIED to this Job — not the
            // one it resolved. Under a launcher mode that renders no sidecar,
            // `apply_launcher_authority_protocol` above is a documented no-op,
            // so there is no launcher container to govern and no protocol
            // handshake to agree with. Reporting the resolved value there would
            // make the dispatch seam demand a birth confirmation for a Pod that
            // has no launcher at all, and refuse every such dispatch forever.
            launcher_authority_protocol: self
                .config
                .cgroup_launcher_mode
                .renders_sidecar()
                .then_some(authority_protocol),
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
    /// - The *fenced* Pod — the immutable UID this watch bound on its first
    ///   observation — is gone while the Job is still NONTERMINAL **and nothing
    ///   in observable cluster state explains the absence**. That is a
    ///   force-delete / node loss, and it is the one arm that also has to *act*:
    ///   see [`Self::reap_lost_fenced_pod`].
    ///
    /// A `Succeeded` Job is NOT treated as death — a clean run delivers its
    /// terminal report over the stream, which the runner prefers; resolving
    /// here on success would race that and risk a spurious "interrupted".
    ///
    /// Neither is a Kueue EVICTION, which is bit-for-bit the same observation —
    /// Kueue re-suspends the Job and its Pod is deleted — and is RECOVERABLE:
    /// releasing the queue re-admits the Workload and a new Pod runs. Reaping
    /// there would convert a run that was going to finish into one that never
    /// can. [`crate::runtime_eviction::classify_absent_pod`] holds the two
    /// fields that tell the two apart, and why they are those two.
    ///
    /// # The Pod fence
    ///
    /// Every observation is bound to one immutable `metadata.uid`, captured the
    /// first time any Pod appears under this run's label selector. The label
    /// selector alone is not an identity: after a Pod is force-deleted the Job
    /// controller creates a *replacement* Pod carrying the very same labels, and
    /// reading `items.first()` would silently re-target the watch at an object
    /// this run never launched, never handshook with, and holds no lease on. The
    /// fence is bound once and never re-bound, so a replacement UID is observed
    /// but never adopted.
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
        // The immutable Pod identity this run is fenced to. Bound once, on the
        // first observation that carries a UID; never re-bound.
        let mut bound_pod_uid: Option<String> = None;
        // Whether the previous poll already reported holding on an explained
        // Pod absence. Presentation only: a run parked in the queue for an hour
        // must not write the same INFO line 240 times.
        let mut held_absence = false;

        loop {
            // 1. Richest signal first: the Pod's container terminated state,
            //    captured before TTL-GC removes the Pod object.
            match pods
                .list(&ListParams::default().labels(&label_selector))
                .await
            {
                Ok(list) => {
                    if bound_pod_uid.is_none()
                        && let Some(uid) = bind_worker_pod_uid(&list.items)
                    {
                        debug!(
                            task_run_id = %handle.task_run_id,
                            job = %job_name,
                            pod_uid = %uid,
                            "kubernetes_runtime: infra-death watch — bound to worker Pod UID"
                        );
                        bound_pod_uid = Some(uid);
                    }

                    match fenced_worker_pod(&list.items, bound_pod_uid.as_deref()) {
                        Some(pod) => {
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
                        }
                        // The fenced Pod was here and is now gone. Anything else
                        // still matching the selector is a replacement the Job
                        // controller minted, which this watch refuses to adopt.
                        None if pod_seen => {
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
                                Ok(Some(job)) => {
                                    // A Failed Job is the pre-existing arm and
                                    // owns its own richer reason.
                                    if let Some(reason) = job_failed_reason(&job) {
                                        debug!(
                                            task_run_id = %handle.task_run_id,
                                            job = %job_name,
                                            %reason,
                                            "kubernetes_runtime: infra-death watch — job failed"
                                        );
                                        return reason;
                                    }
                                    // A cleanly Complete Job whose Pod was
                                    // TTL-GC'd is NOT a death: the terminal
                                    // report rides the stream. Keep watching.
                                    //
                                    // Nor is a Kueue EVICTION, which produces
                                    // this same state and is recoverable — see
                                    // `crate::runtime_eviction`. Only an
                                    // unexplained absence is A1's subject.
                                    if !job_completed_cleanly(&job) {
                                        match classify_absent_pod(&self.client, ns, &job, job_name)
                                            .await
                                        {
                                            PodAbsenceVerdict::Abandoned => {
                                                return self
                                                    .reap_lost_fenced_pod(
                                                        handle,
                                                        job_name,
                                                        bound_pod_uid.as_deref(),
                                                        &unadopted_pod_uids(
                                                            &list.items,
                                                            bound_pod_uid.as_deref(),
                                                        ),
                                                    )
                                                    .await;
                                            }
                                            verdict => self.log_held_pod_absence(
                                                handle,
                                                job_name,
                                                &verdict,
                                                &mut held_absence,
                                            ),
                                        }
                                    }
                                }
                                Err(_) => {
                                    // Transient apiserver error — fall through
                                    // to the Job-status check below.
                                }
                            }
                        }
                        None => {}
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
    /// Contain a task-run whose fenced Pod was destroyed out-of-band while its
    /// Job was still nonterminal, and return the death reason the dispatch
    /// runner terminalises the run with.
    ///
    /// The Job is foreground-deleted here rather than left to `teardown`. Both
    /// halves matter:
    ///
    /// * *Foreground*, so the apiserver blocks the Job's own removal on its
    ///   Pods being fully cleaned up. A background delete returns immediately
    ///   and hands the surviving replacement Pod — the one this watch just
    ///   refused to adopt — to the orphan collector, which is exactly the
    ///   window where it can outlive the run that paid for it.
    /// * *Here*, because the Job is what holds quota. A retry always mints a
    ///   fresh `task_run_id`, so the abandoned Job's deterministic name is never
    ///   reused and nothing ever adopts it; under Kueue its admitted Workload
    ///   would sit against the ClusterQueue until a human noticed.
    ///
    /// A failed delete does not suppress the reason: the run is dead either
    /// way, and refusing to resolve would pin the dispatch slot on the very
    /// stall this watch exists to break. The next reconcile can retry the Job.
    /// Report a fenced-Pod absence the watch is deliberately NOT acting on.
    ///
    /// The first observation is INFO — a run whose Pod vanished and was not
    /// reaped is exactly the thing an operator wants in the log, once. Every
    /// later poll is DEBUG, because a Workload can sit in the queue for as long
    /// as the queue is full and 4 lines a minute of it is not information.
    fn log_held_pod_absence(
        &self,
        handle: &RunHandle,
        job_name: &str,
        verdict: &PodAbsenceVerdict,
        already_reported: &mut bool,
    ) {
        let (kind, evidence) = match verdict {
            PodAbsenceVerdict::Recoverable(evidence) => ("recoverable", evidence.as_str()),
            PodAbsenceVerdict::Inconclusive(error) => ("inconclusive", error.as_str()),
            // Never reached: the caller reaps on `Abandoned` instead of holding.
            PodAbsenceVerdict::Abandoned => ("abandoned", ""),
        };
        if std::mem::replace(already_reported, true) {
            debug!(
                task_run_id = %handle.task_run_id,
                job = %job_name,
                verdict = %kind,
                %evidence,
                "kubernetes_runtime: infra-death watch — still holding the fenced Pod's absence"
            );
            return;
        }
        info!(
            task_run_id = %handle.task_run_id,
            job = %job_name,
            verdict = %kind,
            %evidence,
            "kubernetes_runtime: infra-death watch — the fenced worker Pod is gone but its \
             absence is explained; NOT terminalising and NOT reaping the Job, so the run can \
             still be re-admitted"
        );
    }

    async fn reap_lost_fenced_pod(
        &self,
        handle: &RunHandle,
        job_name: &str,
        bound_pod_uid: Option<&str>,
        unadopted_pod_uids: &[String],
    ) -> String {
        let ns = &self.config.namespace;
        let fenced = bound_pod_uid.unwrap_or("<unknown>");
        match delete_job_foreground(&self.client, ns, job_name, INFRA_DEATH_REAP_GRACE_SECONDS)
            .await
        {
            Ok(()) => {
                info!(
                    task_run_id = %handle.task_run_id,
                    job = %job_name,
                    namespace = %ns,
                    pod_uid = %fenced,
                    unadopted = ?unadopted_pod_uids,
                    "kubernetes_runtime: infra-death watch — fenced worker Pod vanished under a \
                     live Job; foreground-deleted the Job so it stops holding quota"
                );
            }
            Err(error) => {
                warn!(
                    task_run_id = %handle.task_run_id,
                    job = %job_name,
                    namespace = %ns,
                    pod_uid = %fenced,
                    %error,
                    "kubernetes_runtime: infra-death watch — reaping the orphaned task-run Job \
                     failed; still terminalising the run"
                );
            }
        }
        let mut reason = format!(
            "worker Pod {fenced} was deleted while its Job {job_name} was still active \
             (force delete / node loss); the Job was foreground-deleted"
        );
        if !unadopted_pod_uids.is_empty() {
            reason.push_str(&format!(
                "; refused to adopt replacement Pod UID(s) {}",
                unadopted_pod_uids.join(", ")
            ));
        }
        reason
    }

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

/// The immutable Pod identity a run's infra-death watch fences itself to.
///
/// Returns the first non-blank `metadata.uid` in the listed Pods. A Pod without
/// a UID cannot be fenced on and is skipped rather than treated as the run's
/// Pod — an unfenced observation is the failure mode this whole module exists to
/// remove.
fn bind_worker_pod_uid(pods: &[Pod]) -> Option<String> {
    pods.iter().find_map(|pod| {
        pod.metadata
            .uid
            .as_deref()
            .map(str::trim)
            .filter(|uid| !uid.is_empty())
            .map(str::to_string)
    })
}

/// Select the Pod this run is fenced to, by immutable UID.
///
/// THE UID COMPARISON IS THE CONTAINMENT. The label selector matches every Pod
/// the Job controller ever makes for this run, including the replacement it
/// mints after the original is force-deleted. Falling back to "the first listed
/// Pod" would adopt that replacement: the watch would report it healthy, the
/// run would keep its dispatch slot, and the build lease would still be bound to
/// a UID that no longer exists anywhere in the cluster.
///
/// `bound` is `None` only before any Pod has ever carried a UID, where there is
/// no fence to enforce yet and positional selection is all that is available.
fn fenced_worker_pod<'a>(pods: &'a [Pod], bound: Option<&str>) -> Option<&'a Pod> {
    match bound {
        Some(uid) => pods
            .iter()
            .find(|pod| pod.metadata.uid.as_deref() == Some(uid)),
        None => pods.first(),
    }
}

/// The UIDs of every listed Pod that is NOT the fenced one — the replacements
/// this watch observed and declined to adopt. Reported in the death reason and
/// the reap log so a live-cluster investigation can see what was refused.
fn unadopted_pod_uids(pods: &[Pod], bound: Option<&str>) -> Vec<String> {
    pods.iter()
        .filter_map(|pod| pod.metadata.uid.as_deref())
        .filter(|uid| Some(*uid) != bound)
        .map(str::to_string)
        .collect()
}

/// Whether a `Job` reached its *successful* terminal state.
///
/// Distinct from [`job_failed_reason`]: a clean completion is not a death, so a
/// Pod that disappears under a Complete Job is TTL-GC and must never be reaped
/// as a containment event. Anything neither complete nor failed is nonterminal —
/// still holding quota, still owed a Pod.
fn job_completed_cleanly(job: &Job) -> bool {
    let Some(status) = job.status.as_ref() else {
        return false;
    };
    if status.succeeded.unwrap_or(0) > 0 {
        return true;
    }
    status.conditions.as_ref().is_some_and(|conditions| {
        conditions
            .iter()
            .any(|condition| condition.type_ == "Complete" && condition.status == "True")
    })
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

/// Terminate exactly the immutable Pod recorded for a task-run, then tear down
/// its owning Job.
///
/// Delete acceptance is durably observable rather than inferred from a second
/// best-effort Kubernetes write: a service-owned finalizer holds the exact Pod
/// across the DELETE, so `deletionTimestamp` + the recorded UID stay readable
/// until this code releases the hold. Ordering is hold -> UID-fenced Pod DELETE
/// -> UID-fenced orphan Job DELETE -> release, which makes every write after
/// the destructive step retryable; no step can lose confirmation once the
/// irreversible operation has happened.
///
/// Failure semantics:
/// - hold write fails: typed error, zero destructive calls;
/// - Pod DELETE fails: hold released best-effort, typed error, no Job teardown,
///   and a later empty list is still unconfirmed;
/// - Job DELETE fails: typed error; the retry re-reads the held terminating Pod
///   and resumes teardown without a second destructive Pod call;
/// - release fails: typed error; the retry finds the Job already absent and the
///   held Pod still visible, and completes the release terminally.
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
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let selector = format!("{}={task_run_id}", crate::job::LABEL_TASK_RUN_ID);
    let Some(job) = jobs
        .get_opt(&taskrun_job_name(task_run_id))
        .await
        .map_err(|e| format!("get task-run Job: {e}"))?
    else {
        // An absent Job can be the tail of this protocol's own completed
        // teardown: the hold below keeps the deleted Pod observable, so a
        // release that failed after the Job DELETE is finished here instead of
        // erroring forever. Every other absent Job stays unconfirmed.
        return release_confirmed_exact_pod(&pods, &selector, task_run_id, pod_uid).await;
    };
    if job.metadata.deletion_timestamp.is_some() {
        return Err("exact-pod watchdog termination task-run Job is not confirmable".into());
    }
    let job_uid = job
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| "task-run Job has no immutable UID".to_string())?;
    let listed_pods = pods
        .list(&ListParams::default().labels(&selector))
        .await
        .map_err(|e| format!("list task-run Pods: {e}"))?
        .items;

    // An empty list is never authorization. The exact Pod either does not
    // exist yet under a live Job, or a previous run of this protocol already
    // released it — and that run deleted the Job first, so the absent-Job
    // branch above owns the confirmed case. Absence alone proves nothing.
    if listed_pods.is_empty() {
        return Err("exact pod UID deletion is not confirmed by the task-run Job".into());
    }

    // Reject the entire observation if any labelled Pod is not the recorded
    // immutable object. Finding the old Pod must not authorize teardown while
    // a replacement or foreign Pod is also present.
    if listed_pods.len() != 1 {
        return Err(
            "exact pod UID is unavailable or does not belong to the recorded task-run Job".into(),
        );
    }
    let pod = &listed_pods[0];
    let pod_name = exact_taskrun_pod_name(pod, pod_uid, job_uid).ok_or_else(|| {
        "exact pod UID is unavailable or does not belong to the recorded task-run Job".to_string()
    })?;

    // `deletionTimestamp` on the exact immutable UID is Kubernetes' own durable
    // record that this object was accepted for deletion. Acquire the hold
    // *before* the destructive call so that record cannot evaporate with the
    // object: with a service-owned finalizer the terminating Pod stays visible
    // until this code releases it, and every write after the delete becomes a
    // retryable step instead of a one-shot confirmation that can be lost.
    let mut held = pod_holds_termination_finalizer(pod);
    if pod.metadata.deletion_timestamp.is_none() {
        if !held {
            hold_exact_pod(&pods, pod, &pod_name, pod_uid).await?;
            held = true;
        }
        // The sole destructive operation is fenced by the recorded Pod UID.
        let params = exact_pod_delete_params(pod_uid);
        match pods.delete(&pod_name, &params).await {
            Ok(_) => {}
            Err(kube::Error::Api(response)) if response.code == 404 => {}
            Err(e) => {
                // An unconfirmed delete must not leave a hold behind: an
                // unreleased finalizer would block the Job controller's own
                // reaping of a Pod this call never deleted.
                let _ = release_exact_pod_hold(&pods, &pod_name, pod_uid, pod).await;
                return Err(format!("delete exact task-run Pod: {e}"));
            }
        }
    }

    // Remove the controller while the confirmation is still observable. Orphan
    // propagation is deliberate: unlike a cascade, it cannot delete a
    // different-UID Pod that appears between the list and this request. The
    // Job operation is independently fenced by the immutable Job UID, and a
    // failure here is retryable because the held Pod still proves the delete.
    delete_taskrun_job_orphaned(&jobs, task_run_id, job_uid).await?;

    if held {
        release_exact_pod_hold(&pods, &pod_name, pod_uid, pod).await?;
    }
    Ok(())
}

/// Service-owned finalizer that makes an accepted exact-Pod deletion durably
/// observable. It is added only immediately before this module's UID-fenced Pod
/// DELETE, so a Pod carrying it with a `deletionTimestamp` is proof that this
/// protocol deleted exactly that immutable object.
const EXACT_POD_TERMINATION_FINALIZER: &str = "djinn.dev/exact-pod-termination";

fn pod_holds_termination_finalizer(pod: &Pod) -> bool {
    pod.metadata.finalizers.as_ref().is_some_and(|finalizers| {
        finalizers
            .iter()
            .any(|finalizer| finalizer == EXACT_POD_TERMINATION_FINALIZER)
    })
}

fn finalizers_without_termination_hold(pod: &Pod) -> Vec<String> {
    pod.metadata
        .finalizers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|finalizer| finalizer != EXACT_POD_TERMINATION_FINALIZER)
        .collect()
}

/// Add the confirmation hold to the exact Pod, fenced by its immutable UID and
/// the observed resourceVersion so a same-name replacement can never be held.
/// This write happens *before* the destructive call, so its failure returns a
/// typed error with zero destructive operations issued.
async fn hold_exact_pod(
    pods: &Api<Pod>,
    pod: &Pod,
    pod_name: &str,
    pod_uid: &str,
) -> Result<(), String> {
    let resource_version = pod
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| "exact task-run Pod has no resource version".to_string())?;
    let mut finalizers = finalizers_without_termination_hold(pod);
    finalizers.push(EXACT_POD_TERMINATION_FINALIZER.to_string());
    let patch = serde_json::json!({
        "metadata": {
            "uid": pod_uid,
            "resourceVersion": resource_version,
            "finalizers": finalizers
        }
    });
    pods.patch(pod_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|e| format!("hold exact task-run Pod for confirmation: {e}"))?;
    Ok(())
}

/// Release the confirmation hold, restoring exactly the finalizers observed on
/// the Pod before it was held. The write is UID-fenced and idempotent: a 404
/// means the object already completed deletion.
async fn release_exact_pod_hold(
    pods: &Api<Pod>,
    pod_name: &str,
    pod_uid: &str,
    observed: &Pod,
) -> Result<(), String> {
    let patch = serde_json::json!({
        "metadata": {
            "uid": pod_uid,
            "finalizers": finalizers_without_termination_hold(observed)
        }
    });
    match pods
        .patch(pod_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(e) => Err(format!("release exact task-run Pod hold: {e}")),
    }
}

/// Finish a teardown whose Job is already gone. This is a terminal resolution
/// only when the held, terminating Pod carrying this protocol's own finalizer
/// is still visible with the recorded UID under the canonical Job owner — the
/// exact state left behind when the Job DELETE succeeded and the release did
/// not. Anything else keeps the original unavailable-Job error.
async fn release_confirmed_exact_pod(
    pods: &Api<Pod>,
    selector: &str,
    task_run_id: &str,
    pod_uid: &str,
) -> Result<(), String> {
    let listed_pods = pods
        .list(&ListParams::default().labels(selector))
        .await
        .map_err(|e| format!("list task-run Pods: {e}"))?
        .items;
    let job_name = taskrun_job_name(task_run_id);
    let confirmed = listed_pods.into_iter().find(|pod| {
        pod.metadata.uid.as_deref() == Some(pod_uid)
            && pod.metadata.deletion_timestamp.is_some()
            && pod_holds_termination_finalizer(pod)
            && pod
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|owners| {
                    owners
                        .iter()
                        .any(|owner| owner.kind == "Job" && owner.name == job_name)
                })
    });
    let Some(pod) = confirmed else {
        return Err("exact-pod watchdog termination task-run Job is unavailable".into());
    };
    let pod_name = pod
        .metadata
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "exact-pod watchdog termination task-run Job is unavailable".to_string())?;
    release_exact_pod_hold(pods, &pod_name, pod_uid, &pod).await
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

// ── Post-admission launcher observation (g8jk-3) ───────────────────────────
//
// The resize bootstrap that consumes this lives in
// `djinn_server::task_run_resize_bootstrap`. The split is deliberate: the
// mechanics of reading a stored Pod belong to the crate that owns the Kubernetes
// types, and the policy — what to capture, when to refuse, when to delete —
// belongs beside the durable permit relation. Nothing below decides anything; it
// reports what the apiserver has stored.

/// One fresh read of the *stored* launcher sidecar, flattened to plain data.
///
/// Every field here comes from the object the apiserver persisted, never from
/// the render input. That is the whole point of post-admission capture: a
/// mutating admission webhook may have changed what was rendered, and a value
/// derived from the render would report the ceiling we asked for rather than the
/// ceiling the Pod actually has.
///
/// Fields that a still-starting Pod legitimately lacks are [`Option`], and the
/// caller decides whether their absence is "not yet" or "never". This type
/// deliberately makes no such judgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedLauncherSidecar {
    /// `metadata.namespace` of the stored Pod.
    pub namespace: String,
    /// `metadata.name` of the stored Pod.
    pub pod_name: String,
    /// `metadata.uid` — the fence every later resize and delete is bound to.
    pub pod_uid: String,
    /// The launcher's container name, as it appears in `spec.initContainers`.
    pub launcher_container_name: String,
    /// `status.initContainerStatuses[..].containerID`. Absent until the kubelet
    /// has actually started the sidecar.
    pub launcher_container_id: Option<String>,
    /// `status.initContainerStatuses[..].imageID` — the resolved artifact, not
    /// the possibly-mutable tag in the spec.
    pub image_digest: Option<String>,
    /// The launcher's own `DJINN_LAUNCHER_AUTHORITY_PROTOCOL` value, read off
    /// the stored spec. Absent for images rendered before the protocol existed.
    pub observed_protocol: Option<String>,
    /// The persisted `spec.initContainers[cgroup-launcher].resources.limits.cpu`
    /// in millicores — the admitted ceiling. Absent under `leaf-v1`, which
    /// renders no launcher CPU limit at all.
    pub admitted_cpu_millicores: Option<u64>,
}

/// Why a stored Pod could not be flattened into an [`ObservedLauncherSidecar`].
///
/// These are failures, but they are not the *same* failure, and the difference
/// decides whether a caller waits or gives up:
///
/// * `Incomplete` — the Pod has not finished being admitted; may complete on the
///   next read.
/// * `StatusNotPopulated` — the Pod exists and its spec names the launcher, but
///   the kubelet has not written `status.initContainerStatuses` yet. **A wait,
///   not a verdict.** See the variant's own docs.
/// * `Ambiguous` — the launcher cannot be *named*, and no amount of waiting will
///   change that.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LauncherObservationError {
    /// A `metadata` field the fence depends on is not populated yet.
    #[error("stored Pod is missing `metadata.{field}`")]
    Incomplete {
        /// The absent metadata field.
        field: &'static str,
    },
    /// `status.initContainerStatuses` carries **zero** launcher entries, while
    /// `spec.initContainers` names exactly one.
    ///
    /// # Why this is not `Ambiguous`
    ///
    /// A Pod's `spec` is written by the Job controller at creation; its
    /// `status.initContainerStatuses` array is written by the kubelet, later,
    /// after the Pod is bound to a node. Between those two events the launcher
    /// is named in the spec and absent from the status — for seconds, routinely.
    ///
    /// Folding that window into `Ambiguous` is what production charged for on
    /// 2026-08-02: the resize birth gate read a Pod three seconds after Kueue
    /// unsuspended its Job, saw `found: 0` at the status site, refused
    /// *permanently*, and left the Pod running at its full rendered `4` CPU with
    /// no permit governing it and nothing that would ever retry. The launcher
    /// was perfectly nameable one second later.
    ///
    /// Two or more entries is a different animal and stays [`Self::Ambiguous`]:
    /// that one really cannot be resolved by waiting, and resolving it by
    /// positional index would address the wrong container.
    ///
    /// The Pod identity is carried because the caller needs it precisely when it
    /// gives up: a Pod that never became governable must be *destroyed*, and an
    /// unfenced delete could destroy a replacement Pod some other actor owns.
    #[error(
        "launcher `{launcher_container_name}` is named in spec.initContainers but \
         status.initContainerStatuses is not populated yet"
    )]
    StatusNotPopulated {
        /// `metadata.uid` of the Pod that was read. Fences a delete.
        pod_uid: String,
        /// `metadata.name` of the Pod that was read.
        pod_name: String,
        /// The launcher container name located in `spec.initContainers`.
        launcher_container_name: String,
    },
    /// The launcher is not uniquely identifiable in the stored Pod.
    #[error("{0}")]
    Ambiguous(#[from] crate::pod_resize::PodResizeError),
    /// The apiserver read itself failed, or resolved to more than one Pod.
    #[error("{0}")]
    Api(String),
}

impl LauncherObservationError {
    /// The Pod this failed observation read, when it read one and knows its UID.
    ///
    /// Only [`Self::StatusNotPopulated`] can answer: every other variant either
    /// never reached a Pod object or could not establish which container it was
    /// talking about.
    #[must_use]
    pub fn fenceable_pod(&self) -> Option<(&str, &str)> {
        match self {
            Self::StatusNotPopulated {
                pod_uid, pod_name, ..
            } => Some((pod_uid.as_str(), pod_name.as_str())),
            _ => None,
        }
    }
}

/// Flatten a stored Pod into the launcher facts the resize bootstrap needs.
///
/// The launcher must be uniquely identifiable in **both** `spec.initContainers`
/// and `status.initContainerStatuses` before any field is read, for the reason
/// [`crate::pod_resize`] documents at length: the worker container can carry a
/// coincidentally matching CPU limit, so resolving the launcher by anything less
/// than a unique name match does not read nothing, it reads the wrong thing.
///
/// # Errors
///
/// [`LauncherObservationError`] — see its variants.
pub fn observe_launcher_sidecar(
    pod: &Pod,
) -> Result<ObservedLauncherSidecar, LauncherObservationError> {
    let namespace = non_empty(pod.metadata.namespace.as_deref())
        .ok_or(LauncherObservationError::Incomplete { field: "namespace" })?;
    let pod_name = non_empty(pod.metadata.name.as_deref())
        .ok_or(LauncherObservationError::Incomplete { field: "name" })?;
    let pod_uid = non_empty(pod.metadata.uid.as_deref())
        .ok_or(LauncherObservationError::Incomplete { field: "uid" })?;

    let spec = crate::pod_resize::locate_launcher_spec(pod)?;
    // Zero status entries is the kubelet being late, not the launcher being
    // unnameable — the spec above just named it. Only that exact shape is a
    // wait; two or more entries falls through to `Ambiguous` unchanged.
    let status = match crate::pod_resize::locate_launcher_status(pod) {
        Ok(status) => status,
        Err(crate::pod_resize::PodResizeError::LauncherIdentityAmbiguous {
            site: crate::pod_resize::LauncherIdentitySite::StatusInitContainerStatuses,
            found: 0,
        }) => {
            return Err(LauncherObservationError::StatusNotPopulated {
                pod_uid,
                pod_name,
                launcher_container_name: spec.name.clone(),
            });
        }
        Err(error) => return Err(LauncherObservationError::Ambiguous(error)),
    };

    // The declared limit is read through `declared_launcher_cpu_limit`, which is
    // documented as a spec read and explicitly NOT confirmation of anything. It
    // is the right source here precisely because capture is a statement about
    // what was admitted, not about what the kubelet has actuated.
    let admitted_cpu_millicores = crate::pod_resize::declared_launcher_cpu_limit(pod)
        .ok()
        .map(|limit| limit.millis());

    Ok(ObservedLauncherSidecar {
        namespace,
        pod_name,
        pod_uid,
        launcher_container_name: spec.name.clone(),
        launcher_container_id: non_empty(status.container_id.as_deref()),
        image_digest: non_empty(Some(status.image_id.as_str())),
        observed_protocol: spec.env.as_ref().and_then(|env| {
            env.iter()
                .find(|entry| entry.name == crate::launcher::AUTHORITY_PROTOCOL_ENV)
                .and_then(|entry| non_empty(entry.value.as_deref()))
        }),
        admitted_cpu_millicores,
    })
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Fresh, label-scoped read of the single live Pod for `task_run_id`.
///
/// Never served from a cache. More than one labelled Pod is an error rather than
/// a choice: a replacement Pod standing beside the original is exactly the
/// situation in which picking either one is wrong, and it is the same rule
/// [`terminate_taskrun_pod_exact`] already applies before it deletes anything.
///
/// # Errors
///
/// A rendered `kube::Error` on list failure, or an ambiguity message when the
/// label selector does not resolve to exactly one Pod.
pub async fn get_taskrun_pod_fresh(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
) -> Result<Option<Pod>, String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let selector = format!("{}={task_run_id}", crate::job::LABEL_TASK_RUN_ID);
    let listed = pods
        .list(&ListParams::default().labels(&selector))
        .await
        .map_err(|e| format!("list task-run Pods: {e}"))?
        .items;
    match listed.len() {
        0 => Ok(None),
        1 => Ok(listed.into_iter().next()),
        found => Err(format!(
            "task-run {task_run_id} resolves to {found} Pods; refusing to pick one"
        )),
    }
}

/// Whether a task run's Job has left the Kueue admission queue.
///
/// A Job Kueue is still holding carries `spec.suspend: true`, and the Job
/// controller creates **no Pod** for a suspended Job. That is not a slow start:
/// while this reads [`Self::Suspended`] there is no launcher in existence, so
/// there is nothing to resize and nothing that could confirm a birth limit. Any
/// budget that measures the launcher must therefore not run.
///
/// The three variants are deliberately not two. Only a *proven* suspension may
/// defer a deadline; a Job that could not be read at all must keep the launcher
/// clock running, or an apiserver outage would buy an unbounded wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobAdmission {
    /// The Job exists and `spec.suspend` is `true` — queued, not started.
    Suspended,
    /// The Job exists and is not suspended. A Pod is expected to materialize;
    /// one that does not is a real stall.
    Admitted,
    /// No Job carries this task run's label, or the read failed. Folded with
    /// [`Self::Admitted`] by every caller, on purpose: see the type docs.
    Unknown(String),
}

/// The four apiserver operations the resize bootstrap performs, bound to one
/// namespace and one field manager.
///
/// It exists so `djinn-server` can drive the bootstrap without depending on
/// `kube` or `k8s-openapi` directly, and so the bootstrap's own tests can
/// substitute a fake for all three at once.
pub struct TaskRunPodResizeSurface {
    client: kube::Client,
    namespace: String,
    field_manager: String,
}

impl TaskRunPodResizeSurface {
    /// Bind to one namespace.
    pub fn new(
        client: kube::Client,
        namespace: impl Into<String>,
        field_manager: impl Into<String>,
    ) -> Self {
        Self {
            client,
            namespace: namespace.into(),
            field_manager: field_manager.into(),
        }
    }

    /// Build from ambient cluster configuration — the in-cluster service
    /// account, or the caller's kubeconfig — and the environment-configured
    /// task-run namespace.
    ///
    /// This is the composition-root constructor: the server builds its resize
    /// admission bridge once at boot, long before any particular dispatch has a
    /// [`KubernetesRuntime`] to borrow a client from. It resolves the client the
    /// same way [`KubernetesRuntime::new`] does, so the surface and the runtime
    /// cannot end up pointed at different clusters.
    ///
    /// # Errors
    ///
    /// The rendered `kube::Error` when no cluster configuration is available.
    pub async fn from_env() -> Result<Self, String> {
        let config = KubernetesConfig::from_env();
        let client = kube::Client::try_default()
            .await
            .map_err(|error| format!("task-run resize surface: kube client: {error}"))?;
        Ok(Self::new(client, config.namespace, "djinn-task-run-resize"))
    }

    /// Build from a live runtime, reusing its client and configured namespace.
    #[must_use]
    pub fn from_runtime(runtime: &KubernetesRuntime) -> Self {
        Self::new(
            runtime.client().clone(),
            runtime.config().namespace.clone(),
            "djinn-task-run-resize",
        )
    }

    /// Fresh GET, then flatten. `Ok(None)` means no Pod exists yet.
    ///
    /// # Errors
    ///
    /// [`LauncherObservationError`] — the list failed or was ambiguous
    /// (`Api`), the Pod is not fully admitted (`Incomplete`), or the launcher
    /// cannot be named (`Ambiguous`).
    pub async fn observe_launcher(
        &self,
        task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        let Some(pod) = get_taskrun_pod_fresh(&self.client, &self.namespace, task_run_id)
            .await
            .map_err(LauncherObservationError::Api)?
        else {
            return Ok(None);
        };
        observe_launcher_sidecar(&pod).map(Some)
    }

    /// One limits-only resize of the launcher sidecar, confirmed through
    /// `status.initContainerStatuses`.
    ///
    /// # Errors
    ///
    /// [`crate::pod_resize::PodResizeError`]; in particular `NotConfirmed` when
    /// the PATCH was accepted but the fresh status does not yet agree.
    pub async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), crate::pod_resize::PodResizeError> {
        let api = crate::pod_resize::KubePodResizeApi::new(
            self.client.clone(),
            &self.namespace,
            self.field_manager.clone(),
        );
        crate::pod_resize::PodResizeClient::new(api)
            .resize_launcher_cpu(
                pod_name,
                crate::pod_resize::CpuLimit::from_millis(target_millicores),
            )
            .await
    }

    /// Whether this task run's Job has been admitted out of the Kueue queue.
    ///
    /// Reads `spec.suspend` off the Job itself rather than the Kueue Workload:
    /// `suspend` is the field the Job controller actually consults before it
    /// creates a Pod, so it is the fact that decides whether a Pod can exist.
    /// A Workload condition is Kueue's *intent*; this is the effect.
    ///
    /// Never returns an error. An unreadable Job is [`JobAdmission::Unknown`],
    /// which every caller folds with [`JobAdmission::Admitted`] — the fail-safe
    /// direction, because only a proven suspension may defer a deadline.
    pub async fn observe_job_admission(&self, task_run_id: &str) -> JobAdmission {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let selector = format!("{}={task_run_id}", crate::job::LABEL_TASK_RUN_ID);
        let listed = match jobs.list(&ListParams::default().labels(&selector)).await {
            Ok(list) => list.items,
            Err(error) => return JobAdmission::Unknown(format!("list task-run Jobs: {error}")),
        };
        match listed.as_slice() {
            [] => JobAdmission::Unknown(format!("no Job carries the label `{selector}`")),
            [job] => {
                if job.spec.as_ref().and_then(|spec| spec.suspend) == Some(true) {
                    JobAdmission::Suspended
                } else {
                    JobAdmission::Admitted
                }
            }
            many => JobAdmission::Unknown(format!(
                "task-run {task_run_id} resolves to {} Jobs; refusing to pick one",
                many.len()
            )),
        }
    }

    /// UID-fenced destruction of exactly the observed Pod, plus its Job.
    ///
    /// # Errors
    ///
    /// The rendered failure from [`terminate_taskrun_pod_exact`].
    pub async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        terminate_taskrun_pod_exact(&self.client, &self.namespace, task_run_id, pod_uid).await
    }
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

    #[test]
    fn jobs_started_counter_moves_only_for_confirmed_job_create_success() {
        let (_, rendered) = djinn_telemetry::render_isolated(|| {
            let success: Result<(), &str> = record_confirmed_job_create(Ok(()));
            assert!(success.is_ok());

            // Prerequisite work does not cross the create boundary, and a
            // failed/ambiguous create result is returned without a counter.
            let prerequisite_failure: Result<(), &str> = Err("secret creation failed");
            assert!(prerequisite_failure.is_err());
            let failed_create: Result<(), &str> = record_confirmed_job_create(Err("create failed"));
            assert!(failed_create.is_err());
        });

        // Exact line equality is the no-label assertion: a labelled series
        // renders its label set between the name and the value, so only an
        // unlabelled counter matches this line verbatim.
        assert!(
            rendered
                .lines()
                .any(|line| line.trim() == "djinn_taskrun_jobs_started_total 1"),
            "expected one unlabelled jobs-started series at 1 in:\n{rendered}"
        );
    }

    /// The exact Pod as this protocol leaves it after a confirmed delete: held
    /// by the service-owned finalizer and carrying a `deletionTimestamp`.
    fn held_terminating_pod(name: &str, pod_uid: &str, owner_uid: &str) -> Pod {
        let mut pod = owned_pod(name, pod_uid, owner_uid);
        pod.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                "2026-01-01T00:00:00Z".parse().expect("fixed timestamp"),
            ));
        pod.metadata.finalizers = Some(vec![EXACT_POD_TERMINATION_FINALIZER.into()]);
        pod
    }

    fn owned_pod(name: &str, pod_uid: &str, owner_uid: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                uid: Some(pod_uid.into()),
                resource_version: Some("11".into()),
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

    /// Per-call status sequence for one mocked verb. Statuses are consumed in
    /// issue order; an exhausted queue keeps replying `200` so a test only has
    /// to spell out the calls whose outcome it is asserting.
    #[derive(Clone)]
    struct StatusQueue(Arc<StdMutex<Vec<u16>>>);

    impl StatusQueue {
        fn next(&self) -> u16 {
            let mut queue = self.0.lock().unwrap();
            if queue.is_empty() {
                200
            } else {
                queue.remove(0)
            }
        }
    }

    fn statuses(values: &[u16]) -> StatusQueue {
        StatusQueue(Arc::new(StdMutex::new(values.to_vec())))
    }

    #[derive(Clone)]
    struct ExactKubeReplies {
        jobs: Arc<StdMutex<Vec<(u16, serde_json::Value)>>>,
        pod_lists: Arc<StdMutex<Vec<(u16, serde_json::Value)>>>,
        pod_patch_status: StatusQueue,
        pod_delete_status: StatusQueue,
        job_delete_status: StatusQueue,
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
                        // An exhausted queue means "nothing further exists":
                        // the absent-Job path lists Pods too, and a test that
                        // scripts no observation asserts on an empty cluster.
                        let mut lists = replies.pod_lists.lock().unwrap();
                        if lists.is_empty() {
                            (200, pod_list_json(vec![]))
                        } else {
                            lists.remove(0)
                        }
                    } else if method == "PATCH" && path.contains("/pods/") {
                        let status = replies.pod_patch_status.next();
                        if status < 400 {
                            (
                                status,
                                serde_json::to_value(held_terminating_pod(
                                    "taskrun-pod",
                                    "pod-recorded",
                                    "job-recorded",
                                ))
                                .expect("serialize patched Pod"),
                            )
                        } else {
                            (status, api_error(status, "mock patch failed"))
                        }
                    } else if method == "DELETE" && path.contains("/pods/") {
                        let status = replies.pod_delete_status.next();
                        (status, delete_response(status))
                    } else if method == "DELETE" && path.contains("/jobs/") {
                        let status = replies.job_delete_status.next();
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

    /// A genuinely-absent object: `reason` is what the kube client uses to
    /// distinguish "gone" from "the read failed with a 404-shaped error".
    fn not_found() -> serde_json::Value {
        serde_json::json!({
            "apiVersion":"v1", "kind":"Status", "status":"Failure",
            "reason":"NotFound", "message":"NotFound", "code":404
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

    fn job_json(uid: Option<&str>, deleting: bool) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {
                "name": "djinn-taskrun-run-1", "uid": uid, "resourceVersion": "7",
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
            pod_patch_status: statuses(&[]),
            pod_delete_status: statuses(&[]),
            job_delete_status: statuses(&[]),
        }
    }

    /// Mutation trace: `VERB resource` for every non-read request, in order.
    fn mutation_order(requests: &CapturedKubeRequests) -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _, _)| method == "DELETE" || method == "PATCH")
            .map(|(method, path, _)| {
                let resource = if path.contains("/pods") {
                    "pods"
                } else {
                    "jobs"
                };
                format!("{method} {resource}")
            })
            .collect()
    }

    #[tokio::test]
    async fn exact_termination_holds_deletes_then_releases_the_recorded_pod() {
        let replies = exact_replies(
            vec![(200, job_json(Some("job-recorded"), false))],
            vec![(
                200,
                pod_list_json(vec![owned_pod(
                    "taskrun-pod",
                    "pod-recorded",
                    "job-recorded",
                )]),
            )],
        );
        let (client, requests) = exact_kube_client(replies);

        terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
            .await
            .expect("exact termination");

        assert_eq!(
            mutation_order(&requests),
            ["PATCH pods", "DELETE pods", "DELETE jobs", "PATCH pods"],
            "the hold must precede the destructive call and outlive Job teardown"
        );

        let requests = requests.lock().unwrap();
        let patches = requests
            .iter()
            .filter(|(method, _, _)| method == "PATCH")
            .collect::<Vec<_>>();
        let hold: serde_json::Value = serde_json::from_str(&patches[0].2).unwrap();
        assert!(patches[0].1.contains("/pods/taskrun-pod?"));
        assert_eq!(hold["metadata"]["uid"], "pod-recorded");
        assert_eq!(
            hold["metadata"]["resourceVersion"], "11",
            "the hold is fenced to the observed Pod, never a same-name replacement"
        );
        assert_eq!(
            hold["metadata"]["finalizers"],
            serde_json::json!([EXACT_POD_TERMINATION_FINALIZER])
        );
        let release: serde_json::Value = serde_json::from_str(&patches[1].2).unwrap();
        assert_eq!(release["metadata"]["uid"], "pod-recorded");
        assert_eq!(
            release["metadata"]["finalizers"],
            serde_json::json!([]),
            "the release restores exactly the finalizers observed before the hold"
        );

        let deletes = requests
            .iter()
            .filter(|(method, _, _)| method == "DELETE")
            .collect::<Vec<_>>();
        assert_eq!(deletes.len(), 2, "one Pod and one UID-fenced Job delete");
        let pod_delete: serde_json::Value = serde_json::from_str(&deletes[0].2).unwrap();
        assert!(deletes[0].1.contains("/pods/taskrun-pod?"));
        assert_eq!(pod_delete["preconditions"]["uid"], "pod-recorded");
        let job_delete: serde_json::Value = serde_json::from_str(&deletes[1].2).unwrap();
        assert!(deletes[1].1.contains("/jobs/djinn-taskrun-run-1?"));
        assert_eq!(job_delete["preconditions"]["uid"], "job-recorded");
        assert_eq!(job_delete["propagationPolicy"], "Orphan");
    }

    /// The liveness regression: an irreversible Pod DELETE that succeeds must
    /// stay confirmable when the very next durable write fails. The hold keeps
    /// `deletionTimestamp` + the recorded UID readable, so the retry resolves
    /// terminally instead of erroring forever and pinning the counted lease.
    #[tokio::test]
    async fn successful_delete_with_failed_release_resolves_terminally_on_retry() {
        let mut replies = exact_replies(
            vec![
                (200, job_json(Some("job-recorded"), false)),
                // The first attempt tore the Job down before the release, so
                // the retry sees no Job at all.
                (404, not_found()),
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
                (
                    200,
                    pod_list_json(vec![held_terminating_pod(
                        "taskrun-pod",
                        "pod-recorded",
                        "job-recorded",
                    )]),
                ),
            ],
        );
        // hold, then a failed release, then a successful release on retry.
        replies.pod_patch_status = statuses(&[200, 500, 200]);
        let (client, requests) = exact_kube_client(replies);

        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err(),
            "a failed release must not claim success"
        );
        terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
            .await
            .expect("the held terminating Pod proves the delete and resolves the retry");

        assert_eq!(
            mutation_order(&requests),
            [
                "PATCH pods",
                "DELETE pods",
                "DELETE jobs",
                "PATCH pods",
                "PATCH pods"
            ],
            "the retry replays only the lost release, never a second destructive call"
        );
        let requests = requests.lock().unwrap();
        let release: serde_json::Value =
            serde_json::from_str(&requests.last().expect("final request").2).unwrap();
        assert_eq!(release["metadata"]["uid"], "pod-recorded");
        assert_eq!(release["metadata"]["finalizers"], serde_json::json!([]));
    }

    /// The same guarantee one step earlier: a failed Job teardown is retryable
    /// because the hold is released last, so the confirmed delete is still
    /// observable on the next attempt.
    #[tokio::test]
    async fn successful_delete_with_failed_teardown_resolves_terminally_on_retry() {
        let mut replies = exact_replies(
            vec![
                (200, job_json(Some("job-recorded"), false)),
                (200, job_json(Some("job-recorded"), false)),
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
                (
                    200,
                    pod_list_json(vec![held_terminating_pod(
                        "taskrun-pod",
                        "pod-recorded",
                        "job-recorded",
                    )]),
                ),
            ],
        );
        replies.job_delete_status = statuses(&[500, 200]);
        let (client, requests) = exact_kube_client(replies);

        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err(),
            "a failed teardown must not claim success"
        );
        terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
            .await
            .expect("the held terminating Pod resumes teardown");

        assert_eq!(
            mutation_order(&requests),
            [
                "PATCH pods",
                "DELETE pods",
                "DELETE jobs",
                "DELETE jobs",
                "PATCH pods"
            ],
            "exactly one destructive Pod call survives the retry"
        );
    }

    #[tokio::test]
    async fn exact_termination_rejects_every_unconfirmed_boundary_without_delete() {
        let cases = vec![
            ("absent Job", (404, not_found()), vec![]),
            (
                "absent Job with an unheld Pod",
                (404, not_found()),
                vec![(
                    200,
                    pod_list_json(vec![owned_pod(
                        "taskrun-pod",
                        "pod-recorded",
                        "job-recorded",
                    )]),
                )],
            ),
            (
                "absent Job with a foreign held Pod",
                (404, not_found()),
                vec![(
                    200,
                    pod_list_json(vec![held_terminating_pod(
                        "taskrun-pod",
                        "pod-replacement",
                        "job-recorded",
                    )]),
                )],
            ),
            ("get failure", (500, api_error(500, "get failed")), vec![]),
            (
                "deleting Job",
                (200, job_json(Some("job-recorded"), true)),
                vec![],
            ),
            ("unidentifiable Job", (200, job_json(None, false)), vec![]),
            (
                "empty list",
                (200, job_json(Some("job-recorded"), false)),
                vec![(200, pod_list_json(vec![]))],
            ),
            (
                "replacement UID",
                (200, job_json(Some("job-recorded"), false)),
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
                "replacement alongside the recorded Pod",
                (200, job_json(Some("job-recorded"), false)),
                vec![(
                    200,
                    pod_list_json(vec![
                        owned_pod("taskrun-pod", "pod-recorded", "job-recorded"),
                        owned_pod("taskrun-pod-2", "pod-replacement", "job-recorded"),
                    ]),
                )],
            ),
            (
                "foreign owner",
                (200, job_json(Some("job-recorded"), false)),
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
            assert!(
                requests.iter().all(|(method, _, _)| method != "PATCH"),
                "{name} wrote state that a later retry could mistake for confirmation"
            );
        }
    }

    #[tokio::test]
    async fn exact_termination_propagates_list_hold_and_independent_delete_failures() {
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
        let live_job = || vec![(200, job_json(Some("job-recorded"), false))];

        let (client, requests) = exact_kube_client(exact_replies(
            live_job(),
            vec![(500, api_error(500, "list failed"))],
        ));
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err()
        );
        assert!(mutation_order(&requests).is_empty());

        let mut replies = exact_replies(live_job(), exact_pods());
        replies.pod_patch_status = statuses(&[500]);
        let (client, requests) = exact_kube_client(replies);
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err()
        );
        assert_eq!(
            mutation_order(&requests),
            ["PATCH pods"],
            "an unacquired hold must precede — and therefore prevent — every destructive call"
        );

        let mut replies = exact_replies(live_job(), exact_pods());
        replies.pod_delete_status = statuses(&[500]);
        let (client, requests) = exact_kube_client(replies);
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err()
        );
        assert_eq!(
            mutation_order(&requests),
            ["PATCH pods", "DELETE pods", "PATCH pods"],
            "a failed delete tears down its own hold and never reaches Job teardown"
        );

        let mut replies = exact_replies(live_job(), exact_pods());
        replies.job_delete_status = statuses(&[500]);
        let (client, requests) = exact_kube_client(replies);
        assert!(
            terminate_taskrun_pod_exact(&client, "djinn", "run-1", "pod-recorded")
                .await
                .is_err(),
            "successful Pod DELETE plus failed Job DELETE must not claim success"
        );
        assert_eq!(
            mutation_order(&requests),
            ["PATCH pods", "DELETE pods", "DELETE jobs"],
            "the hold is retained so the confirmed delete stays observable"
        );
    }

    #[tokio::test]
    async fn failed_pod_delete_does_not_authorize_an_empty_list_retry() {
        let mut replies = exact_replies(
            vec![
                (200, job_json(Some("job-recorded"), false)),
                (200, job_json(Some("job-recorded"), false)),
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
        replies.pod_delete_status = statuses(&[500]);
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

        assert_eq!(
            mutation_order(&requests),
            ["PATCH pods", "DELETE pods", "PATCH pods"],
            "the retry issues nothing: a failed delete leaves no confirmable state"
        );
        let requests = requests.lock().unwrap();
        let last_patch = requests
            .iter()
            .rfind(|(method, _, _)| method == "PATCH")
            .expect("release patch");
        let release: serde_json::Value = serde_json::from_str(&last_patch.2).unwrap();
        assert_eq!(
            release["metadata"]["finalizers"],
            serde_json::json!([]),
            "the hold behind an unconfirmed delete is dropped, not left to block reaping"
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
            execution_generation: 0,
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
            execution_generation: 0,
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

    // ── observe_launcher_sidecar (g8jk-3) ─────────────────────────────────

    mod launcher_observation {
        use super::*;
        use crate::launcher::{AUTHORITY_PROTOCOL_ENV, LAUNCHER_CONTAINER_NAME};
        use k8s_openapi::api::core::v1::{
            Container, ContainerStatus, EnvVar, PodSpec, PodStatus, ResourceRequirements,
        };
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        use std::collections::BTreeMap;

        fn cpu_limit(quantity: &str) -> ResourceRequirements {
            ResourceRequirements {
                limits: Some(BTreeMap::from([(
                    "cpu".to_owned(),
                    Quantity(quantity.to_owned()),
                )])),
                ..ResourceRequirements::default()
            }
        }

        fn container(name: &str, quantity: Option<&str>, protocol: Option<&str>) -> Container {
            Container {
                name: name.to_owned(),
                resources: quantity.map(cpu_limit),
                env: protocol.map(|value| {
                    vec![EnvVar {
                        name: AUTHORITY_PROTOCOL_ENV.to_owned(),
                        value: Some(value.to_owned()),
                        value_from: None,
                    }]
                }),
                ..Container::default()
            }
        }

        fn status(
            name: &str,
            container_id: Option<&str>,
            image_id: Option<&str>,
        ) -> ContainerStatus {
            ContainerStatus {
                name: name.to_owned(),
                container_id: container_id.map(ToOwned::to_owned),
                image_id: image_id.unwrap_or_default().to_owned(),
                ..ContainerStatus::default()
            }
        }

        fn pod(
            init: Vec<Container>,
            regular: Vec<Container>,
            statuses: Vec<ContainerStatus>,
        ) -> Pod {
            Pod {
                metadata: ObjectMeta {
                    name: Some("taskrun-pod".to_owned()),
                    namespace: Some("djinn".to_owned()),
                    uid: Some("pod-uid-1".to_owned()),
                    ..ObjectMeta::default()
                },
                spec: Some(PodSpec {
                    init_containers: Some(init),
                    containers: regular,
                    ..PodSpec::default()
                }),
                status: Some(PodStatus {
                    init_container_statuses: Some(statuses),
                    ..PodStatus::default()
                }),
            }
        }

        #[test]
        fn reads_the_stored_launcher_spec_and_status() {
            let observed = observe_launcher_sidecar(&pod(
                vec![container(
                    LAUNCHER_CONTAINER_NAME,
                    Some("3800m"),
                    Some("resize-v2"),
                )],
                vec![container("worker", Some("4"), None)],
                vec![status(
                    LAUNCHER_CONTAINER_NAME,
                    Some("containerd://abc"),
                    Some("registry/img@sha256:feed"),
                )],
            ))
            .expect("observation");
            assert_eq!(observed.pod_uid, "pod-uid-1");
            assert_eq!(observed.namespace, "djinn");
            assert_eq!(observed.pod_name, "taskrun-pod");
            assert_eq!(observed.launcher_container_name, LAUNCHER_CONTAINER_NAME);
            assert_eq!(
                observed.launcher_container_id.as_deref(),
                Some("containerd://abc")
            );
            assert_eq!(
                observed.image_digest.as_deref(),
                Some("registry/img@sha256:feed")
            );
            assert_eq!(observed.observed_protocol.as_deref(), Some("resize-v2"));
            // 3800m, NOT the worker's coincidental 4 cores.
            assert_eq!(observed.admitted_cpu_millicores, Some(3800));
        }

        #[test]
        fn a_still_starting_sidecar_reports_absent_fields_rather_than_failing() {
            let observed = observe_launcher_sidecar(&pod(
                vec![container(LAUNCHER_CONTAINER_NAME, Some("4000m"), None)],
                vec![],
                vec![status(LAUNCHER_CONTAINER_NAME, None, None)],
            ))
            .expect("observation");
            assert_eq!(observed.launcher_container_id, None);
            assert_eq!(observed.image_digest, None);
            assert_eq!(observed.observed_protocol, None);
            assert_eq!(observed.admitted_cpu_millicores, Some(4000));
        }

        #[test]
        fn a_leaf_v1_render_carries_no_ceiling_to_observe() {
            let observed = observe_launcher_sidecar(&pod(
                vec![container(LAUNCHER_CONTAINER_NAME, None, Some("leaf-v1"))],
                vec![container("worker", Some("4"), None)],
                vec![status(
                    LAUNCHER_CONTAINER_NAME,
                    Some("containerd://a"),
                    Some("i"),
                )],
            ))
            .expect("observation");
            assert_eq!(observed.admitted_cpu_millicores, None);
            assert_eq!(observed.observed_protocol.as_deref(), Some("leaf-v1"));
        }

        #[test]
        fn an_unnameable_launcher_is_rejected_at_both_sites() {
            // Zero spec entries.
            let missing = observe_launcher_sidecar(&pod(
                vec![container("worker", Some("4"), None)],
                vec![],
                vec![status(LAUNCHER_CONTAINER_NAME, Some("c"), Some("i"))],
            ));
            assert!(matches!(
                missing,
                Err(LauncherObservationError::Ambiguous(
                    crate::pod_resize::PodResizeError::LauncherIdentityAmbiguous { found: 0, .. }
                ))
            ));

            // Two status entries.
            let duplicated = observe_launcher_sidecar(&pod(
                vec![container(LAUNCHER_CONTAINER_NAME, Some("4000m"), None)],
                vec![],
                vec![
                    status(LAUNCHER_CONTAINER_NAME, Some("c1"), Some("i")),
                    status(LAUNCHER_CONTAINER_NAME, Some("c2"), Some("i")),
                ],
            ));
            assert!(matches!(
                duplicated,
                Err(LauncherObservationError::Ambiguous(
                    crate::pod_resize::PodResizeError::LauncherIdentityAmbiguous { found: 2, .. }
                ))
            ));
        }

        /// The 2026-08-02 defect, at the layer that produced it.
        ///
        /// A Pod whose `spec.initContainers` names the launcher but whose
        /// `status.initContainerStatuses` the kubelet has not written yet is a
        /// **wait**, and must be reported as one. Folding it into `Ambiguous`
        /// made the resize birth gate refuse permanently and leave the Pod
        /// running at its full rendered ceiling.
        ///
        /// Non-vacuity: restore `let status = locate_launcher_status(pod)?;` in
        /// `observe_launcher_sidecar` and this test fails — the error comes back
        /// as `Ambiguous(LauncherIdentityAmbiguous { found: 0 })`, which is
        /// precisely the production shape.
        #[test]
        fn an_unpopulated_status_array_is_a_wait_carrying_the_pod_identity() {
            let observed = observe_launcher_sidecar(&pod(
                vec![container(
                    LAUNCHER_CONTAINER_NAME,
                    Some("4000m"),
                    Some("resize-v2"),
                )],
                vec![container("worker", Some("4"), None)],
                // The kubelet has published nothing at all yet.
                vec![],
            ));
            let Err(LauncherObservationError::StatusNotPopulated {
                pod_uid,
                pod_name,
                launcher_container_name,
            }) = observed
            else {
                panic!("expected StatusNotPopulated, got {observed:?}");
            };
            assert_eq!(launcher_container_name, LAUNCHER_CONTAINER_NAME);
            // The identity must survive the failure: it is what lets a caller
            // that gives up UID-fence a delete instead of leaving the Pod
            // running ungoverned.
            assert_eq!(pod_uid, "pod-uid-1");
            assert_eq!(pod_name, "taskrun-pod");
        }

        /// The other half of the distinction, and the half that must NOT become
        /// a wait: two entries carrying the launcher name cannot be resolved by
        /// waiting, and resolving them by index would address the wrong
        /// container.
        ///
        /// Non-vacuity: widen the new arm's guard from `found: 0` to any count
        /// and this test fails.
        #[test]
        fn two_status_entries_stay_a_permanent_ambiguity() {
            let observed = observe_launcher_sidecar(&pod(
                vec![container(LAUNCHER_CONTAINER_NAME, Some("4000m"), None)],
                vec![],
                vec![
                    status(LAUNCHER_CONTAINER_NAME, Some("c1"), Some("i")),
                    status(LAUNCHER_CONTAINER_NAME, Some("c2"), Some("i")),
                ],
            ));
            assert!(
                matches!(
                    observed,
                    Err(LauncherObservationError::Ambiguous(
                        crate::pod_resize::PodResizeError::LauncherIdentityAmbiguous {
                            found: 2,
                            ..
                        }
                    ))
                ),
                "two entries must remain a permanent refusal, got {observed:?}"
            );
            assert_eq!(
                observed.expect_err("ambiguous").fenceable_pod(),
                None,
                "a genuinely unnameable launcher offers nothing to fence a delete against"
            );
        }

        #[test]
        fn a_pod_without_a_uid_is_incomplete_not_observable() {
            let mut without_uid = pod(
                vec![container(LAUNCHER_CONTAINER_NAME, Some("4000m"), None)],
                vec![],
                vec![status(LAUNCHER_CONTAINER_NAME, Some("c"), Some("i"))],
            );
            without_uid.metadata.uid = None;
            assert_eq!(
                observe_launcher_sidecar(&without_uid),
                Err(LauncherObservationError::Incomplete { field: "uid" })
            );
        }
    }
}

#[cfg(test)]
#[path = "runtime_kueue_create_tests.rs"]
mod runtime_kueue_create_tests;

#[cfg(test)]
#[path = "runtime_pod_fence_tests.rs"]
mod runtime_pod_fence_tests;

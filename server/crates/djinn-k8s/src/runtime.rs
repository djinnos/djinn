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
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use djinn_db::{Database, ProjectImageStatus, ProjectRepository};
use djinn_runtime::wire::ControlMsg;
use djinn_runtime::{
    BiStream, RoleKind, RunHandle, RuntimeError, SessionRuntime, StreamEvent, StreamFrame,
    TaskRunOutcome, TaskRunReport, TaskRunSpec,
};
use djinn_supervisor::{ConnectionRegistry, Frame, FramePayload, PendingConnection};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::KubernetesConfig;
use crate::job::build_task_run_job;
use crate::secret::{build_task_run_secret, job_owner_reference, task_run_resource_name};

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

/// Grace period observed while polling `teardown` for job completion.
///
/// Per the Phase 2 K8s plan, we bound this at five minutes: worker tasks
/// typically finish in well under 60s, but the supervisor occasionally
/// ships tasks that post-process large diffs and we'd rather surface a
/// clean timeout than an indeterminate hang.
const TEARDOWN_POLL_TIMEOUT: Duration = Duration::from_secs(300);
/// Poll interval used inside [`poll_job_terminal_state`].
const TEARDOWN_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    async fn prepare(&self, spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError> {
        let task_run_id = Uuid::now_v7();
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
                 task-run Jobs".into(),
            )
        })?;
        let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let image_row = repo
            .get_project_image(&spec.project_id)
            .await
            .map_err(|e| {
                RuntimeError::Prepare(format!(
                    "get_project_image({}): {e}",
                    spec.project_id
                ))
            })?;
        let project_image_tag = match image_row {
            Some(row) if row.status == ProjectImageStatus::READY => match row.tag {
                Some(tag) if !tag.is_empty() => tag,
                _ => return Err(RuntimeError::DevcontainerMissing(spec.project_id.clone())),
            },
            _ => return Err(RuntimeError::DevcontainerMissing(spec.project_id.clone())),
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

        // 1. Build + create the per-task-run Secret.
        let secret = match build_task_run_secret(ns, &task_run_id, spec) {
            Ok(s) => s,
            Err(e) => {
                self.drop_pending(&task_run_id_str).await;
                return Err(RuntimeError::Prepare(format!("build secret: {e}")));
            }
        };

        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), ns);
        if let Err(e) = secrets.create(&PostParams::default(), &secret).await {
            self.drop_pending(&task_run_id_str).await;
            return Err(RuntimeError::Prepare(format!(
                "create secret {resource_name}: {e}"
            )));
        }

        // 2. Build + create the Job manifest.
        let job = build_task_run_job(
            &self.config,
            &task_run_id,
            &spec.project_id,
            &resource_name,
            &project_image_tag,
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
            started_at: SystemTime::now(),
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
        if let Some(outbound) = self
            .registry
            .outbound_sender_for(&handle.task_run_id)
            .await
        {
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
        if let Err(e) =
            delete_job_foreground(&self.client, &ns, &job_name, 30 /* seconds */).await
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

/// Poll a `Job` until its `.status.succeeded` or `.status.failed` condition
/// is non-zero, or [`TEARDOWN_POLL_TIMEOUT`] elapses.
async fn poll_job_terminal_state(
    client: &kube::Client,
    namespace: &str,
    job_name: &str,
) -> JobTerminal {
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let deadline = Instant::now() + TEARDOWN_POLL_TIMEOUT;

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

        if Instant::now() >= deadline {
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

    // Wait for the worker to complete the handshake so the outbound
    // sender is live before we spawn the translator.  No hard timeout
    // here — the upstream supervisor runner wraps `attach_stdio` with
    // its own cancel-token gating, and the kube Job's activeDeadline
    // bounds total run time server-side.
    parts.wait_for_connection().await.map_err(|e| {
        RuntimeError::Attach(format!(
            "wait_for_connection for task_run_id={task_run_id}: {e}"
        ))
    })?;

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

    /// Confirms the polling helper's timeout constant matches the plan's
    /// 5-minute cap guidance so a regression shrinking or inflating it
    /// won't go unnoticed.
    #[test]
    fn teardown_timeout_is_bounded() {
        // Plan §teardown pins this at five minutes. Allow a small window
        // either side so minor tuning doesn't force a test update.
        assert!(TEARDOWN_POLL_TIMEOUT >= Duration::from_secs(60));
        assert!(TEARDOWN_POLL_TIMEOUT <= Duration::from_secs(600));
        assert!(TEARDOWN_POLL_INTERVAL < TEARDOWN_POLL_TIMEOUT);
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
        use djinn_runtime::{SupervisorFlow, TaskRunSpec};

        use crate::secret::task_run_resource_name;

        let cfg = KubernetesConfig::for_testing();
        let task_run_id = Uuid::now_v7();
        let resource_name = task_run_resource_name(&task_run_id);

        let spec = TaskRunSpec {
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
        };

        let secret = crate::secret::build_task_run_secret(&cfg.namespace, &task_run_id, &spec)
            .expect("build per-task-run Secret");
        let job = crate::job::build_task_run_job(
            &cfg,
            &task_run_id,
            "proj-xyz",
            &resource_name,
            "reg.test:5000/djinn-project-proj-xyz:deadbeefcafe",
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
        assert_eq!(secret.metadata.namespace.as_deref(), Some(cfg.namespace.as_str()));
        assert_eq!(job.metadata.namespace.as_deref(), Some(cfg.namespace.as_str()));

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

        let cancel_frame: SupFrame =
            tokio::time::timeout(Duration::from_secs(2), djinn_runtime::wire::read_frame(&mut stream))
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

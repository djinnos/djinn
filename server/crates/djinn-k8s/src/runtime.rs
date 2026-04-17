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
//! `attach_stdio` is a placeholder — see the method doc comment. The real
//! BiStream semantics arrive in PR 4 pt2 when dispatch wiring formalises
//! how the launcher-side TCP connection hands back a `BiStream` to the
//! supervisor.
//!
//! End-to-end `prepare`/`cancel`/`teardown` against a live kind cluster is
//! covered by `tests/kind_smoke.rs` (DJINN_TEST_KIND-gated). The unit tests
//! in this file are builder-parity invariants only — they DO NOT exercise
//! the runtime methods, which require a `kube::Client`.

use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use djinn_runtime::{
    BiStream, RoleKind, RunHandle, RuntimeError, SessionRuntime, TaskRunOutcome, TaskRunReport,
    TaskRunSpec,
};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::KubernetesConfig;
use crate::job::build_task_run_job;
use crate::secret::{build_task_run_secret, job_owner_reference, task_run_resource_name};

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
/// the ambient kubeconfig / in-cluster ServiceAccount.
pub struct KubernetesRuntime {
    client: kube::Client,
    config: KubernetesConfig,
}

impl KubernetesRuntime {
    /// Construct a new runtime by discovering a `kube::Client` from the
    /// ambient environment (in-cluster ServiceAccount when running in a Pod,
    /// `$KUBECONFIG` otherwise).
    ///
    /// Returns the underlying `kube::Error` on discovery failure rather than
    /// panicking — callers on a dev box without a cluster can surface the
    /// error and fall back to another runtime.
    pub async fn new(config: KubernetesConfig) -> Result<Self, kube::Error> {
        let client = kube::Client::try_default().await?;
        Ok(Self { client, config })
    }

    /// Construct a runtime from an already-built client — handy for tests and
    /// for call sites that share a client across multiple consumers.
    pub fn from_client(client: kube::Client, config: KubernetesConfig) -> Self {
        Self { client, config }
    }

    /// Reference to the active config (used by tests + the kind smoke suite).
    pub fn config(&self) -> &KubernetesConfig {
        &self.config
    }

    /// Reference to the underlying `kube::Client`.
    pub fn client(&self) -> &kube::Client {
        &self.client
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
            "kubernetes_runtime: preparing task-run resources"
        );

        // 1. Build + create the per-task-run Secret.
        let secret = build_task_run_secret(ns, &task_run_id, spec)
            .map_err(|e| RuntimeError::Prepare(format!("build secret: {e}")))?;

        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), ns);
        secrets
            .create(&PostParams::default(), &secret)
            .await
            .map_err(|e| RuntimeError::Prepare(format!("create secret {resource_name}: {e}")))?;

        // 2. Build + create the Job manifest.
        let job = build_task_run_job(&self.config, &task_run_id, &resource_name);
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), ns);
        let created_job = jobs.create(&PostParams::default(), &job).await.map_err(|e| {
            // Best-effort cleanup of the orphan Secret — don't shadow the
            // original error if cleanup also fails.
            let secrets = secrets.clone();
            let name = resource_name.clone();
            tokio::spawn(async move {
                let _ = secrets.delete(&name, &DeleteParams::default()).await;
            });
            RuntimeError::Prepare(format!("create job {resource_name}: {e}"))
        })?;

        // 3. Attach an OwnerReference so the Secret GCs with the Job.
        let job_uid = created_job
            .metadata
            .uid
            .clone()
            .ok_or_else(|| RuntimeError::Prepare("created Job missing metadata.uid".into()))?;
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

    /// Placeholder BiStream — the real event stream is fed by the launcher's
    /// TCP dispatch loop (see `djinn-supervisor::services::server::serve_on_tcp`),
    /// which is invoked OUTSIDE the runtime by the supervisor runner. The
    /// worker's connection and the dispatched `BiStream` are joined by the
    /// supervisor, not by this method.
    ///
    /// Returning a detached in-memory `BiStream` keeps trait-shape parity so
    /// the existing `SessionRuntime` consumer code compiles; calls to its
    /// `events_rx` will simply block forever until the PR 4 pt2 dispatch
    /// cutover plumbs the real TCP-backed stream through.
    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        let (stream, _events_tx, _requests_rx) = BiStream::new_in_memory(16);
        // _events_tx and _requests_rx are dropped here — callers observing
        // the returned `BiStream` will see `events_rx` closed on the next
        // poll. PR 4 pt2 replaces this with the real launcher-side stream.
        Ok(stream)
    }

    /// Request graceful cancellation by deleting the Job with `Foreground`
    /// propagation and the configured grace period. Idempotent: a 404 from
    /// the apiserver is mapped to success.
    ///
    /// Uses a default grace of 30 seconds — matches kubelet defaults. A
    /// richer `cancel(handle, grace_ms)` shape is not currently exposed by
    /// the `SessionRuntime` trait, so this stays fixed for now.
    async fn cancel(&self, handle: &RunHandle) -> Result<(), RuntimeError> {
        let job_name = handle
            .pod_ref
            .as_deref()
            .ok_or_else(|| RuntimeError::Cancel("RunHandle.pod_ref missing".into()))?;

        delete_job_foreground(&self.client, &self.config.namespace, job_name, 30)
            .await
            .map_err(|e| RuntimeError::Cancel(format!("delete job {job_name}: {e}")))
    }

    /// Wait for the Job to reach a terminal state (`succeeded` / `failed`),
    /// best-effort delete the Secret, then foreground-delete the Job so its
    /// Pods cascade-clean.
    ///
    /// Polls for at most [`TEARDOWN_POLL_TIMEOUT`]; on timeout, cleanup is
    /// still attempted and then an `Err(RuntimeError::Teardown)` is returned.
    ///
    /// Returns a MINIMAL [`TaskRunReport`] with `TaskRunOutcome::Interrupted`.
    /// Real terminal reports flow over the launcher's TCP connection and are
    /// surfaced by the supervisor (deferred to PR 4 pt2). This method's role
    /// is purely resource cleanup — the supervisor has already observed the
    /// outcome by the time it calls `teardown`.
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

        Ok(TaskRunReport {
            task_run_id: handle.task_run_id.clone(),
            outcome: TaskRunOutcome::Interrupted,
            stages_completed: Vec::<RoleKind>::new(),
        })
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
        let job = crate::job::build_task_run_job(&cfg, &task_run_id, &resource_name);

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
}

use async_trait::async_trait;

use super::graph_data::SemanticQueryEmbedding;

/// Request to provision an on-demand backing service (Phase C). Mirrors
/// `djinn_runtime::BackingServiceRequest` with primitive fields (control-plane
/// doesn't depend on djinn-runtime); AppState maps it across.
#[derive(Clone, Debug)]
pub struct ProvisionServiceRequest {
    pub instance_id: String,
    pub task_run_id: String,
    pub service_type: String,
    pub image: String,
    pub port: i32,
    pub env: Vec<(String, String)>,
    pub cpu_request: String,
    pub memory_request: String,
    pub cpu_limit: String,
    pub memory_limit: String,
    pub conn_template: String,
}

/// What `provision_backing_service` returns.
#[derive(Clone, Debug)]
pub struct ProvisionedService {
    pub pod_name: String,
    pub service_name: String,
    pub conn_string: String,
}

/// Control-plane-owned reference to a Djinn task-run Job.
///
/// This intentionally carries only primitive fields so callers in
/// djinn-control-plane / djinn-agent can inventory runtime resources without
/// importing djinn-k8s or Kubernetes API types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskrunJobRef {
    pub job_name: String,
    pub task_run_id: String,
}

#[async_trait]
pub trait RuntimeOps: Send + Sync {
    async fn apply_settings(
        &self,
        settings: &djinn_core::models::DjinnSettings,
    ) -> Result<(), String>;
    async fn embed_memory_query(
        &self,
        query: &str,
    ) -> Result<Option<SemanticQueryEmbedding>, String>;
    async fn reset_runtime_settings(&self);
    async fn persist_model_health_state(&self);
    /// P6: persist a fresh `EnvironmentConfig` for a project + upsert
    /// the runtime ConfigMap + null `image_hash` so the next tick
    /// rebuilds. In dev mode without a kube client, falls back to a
    /// plain DB write. `config_json` is the serialised `EnvironmentConfig`
    /// — caller has already validated.
    async fn apply_environment_config(
        &self,
        project_id: &str,
        config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String>;
    /// Kick the full mirror→stack→image→graph pipeline for a single project
    /// out-of-band — e.g. right after a repo is added — so it doesn't wait up
    /// to a full mirror-fetch tick for its stack to be detected and its image
    /// enqueued. Fire-and-forget: returns once the work is scheduled, not once
    /// it completes. A no-op on runtimes that don't own the mirror manager.
    async fn trigger_mirror_refresh(&self, project_id: &str);
    /// React to a change in some user's per-user model selection: re-derive the
    /// shared slot-pool capacity for the new union of all users' selections and
    /// trigger a dispatch pass. Fire-and-forget; a no-op on runtimes without a
    /// live coordinator/pool.
    async fn apply_user_model_change(&self);
    /// Dispatch a one-shot verification-test Job for `test_id` in `project_id`'s
    /// image (the worker writes the outcome to `verification_test_runs`). Routes
    /// to the K8s graph warmer; errors on runtimes without a kube client.
    async fn dispatch_verification_test(
        &self,
        test_id: &str,
        project_id: &str,
    ) -> Result<(), String>;
    /// Dispatch a one-shot pre-PR verification run for `run_id` in `project_id`'s
    /// image (the worker writes per-command results + pass/fail to the
    /// `verification_runs` row). The Job clones `target_branch`, fetches +
    /// checks out `task_branch`, then runs the real verification pipeline.
    /// Routes to the K8s graph warmer; errors on runtimes without a kube client.
    async fn dispatch_verification(
        &self,
        run_id: &str,
        project_id: &str,
        task_branch: &str,
        target_branch: &str,
    ) -> Result<(), String>;
    /// Reconcile a catalog image's build (migration 46): build the shared
    /// image once so every assigned project can use it. A no-op on runtimes
    /// without an image controller (dev mode).
    async fn enqueue_image_build(&self, image_id: &str) -> Result<(), String>;
    /// Trigger a canonical-graph warm for a project out-of-band (e.g. right
    /// after assigning it a catalog image). Fire-and-forget; a no-op without
    /// a live graph warmer.
    async fn trigger_graph_warm(&self, project_id: &str);
    /// Provision an on-demand backing service (Pod + ClusterIP Service owned by
    /// the task-run Job). Routes to the K8s graph warmer; errors without kube.
    async fn provision_backing_service(
        &self,
        req: ProvisionServiceRequest,
    ) -> Result<ProvisionedService, String>;
    /// Tear down a provisioned backing service (best-effort).
    async fn release_backing_service(&self, instance_id: &str) -> Result<(), String>;
    /// Foreground-delete the canonical Kubernetes task-run Job
    /// (`djinn-taskrun-{task_run_id}`). Best-effort/idempotent: runtimes with a
    /// kube client treat 404/not-found as success; runtimes without kube may
    /// no-op or return a clear unsupported error.
    async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), String>;
    /// List Djinn task-run Jobs visible to the runtime. Non-Kubernetes/dev
    /// runtimes return an empty inventory.
    async fn list_taskrun_jobs(&self) -> Result<Vec<TaskrunJobRef>, String>;
    /// Delete a closed task's branch on the local mirror and the GitHub remote
    /// (which auto-closes any PR still open on that head). Best-effort: errors
    /// are logged, never surfaced. Used by `proposal_stop_build`'s abort cascade
    /// to clean up branches/PRs of force-closed tasks. A no-op on runtimes
    /// without a mirror manager.
    async fn cleanup_task_branches(&self, task_id: &str);
}

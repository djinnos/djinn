use async_trait::async_trait;

use super::graph_data::SemanticQueryEmbedding;

/// Control-plane-owned reference to a Djinn task-run Job.
///
/// This intentionally carries only primitive fields so callers in
/// djinn-control-plane / djinn-agent can inventory runtime resources without
/// importing djinn-k8s or Kubernetes API types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskrunJobRef {
    pub job_name: String,
    pub task_run_id: String,
    /// The Job's `metadata.creation_timestamp`, as a std wall-clock instant.
    /// Mirrors [`djinn_runtime::TaskrunJobRef::created_at`]; the coordinator's
    /// backstop reaper uses it to grace-period young Jobs whose DB owner rows
    /// may simply not exist yet (the worker creates them from inside the pod
    /// after boot). `None` is treated as old/eligible for reaping.
    pub created_at: Option<std::time::SystemTime>,
}

/// Structured error returned from runtime dispatch calls so callers can
/// distinguish transient `ImageNotReady` (catalog image mid-rebuild) from
/// hard backend failures. The string-form preserved via `Display` is the
/// historical wire format; structured fields let callers decide whether to
/// requeue with backoff or surface a terminal error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeDispatchError {
    /// The project's dispatch image is not currently `ready`. See
    /// [`djinn_runtime::WarmerError::ImageNotReady`] for the underlying
    /// semantics; the bridge preserves the structured fields so callers
    /// can implement a bounded requeue without parsing strings.
    ImageNotReady {
        project_id: String,
        image_status: String,
        tag: Option<String>,
        /// `true` when the missing image is expected to become ready
        /// (catalog image is `building`, or the project is awaiting first
        /// assignment); `false` when the project has no catalog image
        /// selected at all.
        transient: bool,
    },
    /// Any other (hard) runtime failure — K8s API error, kube-client
    /// unavailable, malformed spec, etc. Surfaced as a terminal error
    /// (the caller logs, marks the run row errored, bails).
    Backend(String),
}

impl std::fmt::Display for RuntimeDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImageNotReady {
                project_id,
                image_status,
                transient,
                ..
            } => write!(
                f,
                "project {project_id} image not ready (status: {image_status}, transient: {transient})"
            ),
            Self::Backend(msg) => write!(f, "runtime dispatch error: {msg}"),
        }
    }
}

impl std::error::Error for RuntimeDispatchError {}

impl RuntimeDispatchError {
    /// True when this error represents a transient "image not ready"
    /// condition that the caller may defer / requeue (catalog image is
    /// being rebuilt or project awaits first-time assignment).
    pub fn is_image_not_ready_transient(&self) -> bool {
        matches!(
            self,
            Self::ImageNotReady {
                transient: true,
                ..
            }
        )
    }

    /// True when this error represents a permanently-missing image (no
    /// catalog image selected, project needs configuration).
    pub fn is_image_not_ready_permanent(&self) -> bool {
        matches!(
            self,
            Self::ImageNotReady {
                transient: false,
                ..
            }
        )
    }
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
    /// Reconcile a catalog image's build (migration 46): build the shared
    /// image once so every assigned project can use it. A no-op on runtimes
    /// without an image controller (dev mode).
    async fn enqueue_image_build(&self, image_id: &str) -> Result<(), String>;
    /// Trigger a canonical-graph warm for a project out-of-band (e.g. right
    /// after assigning it a catalog image). Fire-and-forget; a no-op without
    /// a live graph warmer.
    async fn trigger_graph_warm(&self, project_id: &str);
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

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the discriminator contract that dispatch callers rely on: a
    // transient `ImageNotReady` is requeueable; a permanent
    // `ImageNotReady` and any `Backend` error
    // are terminal. The string-form preserved via `Display` is
    // historical; structured fields are what callers actually match on.
    #[test]
    fn runtime_dispatch_error_discriminates_transient_vs_permanent() {
        let transient = RuntimeDispatchError::ImageNotReady {
            project_id: "p1".into(),
            image_status: "building".into(),
            tag: Some("reg/img:abc".into()),
            transient: true,
        };
        assert!(transient.is_image_not_ready_transient());
        assert!(!transient.is_image_not_ready_permanent());
        // Display preserves the structured fields so an operator
        // looking at logs can tell transient vs permanent apart without
        // structured-log parsing.
        let s = transient.to_string();
        assert!(s.contains("transient: true"), "got: {s}");
        assert!(s.contains("status: building"), "got: {s}");

        let permanent = RuntimeDispatchError::ImageNotReady {
            project_id: "p1".into(),
            image_status: "none".into(),
            tag: None,
            transient: false,
        };
        assert!(!permanent.is_image_not_ready_transient());
        assert!(permanent.is_image_not_ready_permanent());
        let s = permanent.to_string();
        assert!(s.contains("transient: false"), "got: {s}");

        let backend = RuntimeDispatchError::Backend("kube apiserver 503".into());
        assert!(!backend.is_image_not_ready_transient());
        assert!(!backend.is_image_not_ready_permanent());
        assert!(backend.to_string().contains("kube apiserver 503"));
    }
}

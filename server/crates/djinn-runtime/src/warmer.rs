//! `GraphWarmerService` — cross-crate trait for canonical-graph warming.
//!
//! The trait is intentionally narrow (two methods) and mirrors how
//! [`crate::SessionRuntime`] evolved: a single object-safe seam that the
//! coordinator and agent lifecycle code dial, with per-deployment
//! implementations (`InProcessGraphWarmer`, `K8sGraphWarmer`) behind it.
//!
//! Phase 3 PR 7 introduces this trait and replaces the legacy
//! `djinn_agent::context::CanonicalGraphWarmer`.  The shape was picked so the
//! future `K8sGraphWarmer` (Phase 3 PR 8) can implement the exact same surface
//! without leaking Job/Kubernetes concerns upward.

use std::time::Duration;

use async_trait::async_trait;

/// Runtime-owned reference to a Kubernetes task-run Job.
///
/// Primitive fields only: this crate is shared by the agent/control-plane
/// layers and must not expose Kubernetes API objects across that boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskrunJobRef {
    pub job_name: String,
    pub task_run_id: String,
    /// The Job's `metadata.creation_timestamp`, as a std wall-clock instant.
    /// The backstop reaper age-gates young Jobs against this: the worker
    /// inserts the `task_runs` row from INSIDE the pod (only after pod
    /// scheduling + image pull + worker boot), so a freshly-created Job
    /// legitimately has no DB owner rows yet. `None` when the Job carries no
    /// timestamp (shouldn't happen for live Jobs); callers treat `None` as
    /// old/eligible so the backstop's cleanup guarantee is preserved.
    pub created_at: Option<std::time::SystemTime>,
}

/// Server-wide canonical-graph warmer.
///
/// The service owns its own single-flight + cache-freshness logic.  Callers
/// talk to it in two shapes:
///
/// * [`GraphWarmerService::trigger`] — fire-and-forget.  Posts a "please warm"
///   intent and returns immediately.  Used from the coordinator tick loop and
///   from the mirror fetcher tail.  Safe to call on cold caches, hot caches,
///   and while a warm is already in flight.
/// * [`GraphWarmerService::await_fresh`] — best-effort wait for a fresh graph.
///   The architect role calls this before starting a session so workers pick
///   up the rendered repo-map note via the standard note pipeline.  The
///   timeout is the backstop — on expiry the method returns `Ok(())` and the
///   architect proceeds without a warm skeleton.
#[async_trait]
pub trait GraphWarmerService: Send + Sync {
    /// Allow downcasting to a concrete implementation. Used by the
    /// coordinator composition root to extract a warm-job guard from
    /// the Kubernetes runtime without leaking Kubernetes types through
    /// the trait boundary.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Fire-and-forget: start a warm if one isn't already in flight for this
    /// project.  Cold-cache / warm-cache / inflight-coalesce policy lives in
    /// the concrete implementation.
    async fn trigger(&self, project_id: &str);

    /// Wait for a warm graph:
    ///
    /// * if the cache is fresher than `ttl` → return immediately,
    /// * if a warm is already in flight → wait up to `timeout` for it to
    ///   complete,
    /// * otherwise call [`Self::trigger`] and wait up to `timeout`.
    ///
    /// On `timeout` expiration, implementations return `Ok(())`.  The caller
    /// (typically the architect lifecycle) proceeds on best-effort; the
    /// backstop must NEVER deadlock.
    async fn await_fresh(
        &self,
        project_id: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<(), WarmerError>;

    /// Foreground-delete the canonical task-run Job (`djinn-taskrun-{task_run_id}`).
    ///
    /// Default impl errors — only a backend that owns a kube client (the
    /// `K8sGraphWarmer`) can delete the Job. The MCP/runtime bridge maps this
    /// to a string error; lifecycle callers treat it as best-effort.
    async fn teardown_taskrun_job(&self, _task_run_id: &str) -> Result<(), WarmerError> {
        Err(WarmerError::Backend(
            "task-run Job teardown requires the kubernetes runtime".to_string(),
        ))
    }

    async fn terminate_taskrun_pod_exact(
        &self,
        _task_run_id: &str,
        _pod_uid: &str,
    ) -> Result<(), WarmerError> {
        Err(WarmerError::Backend(
            "exact-pod task-run termination requires the kubernetes runtime".to_string(),
        ))
    }

    /// List Kubernetes task-run Jobs known to the runtime.
    ///
    /// Default impl returns an empty inventory for non-Kubernetes runtimes so
    /// callers can run the same reconciliation code in dev/test contexts.
    async fn list_taskrun_jobs(&self) -> Result<Vec<TaskrunJobRef>, WarmerError> {
        Ok(Vec::new())
    }
}

/// Errors surfaced by a [`GraphWarmerService`] implementation.
///
/// The concrete cases are intentionally generic so the `K8sGraphWarmer` can
/// wrap `kube::Error` and the `InProcessGraphWarmer` can wrap its string-typed
/// pipeline error without leaking dependencies through this trait.
#[derive(Debug, thiserror::Error)]
pub enum WarmerError {
    /// Backend-specific error from the warmer implementation (e.g. a K8s API
    /// error or a graph pipeline failure).
    #[error("warmer backend error: {0}")]
    Backend(String),
    /// The project's dispatch image is not currently `ready` — either the
    /// assigned catalog image is mid-rebuild (`building`) or the project has
    /// no catalog image selected yet (`none`). The caller can use the
    /// `transient` flag to decide whether to requeue with backoff (catalog
    /// image is being built / first-time selection pending) or to surface
    /// the failure immediately (permanent configuration issue).
    ///
    /// `image_status` carries the raw `ProjectImageStatus` value
    /// (`none` | `building` | `ready` | `failed`) so callers can log /
    /// surface the underlying state. `tag` is the resolved image tag (when
    /// known) so a poller can show "rebuilding X" rather than a generic
    /// "no image" message.
    #[error(
        "project {project_id} image not ready (status: {image_status}, transient: {transient})"
    )]
    ImageNotReady {
        project_id: String,
        image_status: String,
        /// Tag of the assigned catalog image, if any. `None` when the
        /// project has no catalog image selected.
        tag: Option<String>,
        /// `true` when the missing image is expected to become ready
        /// (catalog image is `building`, or the project is awaiting first
        /// assignment); `false` when the project has no catalog image
        /// selected at all and won't be dispatchable until an operator
        /// assigns one.
        transient: bool,
    },
}

impl WarmerError {
    /// True when this error represents a transient "image not ready"
    /// condition that the caller may defer / requeue. False for any
    /// hard backend failure or a permanently-missing image.
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
    /// catalog image assigned, project needs configuration).
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

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the discriminator contract that the K8s graph warmer's image-ready
    // requeue path relies on. The structured `transient` flag is the
    // single source of truth: callers MUST NOT parse the `Display`
    // string to recover this classification.
    #[test]
    fn warmer_error_image_not_ready_transient_vs_permanent() {
        let transient = WarmerError::ImageNotReady {
            project_id: "p1".into(),
            image_status: "building".into(),
            tag: Some("reg/img:abc".into()),
            transient: true,
        };
        assert!(transient.is_image_not_ready_transient());
        assert!(!transient.is_image_not_ready_permanent());
        assert!(transient.to_string().contains("transient: true"));

        let permanent = WarmerError::ImageNotReady {
            project_id: "p1".into(),
            image_status: "none".into(),
            tag: None,
            transient: false,
        };
        assert!(!permanent.is_image_not_ready_transient());
        assert!(permanent.is_image_not_ready_permanent());
        assert!(permanent.to_string().contains("transient: false"));

        let backend = WarmerError::Backend("kube apiserver 503".into());
        assert!(!backend.is_image_not_ready_transient());
        assert!(!backend.is_image_not_ready_permanent());
    }
}

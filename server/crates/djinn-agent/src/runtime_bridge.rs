//! Adapters that bridge `djinn-runtime` / `djinn-k8s` / `djinn-supervisor`
//! for the Phase 2 K8s PR 4 pt2 dispatch cutover.
//!
//! Two adapters live here because neither fits cleanly into any single crate
//! without introducing a dependency cycle:
//!
//! - [`SupervisorTaskRunner`] — impl of
//!   [`djinn_runtime::test_runtime::TaskRunner`] that drives
//!   [`djinn_supervisor::TaskRunSupervisor::run`] in-process.  Used by
//!   [`djinn_runtime::TestRuntime`] on the `DJINN_RUNTIME=test` path (and by
//!   `phase1_supervisor.rs` via `services_for_agent_context_with_provider_override`
//!   in the future, once the test adopts the runtime).  Lives here because
//!   `djinn-runtime` cannot depend on `djinn-supervisor` (cycle).
//!
//! - [`K8sTokenReviewValidator`] — real
//!   [`djinn_supervisor::TokenValidator`] wrapping
//!   [`djinn_k8s::token_review::review_token`].  Lives here so `djinn-supervisor`
//!   stays free of `kube-rs` (and `djinn-k8s` stays free of the supervisor's
//!   wire types).

use std::sync::Arc;

use async_trait::async_trait;
use djinn_runtime::test_runtime::{RunnerCancel, TaskRunner};
use djinn_runtime::{RuntimeError, TaskRunReport, TaskRunSpec};
use djinn_supervisor::{
    SupervisorServices, TaskRunSupervisor, TokenValidation, TokenValidator,
};
use djinn_workspace::MirrorManager;

// ─── SupervisorTaskRunner ───────────────────────────────────────────────────

/// [`TaskRunner`] impl that drives [`TaskRunSupervisor::run`] in-process.
///
/// Used by [`djinn_runtime::TestRuntime`] when the dispatch path picks
/// `RuntimeKind::Test`.  The runner holds onto everything needed to
/// materialize a fresh supervisor per run:
///
/// - `task_runs` — the repo the supervisor writes the `task_run` row into.
/// - `mirror` — the shared [`MirrorManager`] used for the ephemeral clone.
/// - `services` — the already-wired `Arc<dyn SupervisorServices>` that carries
///   the in-process `AgentContext` (and any test-only provider override).
pub struct SupervisorTaskRunner {
    task_runs: Arc<djinn_db::TaskRunRepository>,
    mirror: Arc<MirrorManager>,
    services: Arc<dyn SupervisorServices>,
}

impl SupervisorTaskRunner {
    pub fn new(
        task_runs: Arc<djinn_db::TaskRunRepository>,
        mirror: Arc<MirrorManager>,
        services: Arc<dyn SupervisorServices>,
    ) -> Self {
        Self {
            task_runs,
            mirror,
            services,
        }
    }
}

#[async_trait]
impl TaskRunner for SupervisorTaskRunner {
    async fn run(
        &self,
        spec: TaskRunSpec,
        _cancel: RunnerCancel,
    ) -> Result<TaskRunReport, RuntimeError> {
        // The supervisor already has its own CancellationToken wired via
        // `SupervisorServices::cancel`; the `RunnerCancel` handed to us by
        // `TestRuntime` is cooperative and redundant here.  `TestRuntime`
        // still aborts the spawned task on `cancel` as a hard backstop, so
        // bounded teardown is preserved.
        let supervisor = TaskRunSupervisor::new(
            self.task_runs.clone(),
            self.mirror.clone(),
            self.services.clone(),
        );
        supervisor
            .run(spec)
            .await
            .map_err(|e| RuntimeError::Internal(format!("supervisor run: {e}")))
    }
}

// ─── K8sTokenReviewValidator ────────────────────────────────────────────────

/// Real [`TokenValidator`] for the djinn-server TCP listener.
///
/// Posts the presented bearer token at the in-cluster apiserver's
/// `authentication.k8s.io/v1/TokenReview` endpoint via
/// [`djinn_k8s::token_review::review_token`] and accepts iff the apiserver
/// reports `authenticated: true`.
///
/// The current impl leaves deeper identity checks (e.g. asserting that the SA
/// username encodes the expected task-run id) as a follow-up; the apiserver
/// already verifies the token's audience matches `djinn`, which is the main
/// thing we care about in v1.
pub struct K8sTokenReviewValidator {
    client: kube::Client,
    audience: String,
}

impl K8sTokenReviewValidator {
    pub fn new(client: kube::Client, audience: impl Into<String>) -> Self {
        Self {
            client,
            audience: audience.into(),
        }
    }
}

#[async_trait]
impl TokenValidator for K8sTokenReviewValidator {
    async fn validate(
        &self,
        token: &str,
        _expected_task_run_id: &str,
    ) -> Result<TokenValidation, String> {
        let review = djinn_k8s::token_review::review_token(&self.client, token, &self.audience)
            .await
            .map_err(|e| e.to_string())?;
        Ok(TokenValidation {
            authenticated: review.authenticated,
            username: review.username,
        })
    }
}

// ─── RuntimeKind ────────────────────────────────────────────────────────────

/// Backend the dispatch layer should use for a given task-run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Production path — per-run Kubernetes Job executed by
    /// [`djinn_k8s::KubernetesRuntime`].
    Kubernetes,
    /// In-process path — [`djinn_runtime::TestRuntime`] wrapping a
    /// [`SupervisorTaskRunner`].
    Test,
}

/// Resolve the active [`RuntimeKind`] from the `DJINN_RUNTIME` env var.
///
/// | Value | Result |
/// |---|---|
/// | unset / `"kubernetes"` / `"k8s"` | [`RuntimeKind::Kubernetes`] |
/// | `"test"` / `"in-process"` | [`RuntimeKind::Test`] |
/// | anything else | logs a warning, defaults to Kubernetes |
pub fn runtime_kind() -> RuntimeKind {
    match std::env::var("DJINN_RUNTIME").as_deref() {
        Err(_) | Ok("") | Ok("kubernetes") | Ok("k8s") => RuntimeKind::Kubernetes,
        Ok("test") | Ok("in-process") | Ok("in_process") => RuntimeKind::Test,
        Ok(other) => {
            tracing::warn!(
                value = %other,
                "DJINN_RUNTIME set to unknown value; falling back to kubernetes"
            );
            RuntimeKind::Kubernetes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-only: `SupervisorTaskRunner` is `TaskRunner`.
    #[allow(dead_code)]
    fn _task_runner_obj_safe(_: &dyn TaskRunner) {}

    /// Compile-only: `K8sTokenReviewValidator` is `TokenValidator`.
    #[allow(dead_code)]
    fn _token_validator_obj_safe(_: &dyn TokenValidator) {}

    #[test]
    fn runtime_kind_env_parsing() {
        // SAFETY: single-threaded unit test, no other threads read env.
        unsafe {
            std::env::remove_var("DJINN_RUNTIME");
        }
        assert_eq!(runtime_kind(), RuntimeKind::Kubernetes);

        unsafe {
            std::env::set_var("DJINN_RUNTIME", "test");
        }
        assert_eq!(runtime_kind(), RuntimeKind::Test);

        unsafe {
            std::env::set_var("DJINN_RUNTIME", "kubernetes");
        }
        assert_eq!(runtime_kind(), RuntimeKind::Kubernetes);

        unsafe {
            std::env::set_var("DJINN_RUNTIME", "bogus");
        }
        assert_eq!(runtime_kind(), RuntimeKind::Kubernetes);

        unsafe {
            std::env::remove_var("DJINN_RUNTIME");
        }
    }
}

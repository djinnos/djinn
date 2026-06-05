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

    /// Dispatch a one-shot verification "test" run for a candidate rule set in
    /// the project's image, writing pass/fail + per-command output to the
    /// `verification_test_runs` row identified by `test_id`. Fire-and-forget:
    /// returns once the Job is created, not once it finishes (poll the row).
    ///
    /// Default impl errors — only a backend that owns a kube client + runtime
    /// config (the `K8sGraphWarmer`) can dispatch the Job. The MCP layer calls
    /// this through the `RuntimeOps` bridge. (Lives on this trait because the
    /// K8s warmer is the one persistent handle that already owns the one-shot
    /// Job dispatcher + project-image resolution.)
    async fn dispatch_verification_test(
        &self,
        _test_id: &str,
        _project_id: &str,
    ) -> Result<(), WarmerError> {
        Err(WarmerError::Backend(
            "verification test dispatch requires the kubernetes runtime".to_string(),
        ))
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
}

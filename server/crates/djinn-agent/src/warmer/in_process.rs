//! In-process [`GraphWarmerService`] implementation.
//!
//! Wraps the server's `ensure_canonical_graph` pipeline behind three
//! callbacks so djinn-agent does not depend on the server crate.  The
//! callbacks encapsulate project-root resolution, cache-freshness checking,
//! and the heavy warm pipeline itself.
//!
//! Used by `AppState` in production and by `TestRuntime` contexts that want a
//! single-process warmer.  The `K8sGraphWarmer` (Phase 3 PR 8) is a peer
//! implementation — it does not use this module.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use djinn_runtime::{GraphWarmerService, WarmerError};

/// Callback that drives a single warm attempt to completion.
///
/// The underlying implementation already fast-paths against the in-memory
/// cache, single-flights on `project_id`, and detaches the heavy pipeline
/// onto a background task.  Calling it a second time for a project that is
/// already being warmed is a no-op that returns quickly.
pub type WarmCallback = Arc<
    dyn Fn(
            String,
            PathBuf,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>
        + Send
        + Sync,
>;

/// Callback that resolves a project id to the on-disk project root used by
/// the warm pipeline.
///
/// Returns `None` when the project has been deleted or cannot be resolved —
/// the warmer treats this as a non-fatal signal and skips the call.
pub type ProjectRootResolver = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<PathBuf>> + Send + 'static>>
        + Send
        + Sync,
>;

/// Callback that decides whether the canonical-graph cache is considered
/// fresh for the given project.  Receives the resolved project root and the
/// caller-supplied TTL — implementations may ignore the TTL if their
/// freshness model is commit-SHA based rather than wall-clock based.
pub type FreshnessProbe = Arc<
    dyn Fn(
            String,
            PathBuf,
            Duration,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'static>>
        + Send
        + Sync,
>;

/// Dependencies wired into an [`InProcessGraphWarmer`] at construction time.
///
/// All three callbacks are required; there is no degraded-mode fallback.
/// The intentional shape keeps this crate free of any server-side imports —
/// the production impl lives in `server::AppState::agent_context()`.
#[derive(Clone)]
pub struct InProcessWarmerDeps {
    pub warm: WarmCallback,
    pub project_root: ProjectRootResolver,
    pub is_fresh: FreshnessProbe,
}

/// Polling interval used by [`InProcessGraphWarmer::await_fresh`] while it
/// waits for a background warm to populate the cache.  Chosen to be well
/// below any realistic warm completion time while staying cheap (the probe
/// is an in-memory RwLock read).
const AWAIT_FRESH_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// In-process canonical-graph warmer.
///
/// Delegates the heavy warm pipeline to a caller-supplied [`WarmCallback`]
/// whose internals already handle fast-path / single-flight / detach.  The
/// `await_fresh` implementation polls the [`FreshnessProbe`] with a short
/// interval until either the cache goes fresh or the supplied `timeout`
/// elapses — in the latter case the method returns `Ok(())` so the caller
/// proceeds best-effort.
pub struct InProcessGraphWarmer {
    deps: InProcessWarmerDeps,
}

impl InProcessGraphWarmer {
    pub fn new(deps: InProcessWarmerDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl GraphWarmerService for InProcessGraphWarmer {
    async fn trigger(&self, project_id: &str) {
        let Some(project_root) = (self.deps.project_root)(project_id.to_string()).await else {
            tracing::debug!(
                project_id = %project_id,
                "InProcessGraphWarmer::trigger: project root unresolved, skipping"
            );
            return;
        };

        if let Err(error) = (self.deps.warm)(project_id.to_string(), project_root).await {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "InProcessGraphWarmer::trigger: warm callback returned error (swallowed)"
            );
        }
    }

    async fn await_fresh(
        &self,
        project_id: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<(), WarmerError> {
        let Some(project_root) = (self.deps.project_root)(project_id.to_string()).await else {
            // Unknown project — treat as "nothing to warm", return Ok so the
            // caller proceeds on best-effort.
            tracing::debug!(
                project_id = %project_id,
                "InProcessGraphWarmer::await_fresh: project root unresolved, returning Ok"
            );
            return Ok(());
        };

        if (self.deps.is_fresh)(project_id.to_string(), project_root.clone(), ttl).await {
            return Ok(());
        }

        // Fire a (single-flight-safe) warm and poll the freshness probe until
        // the cache goes fresh or the timeout backstop fires.
        self.trigger(project_id).await;

        let deadline = Instant::now() + timeout;
        loop {
            if (self.deps.is_fresh)(project_id.to_string(), project_root.clone(), ttl).await {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                tracing::info!(
                    project_id = %project_id,
                    timeout_ms = timeout.as_millis() as u64,
                    "InProcessGraphWarmer::await_fresh: timed out waiting for warm; proceeding best-effort"
                );
                return Ok(());
            }
            let sleep_for = remaining.min(AWAIT_FRESH_POLL_INTERVAL);
            tokio::time::sleep(sleep_for).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn deps_with_counts(
        is_fresh_sequence: Arc<dyn Fn(usize) -> bool + Send + Sync>,
    ) -> (InProcessWarmerDeps, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let warm_count = Arc::new(AtomicUsize::new(0));
        let fresh_calls = Arc::new(AtomicUsize::new(0));

        let warm_count_clone = warm_count.clone();
        let warm: WarmCallback = Arc::new(move |_, _| {
            let warm_count_clone = warm_count_clone.clone();
            Box::pin(async move {
                warm_count_clone.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        let project_root: ProjectRootResolver =
            Arc::new(|_| Box::pin(async { Some(PathBuf::from("/tmp/fake")) }));

        let fresh_calls_clone = fresh_calls.clone();
        let is_fresh: FreshnessProbe = Arc::new(move |_, _, _| {
            let fresh_calls_clone = fresh_calls_clone.clone();
            let is_fresh_sequence = is_fresh_sequence.clone();
            Box::pin(async move {
                let idx = fresh_calls_clone.fetch_add(1, Ordering::SeqCst);
                (is_fresh_sequence)(idx)
            })
        });

        (
            InProcessWarmerDeps {
                warm,
                project_root,
                is_fresh,
            },
            warm_count,
            fresh_calls,
        )
    }

    #[tokio::test]
    async fn await_fresh_returns_immediately_when_cache_is_fresh() {
        let (deps, warm_count, _) = deps_with_counts(Arc::new(|_| true));
        let warmer = InProcessGraphWarmer::new(deps);
        warmer
            .await_fresh("p1", Duration::from_secs(60), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(warm_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn await_fresh_triggers_warm_and_returns_when_cache_becomes_fresh() {
        // First call (initial freshness check) returns false.
        // Subsequent calls (polling loop) return true.
        let (deps, warm_count, _) = deps_with_counts(Arc::new(|idx| idx > 0));
        let warmer = InProcessGraphWarmer::new(deps);
        warmer
            .await_fresh("p1", Duration::from_secs(60), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(warm_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn await_fresh_times_out_and_returns_ok_when_cache_never_fresh() {
        let (deps, _warm_count, _) = deps_with_counts(Arc::new(|_| false));
        let warmer = InProcessGraphWarmer::new(deps);
        let started = Instant::now();
        warmer
            .await_fresh("p1", Duration::from_secs(60), Duration::from_millis(300))
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(250), "elapsed={:?}", elapsed);
        assert!(elapsed < Duration::from_secs(2), "elapsed={:?}", elapsed);
    }
}

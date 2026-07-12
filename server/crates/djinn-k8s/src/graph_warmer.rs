//! [`K8sGraphWarmer`] — [`djinn_runtime::GraphWarmerService`] implementation
//! that runs the canonical-graph warm pipeline inside an ephemeral
//! Kubernetes Job.
//!
//! Phase 3 PR 8 §6.3 / §6.6. Peer implementation of
//! [`djinn_agent::warmer::InProcessGraphWarmer`]; the trait is shared, the
//! backend swaps via `AppState::build_in_process_graph_warmer` vs the
//! K8s variant depending on `DJINN_RUNTIME` and kube-client availability.
//!
//! ## Flow
//!
//! * [`K8sGraphWarmer::trigger`] — check single-flight guard; if another
//!   warm is already in flight for the project, return immediately. Else
//!   resolve `projects.image_tag` via the DB, create a warm Job via the
//!   per-project image, record a [`tokio::sync::Notify`] keyed by
//!   `project_id`, and spawn a watcher that polls Job terminal status and
//!   notifies waiters when the warm completes (either outcome).
//! * [`K8sGraphWarmer::await_fresh`] — probe `repo_graph_cache` for a
//!   freshness-window hit against the project's current `origin/main`
//!   commit (if determinable). On a hit, return immediately. Else subscribe
//!   to the in-flight [`Notify`] (triggering one if absent) and wait up to
//!   the caller-supplied `timeout`. On timeout the method returns `Ok(())`
//!   per the trait contract — the architect proceeds best-effort.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use djinn_core::clock::{Clock, SystemClock};

use async_trait::async_trait;
use djinn_db::{Database, ProjectRepository, RepoGraphCacheRepository};
use djinn_runtime::{GraphWarmerService, TaskrunJobRef, WarmerError};
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, ListParams, PostParams};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

use crate::config::KubernetesConfig;
use crate::warm_job::{LABEL_PROJECT_ID, LABEL_WARM, build_warm_job};

/// Interval used by the Job-watcher loop spawned by [`K8sGraphWarmer::trigger`]
/// to poll `.status.succeeded` / `.status.failed`.
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Backstop cap on how long the watcher loop will poll before giving up
/// and notifying anyway. The Job's `activeDeadlineSeconds` already bounds
/// the cluster-side cost; this is a belt-and-braces guard against watcher
/// leaks if the apiserver returns persistent errors.
const WATCH_DEADLINE: Duration = Duration::from_secs(3600);

/// Default quiet-window for the merge-storm debounce (`DJINN_WARM_DEBOUNCE_SECONDS`).
/// A few minutes: long enough that a burst of PRs landing on `main` every
/// couple of minutes collapses into a single warm run, short enough that a
/// genuinely idle main is re-warmed promptly.
const DEFAULT_WARM_DEBOUNCE_SECONDS: u64 = 180;
/// Default anti-starvation cap (`DJINN_WARM_DEBOUNCE_MAX_WAIT_SECONDS`). A
/// continuously-advancing `main` (a long merge storm) must still be warmed
/// eventually; once the head has been advancing for this long we stop deferring
/// and warm the current tip.
const DEFAULT_WARM_DEBOUNCE_MAX_WAIT_SECONDS: u64 = 900;

/// Temporal debounce policy for automatic head-advance warm triggers.
///
/// This is purely a *scheduling* control — it changes only WHEN an automatic
/// trigger dispatches a warm Job, never WHETHER one is warranted (every
/// existing gate — TTL freshness, current-commit, in-flight, image-readiness —
/// is still enforced at dispatch time). Capacity/slot gating (deferring warms
/// while build pods are busy) is deliberately out of scope; that belongs to the
/// in-refinement "compilation slots" proposal.
///
/// `quiet == 0` disables debouncing entirely: every trigger dispatches
/// immediately, exactly reproducing the pre-debounce behaviour.
#[derive(Clone, Copy, Debug)]
pub struct WarmDebounceConfig {
    /// Quiet window: after a head-advance trigger, defer the warm until `main`
    /// has been quiet (no further trigger) for this long. Each new trigger in
    /// the burst re-arms the window (last-wins), so a storm collapses into one
    /// run after it settles.
    pub quiet: Duration,
    /// Hard cap on total deferral, measured from the FIRST trigger of a burst.
    /// A continuously-advancing head would otherwise re-arm the quiet window
    /// forever; once this elapses we warm the current tip regardless.
    pub max_wait: Duration,
}

impl WarmDebounceConfig {
    /// Debounce disabled — every trigger dispatches immediately (pre-debounce
    /// behaviour). Used by the test constructors so existing tests keep their
    /// synchronous semantics; production reads [`Self::from_env`].
    pub const DISABLED: Self = Self {
        quiet: Duration::ZERO,
        max_wait: Duration::ZERO,
    };

    /// Load the debounce policy from the environment, falling back to the
    /// few-minutes / 15-minute defaults. A malformed value is logged at `warn`
    /// and the default is kept so the warmer still boots.
    pub fn from_env() -> Self {
        Self {
            quiet: Duration::from_secs(env_secs(
                "DJINN_WARM_DEBOUNCE_SECONDS",
                DEFAULT_WARM_DEBOUNCE_SECONDS,
            )),
            max_wait: Duration::from_secs(env_secs(
                "DJINN_WARM_DEBOUNCE_MAX_WAIT_SECONDS",
                DEFAULT_WARM_DEBOUNCE_MAX_WAIT_SECONDS,
            )),
        }
    }

    /// True when the quiet window is non-zero, i.e. debouncing is active.
    fn enabled(&self) -> bool {
        !self.quiet.is_zero()
    }

    /// Effective max-wait, floored at `quiet` so a misconfiguration where
    /// `max_wait < quiet` can never make the hard deadline fire *before* the
    /// first quiet window would.
    fn effective_max_wait(&self) -> Duration {
        self.max_wait.max(self.quiet)
    }
}

/// Parse a `u64` seconds value from `key`, warning and returning `default` on
/// absence or a parse error.
fn env_secs(key: &str, default: u64) -> u64 {
    match std::env::var(key) {
        Ok(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(e) => {
                warn!(
                    key,
                    value = %v,
                    error = %e,
                    "warm debounce: env var is not a valid u64 (seconds) — keeping default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

/// Live counters for the debounce coalescer. Cheap relaxed atomics — read via
/// [`K8sGraphWarmer::debounce_metrics`] for observability/tests.
#[derive(Default)]
struct WarmDebounceMetrics {
    /// Every automatic head-advance trigger received (`K8sGraphWarmer::trigger`).
    triggers_received: AtomicU64,
    /// Triggers folded into an already-pending debounce window (a storm merge
    /// that did not itself launch a warm).
    triggers_coalesced: AtomicU64,
    /// Debounce windows that collapsed into exactly one dispatched warm run.
    warms_debounced: AtomicU64,
}

/// Point-in-time snapshot of [`WarmDebounceMetrics`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WarmDebounceMetricsSnapshot {
    pub triggers_received: u64,
    pub triggers_coalesced: u64,
    pub warms_debounced: u64,
}

/// Per-project pending debounce window. Exactly one exists (and exactly one
/// driver task runs) for a project between the first trigger of a burst and the
/// window's collapse into a dispatch.
struct PendingWarm {
    /// When the (extendable) quiet window currently expires. Re-armed to
    /// `now + quiet` on each trigger, clamped to `hard_deadline`.
    fire_at: Instant,
    /// `first_trigger + max_wait` — the ceiling `fire_at` can never exceed, so
    /// a continuous storm still warms within the anti-starvation bound.
    hard_deadline: Instant,
    /// How many triggers have folded into this window (for the collapse log).
    coalesced: u64,
}

/// Abstraction used by [`K8sGraphWarmer`] to actually create a Job in the
/// cluster. Factored into a trait so unit tests can supply a mock that
/// records the manifest without a live apiserver — the production impl
/// dispatches straight to `kube::Api::<Job>::create`.
#[async_trait]
pub trait WarmJobDispatcher: Send + Sync {
    /// Create the supplied `Job` in the given namespace and return the
    /// server-assigned name (or an error). Implementations wrap
    /// `kube::Error` in-place; the dispatcher trait intentionally doesn't
    /// surface Kubernetes types so the test-dispatcher doesn't have to
    /// reach for a full `kube::Client`.
    async fn dispatch(&self, namespace: &str, job: Job) -> Result<String, String>;
}

/// Production dispatcher backed by a live `kube::Client`.
pub struct KubeClientDispatcher {
    client: kube::Client,
}

impl KubeClientDispatcher {
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl WarmJobDispatcher for KubeClientDispatcher {
    async fn dispatch(&self, namespace: &str, job: Job) -> Result<String, String> {
        let api: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        let created = api
            .create(&PostParams::default(), &job)
            .await
            .map_err(|e| e.to_string())?;
        Ok(created
            .metadata
            .name
            .unwrap_or_else(|| "unnamed-warm-job".to_string()))
    }
}

/// Terminal outcome reported by a [`WarmJobWatcher`]. Drives the in-process
/// convergence hook: on [`WarmTerminalOutcome::Succeeded`] the warm Job pod has
/// already rewritten `repo_graph_cache`, so the server invalidates its RAM slot
/// (see [`WarmCompletionSink`]); on [`WarmTerminalOutcome::Failed`] the row is
/// unchanged and no convergence is needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmTerminalOutcome {
    /// The warm Job reported `.status.succeeded > 0` (or completed and was
    /// reaped before we observed it — see the 404 branch), meaning a fresh
    /// blob was persisted.
    Succeeded,
    /// The warm Job reported `.status.failed > 0`, or the watcher gave up at
    /// its deadline without observing success. No fresh blob was persisted.
    Failed,
}

/// Optional Job-terminal watcher. Production uses
/// [`KubeClientJobWatcher`]; tests pass [`NoopJobWatcher`] to keep the
/// unit tests free of any apiserver dependency.
#[async_trait]
pub trait WarmJobWatcher: Send + Sync {
    /// Poll the Job `job_name` in `namespace` until it reaches a terminal
    /// state (succeeded OR failed) or the watcher's internal deadline
    /// elapses. Implementations MUST NOT block forever. The returned
    /// [`WarmTerminalOutcome`] tells the caller whether a fresh graph blob was
    /// persisted (→ trigger in-process cache convergence).
    async fn wait_terminal(&self, namespace: &str, job_name: &str) -> WarmTerminalOutcome;
}

/// Production watcher backed by `kube::Api::<Job>::get`. Polls on
/// [`WATCH_POLL_INTERVAL`].
pub struct KubeClientJobWatcher {
    client: kube::Client,
}

impl KubeClientJobWatcher {
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl WarmJobWatcher for KubeClientJobWatcher {
    async fn wait_terminal(&self, namespace: &str, job_name: &str) -> WarmTerminalOutcome {
        let api: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        let deadline = SystemClock::new().now_instant() + WATCH_DEADLINE;
        loop {
            match api.get(job_name).await {
                Ok(job) => {
                    if let Some(status) = job.status.as_ref() {
                        if status.succeeded.unwrap_or(0) > 0 {
                            debug!(job = %job_name, "K8sGraphWarmer watcher: succeeded");
                            return WarmTerminalOutcome::Succeeded;
                        }
                        if status.failed.unwrap_or(0) > 0 {
                            warn!(job = %job_name, "K8sGraphWarmer watcher: failed");
                            return WarmTerminalOutcome::Failed;
                        }
                    }
                }
                Err(kube::Error::Api(resp)) if resp.code == 404 => {
                    // The Job is gone. `ttlSecondsAfterFinished` only reaps
                    // *finished* Jobs, and a Job that ran to completion so fast
                    // it was reaped before our first poll is overwhelmingly a
                    // success, so we treat "gone" as Succeeded and let the
                    // server converge its RAM slot. A needless reload on the
                    // rare reaped-failure is harmless (it re-reads the same
                    // latest persisted row).
                    debug!(job = %job_name, "K8sGraphWarmer watcher: job gone (treating as succeeded)");
                    return WarmTerminalOutcome::Succeeded;
                }
                Err(e) => {
                    warn!(
                        job = %job_name,
                        error = %e,
                        "K8sGraphWarmer watcher: api get failed (continuing)"
                    );
                }
            }
            if SystemClock::new().now_instant() >= deadline {
                warn!(
                    job = %job_name,
                    "K8sGraphWarmer watcher: deadline exceeded, notifying anyway"
                );
                // Unknown terminal state → treat as Failed so we don't churn
                // the cache; the read-path revalidation TTL still converges.
                return WarmTerminalOutcome::Failed;
            }
            tokio::time::sleep(WATCH_POLL_INTERVAL).await;
        }
    }
}

/// No-op watcher used by unit tests. Reports success so the in-flight slot is
/// released and the completion hook (if any) fires, matching the common
/// "warm completed" path the tests exercise.
pub struct NoopJobWatcher;

#[async_trait]
impl WarmJobWatcher for NoopJobWatcher {
    async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
        WarmTerminalOutcome::Succeeded
    }
}

/// In-process convergence hook invoked when a warm Job reaches terminal
/// success. The canonical-graph warm runs in a *separate* K8s Job pod that
/// rewrites `repo_graph_cache` but cannot reach the server process's in-memory
/// graph slot; without this hook every `code_graph` query serves the pre-warm
/// blob until the server restarts (or the read-path revalidation TTL elapses).
///
/// `djinn-k8s` intentionally does not depend on `djinn-graph`, so the concrete
/// sink that clears `djinn_graph`'s `GRAPH_CACHE` is wired at the composition
/// root (`AppState`) where both crates are in scope, and injected via
/// [`K8sGraphWarmer::with_completion_sink`]. Kept as a trait (not a bare
/// closure) so unit tests can supply a recording mock with the existing
/// dispatcher/watcher patterns.
#[async_trait]
pub trait WarmCompletionSink: Send + Sync {
    /// Invoked once, in-process, after the warm Job for `project_id` reaches
    /// terminal success and its fresh blob is persisted. Implementations
    /// converge process-local caches (e.g. the canonical-graph RAM slot).
    async fn on_warm_succeeded(&self, project_id: &str);
}

/// Abstraction used by [`K8sGraphWarmer`] to discover warm Jobs that are
/// already running in the cluster (any process, not just this one). The
/// in-process `in_flight` map only serialises triggers within a single
/// server process; this trait provides the cross-process source of truth
/// so two near-simultaneous triggers — e.g. a main-tip-advance from
/// `mirror_fetcher` and a post-build kick from `image_build_watcher` —
/// can never both dispatch a warm Job.
///
/// Production uses [`KubeClientWarmJobLister`]; tests pass a
/// programmable mock that records queries and returns a pre-seeded
/// "exists" / "absent" answer.
#[async_trait]
pub trait WarmJobLister: Send + Sync {
    /// Return `true` if the cluster currently holds at least one
    /// non-terminal warm Job for `project_id`. A non-terminal Job is
    /// one whose `.status.succeeded` and `.status.failed` are both
    /// `None`/zero — i.e. it is still running, has not been observed
    /// completing, and could plausibly hold the shared
    /// `/cache/cargo-target/<project>` base. Implementations SHOULD
    /// surface apiserver errors so callers can choose their own
    /// fail-open / fail-closed policy; the warmer path treats errors
    /// as absent, while the GC path treats them as a retention signal.
    async fn has_in_flight_warm(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<bool, kube::Error>;
}

/// Production lister backed by a live `kube::Client`. Filters on the
/// `djinn.app/warm=true` + `djinn.app/project-id=<id>` labels that
/// [`crate::warm_job::build_warm_job`] writes on every warm Job, then
/// inspects `.status` to filter out terminal Jobs (succeeded or
/// failed). A Job still present in the cluster but flagged terminal
/// is ignored — those are about to be reaped by `ttlSecondsAfterFinished`
/// and are not lock-contending on the cargo base.
pub struct KubeClientWarmJobLister {
    client: kube::Client,
}

impl KubeClientWarmJobLister {
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl WarmJobLister for KubeClientWarmJobLister {
    async fn has_in_flight_warm(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<bool, kube::Error> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        // `sanitize_id` (warm_job.rs) lowercases the project id and
        // replaces disallowed chars with `-`, mirroring the label value
        // we wrote on dispatch. Without the sanitized form the
        // label_selector never matches and the dedup is silently
        // disabled — a quiet regression only observable as
        // re-introduced double-warms.
        let sanitized = crate::warm_job::sanitize_id(project_id);
        let selector = format!("{LABEL_WARM}=true,{LABEL_PROJECT_ID}={sanitized}");
        let list = jobs.list(&ListParams::default().labels(&selector)).await?;
        Ok(list.items.iter().any(|job| {
            let Some(status) = job.status.as_ref() else {
                // No status yet (just created) → still in flight.
                return true;
            };
            let succeeded = status.succeeded.unwrap_or(0) > 0;
            let failed = status.failed.unwrap_or(0) > 0;
            // Non-terminal = neither succeeded nor failed. "Active" pods
            // alone don't count (a Job can be Active without
            // succeeded/failed set yet) — we want to coalesce against
            // anything that hasn't reported a terminal state.
            !succeeded && !failed
        }))
    }
}

/// No-op lister used by unit tests that don't exercise the
/// cluster-side de-dupe. Always returns `Ok(false)`; the in-process
/// `in_flight` map is the only de-dupe exercised in those tests.
pub struct NoopWarmJobLister;

#[async_trait]
impl WarmJobLister for NoopWarmJobLister {
    async fn has_in_flight_warm(
        &self,
        _namespace: &str,
        _project_id: &str,
    ) -> Result<bool, kube::Error> {
        Ok(false)
    }
}

/// Immediate warm-dispatch core: every field the actual Job-launch path needs,
/// grouped so it can be cheaply cloned into the debounce driver task.
///
/// This holds all the pre-debounce single-flight + freshness + cluster-dedupe
/// machinery. [`K8sGraphWarmer`] wraps it with the temporal debounce layer; the
/// architect-facing `await_fresh` path and the disabled-debounce path call
/// [`WarmDispatch::dispatch_warm_now`] directly for synchronous semantics.
#[derive(Clone)]
struct WarmDispatch {
    config: KubernetesConfig,
    db: Database,
    dispatcher: Arc<dyn WarmJobDispatcher>,
    watcher: Arc<dyn WarmJobWatcher>,
    /// Cluster-side dedupe: lists non-terminal warm Jobs for a project so
    /// triggers from any process see the in-flight Jobs created by any
    /// other process (rolling update overlap, server restart mid-warm,
    /// parallel pod). `None` only under the test/mock path that injects
    /// a dispatcher without a live apiserver.
    lister: Option<Arc<dyn WarmJobLister>>,
    /// In-process hook fired after a warm Job succeeds so the server can
    /// converge its canonical-graph RAM slot to the freshly persisted blob
    /// without a restart. `None` under the test/mock path and for non-K8s
    /// wiring; production sets it via [`K8sGraphWarmer::with_completion_sink`].
    completion_sink: Option<Arc<dyn WarmCompletionSink>>,
    in_flight: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl WarmDispatch {
    /// Resolve the image the warm Job should run in, honouring catalog-image
    /// precedence (migration 46): a project on a shared catalog image warms
    /// inside that image; otherwise its own per-project build. Returns `None`
    /// when the resolved image isn't ready yet — the caller logs + skips.
    async fn resolve_project_image_tag(&self, project_id: &str) -> Option<String> {
        let repo = ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());
        match repo.resolve_dispatch_image(project_id).await {
            Ok(Some(img)) => img.pull_ref(),
            Ok(None) => None,
            Err(e) => {
                warn!(
                    project_id,
                    error = %e,
                    "K8sGraphWarmer: resolve_dispatch_image failed"
                );
                None
            }
        }
    }

    /// Check the `repo_graph_cache` for any row whose `built_at` is within
    /// `ttl` of now. Returns `true` on a hit. Uses the row's stored
    /// timestamp string (ISO-8601 UTC); parse failures fall through as
    /// "not fresh" rather than "freshness unknown" so the warmer always
    /// makes forward progress on malformed cache rows.
    async fn cache_is_fresh(&self, project_id: &str, ttl: Duration) -> bool {
        // We intentionally do NOT pin to the project's current `origin/main`
        // commit here — the warmer cares about "did we indexed recently",
        // not "is the graph aligned with tip-of-main". The architect
        // dispatch path uses the result as best-effort; any stale-edge
        // recovery happens on the next mirror-fetch tick.
        let repo = RepoGraphCacheRepository::new(self.db.clone());
        // There is no "list latest" method on the repo today; architects
        // call `await_fresh` with a project_id they also hand to
        // `ensure_canonical_graph`, which will itself re-consult the cache
        // by `(project_id, commit_sha)`. For the freshness gate here we
        // try the most-recent commit SHA we can cheaply discover: the
        // mirror's `refs/heads/main` tip via the bare mirror path.
        let tip = match discover_mirror_main_tip(project_id).await {
            Some(sha) => sha,
            None => return false,
        };
        let row = match repo.get(project_id, &tip).await {
            Ok(Some(r)) => r,
            _ => return false,
        };
        let Some(built_at) = time::OffsetDateTime::parse(
            &row.built_at,
            &time::format_description::well_known::Iso8601::DEFAULT,
        )
        .ok() else {
            return false;
        };
        let now = time::OffsetDateTime::now_utc();
        let age = (now - built_at).unsigned_abs();
        let age_duration = Duration::from_secs(age.as_secs());
        age_duration < ttl
    }

    /// True iff `repo_graph_cache` holds a row keyed by the project's
    /// current `origin/main` tip — i.e. the canonical graph is
    /// commit-aligned (`commits_since_pin == 0`) and a re-index would
    /// produce the identical graph.
    ///
    /// Unlike [`Self::cache_is_fresh`] this ignores row age: a graph built for
    /// the current commit stays valid no matter how old, so the proactive
    /// refresh / mirror-tick path must not re-warm it just because some
    /// wall-clock TTL elapsed. Returns `false` (→ caller dispatches a warm)
    /// when the mirror tip can't be resolved, or when no cache row exists
    /// yet — i.e. first-ever warm, a freshly-advanced commit, or a cache
    /// miss. That fail-open default preserves every legitimate warm.
    async fn cache_has_current_commit(&self, project_id: &str) -> bool {
        let Some(tip) = discover_mirror_main_tip(project_id).await else {
            return false;
        };
        let repo = RepoGraphCacheRepository::new(self.db.clone());
        matches!(repo.get(project_id, &tip).await, Ok(Some(_)))
    }

    /// Cross-process dedupe query: `true` if the cluster (any process)
    /// currently holds at least one non-terminal warm Job for
    /// `project_id`. Centralised so the two `dispatch_warm_now` call sites
    /// (pre-slot + post-slot race-safe re-check) stay in lock-step
    /// and the test mock can substitute a single hook instead of two
    /// duplicated branches. Returns `false` when no lister is wired
    /// (test/mock path) — the in-process `in_flight` map remains the
    /// per-process backstop, and tests that need the cluster check
    /// inject a lister explicitly.
    async fn cluster_has_in_flight_warm(&self, project_id: &str) -> bool {
        match self.lister.as_ref() {
            Some(lister) => match lister
                .has_in_flight_warm(&self.config.namespace, project_id)
                .await
            {
                Ok(in_flight) => in_flight,
                Err(error) => {
                    warn!(
                        project_id,
                        namespace = %self.config.namespace,
                        error = %error,
                        "K8sGraphWarmer: cluster warm-Job lister failed; failing open"
                    );
                    false
                }
            },
            None => false,
        }
    }

    /// Launch a warm Job *now*, preserving every gate (commit-freshness,
    /// image-readiness, in-process single-flight, cluster dedupe). This is the
    /// pre-debounce `trigger` body verbatim; the temporal debounce lives one
    /// level up in [`K8sGraphWarmer::trigger`].
    async fn dispatch_warm_now(&self, project_id: &str) {
        {
            let guard = self.in_flight.lock().await;
            if guard.contains_key(project_id) {
                debug!(
                    project_id,
                    "K8sGraphWarmer: warm already in flight, coalescing"
                );
                return;
            }
        }

        // Commit-aligned freshness short-circuit (ADR-051 §3). Every caller
        // — the 60s mirror-fetch tick (`mirror_fetcher::fetch_one`), the
        // coordinator's 10-min refresh (`refresh_canonical_graphs_if_stale`),
        // and the post-build image watcher — fires `trigger` unconditionally
        // and, per their own call-site comments, relies on the warmer to
        // no-op when nothing has changed. If the cache already holds a graph
        // for the project's current origin/main tip then `commits_since_pin
        // == 0` and a re-index is pure waste, so we skip dispatching a (4Gi)
        // warm Job. Without this gate every tick re-warms every project and
        // pins the cluster. Fail-open on an undeterminable tip / cache miss.
        if self.cache_has_current_commit(project_id).await {
            debug!(
                project_id,
                "K8sGraphWarmer: graph already current for origin/main tip; skipping warm"
            );
            return;
        }

        let Some(image_tag) = self.resolve_project_image_tag(project_id).await else {
            info!(
                project_id,
                "K8sGraphWarmer: no ready project image; skipping warm \
                 (devcontainer image not built yet)"
            );
            return;
        };

        // Cluster-side dedupe (cross-process source of truth). The in-process
        // `in_flight` map above only serialises triggers within THIS server
        // process; a Job running from a previous process incarnation (e.g.
        // `kubectl rollout` overlap, server restart mid-warm) is invisible to
        // the per-process map and would otherwise produce a duplicate Job —
        // and the duplicate then lock-contends with the survivor on
        // `/cache/cargo-target/<project>`, the exact symptom this check
        // prevents. The check happens AFTER the freshness gate so a
        // commit-aligned cache is still short-circuited without burning an
        // apiserver round-trip.
        if self.cluster_has_in_flight_warm(project_id).await {
            debug!(
                project_id,
                namespace = %self.config.namespace,
                "K8sGraphWarmer: cluster has non-terminal warm Job for project; coalescing"
            );
            return;
        }

        let notify = Arc::new(Notify::new());
        {
            let mut guard = self.in_flight.lock().await;
            // Re-check under write lock — another caller may have won the
            // race between our first read and this acquisition.
            if guard.contains_key(project_id) {
                debug!(
                    project_id,
                    "K8sGraphWarmer: warm already in flight (race-lost), coalescing"
                );
                return;
            }
            guard.insert(project_id.to_string(), notify.clone());
        }

        // Race-safe re-check: between the cluster query above and our
        // acquisition of the in-process slot, another process (rolling
        // update overlap, parallel pod) may have won and dispatched a
        // Job. Re-query the cluster under our claim and release the slot
        // if a Job has appeared — this is the only place we can close
        // the cross-process race, because the in-process map is per-
        // process and the apiserver is the only thing all processes
        // share. On fail-open (apiserver hiccup) we proceed; the
        // worst-case is the pre-fix duplicate-warm behaviour, not a
        // stuck cluster, and the freshness gate at the top of the next
        // trigger will reclaim the dispatch on the following tick.
        if self.cluster_has_in_flight_warm(project_id).await {
            debug!(
                project_id,
                namespace = %self.config.namespace,
                "K8sGraphWarmer: cluster warm Job appeared between first check and slot acquisition; releasing slot and coalescing"
            );
            let mut guard = self.in_flight.lock().await;
            if let Some(n) = guard.remove(project_id) {
                n.notify_waiters();
            }
            return;
        }

        // Load the project's EnvironmentConfig to extract the
        // cargo_cache_policy for the warm Job's env vars. Fail-open:
        // if the DB lookup or JSON parse fails, proceed with no policy
        // (backward-compatible default behavior).
        let cargo_cache_policy: Option<djinn_stack::environment::CargoCachePolicy> = {
            let repo =
                ProjectRepository::new(self.db.clone(), djinn_core::events::EventBus::noop());
            match repo.get_environment_config(project_id).await {
                Ok(Some(raw)) => {
                    serde_json::from_str::<djinn_stack::environment::EnvironmentConfig>(&raw)
                        .ok()
                        .and_then(|cfg| cfg.cargo_cache_policy)
                }
                _ => None,
            }
        };

        let job = build_warm_job(
            &self.config,
            project_id,
            &image_tag,
            cargo_cache_policy.as_ref(),
        );
        let namespace = self.config.namespace.clone();
        let job_name = match self.dispatcher.dispatch(&namespace, job).await {
            Ok(name) => name,
            Err(e) => {
                warn!(
                    project_id,
                    error = %e,
                    "K8sGraphWarmer: Job dispatch failed"
                );
                // Drop the in-flight slot + wake any waiters so await_fresh
                // doesn't hang on our failure.
                let mut guard = self.in_flight.lock().await;
                if let Some(n) = guard.remove(project_id) {
                    n.notify_waiters();
                }
                return;
            }
        };

        info!(
            project_id,
            job = %job_name,
            namespace = %namespace,
            image = %image_tag,
            "K8sGraphWarmer: warm Job created"
        );

        let watcher = self.watcher.clone();
        let in_flight = self.in_flight.clone();
        let completion_sink = self.completion_sink.clone();
        let project_id_owned = project_id.to_string();
        let namespace_owned = namespace.clone();
        let job_name_owned = job_name.clone();
        let notify_owned = notify.clone();
        tokio::spawn(async move {
            let outcome = watcher
                .wait_terminal(&namespace_owned, &job_name_owned)
                .await;
            let mut guard = in_flight.lock().await;
            if let Some(n) = guard.remove(&project_id_owned) {
                n.notify_waiters();
            }
            drop(guard);
            // Belt-and-braces: notify both our local handle and anything
            // the map still holds (in case a re-trigger happened mid-flight
            // and reassigned the slot).
            notify_owned.notify_waiters();
            // Event-driven convergence: on success the warm pod has already
            // rewritten `repo_graph_cache`, so invalidate the server's RAM slot
            // now (fast path). The read-path revalidation TTL is the backstop
            // if this hook is absent or the outcome was ambiguous.
            if outcome == WarmTerminalOutcome::Succeeded
                && let Some(sink) = completion_sink.as_ref()
            {
                sink.on_warm_succeeded(&project_id_owned).await;
            }
            debug!(
                project_id = %project_id_owned,
                outcome = ?outcome,
                "K8sGraphWarmer: warm watcher complete, waiters notified"
            );
        });
    }
}

/// Kubernetes-backed canonical-graph warmer.
///
/// Wraps a [`WarmDispatch`] (single-flight + Notify-based fan-out + cluster
/// dedupe) with a merge-storm **debounce** layer over the automatic
/// head-advance trigger path: a storm of `main` advances collapses into a
/// single warm run once it settles (see [`WarmDebounceConfig`]). The underlying
/// Job is dispatched via the [`WarmJobDispatcher`] abstraction so unit tests can
/// run without a live cluster.
pub struct K8sGraphWarmer {
    dispatch: WarmDispatch,
    /// Live kube client for Pod/Service/Job ops that the (Job-only) dispatcher
    /// abstraction doesn't cover — e.g. backing-service provisioning. `None`
    /// under the test/mock-dispatcher path (those ops then error/no-op).
    client: Option<kube::Client>,
    /// Temporal debounce policy for the automatic head-advance trigger path.
    debounce: WarmDebounceConfig,
    /// Per-project pending debounce windows. An entry exists (and one driver
    /// task runs) between the first trigger of a burst and the window's
    /// collapse into a dispatch.
    pending_warms: Arc<Mutex<HashMap<String, PendingWarm>>>,
    /// Live coalescer counters (triggers received / coalesced / debounced).
    metrics: Arc<WarmDebounceMetrics>,
}

impl K8sGraphWarmer {
    /// Construct a warmer backed by a live `kube::Client` (production
    /// path). Reads the merge-storm debounce policy from the environment.
    pub fn new(client: kube::Client, config: KubernetesConfig, db: Database) -> Self {
        let dispatcher = Arc::new(KubeClientDispatcher::new(client.clone()));
        let watcher = Arc::new(KubeClientJobWatcher::new(client.clone()));
        let lister: Arc<dyn WarmJobLister> = Arc::new(KubeClientWarmJobLister::new(client.clone()));
        let mut w = Self::with_dispatcher_and_lister(config, db, dispatcher, watcher, Some(lister));
        w.client = Some(client);
        w.debounce = WarmDebounceConfig::from_env();
        w
    }

    /// Construct a warmer with a caller-supplied dispatcher and watcher.
    /// Unit tests use this to inject mocks. Debounce defaults to
    /// [`WarmDebounceConfig::DISABLED`] so tests keep synchronous dispatch;
    /// opt into debouncing with [`Self::with_debounce`].
    pub fn with_dispatcher(
        config: KubernetesConfig,
        db: Database,
        dispatcher: Arc<dyn WarmJobDispatcher>,
        watcher: Arc<dyn WarmJobWatcher>,
    ) -> Self {
        Self::with_dispatcher_and_lister(config, db, dispatcher, watcher, None)
    }

    /// Construct a warmer with a caller-supplied dispatcher, watcher,
    /// and cluster lister. Production always supplies all three; tests
    /// that want to exercise the cluster-side dedupe pass a
    /// programmable lister; tests that only care about the in-process
    /// single-flight pass `None` (or [`NoopWarmJobLister`] via
    /// [`Self::with_dispatcher`]) and skip the cluster check. Debounce
    /// defaults to [`WarmDebounceConfig::DISABLED`].
    pub fn with_dispatcher_and_lister(
        config: KubernetesConfig,
        db: Database,
        dispatcher: Arc<dyn WarmJobDispatcher>,
        watcher: Arc<dyn WarmJobWatcher>,
        lister: Option<Arc<dyn WarmJobLister>>,
    ) -> Self {
        Self {
            dispatch: WarmDispatch {
                config,
                db,
                dispatcher,
                watcher,
                lister,
                completion_sink: None,
                in_flight: Arc::new(Mutex::new(HashMap::new())),
            },
            client: None,
            debounce: WarmDebounceConfig::DISABLED,
            pending_warms: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(WarmDebounceMetrics::default()),
        }
    }

    /// Attach the in-process warm-completion sink (builder style). Production
    /// wires a sink that clears `djinn_graph`'s canonical-graph RAM slot; tests
    /// inject a recording mock. Returns `self` for chaining off
    /// [`Self::new`]/[`Self::with_dispatcher`].
    #[must_use]
    pub fn with_completion_sink(mut self, sink: Arc<dyn WarmCompletionSink>) -> Self {
        self.dispatch.completion_sink = Some(sink);
        self
    }

    /// Override the merge-storm debounce policy (builder style). Production
    /// takes it from the environment via [`Self::new`]; tests use this to
    /// exercise the quiet-window / max-wait / disabled behaviours with short
    /// real durations.
    #[must_use]
    pub fn with_debounce(mut self, debounce: WarmDebounceConfig) -> Self {
        self.debounce = debounce;
        self
    }

    /// Snapshot the live debounce coalescer counters.
    pub fn debounce_metrics(&self) -> WarmDebounceMetricsSnapshot {
        WarmDebounceMetricsSnapshot {
            triggers_received: self.metrics.triggers_received.load(Ordering::Relaxed),
            triggers_coalesced: self.metrics.triggers_coalesced.load(Ordering::Relaxed),
            warms_debounced: self.metrics.warms_debounced.load(Ordering::Relaxed),
        }
    }

    /// Kubernetes namespace used by this warmer.
    pub fn namespace(&self) -> &str {
        &self.dispatch.config.namespace
    }

    /// Expose the Kubernetes warm-job lister so the coordinator can build a
    /// production [`WarmJobGuard`] that shares the same non-terminal Job
    /// semantics.
    pub fn warm_job_lister(&self) -> Option<Arc<dyn WarmJobLister>> {
        self.dispatch.lister.clone()
    }

    /// Arm (or re-arm) the debounce window for `project_id` in response to an
    /// automatic head-advance trigger, spawning the driver task on the first
    /// trigger of a burst. Returns whether a new driver was spawned (unused in
    /// production; asserted by tests). The caller has already confirmed
    /// debouncing is enabled.
    async fn arm_debounce(&self, project_id: &str) {
        let now = SystemClock::new().now_instant();
        let mut map = self.pending_warms.lock().await;
        if let Some(entry) = map.get_mut(project_id) {
            // Coalesce into the pending window: extend the quiet window but
            // never past the hard max-wait deadline (last-wins).
            entry.fire_at = (now + self.debounce.quiet).min(entry.hard_deadline);
            entry.coalesced += 1;
            self.metrics.triggers_coalesced.fetch_add(1, Ordering::Relaxed);
            debug!(
                project_id,
                coalesced = entry.coalesced,
                "K8sGraphWarmer: head-advance trigger coalesced into pending debounce window"
            );
            return;
        }
        // First trigger of a new burst: open a window and spawn its driver.
        let hard_deadline = now + self.debounce.effective_max_wait();
        let fire_at = (now + self.debounce.quiet).min(hard_deadline);
        map.insert(
            project_id.to_string(),
            PendingWarm {
                fire_at,
                hard_deadline,
                coalesced: 1,
            },
        );
        drop(map);
        debug!(
            project_id,
            quiet_secs = self.debounce.quiet.as_secs(),
            max_wait_secs = self.debounce.effective_max_wait().as_secs(),
            "K8sGraphWarmer: opened debounce window for head-advance trigger"
        );
        let pending = self.pending_warms.clone();
        let metrics = self.metrics.clone();
        let dispatch = self.dispatch.clone();
        let project = project_id.to_string();
        tokio::spawn(async move {
            run_debounce_driver(project, pending, metrics, dispatch).await;
        });
    }
}

/// Debounce driver: owns a single project's pending window from the first
/// trigger of a burst until it dispatches exactly one warm run.
///
/// State machine:
/// 1. **Quiet wait** — sleep until `fire_at`. Each trigger extends `fire_at`
///    (capped at `hard_deadline`), so a storm keeps the driver asleep until it
///    settles; a continuous storm still fires at `hard_deadline`.
/// 2. **In-flight drain** — if a warm is already running (an earlier burst, an
///    architect `await_fresh`), wait for it to finish so the follow-up coalesces
///    into ONE dispatch at the latest tip rather than queueing a stale SHA.
/// 3. **Collapse** — remove the pending entry, log how many triggers folded in,
///    and dispatch the current tip. `dispatch_warm_now` re-checks every gate, so
///    a no-op (nothing changed) is cheap and safe.
async fn run_debounce_driver(
    project_id: String,
    pending: Arc<Mutex<HashMap<String, PendingWarm>>>,
    metrics: Arc<WarmDebounceMetrics>,
    dispatch: WarmDispatch,
) {
    // 1. Quiet wait (re-reads fire_at each iteration to observe extensions).
    loop {
        let remaining = {
            let map = pending.lock().await;
            let Some(entry) = map.get(&project_id) else {
                // Entry vanished — defensive, shouldn't happen. Nothing to do.
                return;
            };
            entry
                .fire_at
                .checked_duration_since(SystemClock::new().now_instant())
        };
        match remaining {
            Some(d) if !d.is_zero() => tokio::time::sleep(d).await,
            _ => break,
        }
    }

    // 2. Drain any in-flight warm so we coalesce into a single follow-up at the
    //    latest tip (never a queue of stale SHAs).
    loop {
        let notify = { dispatch.in_flight.lock().await.get(&project_id).cloned() };
        match notify {
            Some(n) => n.notified().await,
            None => break,
        }
    }

    // 3. Collapse the window and dispatch the current tip.
    let coalesced = {
        let mut map = pending.lock().await;
        map.remove(&project_id).map(|e| e.coalesced).unwrap_or(0)
    };
    metrics.warms_debounced.fetch_add(1, Ordering::Relaxed);
    info!(
        project_id = %project_id,
        coalesced_triggers = coalesced,
        "K8sGraphWarmer: debounce window collapsed {coalesced} head-advance trigger(s) into one warm run"
    );
    dispatch.dispatch_warm_now(&project_id).await;
}

/// Best-effort lookup of the project's `origin/main` tip inside the
/// server's bare-mirror root. Returns `None` on any error (missing
/// mirror, `git` failure, invalid UTF-8, or empty output). The `K8sGraphWarmer`
/// treats `None` as "cache unknown" → it proceeds to trigger + wait.
async fn discover_mirror_main_tip(project_id: &str) -> Option<String> {
    let mirror_path = djinn_workspace::mirror_path_for(project_id);
    let output = djinn_git::run_git_command_in(
        &mirror_path,
        vec!["rev-parse".into(), "refs/heads/main".into()],
    )
    .await
    .ok()?;
    let sha = output.stdout.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

#[async_trait]
impl GraphWarmerService for K8sGraphWarmer {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), WarmerError> {
        let client = self.client.as_ref().ok_or_else(|| {
            WarmerError::Backend("task-run Job teardown requires a live kube client".to_string())
        })?;
        crate::runtime::delete_taskrun_job_foreground(
            client,
            &self.dispatch.config.namespace,
            task_run_id,
        )
        .await
        .map_err(|e| WarmerError::Backend(format!("delete task-run Job: {e}")))
    }

    async fn list_taskrun_jobs(&self) -> Result<Vec<TaskrunJobRef>, WarmerError> {
        let client = self.client.as_ref().ok_or_else(|| {
            WarmerError::Backend("task-run Job inventory requires a live kube client".to_string())
        })?;
        crate::runtime::list_taskrun_jobs(client, &self.dispatch.config.namespace)
            .await
            .map_err(|e| WarmerError::Backend(format!("list task-run Jobs: {e}")))
    }

    /// Automatic head-advance trigger entry point.
    ///
    /// This is the merge-storm debounce layer. Every automatic caller — the
    /// coordinator's periodic `refresh_canonical_graphs_if_stale`, the
    /// post-build `image_build_watcher`, and (transitively) the mirror-fetch
    /// tick — funnels through here. With debouncing enabled we coalesce a burst
    /// of `main` advances into ONE warm run after the storm settles (see
    /// [`run_debounce_driver`]); with it disabled (`quiet == 0`) we dispatch
    /// immediately, exactly reproducing the pre-debounce behaviour. The
    /// architect-facing `await_fresh` path bypasses debouncing and dispatches
    /// synchronously — it is a manual, latency-sensitive warm, not a storm
    /// trigger.
    async fn trigger(&self, project_id: &str) {
        self.metrics
            .triggers_received
            .fetch_add(1, Ordering::Relaxed);
        if self.debounce.enabled() {
            self.arm_debounce(project_id).await;
        } else {
            // Debounce disabled → preserve the exact pre-debounce semantics.
            self.dispatch.dispatch_warm_now(project_id).await;
        }
    }

    async fn await_fresh(
        &self,
        project_id: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<(), WarmerError> {
        if self.dispatch.cache_is_fresh(project_id, ttl).await {
            return Ok(());
        }

        // If a warm is already in flight, grab its Notify before triggering
        // so we don't race a completion.
        let existing_notify = {
            let guard = self.dispatch.in_flight.lock().await;
            guard.get(project_id).cloned()
        };

        let notify = if let Some(n) = existing_notify {
            n
        } else {
            // Kick off a warm synchronously (NOT via the debounce path): the
            // architect is actively blocked on this graph, so we must not defer
            // it behind a quiet window. `dispatch_warm_now` is the immediate
            // launch path; if it succeeds the in-flight map holds a Notify we
            // re-subscribe to.
            self.dispatch.dispatch_warm_now(project_id).await;
            let guard = self.dispatch.in_flight.lock().await;
            match guard.get(project_id).cloned() {
                Some(n) => n,
                None => {
                    // Trigger skipped (no image, dispatch failed); we have
                    // nothing to wait on. Best-effort return per contract.
                    debug!(
                        project_id,
                        "K8sGraphWarmer::await_fresh: trigger produced no in-flight warm; returning Ok"
                    );
                    return Ok(());
                }
            }
        };

        match tokio::time::timeout(timeout, notify.notified()).await {
            Ok(()) => Ok(()),
            Err(_) => {
                info!(
                    project_id,
                    timeout_ms = timeout.as_millis() as u64,
                    "K8sGraphWarmer::await_fresh: timed out; proceeding best-effort"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[path = "graph_warmer_tests.rs"]
mod tests;

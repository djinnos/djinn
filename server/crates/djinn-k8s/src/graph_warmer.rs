// djinn:allow-oversize — legacy warmer plus lease orchestration; split in the recovery follow-up.
//! Kubernetes Job-backed canonical-graph warmer.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use djinn_core::clock::{Clock, SystemClock};

use async_trait::async_trait;
use djinn_db::{Database, ProjectRepository, RepoGraphCacheRepository};
use djinn_runtime::{GraphWarmerService, TaskrunJobRef, WarmerError};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseDeadlines, LeaseFencingToken, LeaseGrant, LeaseState,
};
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, ListParams, PostParams};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

use crate::config::KubernetesConfig;
use crate::graph_warmer_identity::{
    LeasedWarmJobIdentity, deterministic_warm_job_name, stamp_admission_identity, warm_work_id,
};
use crate::warm_job::{LABEL_PROJECT_ID, LABEL_WARM, build_leased_warm_job, build_warm_job};

/// Warm Job manifest accepted by [`WarmJobDispatcher`].
///
/// Consumers can implement the dispatcher boundary without depending directly
/// on Kubernetes crates; ownership of those capability dependencies remains in
/// `djinn-k8s`.
pub type WarmJobManifest = Job;

/// A fencing grant acquired from the coordinator-owned v1 build FIFO.
/// The warmer receives this capability rather than a local counter so graph
/// warming and task invocation use the same durable authority.
#[derive(Clone, Debug)]
pub struct GraphWarmLeaseGrant {
    pub identity: GraphWarmLeaseIdentity,
    pub grant: LeaseGrant,
}

/// One durable occupied warm lease reconstructed after coordinator restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphWarmLeaseRecovery {
    pub identity: GraphWarmLeaseIdentity,
    pub fencing_token: LeaseFencingToken,
    pub bound_pod_uid: Option<String>,
    pub state: LeaseState,
    pub deadlines: LeaseDeadlines,
}

/// Typed v1 lease outcomes that must be handled before Kubernetes create.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphWarmLeaseError {
    #[error("graph warm lease is queued")]
    Queued,
    #[error("graph warm lease timed out")]
    Timeout,
    #[error("graph warm lease service unavailable")]
    Unavailable,
    #[error("graph warm lease rejected: {0}")]
    Rejected(String),
}

/// Data-only bridge to the coordinator's durable graph-warm protocol.
/// `acquire` persists and queues the stable identity, then acknowledges the
/// fencing grant into Launching before it returns. `bind` is idempotent and
/// recovers a lost bind response through durable status.
#[async_trait]
pub trait GraphWarmLease: Send + Sync {
    async fn acquire(
        &self,
        identity: GraphWarmLeaseIdentity,
        deadlines: LeaseDeadlines,
    ) -> Result<GraphWarmLeaseGrant, GraphWarmLeaseError>;

    async fn bind(
        &self,
        identity: &GraphWarmLeaseIdentity,
        fencing_token: LeaseFencingToken,
        pod_uid: String,
    ) -> Result<(), GraphWarmLeaseError>;

    async fn report(&self, _identity: &GraphWarmLeaseIdentity, _fencing_token: LeaseFencingToken, _state: LeaseState) -> Result<(), GraphWarmLeaseError> { Err(GraphWarmLeaseError::Unavailable) }
    async fn release(&self, _identity: &GraphWarmLeaseIdentity, _fencing_token: LeaseFencingToken) -> Result<(), GraphWarmLeaseError> { Err(GraphWarmLeaseError::Unavailable) }

    /// Enumerate retained warm leases for restart reconciliation. Non-durable
    /// test adapters intentionally have no recovery view.
    async fn recoverable(&self) -> Result<Vec<GraphWarmLeaseRecovery>, GraphWarmLeaseError> {
        Ok(Vec::new())
    }
}

mod warm_admission;
pub use crate::graph_warmer_candidates::{
    CleanupObservation, GateObservation, KubeWarmCandidateClient, WarmAnnotationValidation,
    WarmCandidate, WarmCandidateClient, WarmCandidateControl, WarmCandidateInventory,
    WarmCandidateKind, WarmCandidateObject, WarmCandidateSet, WarmCandidateSetState,
    WarmInventoryObservation,
};
pub use warm_admission::{
    WarmAdmission, WarmAdmissionError, WarmAdmissionPermit, WarmAdmissionRequest,
    WarmAdmissionTransition,
};

#[cfg(test)]
struct TestWarmAdmission;

#[cfg(test)]
#[async_trait]
impl WarmAdmission for TestWarmAdmission {
    async fn admit(
        &self,
        _request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError> {
        Ok(WarmAdmissionPermit::new())
    }

    async fn transition(
        &self,
        _permit: &WarmAdmissionPermit,
        _transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        Ok(())
    }
}

/// Test mock constructors explicitly receive an in-memory admission boundary
/// so existing debounce, dedupe, Notify, and completion-sink component tests
/// exercise the same admission-fenced path as production.
#[cfg(test)]
fn mock_warm_admission() -> Option<Arc<dyn WarmAdmission>> {
    Some(Arc::new(TestWarmAdmission))
}

#[cfg(not(test))]
fn mock_warm_admission() -> Option<Arc<dyn WarmAdmission>> {
    None
}

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
    async fn dispatch(&self, namespace: &str, job: WarmJobManifest) -> Result<String, String>;
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

#[path = "graph_warmer_lifecycle.rs"]
mod graph_warmer_lifecycle;
pub use graph_warmer_lifecycle::{
    KubeClientJobWatcher, NoopJobWatcher, WarmJobWatcher, WarmTerminalOutcome,
};
#[cfg(test)]
use graph_warmer_lifecycle::{
    WATCH_DEADLINE_SLACK, WarmJobObservation, terminal_outcome_after_poll, watch_deadline,
};

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
/// server process; this trait extends visibility to Jobs created by other
/// processes — e.g. a main-tip-advance from `mirror_fetcher` and a
/// post-build kick from `image_build_watcher`.
///
/// **Scheduling optimisation, not a writer mutex.** Cluster listing is a
/// *best-effort* coalescing optimisation: it reduces redundant warm dispatches
/// when observation happens to be consistent, but it is NOT a single-writer
/// guarantee. Two independent warmer processes can both observe "no in-flight
/// Job" — because the API list is racy (a list/create race window), transient
/// errors fail open (see [`WarmDispatch::cluster_has_in_flight_warm`]), or the
/// predecessor Job has been deleted/evicted and 404s while its Pod is still
/// terminating. When two overlapping warm Jobs do run concurrently, correctness
/// comes from the worker's per-project PVC advisory lock
/// (`/cache/cargo-target/.warm-locks/<project-id>.lock`, merged in task `t6g0`)
/// which serialises prune/stamp/compile across both Pods and is released on
/// normal completion or process death — NOT from this scheduler-level dedupe.
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
        let sanitized = crate::warm_job::sanitize_id(project_id);
        let selector = format!("{LABEL_WARM}=true,{LABEL_PROJECT_ID}={sanitized}");
        let list = jobs.list(&ListParams::default().labels(&selector)).await?;
        Ok(list.items.iter().any(|job| {
            let Some(status) = job.status.as_ref() else {
                return true;
            };
            let succeeded = status.succeeded.unwrap_or(0) > 0;
            let failed = status.failed.unwrap_or(0) > 0;
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
    /// Coordinator-owned admission boundary. Its absence means build admission
    /// is Off, so warm Jobs bypass admission while retaining normal dispatch.
    admission: Option<Arc<dyn WarmAdmission>>,
    /// v1 durable FIFO bridge. When configured it fences create with a shared
    /// graph-warm/task-invocation lease instead of the legacy v0 controller.
    lease: Option<Arc<dyn GraphWarmLease>>,
    /// Live Kubernetes inventory/gate seam for leased Jobs. Test constructors
    /// leave this absent and therefore cannot authorize a candidate.
    candidates: Option<Arc<WarmCandidateControl<KubeWarmCandidateClient>>>,
    /// Cluster-side dedupe: lists non-terminal warm Jobs for a project so
    /// triggers from any process can coalesce against in-flight Jobs created
    /// by any other process (rolling update overlap, server restart mid-warm,
    /// parallel pod). This is a *best-effort scheduling optimisation*, not a
    /// single-writer guarantee — overlap is handled by the worker's PVC
    /// advisory lock. `None` only under the test/mock path that injects
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
    async fn admission_request(&self, project_id: &str) -> WarmAdmissionRequest {
        let revision = discover_mirror_main_tip(project_id)
            .await
            .unwrap_or_else(|| "unknown".to_string());
        let work_id = warm_work_id(project_id, &revision);
        debug_assert!(
            crate::label_value::is_valid_label_value(&work_id),
            "warm work_id must be label-safe: {work_id}"
        );
        WarmAdmissionRequest {
            domain: "graph-warm".to_string(),
            object_name: deterministic_warm_job_name(project_id, &work_id),
            work_id,
            generation: 1,
        }
    }

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
        let repo = RepoGraphCacheRepository::new(self.db.clone());
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
    ///
    /// **Optimisation, not a correctness mechanism.** This is a best-effort
    /// coalescing check: it reduces redundant warm dispatches but does NOT
    /// guarantee single-writer exclusivity. Two failure modes are inherent:
    /// (1) a list/create race — both processes observe "empty" and both
    /// dispatch; (2) the lister `Err` arm fails open (returns `false`) so the
    /// cluster is never wedged by an apiserver hiccup. When overlap does
    /// occur, the worker's per-project PVC advisory lock
    /// (`/cache/cargo-target/.warm-locks/<project-id>.lock`, task `t6g0`)
    /// serialises prune/stamp/compile across the overlapping Pods; this check
    /// is never the correctness boundary.
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
    ///
    /// **Coalescing is an optimisation, not a writer mutex.** The in-process
    /// `in_flight` map and the cluster-side lister reduce redundant dispatches,
    /// but neither guarantees that at most one warm Job runs concurrently for a
    /// project. Two independent warmer processes with separate `in_flight`
    /// maps can both pass every gate (list/create race, lister fail-open, or
    /// a deleted/evicted predecessor whose Pod is still terminating). Correct
    /// behaviour under object-level overlap is guaranteed downstream by the
    /// worker's per-project PVC advisory lock
    /// (`/cache/cargo-target/.warm-locks/<project-id>.lock`, task `t6g0`),
    /// which serialises prune/stamp/compile and is released on normal
    /// completion or process death. This function does not — and must not —
    /// attempt to provide single-writer semantics.
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
            if guard.contains_key(project_id) {
                debug!(
                    project_id,
                    "K8sGraphWarmer: warm already in flight (race-lost), coalescing"
                );
                return;
            }
            guard.insert(project_id.to_string(), notify.clone());
        }

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

        let admission_request = self.admission_request(project_id).await;
        let lease_grant = if let Some(lease) = self.lease.as_ref() {
            let revision = discover_mirror_main_tip(project_id)
                .await
                .unwrap_or_else(|| "unknown".to_string());
            let now_ms =
                (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
            let deadline_ms =
                now_ms.saturating_add(self.config.warm_job_timeout_seconds.saturating_mul(1_000));
            let identity = GraphWarmLeaseIdentity {
                project_id: project_id.to_string(),
                // The request id is deterministic from the immutable revision
                // work key, so a lost create response queues/reuses one row.
                warm_request_id: admission_request.work_id.clone(),
                graph_revision: revision,
            };
            match lease
                .acquire(
                    identity,
                    LeaseDeadlines {
                        queue_deadline_ms: deadline_ms,
                        launch_deadline_ms: deadline_ms,
                    },
                )
                .await
            {
                Ok(grant) => Some(grant),
                Err(error) => {
                    warn!(project_id, error = %error, "K8sGraphWarmer: v1 lease did not authorize Job POST");
                    self.schedule_admission_retry(project_id, notify.clone());
                    return;
                }
            }
        } else {
            None
        };
        let leased_identity = lease_grant.as_ref().map(|grant| {
            LeasedWarmJobIdentity::new(
                project_id,
                grant.identity.warm_request_id.clone(),
                grant.identity.graph_revision.clone(),
                grant.grant.fencing_token.0,
            )
        });
        let mut job = match lease_grant.as_ref() {
            Some(_) => build_leased_warm_job(
                &self.config,
                project_id,
                &image_tag,
                cargo_cache_policy.as_ref(),
                leased_identity.as_ref().expect("leased grant has identity"),
            ),
            None => build_warm_job(
                &self.config,
                project_id,
                &image_tag,
                cargo_cache_policy.as_ref(),
            ),
        };
        stamp_admission_identity(&mut job, &admission_request);
        let namespace = self.config.namespace.clone();
        let permit = match (lease_grant.as_ref(), self.admission.as_ref()) {
            (Some(_), _) => None,
            (None, Some(admission)) => match admission.admit(admission_request.clone()).await {
                Ok(permit) => {
                    if let Err(error) = admission
                        .transition(&permit, WarmAdmissionTransition::CreateStarted)
                        .await
                    {
                        warn!(project_id, error = %error, "K8sGraphWarmer: CreateStarted was not durable; skipping Job POST");
                        self.schedule_admission_retry(project_id, notify.clone());
                        return;
                    }
                    Some(permit)
                }
                Err(error) => {
                    warn!(project_id, error = %error, "K8sGraphWarmer: admission denied or unavailable; skipping Job POST");
                    self.schedule_admission_retry(project_id, notify.clone());
                    return;
                }
            },
            (None, None) => None,
        };
        let job_name = match self.dispatcher.dispatch(&namespace, job).await {
            Ok(name) => name,
            Err(e) => {
                warn!(
                    project_id,
                    error = %e,
                    "K8sGraphWarmer: Job dispatch failed"
                );
                if let (Some(admission), Some(permit)) = (self.admission.as_ref(), permit.as_ref())
                {
                    let transition = if dispatcher_error_is_definitive(&e) {
                        WarmAdmissionTransition::DefinitiveFailure {
                            diagnostic: e.clone(),
                        }
                    } else {
                        WarmAdmissionTransition::CreateUnknown {
                            diagnostic: e.clone(),
                        }
                    };
                    if let Err(error) = admission.transition(permit, transition).await {
                        warn!(project_id, error = %error, "K8sGraphWarmer: failed to record dispatcher outcome");
                    }
                }
                if dispatcher_error_is_definitive(&e) || lease_grant.is_none() {
                    self.release_in_flight(project_id).await;
                    return;
                }

                // A lost response is not proof that Kubernetes did not create
                // the deterministic object. Continue with its deterministic
                // name and the same launching grant; the inventory loop below
                // also covers delayed Job-controller Pod materialisation.
                admission_request.object_name.clone()
            }
        };

        // POST success proves neither a unique Pod nor its immutable UID.
        // Inventory repeatedly because the Job controller normally creates the
        // Pod after POST returns. Keep this launching grant and in-flight slot
        // intact so legacy Job dedupe cannot short-circuit recovery.
        if lease_grant.is_some()
            && !self
                .wait_for_bind_and_open_leased_candidate(
                    project_id,
                    leased_identity.as_ref(),
                    lease_grant.as_ref(),
                )
                .await
        {
            self.schedule_admission_retry(project_id, notify.clone());
            return;
        }

        let uid = self.watcher.job_uid(&namespace, &job_name).await;
        if let (Some(admission), Some(permit)) = (self.admission.as_ref(), permit.as_ref()) {
            let transition = match uid.as_ref() {
                Some(uid) => WarmAdmissionTransition::Live { uid: uid.clone() },
                None => WarmAdmissionTransition::CreateUnknown {
                    diagnostic: "Kubernetes create succeeded but Job UID could not be observed"
                        .to_string(),
                },
            };
            if let Err(error) = admission.transition(permit, transition).await {
                warn!(project_id, error = %error, "K8sGraphWarmer: failed to record create outcome");
            }
        }

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
        let admission = self.admission.clone();
        let permit = permit.clone();
        let uid = uid.clone();
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
            notify_owned.notify_waiters();
            if let (Some(admission), Some(permit), Some(uid)) =
                (admission.as_ref(), permit.as_ref(), uid.as_ref())
                && let Err(error) = admission
                    .transition(
                        permit,
                        WarmAdmissionTransition::Terminal { uid: uid.clone() },
                    )
                    .await
            {
                warn!(project_id = %project_id_owned, error = %error, "K8sGraphWarmer: failed to record terminal Job");
            }
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

    async fn release_in_flight(&self, project_id: &str) {
        let mut guard = self.in_flight.lock().await;
        if let Some(notify) = guard.remove(project_id) {
            notify.notify_waiters();
        }
    }

    async fn bind_and_open_leased_candidate(
        &self,
        project_id: &str,
        identity: Option<&LeasedWarmJobIdentity>,
        grant: Option<&GraphWarmLeaseGrant>,
    ) -> bool {
        let (Some(control), Some(identity), Some(grant), Some(lease)) = (
            self.candidates.as_ref(),
            identity,
            grant,
            self.lease.as_ref(),
        ) else {
            warn!(
                project_id,
                "K8sGraphWarmer: lease candidate control unavailable; gate remains closed"
            );
            return false;
        };
        let inventory = control.inventory(identity).await;
        let Some(candidate) = inventory.selected_pod() else {
            warn!(project_id, inventory = ?inventory.observation, pods = ?inventory.pods.state, "K8sGraphWarmer: no unique Pod candidate; gate remains closed");
            return false;
        };
        let Some(pod_uid) = candidate.uid.clone() else {
            return false;
        };
        if let Err(error) = lease
            .bind(&grant.identity, grant.grant.fencing_token.clone(), pod_uid)
            .await
        {
            warn!(project_id, error = %error, "K8sGraphWarmer: bind not durably confirmed; gate remains closed");
            return false;
        }
        match control.open_selected_pod_gate(identity, &inventory).await {
            GateObservation::Opened => true,
            outcome => {
                warn!(project_id, ?outcome, "K8sGraphWarmer: gate was not opened");
                false
            }
        }
    }

    /// Retry inventory/bind/gate activation without returning through the
    /// create path. This covers both normal delayed Pod appearance and an
    /// accepted POST whose response was lost.
    async fn wait_for_bind_and_open_leased_candidate(
        &self,
        project_id: &str,
        identity: Option<&LeasedWarmJobIdentity>,
        grant: Option<&GraphWarmLeaseGrant>,
    ) -> bool {
        let timeout = Duration::from_secs(self.config.warm_job_timeout_seconds.max(1) as u64);
        let clock = SystemClock::new();
        let deadline = clock.now_instant() + timeout;
        loop {
            if self
                .bind_and_open_leased_candidate(project_id, identity, grant)
                .await
            {
                return true;
            }
            if clock.now_instant() >= deadline {
                warn!(
                    project_id,
                    "K8sGraphWarmer: candidate inventory/bind deadline elapsed; gate remains closed"
                );
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Keep an admission-gated identity coalesced briefly, then retry it.
    /// In particular, a denial and a non-durable CreateStarted are neither a
    /// completed warm nor a dispatcher failure; dropping their slot immediately
    /// would let every trigger create a fresh reservation attempt.
    fn schedule_admission_retry(&self, project_id: &str, notify: Arc<Notify>) {
        let dispatch = self.clone();
        let project_id = project_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let removed = {
                let mut guard = dispatch.in_flight.lock().await;
                match guard.get(&project_id) {
                    Some(current) if Arc::ptr_eq(current, &notify) => guard.remove(&project_id),
                    _ => None,
                }
            };
            if let Some(current) = removed {
                current.notify_waiters();
                dispatch.dispatch_warm_now(&project_id).await;
            }
        });
    }
}

/// With the legacy string-only dispatcher contract, classify only explicit
/// client rejections as definitive. Transport and unrecognised errors remain
/// conservatively occupying `CreateUnknown` outcomes.
fn dispatcher_error_is_definitive(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "status code 400",
        "status code 401",
        "status code 403",
        "status code 404",
        "status code 405",
        "status code 406",
        "status code 413",
        "status code 415",
        "status code 422",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

/// Kubernetes-backed canonical-graph warmer.
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
        let watcher = Arc::new(KubeClientJobWatcher::new(
            client.clone(),
            config.warm_job_timeout_seconds,
        ));
        let lister: Arc<dyn WarmJobLister> = Arc::new(KubeClientWarmJobLister::new(client.clone()));
        let mut w = Self::with_dispatcher_and_lister(config, db, dispatcher, watcher, Some(lister));
        w.dispatch.candidates = Some(Arc::new(WarmCandidateControl::new(
            KubeWarmCandidateClient::new(client.clone(), w.dispatch.config.namespace.clone()),
        )));
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
                admission: mock_warm_admission(),
                lease: None,
                candidates: None,
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

    /// Attach the coordinator-owned warm-admission boundary (builder style).
    ///
    /// This crate deliberately supplies no default admission implementation:
    /// callers that need admission-controlled dispatch must inject one. The
    /// lifecycle-ordering integration is responsible for consuming this seam
    /// before a Kubernetes POST, preserving the current dispatcher and watcher
    /// behaviour until then.
    #[must_use]
    pub fn with_warm_admission(mut self, admission: Arc<dyn WarmAdmission>) -> Self {
        self.dispatch.admission = Some(admission);
        self
    }

    /// Attach the coordinator v1 durable FIFO. This takes precedence over the
    /// legacy v0 admission seam without deleting that seam for the cutover epic.
    #[must_use]
    pub fn with_graph_warm_lease(mut self, lease: Arc<dyn GraphWarmLease>) -> Self {
        self.dispatch.lease = Some(lease);
        self
    }

    /// Return the explicitly injected admission boundary, if configured.
    ///
    /// `None` represents Off mode: the warmer bypasses admission and dispatches
    /// normally without manufacturing a no-op admission controller.
    pub fn warm_admission(&self) -> Option<Arc<dyn WarmAdmission>> {
        self.dispatch.admission.clone()
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

    /// Inventory retained leases without creating a new Job. Only a uniquely
    /// matching immutable Pod can be bound or authorized; incomplete inventory,
    /// API errors, duplicates, and name reuse all leave the init gate closed.
    pub async fn reconcile_durable_warm_leases(&self) {
        let Some(lease) = self.dispatch.lease.as_ref() else {
            return;
        };
        let recoveries = match lease.recoverable().await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(error = %error, "K8sGraphWarmer: durable lease recovery unavailable");
                return;
            }
        };
        let Some(control) = self.dispatch.candidates.as_ref() else {
            warn!("K8sGraphWarmer: no candidate control for durable lease recovery");
            return;
        };
        for recovery in recoveries {
            let job_identity = LeasedWarmJobIdentity::new(
                &recovery.identity.project_id,
                recovery.identity.warm_request_id.clone(),
                recovery.identity.graph_revision.clone(),
                recovery.fencing_token.0,
            );
            let inventory = control.inventory(&job_identity).await;
            let expired = recovery.deadlines.launch_deadline_ms > 0 && time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1_000_000 >= recovery.deadlines.launch_deadline_ms;
            if inventory.observation != WarmInventoryObservation::Observed { let _ = lease.report(&recovery.identity, recovery.fencing_token.clone(), LeaseState::Suspect).await; continue; }
            if expired { let mut pending = false; for candidate in inventory.jobs.candidates.iter().chain(inventory.pods.candidates.iter()) { pending |= candidate.uid.is_none() || !matches!(control.delete_candidate(candidate).await, CleanupObservation::ConfirmedDelete); } if pending || !inventory.jobs.candidates.is_empty() || !inventory.pods.candidates.is_empty() { let _ = lease.report(&recovery.identity, recovery.fencing_token.clone(), LeaseState::Suspect).await; continue; } }
            if inventory.jobs.candidates.is_empty() && inventory.pods.candidates.is_empty() { let _ = lease.release(&recovery.identity, recovery.fencing_token.clone()).await; continue; }
            let Some(candidate) = inventory.selected_pod() else {
                let _ = lease.report(&recovery.identity, recovery.fencing_token.clone(), LeaseState::Suspect).await;
                warn!(request_id = %recovery.identity.warm_request_id, inventory = ?inventory.observation, pods = ?inventory.pods.state, "K8sGraphWarmer: recovered lease remains suspect with gate closed");
                continue;
            };
            let Some(uid) = candidate.uid.clone() else {
                continue;
            };
            if recovery
                .bound_pod_uid
                .as_deref()
                .is_some_and(|bound| bound != uid)
            {
                warn!(request_id = %recovery.identity.warm_request_id, bound_uid = ?recovery.bound_pod_uid, observed_uid = %uid, "K8sGraphWarmer: recovered UID mismatch; gate remains closed");
                continue;
            }
            if let Err(error) = lease
                .bind(&recovery.identity, recovery.fencing_token.clone(), uid)
                .await
            {
                warn!(request_id = %recovery.identity.warm_request_id, error = %error, "K8sGraphWarmer: recovered bind not confirmed; gate remains closed");
                continue;
            }
            if !matches!(
                control
                    .open_selected_pod_gate(&job_identity, &inventory)
                    .await,
                GateObservation::Opened
            ) {
                let _ = lease.report(&recovery.identity, recovery.fencing_token.clone(), LeaseState::Suspect).await;
                warn!(request_id = %recovery.identity.warm_request_id, "K8sGraphWarmer: recovered gate remains closed");
            } else { let _ = lease.report(&recovery.identity, recovery.fencing_token.clone(), LeaseState::Active).await; }
        }
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
            entry.fire_at = (now + self.debounce.quiet).min(entry.hard_deadline);
            entry.coalesced += 1;
            self.metrics
                .triggers_coalesced
                .fetch_add(1, Ordering::Relaxed);
            debug!(
                project_id,
                coalesced = entry.coalesced,
                "K8sGraphWarmer: head-advance trigger coalesced into pending debounce window"
            );
            return;
        }
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
    loop {
        let remaining = {
            let map = pending.lock().await;
            let Some(entry) = map.get(&project_id) else {
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

    loop {
        let notify = { dispatch.in_flight.lock().await.get(&project_id).cloned() };
        match notify {
            Some(n) => n.notified().await,
            None => break,
        }
    }

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

        let existing_notify = {
            let guard = self.dispatch.in_flight.lock().await;
            guard.get(project_id).cloned()
        };

        let notify = if let Some(n) = existing_notify {
            n
        } else {
            self.dispatch.dispatch_warm_now(project_id).await;
            let guard = self.dispatch.in_flight.lock().await;
            match guard.get(project_id).cloned() {
                Some(n) => n,
                None => {
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
#[path = "graph_warmer_admission_tests.rs"]
mod admission_tests;
#[cfg(test)]
#[path = "graph_warmer_tests.rs"]
mod tests;

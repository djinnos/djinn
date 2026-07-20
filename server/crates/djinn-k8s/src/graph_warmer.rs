//! Kubernetes Job-backed canonical-graph warmer.

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

/// Warm Job manifest accepted by [`WarmJobDispatcher`].
///
/// Consumers can implement the dispatcher boundary without depending directly
/// on Kubernetes crates; ownership of those capability dependencies remains in
/// `djinn-k8s`.
pub type WarmJobManifest = Job;

mod warm_admission;
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
use graph_warmer_lifecycle::{WarmJobObservation, terminal_outcome_after_poll};

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
        let mut job = build_warm_job(
            &self.config,
            project_id,
            &image_tag,
            cargo_cache_policy.as_ref(),
        );
        stamp_admission_identity(&mut job, &admission_request);
        let namespace = self.config.namespace.clone();
        let permit = match self.admission.as_ref() {
            Some(admission) => match admission.admit(admission_request).await {
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
            None => None,
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
                self.release_in_flight(project_id).await;
                return;
            }
        };

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

/// Apply the admission identity (deterministic name + the three
/// `djinn.app/admission-*` labels) to a freshly built warm Job.
///
/// Extracted from the dispatch path so the manifest that actually reaches the
/// apiserver can be asserted against Kubernetes label validation in a unit
/// test — the validation gap that let an invalid manifest ship.
fn stamp_admission_identity(job: &mut Job, request: &WarmAdmissionRequest) {
    job.metadata.name = Some(request.object_name.clone());
    let labels = job.metadata.labels.get_or_insert_default();
    labels.insert(
        crate::workload_inventory::LABEL_ADMISSION_DOMAIN.into(),
        "warm_build".into(),
    );
    labels.insert(
        crate::workload_inventory::LABEL_ADMISSION_WORK_ID.into(),
        request.work_id.clone(),
    );
    labels.insert(
        crate::workload_inventory::LABEL_ADMISSION_GENERATION.into(),
        request.generation.to_string(),
    );
}

/// Longest project segment that keeps [`deterministic_warm_job_name`] inside
/// the label-value budget: `djinn-warm-` (11) + project + `-g1-` (4) +
/// 16 hex digits = 31 + project, so the project segment gets 32 bytes.
const WARM_NAME_PROJECT_BUDGET: usize = crate::label_value::LABEL_VALUE_MAX_BYTES - 31;

/// Reduce `raw` to lowercase alphanumerics and `-`, capped at `budget` bytes,
/// with non-alphanumeric edges trimmed so the result can sit at the end of a
/// label value.
fn warm_id_segment(raw: &str, budget: usize) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(budget)
        .collect();
    mapped
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// Stable admission identity for one (project, revision) warm.
///
/// This value is stamped into `djinn.app/admission-work-id` and read back out
/// by inventory reconciliation to rebuild the journal key, so it must be a
/// legal label value *natively* — sanitising it at the stamp site would make
/// the label-derived key differ from the durable one. Budget: `gw.` (3) +
/// project (≤32) + `.` (1) + revision (≤12) = 48 bytes worst case.
pub fn warm_work_id(project_id: &str, revision: &str) -> String {
    let project = warm_id_segment(project_id, WARM_NAME_PROJECT_BUDGET);
    let revision = warm_id_segment(revision, 12);
    let revision = if revision.is_empty() {
        "unknown".to_string()
    } else {
        revision
    };
    format!("gw.{project}.{revision}")
}

/// Deterministic Job name for one warm generation.
///
/// Kept within [`LABEL_VALUE_MAX_BYTES`] rather than the 253-byte object-name
/// budget: the Job controller defaults `metadata.name` into
/// `spec.template.labels[job-name]`, where the *label* limit applies. A name
/// that is legal as a name but oversized as a label 422s the whole create.
///
/// [`LABEL_VALUE_MAX_BYTES`]: crate::label_value::LABEL_VALUE_MAX_BYTES
fn deterministic_warm_job_name(project_id: &str, work_id: &str) -> String {
    let project = warm_id_segment(project_id, WARM_NAME_PROJECT_BUDGET);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in work_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("djinn-warm-{project}-g1-{hash:016x}")
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
                admission: mock_warm_admission(),
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
#[path = "graph_warmer_label_tests.rs"]
mod label_tests;
#[cfg(test)]
#[path = "graph_warmer_tests.rs"]
mod tests;

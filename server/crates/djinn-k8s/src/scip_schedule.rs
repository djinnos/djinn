//! Cadence + change-detection gate for the standalone SCIP-index Job.
//!
//! # The requirement
//!
//! Run the semantic index **every 3 hours, and only if the repository head has
//! advanced since the last successful index**. If nothing changed, skip
//! *without creating a Job* — a leaseless 16Gi Pod that re-indexes an unchanged
//! tree is pure waste.
//!
//! # Why this does not reuse the warm debounce
//!
//! [`crate::graph_warmer`] has a debounce with a 180s quiet window and a 900s
//! anti-starvation cap. **It is dead and must not be built on.**
//! `server/src/mirror_fetcher.rs` (`DEFAULT_INTERVAL_SECS = 60`) calls
//! `graph_warmer().trigger(project_id)` unconditionally every 60 seconds for
//! every project with indexable code, so the 180s quiet window can never
//! elapse; every window runs to the 900s cap. That was verified in production:
//! a window opened and collapsed at exactly 900.000s having coalesced 17
//! triggers. A cadence built on that path would be a cadence built on a timer
//! that always fires.
//!
//! This gate is therefore *pull*, not push: it is driven by its own tick and
//! decides from observed state. It never subscribes to `trigger`.
//!
//! # Where the "last successful index" state lives
//!
//! In the cluster, not in Postgres. Each SCIP Job is named deterministically
//! from `(project, revision)` and annotated with
//! [`crate::scip_job::ANNOTATION_SCIP_REVISION`], and
//! `scip_job_ttl_seconds` (4h) is deliberately longer than
//! `scip_index_interval_seconds` (3h) — so the retained succeeded-Job set *is*
//! the ledger, and it is durable across server restarts because it is apiserver
//! state.
//!
//! The failure mode of an early-GC'd Job is a redundant dispatch, and a
//! redundant dispatch is cheap: the SCIP artifact cache is content-addressed
//! (source/config/lockfile hashes), so re-running against an unchanged tree
//! hits the cache for every workspace. Losing the ledger costs a fast no-op,
//! never a wrong graph — which is why this is worth avoiding a schema
//! migration for.
//!
//! # Failure and skip degradation
//!
//! Every non-`Dispatch` outcome, including every error, is a **skip**. Skipping
//! cannot degrade the served graph: this Job only fills a cache. When it does
//! not run, `ensure_canonical_graph` re-runs the indexers inline exactly as it
//! does today, its `node_count == 0` guard still refuses to publish an empty
//! blob, and per-workspace salvage still splices the previous artifact for any
//! workspace whose indexer failed. The worst case of a skipped or failed SCIP
//! Job is a slower warm, never an empty graph.
//!
//! Uncertainty is therefore resolved toward *not dispatching*: an unreadable
//! head or an unreadable inventory yields a skip, because dispatching on
//! unknown state is the only branch that can cost anything.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::chrono::{DateTime, Utc};
use kube::api::{Api, ListParams};
use tracing::{debug, info, warn};

use crate::config::KubernetesConfig;
use crate::graph_warmer::WarmJobDispatcher;
use crate::scip_job::{
    ANNOTATION_SCIP_REVISION, LABEL_PROJECT_ID, LABEL_SCIP_INDEX, build_scip_index_job,
};

/// What one scheduling tick decided for one project. Every variant except
/// [`Self::Dispatch`] creates no Kubernetes object at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScipIndexDecision {
    /// Head advanced, cadence satisfied, nothing in flight: create the Job.
    Dispatch { revision: String },
    /// The mirror head could not be resolved. Fail closed — a Job is named
    /// after its revision, so an unknown revision has no correct name.
    SkipUnknownRevision,
    /// The cluster inventory could not be read. Fail closed: an unreadable
    /// ledger is not evidence that the head advanced.
    SkipInventoryUnavailable,
    /// A non-terminal SCIP Job already exists for this project.
    SkipInFlight,
    /// The last successful index already covers this exact revision. **This is
    /// the change-detection gate** and it is checked before the cadence, so an
    /// unchanged head never dispatches no matter how much time has passed.
    SkipHeadUnchanged { revision: String },
    /// Head advanced, but the cadence floor has not elapsed since the last
    /// dispatch. Rate limit on a continuously-advancing `main`.
    SkipCadence { remaining: Duration },
}

impl ScipIndexDecision {
    /// The revision to index, if this decision dispatches.
    #[must_use]
    pub fn dispatch_revision(&self) -> Option<&str> {
        match self {
            Self::Dispatch { revision } => Some(revision.as_str()),
            _ => None,
        }
    }

    /// Closed-enum reason label, safe for metrics and logs.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Dispatch { .. } => "dispatch",
            Self::SkipUnknownRevision => "unknown_revision",
            Self::SkipInventoryUnavailable => "inventory_unavailable",
            Self::SkipInFlight => "in_flight",
            Self::SkipHeadUnchanged { .. } => "head_unchanged",
            Self::SkipCadence { .. } => "cadence",
        }
    }
}

/// What the cluster currently holds for one project's SCIP-index population.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScipJobObservation {
    /// At least one SCIP Job for this project is neither succeeded nor failed.
    pub in_flight: bool,
    /// Revision of the most recently *succeeded* SCIP Job, read off its
    /// [`ANNOTATION_SCIP_REVISION`] annotation. `None` when no succeeded Job is
    /// retained — first run, or the ledger aged out.
    pub last_indexed_revision: Option<String>,
    /// Age of the most recent SCIP Job of any outcome, from its
    /// `creationTimestamp`. `None` when none is retained.
    pub since_last_dispatch: Option<Duration>,
}

/// Reads the retained SCIP-Job inventory that backs the change-detection gate.
#[async_trait]
pub trait ScipJobInventory: Send + Sync {
    async fn observe(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<ScipJobObservation, String>;
}

/// The pure decision. No I/O, no clock, no cluster — every input is an
/// argument, so every branch is directly unit-testable.
///
/// Ordering is load-bearing:
/// 1. unknown head and unreadable inventory fail closed;
/// 2. in-flight coalesces;
/// 3. **head-unchanged wins over the cadence**, because the requirement is
///    "every 3 hours *and only if* the head advanced", not "or";
/// 4. the cadence floor rate-limits an advancing head.
#[must_use]
pub fn decide(
    head_revision: Option<&str>,
    observation: Option<&ScipJobObservation>,
    interval: Duration,
) -> ScipIndexDecision {
    let Some(head) = head_revision.map(str::trim).filter(|h| !h.is_empty()) else {
        return ScipIndexDecision::SkipUnknownRevision;
    };
    let Some(observation) = observation else {
        return ScipIndexDecision::SkipInventoryUnavailable;
    };
    if observation.in_flight {
        return ScipIndexDecision::SkipInFlight;
    }
    if observation.last_indexed_revision.as_deref() == Some(head) {
        return ScipIndexDecision::SkipHeadUnchanged {
            revision: head.to_string(),
        };
    }
    if let Some(elapsed) = observation.since_last_dispatch
        && elapsed < interval
    {
        return ScipIndexDecision::SkipCadence {
            remaining: interval - elapsed,
        };
    }
    ScipIndexDecision::Dispatch {
        revision: head.to_string(),
    }
}

/// Drives [`decide`] against the live cluster and creates the Job when it says
/// to. Deliberately owns no timer: the caller supplies the tick, so the loop
/// interval and the cadence floor stay independently configurable and the
/// scheduler stays testable without `tokio::time`.
pub struct ScipIndexScheduler {
    config: KubernetesConfig,
    inventory: Arc<dyn ScipJobInventory>,
    dispatcher: Arc<dyn WarmJobDispatcher>,
}

impl ScipIndexScheduler {
    #[must_use]
    pub fn new(
        config: KubernetesConfig,
        inventory: Arc<dyn ScipJobInventory>,
        dispatcher: Arc<dyn WarmJobDispatcher>,
    ) -> Self {
        Self {
            config,
            inventory,
            dispatcher,
        }
    }

    /// The configured cadence floor.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.config.scip_index_interval_seconds)
    }

    /// Evaluate one project without creating anything. Exposed so the decision
    /// can be observed (and asserted) separately from the side effect.
    pub async fn evaluate(
        &self,
        project_id: &str,
        head_revision: Option<&str>,
    ) -> ScipIndexDecision {
        let observation = match self
            .inventory
            .observe(&self.config.namespace, project_id)
            .await
        {
            Ok(observation) => Some(observation),
            Err(error) => {
                warn!(
                    project_id,
                    %error,
                    "scip-index: cluster inventory unavailable; skipping this tick"
                );
                None
            }
        };
        decide(head_revision, observation.as_ref(), self.interval())
    }

    /// Evaluate one project and, only on [`ScipIndexDecision::Dispatch`],
    /// create the Job. Returns the decision that was acted on.
    ///
    /// A dispatch failure is not promoted to an error: the Job name is
    /// deterministic in `(project, revision)`, so a lost create response cannot
    /// duplicate work, and the next tick re-evaluates from cluster state.
    pub async fn tick_project(
        &self,
        project_id: &str,
        head_revision: Option<&str>,
        image_tag: &str,
        policy: Option<&djinn_stack::environment::CargoCachePolicy>,
    ) -> ScipIndexDecision {
        let decision = self.evaluate(project_id, head_revision).await;
        let Some(revision) = decision.dispatch_revision() else {
            debug!(
                project_id,
                reason = decision.reason(),
                "scip-index: no Job created this tick"
            );
            return decision;
        };
        let job = build_scip_index_job(&self.config, project_id, image_tag, revision, policy);
        match self.dispatcher.dispatch(&self.config.namespace, job).await {
            Ok(name) => info!(
                project_id,
                job = %name,
                revision,
                "scip-index: standalone semantic index Job created (no build lease, \
                 CPU folded into protected capacity)"
            ),
            Err(error) => warn!(
                project_id,
                revision,
                %error,
                "scip-index: Job create failed; the warm path re-indexes inline as \
                 it does today and the next tick retries"
            ),
        }
        decision
    }
}

/// Production inventory backed by a live `kube::Client`.
pub struct KubeClientScipJobInventory {
    client: kube::Client,
}

impl KubeClientScipJobInventory {
    #[must_use]
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ScipJobInventory for KubeClientScipJobInventory {
    async fn observe(
        &self,
        namespace: &str,
        project_id: &str,
    ) -> Result<ScipJobObservation, String> {
        let api: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        let sanitized = crate::warm_job::sanitize_id(project_id);
        let selector = format!("{LABEL_SCIP_INDEX}=true,{LABEL_PROJECT_ID}={sanitized}");
        let list = api
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| e.to_string())?;
        Ok(observe_from_jobs(&list.items, Utc::now()))
    }
}

/// Fold a retained Job list into an observation. Split out of the apiserver
/// call so the projection is testable from fixtures.
#[must_use]
pub fn observe_from_jobs(jobs: &[Job], now: DateTime<Utc>) -> ScipJobObservation {
    let mut observation = ScipJobObservation::default();
    let mut newest_succeeded: Option<(DateTime<Utc>, String)> = None;
    let mut newest_created: Option<DateTime<Utc>> = None;

    for job in jobs {
        let created = job
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0)
            .unwrap_or(now);
        newest_created = Some(newest_created.map_or(created, |c| c.max(created)));

        let status = job.status.as_ref();
        let succeeded = status.and_then(|s| s.succeeded).unwrap_or(0) > 0;
        let failed = status.and_then(|s| s.failed).unwrap_or(0) > 0;
        if !succeeded && !failed {
            observation.in_flight = true;
            continue;
        }
        if !succeeded {
            continue;
        }
        // Only a SUCCEEDED Job advances the ledger. A failed index leaves the
        // last known-good revision in place, so the next tick still sees the
        // head as advanced and retries.
        let Some(revision) = job
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(ANNOTATION_SCIP_REVISION))
            .filter(|r| !r.trim().is_empty())
        else {
            continue;
        };
        if newest_succeeded
            .as_ref()
            .is_none_or(|(at, _)| created >= *at)
        {
            newest_succeeded = Some((created, revision.clone()));
        }
    }

    observation.last_indexed_revision = newest_succeeded.map(|(_, revision)| revision);
    // A negative age (clock skew, or a Job created "in the future") clamps to
    // zero rather than wrapping: a wrapped age would read as an enormous
    // elapsed time and defeat the cadence floor.
    observation.since_last_dispatch = newest_created
        .map(|created| Duration::from_secs((now - created).num_seconds().max(0) as u64));
    observation
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::batch::v1::JobStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use k8s_openapi::chrono::TimeDelta;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const THREE_HOURS: Duration = Duration::from_secs(10_800);

    fn observed(
        in_flight: bool,
        last: Option<&str>,
        since: Option<Duration>,
    ) -> ScipJobObservation {
        ScipJobObservation {
            in_flight,
            last_indexed_revision: last.map(str::to_string),
            since_last_dispatch: since,
        }
    }

    /// **The change-detection gate.** An unchanged head skips, and it skips no
    /// matter how long ago the last index ran — the requirement is "every 3
    /// hours AND only if the head advanced", so time elapsing must not be able
    /// to authorize an index of an unchanged tree.
    #[test]
    fn unchanged_head_never_dispatches_however_much_time_passed() {
        for elapsed in [
            Some(Duration::ZERO),
            Some(THREE_HOURS),
            Some(Duration::from_secs(30 * 86_400)),
            None,
        ] {
            assert_eq!(
                decide(
                    Some(HEAD),
                    Some(&observed(false, Some(HEAD), elapsed)),
                    THREE_HOURS
                ),
                ScipIndexDecision::SkipHeadUnchanged {
                    revision: HEAD.to_string()
                },
                "elapsed={elapsed:?} must not defeat the head-advance gate",
            );
        }
    }

    /// The complement: an ADVANCED head with the cadence satisfied dispatches.
    /// Without this the previous test would also pass for a gate that never
    /// dispatches anything.
    #[test]
    fn advanced_head_past_the_cadence_dispatches() {
        assert_eq!(
            decide(
                Some(HEAD),
                Some(&observed(false, Some(OLD), Some(THREE_HOURS))),
                THREE_HOURS,
            ),
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            },
        );
        // First ever run: no ledger, no prior dispatch.
        assert_eq!(
            decide(
                Some(HEAD),
                Some(&ScipJobObservation::default()),
                THREE_HOURS
            ),
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            },
        );
    }

    /// The cadence floor rate-limits a continuously-advancing `main` so a
    /// leaseless 16Gi Job cannot become a permanent resident.
    #[test]
    fn advanced_head_inside_the_cadence_window_waits() {
        let decision = decide(
            Some(HEAD),
            Some(&observed(false, Some(OLD), Some(Duration::from_secs(600)))),
            THREE_HOURS,
        );
        assert_eq!(
            decision,
            ScipIndexDecision::SkipCadence {
                remaining: Duration::from_secs(10_200)
            },
        );
        // Exactly at the boundary the floor is satisfied.
        assert!(matches!(
            decide(
                Some(HEAD),
                Some(&observed(false, Some(OLD), Some(THREE_HOURS))),
                THREE_HOURS,
            ),
            ScipIndexDecision::Dispatch { .. }
        ));
    }

    /// Uncertainty never dispatches. An unknown head has no correct Job name,
    /// and an unreadable inventory is not evidence that anything advanced.
    #[test]
    fn unknown_head_and_unreadable_inventory_fail_closed() {
        let fresh = observed(false, None, None);
        assert_eq!(
            decide(None, Some(&fresh), THREE_HOURS),
            ScipIndexDecision::SkipUnknownRevision
        );
        assert_eq!(
            decide(Some("   "), Some(&fresh), THREE_HOURS),
            ScipIndexDecision::SkipUnknownRevision
        );
        assert_eq!(
            decide(Some(HEAD), None, THREE_HOURS),
            ScipIndexDecision::SkipInventoryUnavailable
        );
    }

    #[test]
    fn in_flight_job_coalesces() {
        assert_eq!(
            decide(
                Some(HEAD),
                Some(&observed(true, Some(OLD), None)),
                THREE_HOURS
            ),
            ScipIndexDecision::SkipInFlight
        );
    }

    // ---- inventory projection ---------------------------------------------

    fn scip_job(revision: &str, succeeded: bool, failed: bool, age_secs: i64) -> Job {
        Job {
            metadata: ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    ANNOTATION_SCIP_REVISION.to_string(),
                    revision.to_string(),
                )])),
                creation_timestamp: Some(Time(now() - TimeDelta::seconds(age_secs))),
                ..ObjectMeta::default()
            },
            status: Some(JobStatus {
                succeeded: succeeded.then_some(1),
                failed: failed.then_some(1),
                ..JobStatus::default()
            }),
            ..Job::default()
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_000_000, 0).expect("fixed test instant")
    }

    /// Only a SUCCEEDED Job advances the ledger. A failed index must leave the
    /// previous known-good revision in place so the head still reads as
    /// advanced and a later tick retries — otherwise one failure would mark the
    /// tree as indexed and suppress every retry until the next commit.
    ///
    /// The retry is gated by the cadence floor, not by the ledger: a failed
    /// index is still a dispatch, so a Pod that OOMs or times out cannot be
    /// re-created in a tight loop. Both halves are asserted, because "the
    /// ledger did not advance" and "the retry actually happens" are different
    /// claims and only one of them is about the ledger.
    #[test]
    fn a_failed_index_does_not_advance_the_ledger_and_retries_after_the_cadence() {
        let past_the_floor = observe_from_jobs(
            &[
                scip_job(OLD, true, false, 40_000),
                scip_job(HEAD, false, true, 20_000),
            ],
            now(),
        );
        assert_eq!(
            past_the_floor.last_indexed_revision.as_deref(),
            Some(OLD),
            "a FAILED Job carrying the head revision must not be read as \
             evidence that the head was indexed"
        );
        assert!(!past_the_floor.in_flight);
        assert_eq!(
            decide(Some(HEAD), Some(&past_the_floor), THREE_HOURS),
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            },
            "once the cadence floor has elapsed the head still reads as \
             advanced, so the failed index is retried"
        );

        // ...but not immediately. A failure 100s ago is still a dispatch for
        // cadence purposes, so a crash-looping index cannot re-create a
        // leaseless 16Gi Pod every tick.
        let inside_the_floor = observe_from_jobs(
            &[
                scip_job(OLD, true, false, 40_000),
                scip_job(HEAD, false, true, 100),
            ],
            now(),
        );
        assert_eq!(inside_the_floor.last_indexed_revision.as_deref(), Some(OLD));
        assert!(matches!(
            decide(Some(HEAD), Some(&inside_the_floor), THREE_HOURS),
            ScipIndexDecision::SkipCadence { .. }
        ));
    }

    #[test]
    fn newest_succeeded_revision_wins_and_non_terminal_marks_in_flight() {
        let observation = observe_from_jobs(
            &[
                scip_job(OLD, true, false, 40_000),
                scip_job(HEAD, true, false, 100),
            ],
            now(),
        );
        assert_eq!(observation.last_indexed_revision.as_deref(), Some(HEAD));
        assert_eq!(
            observation.since_last_dispatch,
            Some(Duration::from_secs(100))
        );

        let running = observe_from_jobs(
            &[Job {
                metadata: ObjectMeta {
                    creation_timestamp: Some(Time(now())),
                    ..ObjectMeta::default()
                },
                status: None,
                ..Job::default()
            }],
            now(),
        );
        assert!(running.in_flight);
        assert_eq!(running.last_indexed_revision, None);
    }

    #[test]
    fn empty_inventory_is_a_first_run_not_an_error() {
        let observation = observe_from_jobs(&[], now());
        assert_eq!(observation, ScipJobObservation::default());
    }

    // ---- scheduler side effect --------------------------------------------

    struct RecordingInventory(Result<ScipJobObservation, String>);

    #[async_trait]
    impl ScipJobInventory for RecordingInventory {
        async fn observe(&self, _ns: &str, _p: &str) -> Result<ScipJobObservation, String> {
            self.0.clone()
        }
    }

    #[derive(Default)]
    struct RecordingDispatcher(Mutex<Vec<Job>>);

    #[async_trait]
    impl WarmJobDispatcher for RecordingDispatcher {
        async fn dispatch(&self, _ns: &str, job: Job) -> Result<String, String> {
            let name = job.metadata.name.clone().unwrap_or_default();
            self.0.lock().expect("lock").push(job);
            Ok(name)
        }
    }

    async fn run_tick(
        observation: Result<ScipJobObservation, String>,
        head: Option<&str>,
    ) -> (ScipIndexDecision, Vec<Job>) {
        let dispatcher = Arc::new(RecordingDispatcher::default());
        let scheduler = ScipIndexScheduler::new(
            KubernetesConfig::for_testing(),
            Arc::new(RecordingInventory(observation)),
            dispatcher.clone(),
        );
        let decision = scheduler.tick_project("proj", head, "img:tag", None).await;
        let created = dispatcher.0.lock().expect("lock").clone();
        (decision, created)
    }

    /// **A skip must create no Kubernetes object.** Asserting the decision
    /// alone would stay green if `tick_project` dispatched unconditionally, so
    /// this asserts the side effect.
    #[tokio::test]
    async fn skips_create_no_job_and_dispatch_creates_exactly_one() {
        for (label, observation, head) in [
            (
                "unchanged head",
                Ok(observed(false, Some(HEAD), Some(THREE_HOURS))),
                Some(HEAD),
            ),
            ("in flight", Ok(observed(true, None, None)), Some(HEAD)),
            (
                "inside cadence",
                Ok(observed(false, Some(OLD), Some(Duration::from_secs(60)))),
                Some(HEAD),
            ),
            ("unknown head", Ok(ScipJobObservation::default()), None),
            (
                "inventory error",
                Err("apiserver unreachable".to_string()),
                Some(HEAD),
            ),
        ] {
            let (decision, created) = run_tick(observation, head).await;
            assert!(
                created.is_empty(),
                "{label}: decision {decision:?} created {} Job(s); a skip must \
                 create nothing at all",
                created.len(),
            );
        }

        let (decision, created) = run_tick(
            Ok(observed(false, Some(OLD), Some(THREE_HOURS))),
            Some(HEAD),
        )
        .await;
        assert_eq!(
            decision,
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            }
        );
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].metadata.name.as_deref(),
            Some(crate::scip_job::scip_index_job_name("proj", HEAD).as_str()),
        );
        assert_eq!(
            created[0]
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(ANNOTATION_SCIP_REVISION))
                .map(String::as_str),
            Some(HEAD),
            "the created Job must carry the revision the next tick reads back"
        );
    }

    /// The dispatched Job round-trips through the inventory projection into a
    /// `SkipHeadUnchanged` on the following tick. Without this the ledger and
    /// the gate could agree in the tests and disagree in production.
    #[tokio::test]
    async fn a_dispatched_job_closes_the_gate_on_the_next_tick() {
        let (_, created) = run_tick(
            Ok(observed(false, Some(OLD), Some(THREE_HOURS))),
            Some(HEAD),
        )
        .await;
        let mut succeeded = created[0].clone();
        succeeded.metadata.creation_timestamp = Some(Time(now()));
        succeeded.status = Some(JobStatus {
            succeeded: Some(1),
            ..JobStatus::default()
        });

        let observation = observe_from_jobs(&[succeeded], now());
        assert_eq!(observation.last_indexed_revision.as_deref(), Some(HEAD));
        assert_eq!(
            decide(Some(HEAD), Some(&observation), THREE_HOURS),
            ScipIndexDecision::SkipHeadUnchanged {
                revision: HEAD.to_string()
            },
        );
    }

    #[test]
    fn interval_comes_from_config() {
        let mut cfg = KubernetesConfig::for_testing();
        assert_eq!(cfg.scip_index_interval_seconds, 10_800, "3 hours");
        cfg.scip_index_interval_seconds = 42;
        let scheduler = ScipIndexScheduler::new(
            cfg,
            Arc::new(RecordingInventory(Ok(ScipJobObservation::default()))),
            Arc::new(RecordingDispatcher::default()),
        );
        assert_eq!(scheduler.interval(), Duration::from_secs(42));
    }
}

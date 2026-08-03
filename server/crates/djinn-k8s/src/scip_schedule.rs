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
use djinn_db::{Database, WarmGraphAttemptRepository, WarmGraphOutcome};
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
    /// A non-terminal **warm** Job already exists for this project. The warm
    /// Job runs the same `rust-analyzer scip` fan-out inline, so dispatching
    /// here would put two ~10 GB indexers on one node to compute the same
    /// artifacts. See the gate's rationale on [`decide`].
    SkipWarmInFlight,
    /// Durable warm publication already covers this exact mirror head.
    SkipGraphAlreadyCoversHead,
    /// Warm has not yet made a durable attempt for this exact mirror head.
    SkipWarmNotYetAttempted,
    /// The last successful index already covers this exact revision. **This is
    /// the change-detection gate** and it is checked before the cadence, so an
    /// unchanged head never dispatches no matter how much time has passed.
    SkipHeadUnchanged { revision: String },
    /// Head advanced, but the tree has not stood still long enough for the
    /// index to be worth producing — it would very likely be stale before it
    /// finished. `None` age means the head's commit time was unreadable, which
    /// is treated the same way. See [`decide`].
    SkipHeadNotQuiescent { age: Option<Duration> },
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
            Self::SkipWarmInFlight => "warm_in_flight",
            Self::SkipGraphAlreadyCoversHead => "graph_already_covers_head",
            Self::SkipWarmNotYetAttempted => "warm_not_yet_attempted",
            Self::SkipHeadUnchanged { .. } => "head_unchanged",
            Self::SkipHeadNotQuiescent { .. } => "head_not_quiescent",
            Self::SkipCadence { .. } => "cadence",
        }
    }
}

/// What the cluster currently holds for one project's SCIP-index population.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScipJobObservation {
    /// At least one SCIP Job for this project is neither succeeded nor failed.
    pub in_flight: bool,
    /// At least one **warm** Job for this project is neither succeeded nor
    /// failed. Read through the same `djinn.app/warm=true` predicate the warm
    /// dispatcher itself uses ([`crate::graph_warmer::WarmJobLister`]), so the
    /// two populations cannot drift apart in what "in flight" means.
    pub warm_in_flight: bool,
    /// Revision of the most recently *verified indexed* SCIP Job, read off its
    /// [`ANNOTATION_SCIP_REVISION`] annotation. The standalone worker's
    /// completion contract makes Job success an explicit `Indexed` proof;
    /// cold-base and contention skips exit nonzero and cannot advance this
    /// ledger. `None` when no verified Job is retained — first run, or the
    /// ledger aged out.
    pub last_indexed_revision: Option<String>,
    /// Age of the most recent SCIP Job of any outcome, from its
    /// `creationTimestamp`. `None` when none is retained.
    pub since_last_dispatch: Option<Duration>,
    /// Durable warm lifecycle classification for the exact mirror head passed
    /// to [`decide`]. `None` is an unavailable read and fails closed.
    pub warm_outcome: Option<WarmGraphOutcome>,
}

/// Reads the durable warm lifecycle for the exact head being scheduled.
#[async_trait]
pub trait WarmOutcomeSource: Send + Sync {
    async fn warm_outcome_for_head(
        &self,
        project_id: &str,
        exact_head_revision: &str,
    ) -> Result<WarmGraphOutcome, String>;
}

/// Production durable warm lifecycle source.
pub struct RepositoryWarmOutcomeSource {
    db: Database,
}

impl RepositoryWarmOutcomeSource {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl WarmOutcomeSource for RepositoryWarmOutcomeSource {
    async fn warm_outcome_for_head(
        &self,
        project_id: &str,
        exact_head_revision: &str,
    ) -> Result<WarmGraphOutcome, String> {
        WarmGraphAttemptRepository::new(self.db.clone())
            .warm_outcome_for_head(project_id, exact_head_revision)
            .await
            .map_err(|error| error.to_string())
    }
}

struct UnconfiguredWarmOutcomeSource;

#[async_trait]
impl WarmOutcomeSource for UnconfiguredWarmOutcomeSource {
    async fn warm_outcome_for_head(
        &self,
        _project_id: &str,
        _exact_head_revision: &str,
    ) -> Result<WarmGraphOutcome, String> {
        Err("durable warm outcome source is not configured".to_string())
    }
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

/// The observed head of a project's mirror, and how long it has been the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorHead {
    /// `refs/heads/main` in the project's bare mirror.
    pub revision: String,
    /// Age of that commit, i.e. how long the tree has been unchanged.
    pub age: Duration,
}

/// Resolves [`MirrorHead`] for a project. A separate seam from the warm path's
/// own tip discovery so the SCIP cadence cannot be coupled to warm dispatch.
#[async_trait]
pub trait MirrorHeadSource: Send + Sync {
    async fn head(&self, project_id: &str) -> Option<MirrorHead>;
}

/// The pure decision. No I/O, no clock, no cluster — every input is an
/// argument, so every branch is directly unit-testable.
///
/// Ordering is load-bearing:
/// 1. unknown head and unreadable inventory fail closed;
/// 2. in-flight coalesces — a SCIP Job, and equally a **warm** Job;
/// 3. **head-unchanged wins over the cadence**, because the requirement is
///    "every 3 hours *and only if* the head advanced", not "or";
/// 4. **head-not-quiescent** skips — see below;
/// 5. the cadence floor rate-limits an advancing head;
/// 6. only a durable warm attempt that did not publish authorizes recovery.
///
/// # Why an in-flight warm Job blocks a dispatch
///
/// The warm Job runs the identical `rust-analyzer scip` fan-out inline. On
/// 2026-07-28 the first successful standalone run overlapped a warm by ~57
/// seconds and both indexed the same workspace at the same commit, peaking at
/// 10.2 GB and 9.5 GB RSS simultaneously — the split's entire purpose is that
/// the warm *skips* the index, and instead the node paid for it twice.
///
/// Not dispatching into a running warm is the cheapest possible fix for the
/// common case, and it is stateless: it reads the same cluster inventory this
/// module already reads. It is **not** sufficient on its own — a warm Job
/// created one second after this list is racy, and the warm dispatcher is
/// driven by a 60s `mirror_fetcher` tick — so the authoritative exclusion is
/// `djinn_graph::semantic_index_claim`, a cross-Pod `flock` on the shared cache
/// PVC that both Pods take around their indexer fan-out. This gate keeps the
/// leaseless 16Gi Pod from being created at all in the case that gate can see.
///
/// It cannot wedge: it is re-evaluated every tick and a warm Job is bounded by
/// its own `activeDeadlineSeconds`, so "warm in flight" is self-clearing state
/// owned by the apiserver, not something this module has to reclaim. It does
/// mean a project whose warm Job is almost always running dispatches SCIP
/// rarely — which is the correct trade: while a warm is indexing this project
/// inline, a standalone index of it is either the same work twice or a second
/// ~10 GB indexer on the same node.
///
/// # The quiescence gate, and why the benefit is conditional
///
/// The SCIP artifact cache key includes `source_hashes` over the workspace's
/// sources (`CacheKeyIngredients`, `scip_indexer/cache.rs`). So an index
/// produced at head `H0` is only a `CachedHit` for a warm that runs against
/// `H0`'s sources. Warm chases head continuously — `mirror_fetcher` triggers
/// every 60s and the dead debounce collapses every window at its 900s cap — so
/// **if `main` advances during the ~59 minutes this Job takes, the warm that
/// follows misses the cache and re-indexes inline for the full 58m43s.** On a
/// repository merging more often than once an hour the hit rate tends toward
/// zero and the SCIP Job's work is redundant.
///
/// (The key is per-workspace, so a commit touching only workspace A leaves
/// workspace B's key intact and B still hits. That helps a polyglot repo, but
/// it does not help the case that dominates the cost here: djinn's own Rust
/// `server/` workspace is both the most expensive to index and the most
/// frequently touched.)
///
/// This gate is the cheap, stateless mitigation: **do not index a tree that has
/// not been stable for at least as long as indexing it takes.** `quiescence`
/// defaults to the measured 3523s phase cost, so a dispatch only happens when
/// the head has already stood still longer than the run will last — the
/// condition under which the result is likely to still be current when it
/// lands. It cannot prevent a commit arriving *during* the run; nothing can.
///
/// An unreadable head age is treated as not quiescent, consistent with every
/// other uncertainty here resolving toward not dispatching.
#[must_use]
pub fn decide(
    head_revision: Option<&str>,
    head_age: Option<Duration>,
    observation: Option<&ScipJobObservation>,
    interval: Duration,
    quiescence: Duration,
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
    if observation.warm_in_flight {
        return ScipIndexDecision::SkipWarmInFlight;
    }
    if observation.last_indexed_revision.as_deref() == Some(head) {
        return ScipIndexDecision::SkipHeadUnchanged {
            revision: head.to_string(),
        };
    }
    if !quiescence.is_zero() {
        let Some(age) = head_age else {
            return ScipIndexDecision::SkipHeadNotQuiescent { age: None };
        };
        if age < quiescence {
            return ScipIndexDecision::SkipHeadNotQuiescent { age: Some(age) };
        }
    }
    if let Some(elapsed) = observation.since_last_dispatch
        && elapsed < interval
    {
        return ScipIndexDecision::SkipCadence {
            remaining: interval - elapsed,
        };
    }
    match observation.warm_outcome.as_ref() {
        Some(WarmGraphOutcome::Published(_)) => ScipIndexDecision::SkipGraphAlreadyCoversHead,
        Some(WarmGraphOutcome::NotTriedYet) => ScipIndexDecision::SkipWarmNotYetAttempted,
        Some(WarmGraphOutcome::InProgress(_)) => ScipIndexDecision::SkipWarmInFlight,
        Some(WarmGraphOutcome::TriedAndDidNotPublish(_)) => ScipIndexDecision::Dispatch {
            revision: head.to_string(),
        },
        None => ScipIndexDecision::SkipInventoryUnavailable,
    }
}

/// Drives [`decide`] against the live cluster and creates the Job when it says
/// to. Deliberately owns no timer: the caller supplies the tick, so the loop
/// interval and the cadence floor stay independently configurable and the
/// scheduler stays testable without `tokio::time`.
pub struct ScipIndexScheduler {
    config: KubernetesConfig,
    inventory: Arc<dyn ScipJobInventory>,
    warm_outcomes: Arc<dyn WarmOutcomeSource>,
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
            warm_outcomes: Arc::new(UnconfiguredWarmOutcomeSource),
            dispatcher,
        }
    }

    /// Supply the durable warm lifecycle source. Production uses
    /// [`RepositoryWarmOutcomeSource`]; tests can inject a deterministic seam.
    #[must_use]
    pub fn with_warm_outcome_source(mut self, warm_outcomes: Arc<dyn WarmOutcomeSource>) -> Self {
        self.warm_outcomes = warm_outcomes;
        self
    }

    /// The configured cadence floor.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.config.scip_index_interval_seconds)
    }

    /// How long the head must have stood still before an index is worth
    /// producing. See [`decide`].
    #[must_use]
    pub fn quiescence(&self) -> Duration {
        Duration::from_secs(self.config.scip_quiescence_seconds)
    }

    /// Whether the operator has armed SCIP-index dispatch at all. Defaults to
    /// `false`: the composition root wires the real scheduler unconditionally,
    /// and this flag is the only thing that decides whether it creates Jobs, so
    /// arming it is a config flip rather than a redeploy.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.scip_index_enabled
    }

    /// Evaluate one project without creating anything. Exposed so the decision
    /// can be observed (and asserted) separately from the side effect.
    pub async fn evaluate(&self, project_id: &str, head: Option<&MirrorHead>) -> ScipIndexDecision {
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
        let observation = match (observation, head) {
            (Some(mut observation), Some(head)) => match self
                .warm_outcomes
                .warm_outcome_for_head(project_id, &head.revision)
                .await
            {
                Ok(outcome) => {
                    observation.warm_outcome = Some(outcome);
                    Some(observation)
                }
                Err(error) => {
                    warn!(project_id, revision = %head.revision, %error, "scip-index: durable warm outcome unavailable; skipping this tick");
                    None
                }
            },
            (observation, _) => observation,
        };
        decide(
            head.map(|h| h.revision.as_str()),
            head.map(|h| h.age),
            observation.as_ref(),
            self.interval(),
            self.quiescence(),
        )
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
        head: Option<&MirrorHead>,
        image_tag: &str,
        policy: Option<&djinn_stack::environment::CargoCachePolicy>,
    ) -> ScipIndexDecision {
        let decision = self.evaluate(project_id, head).await;
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

/// Reconcile durable warm attempts on the scheduler's existing tick.
///
/// Retained Kubernetes Jobs are deliberately not an input. Exact-head graph
/// publication and immutable attempt deadlines are durable evidence, so
/// publication is reconciled before stale rows are reaped. The caller supplies
/// `now` and the scheduler-tick `grace` so strict boundaries need no sleep.
pub async fn reconcile_warm_attempts_for_heads(
    db: Database,
    heads: &[(String, String)],
    now: &str,
    grace: Duration,
) -> anyhow::Result<Vec<String>> {
    let attempts = WarmGraphAttemptRepository::new(db);
    for (project_id, revision) in heads {
        // A bad or cross-revision inventory is not completion evidence. Keep
        // reconciling other heads instead of blocking stale recovery globally.
        if let Err(error) = attempts.warm_outcome_for_head(project_id, revision).await {
            warn!(
                project_id,
                revision,
                %error,
                "warm-attempt reconciliation: exact-head publication unavailable"
            );
        }
    }
    let grace = k8s_openapi::chrono::Duration::from_std(grace)?;
    Ok(attempts.reconcile_stale_attempts(now, grace).await?)
}

/// Reads `refs/heads/main` and its commit age straight out of the project's
/// bare mirror.
///
/// This deliberately duplicates the two-line git call in
/// `graph_warmer::discover_mirror_main_tip` rather than sharing it. The warm
/// path's tip discovery is private to its dispatch flow, and the duplication
/// keeps this module free of any coupling to warm dispatch — the two cadences
/// must be able to move independently, which was the whole point of not
/// building on the (dead) warm debounce.
pub struct GitMirrorHeadSource;

#[async_trait]
impl MirrorHeadSource for GitMirrorHeadSource {
    async fn head(&self, project_id: &str) -> Option<MirrorHead> {
        let mirror = djinn_workspace::mirror_path_for(project_id);
        let rev = djinn_git::run_git_command_in(
            &mirror,
            vec!["rev-parse".into(), "refs/heads/main".into()],
        )
        .await
        .ok()?;
        let revision = rev.stdout.trim().to_string();
        if revision.is_empty() {
            return None;
        }
        // Committer timestamp of the head commit. `decide` treats an
        // unreadable age as "not quiescent", so a failure here skips rather
        // than dispatching against an unknown tree.
        let stamp = djinn_git::run_git_command_in(
            &mirror,
            vec![
                "log".into(),
                "-1".into(),
                "--format=%ct".into(),
                revision.clone(),
            ],
        )
        .await
        .ok()?;
        let committed_at = stamp.stdout.trim().parse::<i64>().ok()?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        // A commit dated in the future (clock skew) reads as age zero, i.e.
        // maximally NOT quiescent — the fail-closed direction.
        let age = Duration::from_secs(now.saturating_sub(committed_at).max(0) as u64);
        Some(MirrorHead { revision, age })
    }
}

/// Production inventory backed by a live `kube::Client`.
pub struct KubeClientScipJobInventory {
    client: kube::Client,
    /// The warm-Job predicate, deliberately **reused** rather than
    /// reimplemented: "is a warm in flight" must mean the same thing here as it
    /// does to the dispatcher that creates warm Jobs, or the two views drift
    /// and this gate stops gating.
    warm: Arc<dyn crate::graph_warmer::WarmJobLister>,
}

impl KubeClientScipJobInventory {
    #[must_use]
    pub fn new(client: kube::Client) -> Self {
        Self {
            warm: Arc::new(crate::graph_warmer::KubeClientWarmJobLister::new(
                client.clone(),
            )),
            client,
        }
    }

    /// Construct with an explicit warm-Job lister. Exists so the warm gate can
    /// be exercised without a cluster.
    #[must_use]
    pub fn with_warm_lister(
        client: kube::Client,
        warm: Arc<dyn crate::graph_warmer::WarmJobLister>,
    ) -> Self {
        Self { client, warm }
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
        observe_including_warm(
            &list.items,
            self.warm.as_ref(),
            namespace,
            project_id,
            Utc::now(),
        )
        .await
    }
}

/// Fold the SCIP-Job projection together with the warm-Job predicate.
///
/// Split from the apiserver call so everything except the single `list` line is
/// testable without a cluster — in particular that the production inventory
/// *does* consult the warm population, which is the difference between this
/// gate existing and this gate being inert.
pub(crate) async fn observe_including_warm(
    scip_jobs: &[Job],
    warm: &dyn crate::graph_warmer::WarmJobLister,
    namespace: &str,
    project_id: &str,
    now: DateTime<Utc>,
) -> Result<ScipJobObservation, String> {
    let mut observation = observe_from_jobs(scip_jobs, now);
    // An unreadable warm population is an unreadable inventory: `decide` turns
    // that into `SkipInventoryUnavailable`, because dispatching on unknown
    // state is the only branch here that can cost anything.
    observation.warm_in_flight = warm
        .has_in_flight_warm(namespace, project_id)
        .await
        .map_err(|e| format!("list warm Jobs: {e}"))?;
    Ok(observation)
}

/// Fold a retained **SCIP** Job list into an observation. Split out of the
/// apiserver call so the projection is testable from fixtures.
///
/// Leaves `warm_in_flight` at its default: warm Jobs are a different label
/// selector and a different population, folded in by the caller.
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
        // Only a SUCCEEDED Job advances the ledger. The standalone worker exits
        // zero only for `ScipIndexOutcome::Indexed`; cold-base and contention
        // skips intentionally fail the Job. Thus success is an explicit proof
        // of indexing rather than a generic process-completed signal. Any
        // non-success leaves the last known-good revision in place so the next
        // tick still sees the head as advanced and retries.
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
    use djinn_db::{
        Database, WarmGraphAttempt, WarmGraphAttemptRepository, WarmGraphAttemptStatus,
        test_support::seed_project,
    };
    use k8s_openapi::api::batch::v1::JobStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use k8s_openapi::chrono::TimeDelta;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const THREE_HOURS: Duration = Duration::from_secs(10_800);
    /// Long enough to satisfy the default quiescence gate in tests that are
    /// not about it.
    const QUIET: Option<Duration> = Some(Duration::from_secs(9_999));
    const QUIESCENCE: Duration = Duration::from_secs(3_523);

    fn attempt(status: WarmGraphAttemptStatus) -> WarmGraphAttempt {
        WarmGraphAttempt {
            attempt_id: "attempt".to_string(),
            project_id: "proj".to_string(),
            revision: HEAD.to_string(),
            status,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            deadline_at: "2099-01-01T00:00:00Z".to_string(),
            finished_at: None,
            detail: None,
        }
    }

    fn recovery_outcome() -> WarmGraphOutcome {
        WarmGraphOutcome::TriedAndDidNotPublish(attempt(WarmGraphAttemptStatus::Failed))
    }

    /// There are deliberately no Kubernetes Job fixtures here. Job retention
    /// is a hint only: normal terminal state survives its TTL and an absent Job
    /// cannot make a durable running attempt time out before its grace window.
    #[tokio::test]
    async fn failed_warm_remains_recoverable_after_job_gc() {
        let db = Database::open_in_memory().expect("test database");
        seed_project(&db, "proj", "proj").await;
        let attempts = WarmGraphAttemptRepository::new(db.clone());
        let tick = Duration::from_secs(300);
        // `start_attempt` records `started_at` with PostgreSQL's transaction
        // clock, so use a deadline safely after that real clock. Derive all
        // reconciliation instants from it to keep this boundary fixture
        // deterministic without sleeping.
        let deadline_at = Utc::now() + TimeDelta::days(1);
        let deadline = deadline_at.to_rfc3339();
        let running = attempts
            .start_attempt("proj", HEAD, &deadline)
            .await
            .expect("persist running attempt");
        let failed = attempts
            .start_attempt("proj", OLD, &deadline)
            .await
            .expect("persist terminal attempt");
        assert!(
            attempts
                .finish_attempt_if_running(
                    &failed,
                    WarmGraphAttemptStatus::Failed,
                    Some("normal failure")
                )
                .await
                .expect("finish failed attempt")
        );

        let heads = vec![("proj".to_string(), HEAD.to_string())];
        let timeout_boundary = deadline_at + TimeDelta::seconds(tick.as_secs() as i64);
        for now in [
            timeout_boundary - TimeDelta::milliseconds(1),
            timeout_boundary,
        ] {
            assert!(
                reconcile_warm_attempts_for_heads(db.clone(), &heads, &now.to_rfc3339(), tick)
                    .await
                    .expect("reconcile absent Job")
                    .is_empty()
            );
            assert_eq!(
                attempts
                    .get_attempt(&running)
                    .await
                    .expect("read running")
                    .expect("row")
                    .status,
                WarmGraphAttemptStatus::Running,
            );
        }
        assert_eq!(
            reconcile_warm_attempts_for_heads(
                db.clone(),
                &heads,
                &(timeout_boundary + TimeDelta::milliseconds(1)).to_rfc3339(),
                tick,
            )
            .await
            .expect("post-boundary reconcile"),
            vec![running.clone()],
        );
        assert!(
            reconcile_warm_attempts_for_heads(
                db.clone(),
                &heads,
                &(timeout_boundary + TimeDelta::seconds(tick.as_secs() as i64)).to_rfc3339(),
                tick,
            )
            .await
            .expect("idempotent reconcile")
            .is_empty()
        );
        assert_eq!(
            attempts
                .get_attempt(&running)
                .await
                .expect("read timeout")
                .expect("row")
                .status,
            WarmGraphAttemptStatus::TimedOut,
        );
        assert_eq!(
            attempts
                .get_attempt(&failed)
                .await
                .expect("read failure")
                .expect("row")
                .status,
            WarmGraphAttemptStatus::Failed,
            "a normal terminal failure survives Job garbage collection",
        );
    }

    fn observed(
        in_flight: bool,
        last: Option<&str>,
        since: Option<Duration>,
    ) -> ScipJobObservation {
        ScipJobObservation {
            in_flight,
            last_indexed_revision: last.map(str::to_string),
            since_last_dispatch: since,
            warm_outcome: Some(recovery_outcome()),
            ..ScipJobObservation::default()
        }
    }

    #[test]
    fn warm_outcome() {
        let base = observed(false, Some(OLD), Some(THREE_HOURS));
        for (outcome, expected) in [
            (
                WarmGraphOutcome::Published(attempt(WarmGraphAttemptStatus::PublishedComplete)),
                ScipIndexDecision::SkipGraphAlreadyCoversHead,
            ),
            (
                WarmGraphOutcome::NotTriedYet,
                ScipIndexDecision::SkipWarmNotYetAttempted,
            ),
            (
                WarmGraphOutcome::InProgress(attempt(WarmGraphAttemptStatus::Running)),
                ScipIndexDecision::SkipWarmInFlight,
            ),
            (
                recovery_outcome(),
                ScipIndexDecision::Dispatch {
                    revision: HEAD.to_string(),
                },
            ),
        ] {
            let mut observation = base.clone();
            observation.warm_outcome = Some(outcome);
            assert_eq!(
                decide(
                    Some(HEAD),
                    QUIET,
                    Some(&observation),
                    THREE_HOURS,
                    QUIESCENCE
                ),
                expected
            );
        }
        let mut unavailable = base;
        unavailable.warm_outcome = None;
        assert_eq!(
            decide(
                Some(HEAD),
                QUIET,
                Some(&unavailable),
                THREE_HOURS,
                QUIESCENCE
            ),
            ScipIndexDecision::SkipInventoryUnavailable
        );
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
                    QUIET,
                    Some(&observed(false, Some(HEAD), elapsed)),
                    THREE_HOURS,
                    QUIESCENCE,
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
                QUIET,
                Some(&observed(false, Some(OLD), Some(THREE_HOURS))),
                THREE_HOURS,
                QUIESCENCE,
            ),
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            },
        );
        // First ever run: no ledger, no prior dispatch.
        assert_eq!(
            decide(
                Some(HEAD),
                QUIET,
                Some(&observed(false, None, None)),
                THREE_HOURS,
                QUIESCENCE
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
            QUIET,
            Some(&observed(false, Some(OLD), Some(Duration::from_secs(600)))),
            THREE_HOURS,
            QUIESCENCE,
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
                QUIET,
                Some(&observed(false, Some(OLD), Some(THREE_HOURS))),
                THREE_HOURS,
                QUIESCENCE,
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
            decide(None, QUIET, Some(&fresh), THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::SkipUnknownRevision
        );
        assert_eq!(
            decide(Some("   "), QUIET, Some(&fresh), THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::SkipUnknownRevision
        );
        assert_eq!(
            decide(Some(HEAD), QUIET, None, THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::SkipInventoryUnavailable
        );
    }

    /// **The quiescence gate.** The SCIP cache key folds in `source_hashes`, so
    /// an index produced at head `H0` only serves a warm running against `H0`'s
    /// sources. Indexing a tree that is still moving produces an artifact that
    /// is stale before it lands — the warm then re-indexes inline and the run
    /// bought nothing. Do not index a tree that has not stood still for at
    /// least as long as indexing it takes.
    #[test]
    fn a_head_that_has_not_stood_still_long_enough_is_not_indexed() {
        let fresh = observed(false, Some(OLD), Some(THREE_HOURS));

        // One second short of the measured phase cost: the index would very
        // likely be stale on arrival.
        assert_eq!(
            decide(
                Some(HEAD),
                Some(QUIESCENCE - Duration::from_secs(1)),
                Some(&fresh),
                THREE_HOURS,
                QUIESCENCE,
            ),
            ScipIndexDecision::SkipHeadNotQuiescent {
                age: Some(QUIESCENCE - Duration::from_secs(1))
            },
        );
        // At the threshold it dispatches — without this the test above would
        // also pass for a gate that never dispatches.
        assert!(matches!(
            decide(
                Some(HEAD),
                Some(QUIESCENCE),
                Some(&fresh),
                THREE_HOURS,
                QUIESCENCE
            ),
            ScipIndexDecision::Dispatch { .. }
        ));
        // An unreadable commit time is uncertainty, and uncertainty never
        // dispatches.
        assert_eq!(
            decide(Some(HEAD), None, Some(&fresh), THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::SkipHeadNotQuiescent { age: None },
        );
        // Zero disables the gate entirely, for a deployment that would rather
        // index eagerly and accept the miss rate.
        assert!(matches!(
            decide(
                Some(HEAD),
                Some(Duration::ZERO),
                Some(&fresh),
                THREE_HOURS,
                Duration::ZERO,
            ),
            ScipIndexDecision::Dispatch { .. }
        ));
    }

    /// An unchanged head still wins over quiescence: re-indexing an already
    /// indexed revision is waste no matter how still the tree is.
    #[test]
    fn head_unchanged_outranks_quiescence() {
        assert_eq!(
            decide(
                Some(HEAD),
                Some(Duration::from_secs(30 * 86_400)),
                Some(&observed(false, Some(HEAD), Some(THREE_HOURS))),
                THREE_HOURS,
                QUIESCENCE,
            ),
            ScipIndexDecision::SkipHeadUnchanged {
                revision: HEAD.to_string()
            },
        );
    }

    /// The default quiescence window is the measured phase cost, not a round
    /// number someone liked — and the feature ships disarmed.
    #[test]
    fn config_defaults_are_the_measured_values_and_the_switch_is_off() {
        let cfg = KubernetesConfig::for_testing();
        assert_eq!(
            cfg.scip_quiescence_seconds as i64,
            crate::scip_job::MEASURED_SCIP_PHASE_SECONDS,
            "the quiescence window must track the measured SCIP phase cost: an \
             index is only worth starting if the tree has already been still \
             for longer than the run will last"
        );
        assert!(
            !cfg.scip_index_enabled,
            "SCIP-index dispatch must ship disarmed; arming it creates leaseless \
             16Gi Pods and is an operator decision"
        );
        let scheduler = ScipIndexScheduler::new(
            cfg,
            Arc::new(RecordingInventory(Ok(ScipJobObservation::default()))),
            Arc::new(RecordingDispatcher::default()),
        );
        assert!(!scheduler.enabled());
        assert_eq!(scheduler.quiescence(), QUIESCENCE);
    }

    #[test]
    fn in_flight_job_coalesces() {
        assert_eq!(
            decide(
                Some(HEAD),
                QUIET,
                Some(&observed(true, Some(OLD), None)),
                THREE_HOURS,
                QUIESCENCE,
            ),
            ScipIndexDecision::SkipInFlight
        );
    }

    /// **The duplicate-index gate.** A warm Job in flight is already running
    /// the same `rust-analyzer scip` fan-out over the same workspace; a SCIP
    /// Job created now puts two ~10 GB indexers on one node to produce the same
    /// artifacts. That is what happened on 2026-07-28.
    ///
    /// It must hold under conditions that would otherwise be a textbook
    /// dispatch — advanced head, quiescent tree, cadence long satisfied — or
    /// the gate is not a gate.
    #[test]
    fn a_warm_job_in_flight_blocks_the_dispatch() {
        let mut warming = observed(false, Some(OLD), Some(THREE_HOURS));
        warming.warm_in_flight = true;
        assert_eq!(
            decide(Some(HEAD), QUIET, Some(&warming), THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::SkipWarmInFlight,
        );
        assert_eq!(
            ScipIndexDecision::SkipWarmInFlight.reason(),
            "warm_in_flight"
        );
        assert!(
            ScipIndexDecision::SkipWarmInFlight
                .dispatch_revision()
                .is_none()
        );

        // The identical observation WITHOUT a warm in flight dispatches, so the
        // assertion above is about the warm flag and nothing else.
        warming.warm_in_flight = false;
        assert!(matches!(
            decide(Some(HEAD), QUIET, Some(&warming), THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::Dispatch { .. }
        ));
    }

    /// The warm gate must create no Kubernetes object. Asserting the decision
    /// alone would stay green if `tick_project` dispatched regardless.
    #[tokio::test]
    async fn a_warm_in_flight_tick_creates_no_job() {
        let mut warming = observed(false, Some(OLD), Some(THREE_HOURS));
        warming.warm_in_flight = true;
        let (decision, created) = run_tick(Ok(warming), Some(HEAD)).await;
        assert_eq!(decision, ScipIndexDecision::SkipWarmInFlight);
        assert!(
            created.is_empty(),
            "a warm-gated tick must create no leaseless 16Gi Pod"
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
        let mut past_the_floor = observe_from_jobs(
            &[
                scip_job(OLD, true, false, 40_000),
                scip_job(HEAD, false, true, 20_000),
            ],
            now(),
        );
        past_the_floor.warm_outcome = Some(recovery_outcome());
        assert_eq!(
            past_the_floor.last_indexed_revision.as_deref(),
            Some(OLD),
            "a FAILED Job carrying the head revision must not be read as \
             evidence that the head was indexed"
        );
        assert!(!past_the_floor.in_flight);
        assert_eq!(
            decide(
                Some(HEAD),
                QUIET,
                Some(&past_the_floor),
                THREE_HOURS,
                QUIESCENCE
            ),
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            },
            "once the cadence floor has elapsed the head still reads as \
             advanced, so the failed index is retried"
        );

        // ...but not immediately. A failure 100s ago is still a dispatch for
        // cadence purposes, so a crash-looping index cannot re-create a
        // leaseless 16Gi Pod every tick.
        let mut inside_the_floor = observe_from_jobs(
            &[
                scip_job(OLD, true, false, 40_000),
                scip_job(HEAD, false, true, 100),
            ],
            now(),
        );
        inside_the_floor.warm_outcome = Some(recovery_outcome());
        assert_eq!(inside_the_floor.last_indexed_revision.as_deref(), Some(OLD));
        assert!(matches!(
            decide(
                Some(HEAD),
                QUIET,
                Some(&inside_the_floor),
                THREE_HOURS,
                QUIESCENCE
            ),
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

    /// The worker maps both no-artifact outcomes to a failed Job, while it maps
    /// `Indexed` to success. The retained Job projection must preserve that
    /// completion contract: skips consume cadence like every dispatch, but do
    /// not claim the exact head is covered and therefore remain retryable.
    #[test]
    fn skipped_recovery_does_not_advance_revision() {
        for skipped in ["cold cargo base", "concurrent index claim"] {
            let mut observation = observe_from_jobs(
                &[
                    scip_job(OLD, true, false, 40_000),
                    scip_job(HEAD, false, true, 20_000),
                ],
                now(),
            );
            observation.warm_outcome = Some(recovery_outcome());

            assert_eq!(
                observation.last_indexed_revision.as_deref(),
                Some(OLD),
                "a {skipped} skip must not claim the head was indexed"
            );
            assert_eq!(
                decide(
                    Some(HEAD),
                    QUIET,
                    Some(&observation),
                    THREE_HOURS,
                    QUIESCENCE
                ),
                ScipIndexDecision::Dispatch {
                    revision: HEAD.to_string()
                },
                "a {skipped} skip remains eligible after the cadence floor"
            );
        }

        let indexed = observe_from_jobs(
            &[
                scip_job(OLD, true, false, 40_000),
                scip_job(HEAD, true, false, 20_000),
            ],
            now(),
        );
        assert_eq!(
            indexed.last_indexed_revision.as_deref(),
            Some(HEAD),
            "the Indexed completion complement closes the exact-head gate"
        );
    }

    #[test]
    fn empty_inventory_is_a_first_run_not_an_error() {
        let observation = observe_from_jobs(&[], now());
        assert_eq!(observation, ScipJobObservation::default());
    }

    /// A recording warm lister: proves the production inventory *asks*, and
    /// with which arguments. A gate that never queries would pass every
    /// decision test above and gate nothing in the cluster.
    struct RecordingWarmLister {
        answer: Result<bool, ()>,
        seen: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl crate::graph_warmer::WarmJobLister for RecordingWarmLister {
        async fn has_in_flight_warm(
            &self,
            namespace: &str,
            project_id: &str,
        ) -> Result<bool, kube::Error> {
            self.seen
                .lock()
                .expect("lock")
                .push((namespace.to_string(), project_id.to_string()));
            self.answer.map_err(|()| {
                kube::Error::Api(kube::error::ErrorResponse {
                    status: "Failure".into(),
                    message: "apiserver unreachable".into(),
                    reason: "InternalError".into(),
                    code: 500,
                })
            })
        }
    }

    #[tokio::test]
    async fn the_production_inventory_consults_the_warm_population() {
        let lister = RecordingWarmLister {
            answer: Ok(true),
            seen: Mutex::new(Vec::new()),
        };
        let observation = observe_including_warm(
            &[scip_job(OLD, true, false, 40_000)],
            &lister,
            "ns",
            "proj",
            now(),
        )
        .await
        .expect("observation");
        assert!(
            observation.warm_in_flight,
            "the warm answer must reach the observation `decide` reads"
        );
        assert_eq!(observation.last_indexed_revision.as_deref(), Some(OLD));
        assert_eq!(
            lister.seen.lock().expect("lock").as_slice(),
            [("ns".to_string(), "proj".to_string())],
            "the warm population must be queried for THIS namespace and project"
        );

        // An unreadable warm population is an unreadable inventory, and an
        // unreadable inventory never dispatches.
        let failing = RecordingWarmLister {
            answer: Err(()),
            seen: Mutex::new(Vec::new()),
        };
        let error = observe_including_warm(&[], &failing, "ns", "proj", now()).await;
        assert!(
            error.is_err(),
            "a warm list failure must not read as absent"
        );
        assert_eq!(
            decide(Some(HEAD), QUIET, None, THREE_HOURS, QUIESCENCE),
            ScipIndexDecision::SkipInventoryUnavailable,
        );
    }

    #[tokio::test]
    async fn observation_reads_durable_warm_outcome() {
        let dispatcher = Arc::new(RecordingDispatcher::default());
        let scheduler = ScipIndexScheduler::new(
            KubernetesConfig::for_testing(),
            Arc::new(RecordingInventory(Ok(ScipJobObservation::default()))),
            dispatcher,
        )
        .with_warm_outcome_source(Arc::new(FixedWarmOutcomeSource(Ok(recovery_outcome()))));
        assert_eq!(
            scheduler.evaluate("proj", Some(&quiescent(HEAD))).await,
            ScipIndexDecision::Dispatch {
                revision: HEAD.to_string()
            }
        );
        let failing = ScipIndexScheduler::new(
            KubernetesConfig::for_testing(),
            Arc::new(RecordingInventory(Ok(ScipJobObservation::default()))),
            Arc::new(RecordingDispatcher::default()),
        )
        .with_warm_outcome_source(Arc::new(FixedWarmOutcomeSource(Err(
            "coverage inconsistent".to_string(),
        ))));
        assert_eq!(
            failing.evaluate("proj", Some(&quiescent(HEAD))).await,
            ScipIndexDecision::SkipInventoryUnavailable
        );
    }

    // ---- scheduler side effect --------------------------------------------

    struct RecordingInventory(Result<ScipJobObservation, String>);

    #[async_trait]
    impl ScipJobInventory for RecordingInventory {
        async fn observe(&self, _ns: &str, _p: &str) -> Result<ScipJobObservation, String> {
            self.0.clone()
        }
    }

    struct FixedWarmOutcomeSource(Result<WarmGraphOutcome, String>);

    #[async_trait]
    impl WarmOutcomeSource for FixedWarmOutcomeSource {
        async fn warm_outcome_for_head(
            &self,
            _project_id: &str,
            _head: &str,
        ) -> Result<WarmGraphOutcome, String> {
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

    /// A head that satisfies the quiescence gate, so these tests exercise the
    /// branch they name rather than tripping over quiescence.
    fn quiescent(revision: &str) -> MirrorHead {
        MirrorHead {
            revision: revision.to_string(),
            age: Duration::from_secs(9_999),
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
        )
        .with_warm_outcome_source(Arc::new(FixedWarmOutcomeSource(Ok(recovery_outcome()))));
        let head = head.map(quiescent);
        let decision = scheduler
            .tick_project("proj", head.as_ref(), "img:tag", None)
            .await;
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
            decide(
                Some(HEAD),
                QUIET,
                Some(&observation),
                THREE_HOURS,
                QUIESCENCE
            ),
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

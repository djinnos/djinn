//! Regression coverage for the production wedge where the v1 build-lease FIFO
//! fills with occupying leases that have no Kubernetes object.
//!
//! Production symptom: three `granted` graph-warm rows against a cap of three,
//! `kubectl get jobs -n djinn` holding no warm Job at all, every later warm
//! answered `K8sGraphWarmer: v1 lease did not authorize Job POST /
//! graph warm lease is queued` roughly nine times per forty minutes, and a warm
//! base that had not re-converged for four days while still seeding every task
//! pod perfectly.
//!
//! Nothing here writes a lease row by hand. Every stale row is produced by the
//! production [`BuildLeaseService`] against a real
//! [`BuildLeaseRepository`] on a fresh database: a lease is queued behind a
//! full cap, its requester gives up on the `Queued` answer exactly as
//! `BuildLeaseGraphWarmAdapter::acquire` does, and the FIFO later grants it to
//! nobody. Producing that shape is as much the behaviour under test as the
//! reclamation is.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    ReclaimAbsentBuildLeaseInput, ReclaimAbsentBuildLeaseOutcome,
};
use djinn_k8s::{
    LeasedWarmJobIdentity, ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind,
    WorkloadRecord,
};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest, LeaseIdentity,
    LeaseQueueRequest, LeaseReleaseRequest, LeaseResult,
};
use tokio::sync::RwLock;

use crate::build_lease::{BuildLeaseService, ManualLeaseClock};
use crate::build_lease_reclaim::{BuildLeaseReclaimer, lease_object_name};

const PROJECT: &str = "019ea3bd-a305-73e3-806c-4edcc96ebfe2";

/// An inventory whose namespace holds exactly the Jobs it was given, answering
/// `presence` from that same set the way a live API server does.
struct NamespaceInventory {
    records: RwLock<Vec<WorkloadRecord>>,
    /// When set, every probe is a transport failure: the API server could not
    /// answer, which is never proof of anything.
    degraded: bool,
}

impl NamespaceInventory {
    fn empty() -> Self {
        Self {
            records: RwLock::new(Vec::new()),
            degraded: false,
        }
    }
    fn holding(names: &[&str]) -> Self {
        Self {
            records: RwLock::new(
                names
                    .iter()
                    .map(|name| WorkloadRecord {
                        kind: WorkloadObjectKind::Job,
                        name: (*name).to_string(),
                        uid: Some(format!("uid-{name}")),
                        labels: Default::default(),
                        terminal: false,
                        images: Vec::new(),
                        commands: Vec::new(),
                    })
                    .collect(),
            ),
            degraded: false,
        }
    }
    /// A namespace that lists cleanly but cannot answer a direct GET.
    fn uncertain() -> Self {
        Self {
            records: RwLock::new(Vec::new()),
            degraded: true,
        }
    }
}

#[async_trait]
impl WorkloadInventory for NamespaceInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(self.records.read().await.clone())
    }
    async fn get_uid(&self, _kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult {
        if self.degraded {
            return UidGetResult::Uncertain;
        }
        match self
            .records
            .read()
            .await
            .iter()
            .find(|record| record.name == name)
        {
            Some(record) if record.uid.as_deref() == Some(uid) => UidGetResult::Present,
            Some(_) => UidGetResult::Uncertain,
            None => UidGetResult::NotFound,
        }
    }
    async fn presence(&self, _kind: WorkloadObjectKind, name: &str) -> ObjectPresence {
        if self.degraded {
            return ObjectPresence::Uncertain;
        }
        match self
            .records
            .read()
            .await
            .iter()
            .find(|record| record.name == name)
        {
            Some(record) => ObjectPresence::Present {
                uid: record.uid.clone(),
            },
            None => ObjectPresence::Absent,
        }
    }
}

fn warm(request_id: &str) -> LeaseIdentity {
    LeaseIdentity::GraphWarm(GraphWarmLeaseIdentity {
        project_id: PROJECT.into(),
        warm_request_id: request_id.into(),
        graph_revision: format!("rev-{request_id}"),
    })
}

fn queue_request(request_id: &str) -> LeaseQueueRequest {
    LeaseQueueRequest {
        identity: warm(request_id),
        // Production carries a two-hour deadline on both edges, and the clock
        // starts at 100ms, so every lease in these tests is comfortably INSIDE
        // its launch deadline. That is deliberate: reclamation is driven by a
        // proven-absent Kubernetes object, never by a deadline, so an
        // unexpired lease must still be reclaimable.
        deadlines: LeaseDeadlines {
            queue_deadline_ms: 100 + 7_200_000,
            launch_deadline_ms: 100 + 7_200_000,
        },
    }
}

fn undeadlined_request(request_id: &str) -> LeaseQueueRequest {
    LeaseQueueRequest {
        identity: warm(request_id),
        // A non-positive value means "no deadline": the durable column stays
        // NULL and the lease never expires on that edge.
        deadlines: LeaseDeadlines {
            queue_deadline_ms: 0,
            launch_deadline_ms: 0,
        },
    }
}

fn key(request_id: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::GraphWarm,
        consumer_id: request_id.into(),
    }
}

/// The Kubernetes name the production warm dispatch path commits to for a
/// request id, derived through the production identity type.
fn warm_job_name(request_id: &str) -> String {
    LeasedWarmJobIdentity::new(PROJECT, request_id, "rev", 1).object_name
}

async fn service(cap: i64) -> (Arc<BuildLeaseService>, Arc<BuildLeaseRepository>) {
    let repository = Arc::new(BuildLeaseRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let service = Arc::new(BuildLeaseService::with_seams(
        Arc::clone(&repository),
        cap,
        Arc::new(ManualLeaseClock::new(100)),
        Arc::new(crate::build_lease::NoopLeaseTransactionPause),
        Arc::new(crate::build_lease::NoopLeaseTelemetry),
    ));
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert!(matches!(service.set_cap(cap).await, LeaseResult::Status(_)));
    (service, repository)
}

/// Reproduce the production wedge: a `granted` lease nobody holds.
///
/// `holder` takes the only slot and acknowledges it. `abandoned` is queued
/// behind the full cap and its requester gives up on the `Queued` answer, which
/// is exactly what `BuildLeaseGraphWarmAdapter::acquire` returns to the warmer.
/// Releasing `holder` lets the FIFO grant `abandoned` to a requester that is
/// already gone, and nothing in the ledger can ever retire it.
async fn wedge(service: &Arc<BuildLeaseService>) {
    let granted = service.queue(queue_request("holder")).await;
    let LeaseResult::Granted(grant) = granted else {
        panic!("the first queue against a free cap must grant: {granted:?}");
    };
    assert!(matches!(
        service
            .grant(LeaseGrantRequest {
                identity: warm("holder"),
                fencing_token: grant.fencing_token.clone(),
            })
            .await,
        LeaseResult::Status(_)
    ));
    assert!(
        matches!(
            service.queue(queue_request("abandoned")).await,
            LeaseResult::Queued(_)
        ),
        "the second warm must be queued behind the full cap"
    );
    assert!(matches!(
        service
            .release(LeaseReleaseRequest {
                identity: warm("holder"),
                fencing_token: grant.fencing_token,
                candidate_cleanup: false,
            })
            .await,
        LeaseResult::Released { .. }
    ));
}

fn reclaimer(
    repository: &Arc<BuildLeaseRepository>,
    inventory: NamespaceInventory,
) -> BuildLeaseReclaimer {
    // Warm and invocation reclamation prove absence from Kubernetes and nothing
    // else: the only authority the reclaimer consults is the namespace.
    BuildLeaseReclaimer::with_settle_window(
        Arc::clone(repository),
        Arc::new(inventory),
        Duration::ZERO,
    )
}

/// The wedge end to end: an abandoned grant holds the whole cap, every later
/// warm is answered `Queued`, and reclamation against an authoritative empty
/// namespace frees the slot so warming runs again.
#[tokio::test]
async fn abandoned_grant_wedges_the_cap_until_its_absent_object_is_proven() {
    let (service, repository) = service(1).await;
    wedge(&service).await;

    // The live warm request. Its own queue drains the FIFO, which grants the
    // abandoned lease rather than this one: the production symptom.
    assert!(
        matches!(
            service.queue(queue_request("live")).await,
            LeaseResult::Queued(_)
        ),
        "this is the wedge: the live warm cannot be granted"
    );
    let abandoned = repository.get(&key("abandoned")).await.unwrap().unwrap();
    assert_eq!(
        abandoned.state,
        BuildLeaseState::Granted,
        "the abandoned lease occupies the cap in `granted`, which no existing sweep can see"
    );
    assert_eq!(
        lease_object_name(&abandoned).as_deref(),
        Some(warm_job_name("abandoned").as_str()),
        "the absence probe must ask about the exact object the warm dispatch path would create"
    );

    let report = reclaimer(&repository, NamespaceInventory::empty())
        .reclaim()
        .await;
    assert!(
        report.blockers.is_empty(),
        "blockers: {:?}",
        report.blockers
    );
    assert!(
        report.failures.is_empty(),
        "failures: {:?}",
        report.failures
    );
    // Two nonterminal rows: the phantom `granted` occupant and the `live` warm
    // still queued behind it. The sweep examines both and retires only the one
    // it has a proof about — a queued warm has no object to prove anything
    // with.
    assert_eq!(report.examined, 2);
    assert_eq!(report.absent, 1);
    assert_eq!(report.reclaimed, 1);
    assert_eq!(report.fenced, 0);

    assert_eq!(
        repository
            .get(&key("abandoned"))
            .await
            .unwrap()
            .unwrap()
            .terminal_reason
            .as_deref(),
        Some("reclaimed_absent")
    );
    assert!(
        matches!(
            service.queue(queue_request("live")).await,
            LeaseResult::Granted(_)
        ),
        "the live warm must be granted once the phantom occupant is retired"
    );
}

/// Reclamation reads no deadline at all, so nothing it does can depend on how
/// a deadline round-trips.
///
/// This matters because the deadline round-trip was itself broken until #2605:
/// the shared column list rendered PostgreSQL's own timestamp format while
/// `build_lease::ms` parsed RFC3339 and mapped failure to `0` — which already
/// means "no deadline" — so every echoed deadline read as unbounded. A
/// reclaimer keyed on `launch_deadline` would have behaved one way against that
/// defect and another way after it was fixed. This one is keyed on the settle
/// window (a SQL predicate on `updated_at`) and a proven-absent object, so both
/// a lease with no durable deadline at all and a lease well inside a real
/// two-hour deadline are reclaimed identically.
#[tokio::test]
async fn reclamation_reads_no_deadline_and_retires_bounded_and_unbounded_leases_alike() {
    let (service, repository) = service(2).await;

    // One lease with a real, unexpired two-hour launch deadline; one with no
    // durable deadline at all. Both are granted, both are abandoned.
    for request in [queue_request("bounded"), undeadlined_request("unbounded")] {
        assert!(matches!(
            service.queue(request).await,
            LeaseResult::Granted(_)
        ));
    }
    let bounded = repository.get(&key("bounded")).await.unwrap().unwrap();
    let unbounded = repository.get(&key("unbounded")).await.unwrap().unwrap();
    assert!(
        bounded.launch_deadline.is_some(),
        "the bounded lease must carry a durable deadline: {bounded:?}"
    );
    assert!(
        unbounded.launch_deadline.is_none(),
        "the unbounded lease must carry no durable deadline: {unbounded:?}"
    );
    assert_eq!(bounded.state, BuildLeaseState::Granted);
    assert_eq!(unbounded.state, BuildLeaseState::Granted);

    let report = reclaimer(&repository, NamespaceInventory::empty())
        .reclaim()
        .await;
    assert!(
        report.blockers.is_empty(),
        "blockers: {:?}",
        report.blockers
    );
    assert_eq!(report.examined, 2);
    assert_eq!(
        report.reclaimed, 2,
        "an unexpired deadline is not a reason to keep a phantom lease, and an \
         absent deadline is not a reason to keep one either"
    );
    for id in ["bounded", "unbounded"] {
        assert_eq!(
            repository.get(&key(id)).await.unwrap().unwrap().state,
            BuildLeaseState::Terminal,
            "{id} must be retired on the absence proof alone"
        );
    }
}

/// The negative that matters most: a lease whose Job is still in the namespace
/// is live work, and no amount of age makes it reclaimable.
#[tokio::test]
async fn a_lease_whose_object_still_exists_is_never_retired() {
    let (service, repository) = service(1).await;
    wedge(&service).await;

    let report = reclaimer(
        &repository,
        NamespaceInventory::holding(&[&warm_job_name("abandoned")]),
    )
    .reclaim()
    .await;
    assert_eq!(report.examined, 1);
    assert_eq!(report.absent, 0, "the object was listed; nothing is absent");
    assert_eq!(report.reclaimed, 0);
    assert_eq!(
        repository
            .get(&key("abandoned"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Granted
    );
}

/// A namespace that lists cleanly but cannot answer a direct GET proves
/// nothing. A degraded API server must leave every lease occupying.
#[tokio::test]
async fn an_uncertain_probe_is_never_proof_of_absence() {
    let (service, repository) = service(1).await;
    wedge(&service).await;

    let report = reclaimer(&repository, NamespaceInventory::uncertain())
        .reclaim()
        .await;
    assert_eq!(report.examined, 1);
    assert_eq!(report.absent, 0, "`Uncertain` is never proof");
    assert_eq!(report.reclaimed, 0);
    assert_eq!(
        repository
            .get(&key("abandoned"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Granted
    );
}

/// An unusable listing is a pass-level blocker, and it must stop the pass
/// before any lease is judged.
#[tokio::test]
async fn an_unusable_listing_blocks_the_pass_and_retires_nothing() {
    struct Unlistable;
    #[async_trait]
    impl WorkloadInventory for Unlistable {
        async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
            Err("connection refused".into())
        }
        async fn get_uid(&self, _: WorkloadObjectKind, _: &str, _: &str) -> UidGetResult {
            panic!("no lease may be judged without an authoritative listing")
        }
        async fn presence(&self, _: WorkloadObjectKind, _: &str) -> ObjectPresence {
            panic!("no lease may be judged without an authoritative listing")
        }
    }

    let (service, repository) = service(1).await;
    wedge(&service).await;
    let report = BuildLeaseReclaimer::with_settle_window(
        Arc::clone(&repository),
        Arc::new(Unlistable),
        Duration::ZERO,
    )
    .reclaim()
    .await;
    assert_eq!(report.blockers.len(), 1);
    assert_eq!(report.reclaimed, 0);
    assert_eq!(
        repository
            .get(&key("abandoned"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Granted
    );
}

/// A lease that has not settled is not yet judged at all: a create the API
/// server has not made visible must be allowed to appear first.
#[tokio::test]
async fn an_unsettled_lease_is_not_judged_by_a_listing_it_could_predate() {
    let (service, repository) = service(1).await;
    wedge(&service).await;

    let report = BuildLeaseReclaimer::with_settle_window(
        Arc::clone(&repository),
        Arc::new(NamespaceInventory::empty()),
        Duration::from_secs(3600),
    )
    .reclaim()
    .await;
    assert_eq!(report.examined, 1);
    assert_eq!(report.absent, 0);
    assert_eq!(report.reclaimed, 0);
}

/// The durable fence itself: evidence gathered before the holder acted must not
/// retire the lease the holder just moved.
#[tokio::test]
async fn evidence_that_predates_a_holder_acknowledgement_is_fenced() {
    let (service, repository) = service(1).await;
    wedge(&service).await;
    let observed = repository.get(&key("abandoned")).await.unwrap().unwrap();

    // The holder wakes up and acknowledges its grant after the proof was taken.
    assert!(matches!(
        service
            .grant(LeaseGrantRequest {
                identity: warm("abandoned"),
                fencing_token: LeaseFencingToken(observed.fencing_token.unwrap() as u64),
            })
            .await,
        LeaseResult::Status(_)
    ));

    let outcome = repository
        .reclaim_absent_object(&ReclaimAbsentBuildLeaseInput {
            key: observed.key.clone(),
            observed_state: observed.state,
            observed_immutable_identity: observed.immutable_identity.clone(),
            observed_fencing_token: observed.fencing_token,
            observed_bound_pod_uid: observed.bound_pod_uid.clone(),
            observed_updated_at: observed.updated_at.clone(),
            terminal_reason: djinn_db::BuildLeaseTerminalReason::ReclaimedAbsent,
        })
        .await
        .unwrap();
    assert!(
        matches!(outcome, ReclaimAbsentBuildLeaseOutcome::Fenced { .. }),
        "a lease that moved after its absence proof must not be retired: {outcome:?}"
    );
    assert_eq!(
        repository
            .get(&key("abandoned"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Launching
    );
}

/// Task-invocation leases share the same cap and the same reclaimer; their
/// object name is derived from the durable identity, never guessed.
#[test]
fn lease_object_names_come_from_the_durable_identity_or_nowhere() {
    let row = |kind, identity: &str| djinn_db::BuildLeaseRow {
        key: BuildLeaseKey {
            consumer_kind: kind,
            consumer_id: "consumer".into(),
        },
        immutable_identity: identity.into(),
        enqueue_sequence: 1,
        fencing_token: Some(1),
        state: BuildLeaseState::Granted,
        queue_deadline: None,
        launch_deadline: None,
        bound_pod_uid: None,
        candidate_cleanup: None,
        terminal_reason: None,
        // One full build slot: the weight a granted lease is charged, which is
        // what makes this row occupying rather than a free re-entry.
        weight: 1,
        timeout_credit_consumed: false,
        created_at: "now".into(),
        updated_at: "now".into(),
        granted_at: None,
        terminal_at: None,
    };
    assert_eq!(
        lease_object_name(&row(
            BuildLeaseConsumerKind::TaskInvocation,
            "task:task-1:run-9:invocation-3"
        ))
        .as_deref(),
        Some(djinn_k8s::taskrun_job_name("run-9").as_str())
    );
    assert_eq!(
        lease_object_name(&row(BuildLeaseConsumerKind::GraphWarm, "warm:proj:req:rev")).as_deref(),
        Some(
            LeasedWarmJobIdentity::new("proj", "req", "rev", 1)
                .object_name
                .as_str()
        )
    );
    // An identity this reclaimer does not recognise is never reclaimed: a
    // guessed name turns an absence proof into a vacuous one.
    assert_eq!(
        lease_object_name(&row(BuildLeaseConsumerKind::GraphWarm, "something-else")),
        None
    );
    assert_eq!(
        lease_object_name(&row(
            BuildLeaseConsumerKind::TaskInvocation,
            "task::run:inv"
        ))
        .as_deref(),
        Some(djinn_k8s::taskrun_job_name("run").as_str())
    );
    assert_eq!(
        lease_object_name(&row(BuildLeaseConsumerKind::TaskInvocation, "task:t::inv")),
        None
    );
}

/// The load-bearing premise of `OwnerlessProof::DispatchAuthorityDeleted`.
///
/// A settled occupying `task_dispatch` lease is retired unconditionally, and
/// that is only safe because NOTHING can acquire one any more: the pre-create
/// dispatch reservation was stood down by the Kueue cutover (o53p), and
/// `LeaseIdentity::TaskDispatch` has no constructor left in the workspace.
///
/// If someone reintroduces one, this fails BEFORE the reclaimer starts eating
/// live dispatch leases. That ordering is the whole point: the failure mode
/// being guarded is silent — a reclaimed live lease looks exactly like a
/// reclaimed legacy one until the board stops dispatching.
///
/// The scan is over source text rather than types because "no value of this
/// variant is ever constructed" is not expressible in Rust's type system while
/// the variant must stay constructible for the durable rows that already exist.
/// `identity()` in `build_lease.rs` MATCHES on the variant to map a legacy row,
/// which is a read, not an acquisition — hence the pattern-vs-construction
/// distinction below.
#[test]
fn no_dispatch_lease_can_ever_be_acquired_again() {
    // Assembled at runtime so this test does not match its own literals.
    let ctor = format!("LeaseIdentity::TaskDispatch{}", "(");
    let struct_ctor = format!("TaskDispatchLeaseIdentity {}", "{");

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if path.is_dir() {
                if name == "target" || name == ".git" || name == "node_modules" {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("server dir")
        .to_path_buf();
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(
        files.len() > 100,
        "the source scan must actually reach the workspace; found only {} files under {}",
        files.len(),
        root.display()
    );

    let mut offenders: Vec<String> = Vec::new();
    for path in files {
        // This test's own source names both constructors in its assertions.
        if path.ends_with("build_lease_reclaim_tests.rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in source.lines() {
            let trimmed = line.trim_start();
            // `identity()` and friends PATTERN-MATCH the variant to read a
            // legacy row. That is not an acquisition.
            if trimmed.starts_with("//") || trimmed.contains("=>") {
                continue;
            }
            // The type and variant must stay DECLARED — legacy rows still map
            // through them. Only a construction site is an acquisition.
            if trimmed.starts_with("pub struct")
                || trimmed.starts_with("struct")
                || trimmed.starts_with("pub enum")
                || trimmed.starts_with("enum")
            {
                continue;
            }
            if line.contains(&ctor) || line.contains(&struct_ctor) {
                offenders.push(format!("{}: {}", path.display(), line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a `task_dispatch` build lease is being acquired again, but \
         `BuildLeaseReclaimer` retires every settled occupying one on the \
         grounds that no acquirer exists. Replace `DispatchAuthorityDeleted` \
         with a real ownership proof BEFORE landing this:\n{}",
        offenders.join("\n")
    );
}

// =============================================================================
// Presence is not liveness (2026-07-30, v0.7.31)
// =============================================================================
//
// Production, dispatch paused, every task-run Job at `Complete` with zero Pods:
// three `task_invocation` rows stuck in `launching` and one in `queued`. They
// outlived ~7 minutes of polling, a full `djinn-server` rollout restart, and
// every owning Job reaching a terminal condition, and were ended by a
// hand-written SQL `UPDATE` against production. Twice that day.
//
// Nothing below writes a lease row by hand. Every stranded row is produced by
// `BuildLeaseService` against a real `BuildLeaseRepository`, exactly as a worker
// that dies mid-invocation leaves one.

use djinn_supervisor::services::TaskInvocationLeaseIdentity;

const TASK: &str = "019fba8a-a25e-77c3-a38f-b74943e79893";
const RUN: &str = "019fba9a-5992-7083-9beb-641f878200e1";

fn invocation(invocation_id: &str) -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: TASK.into(),
        task_run_id: RUN.into(),
        invocation_id: invocation_id.into(),
    })
}

fn invocation_request(invocation_id: &str) -> LeaseQueueRequest {
    LeaseQueueRequest {
        identity: invocation(invocation_id),
        // Deliberately far in the future on BOTH edges. Every assertion below
        // must hold for a lease that is nowhere near any deadline, because the
        // fix must be an observable-state proof and not a timeout in disguise.
        deadlines: LeaseDeadlines {
            queue_deadline_ms: 100 + 7_200_000,
            launch_deadline_ms: 100 + 7_200_000,
        },
    }
}

/// The durable key `BuildLeaseService` composes for an invocation identity: the
/// invocation id alone is the consumer id, while the task and run live in the
/// immutable identity that `lease_object_name` reads.
fn invocation_key(invocation_id: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: invocation_id.to_owned(),
    }
}

/// The name of the task-run Job that owns every invocation lease here.
fn owning_job_name() -> String {
    djinn_k8s::taskrun_job_name(RUN)
}

impl NamespaceInventory {
    /// A namespace holding Jobs that have reached a terminal condition — the
    /// shape a completed task-run leaves behind for its whole
    /// `ttlSecondsAfterFinished`, which is 3600s.
    fn holding_finished(names: &[&str]) -> Self {
        let mut inventory = Self::holding(names);
        for record in inventory.records.get_mut() {
            record.terminal = true;
        }
        inventory
    }
}

/// Drive a `task_invocation` lease to `launching` — granted, acknowledged, and
/// then abandoned because the worker holding it died.
async fn launching_invocation(service: &Arc<BuildLeaseService>, invocation_id: &str) {
    let queued = service.queue(invocation_request(invocation_id)).await;
    let LeaseResult::Granted(grant) = queued else {
        panic!("a queue against a free cap must grant: {queued:?}");
    };
    // `grant` is the worker acknowledging the slot: `granted` → `launching`.
    // This is the last thing the dead worker ever did.
    assert!(matches!(
        service
            .grant(LeaseGrantRequest {
                identity: invocation(invocation_id),
                fencing_token: grant.fencing_token,
            })
            .await,
        LeaseResult::Status(_)
    ));
}

/// **The production incident, verbatim.**
///
/// A `task_invocation` lease sits in `launching`. Its owning task-run Job is
/// still LISTED — it completed, and Kubernetes keeps a finished Job for its
/// `ttlSecondsAfterFinished` — but it has reached a terminal condition and has
/// no Pods. The worker that would have sent `release_lease` is gone.
///
/// The mutation this fails on: collapse the LIST back to a set of names
/// (`records.into_iter().map(|record| record.name)`) and the terminal flag is
/// gone, the object reads as live, and the lease is never retired. That one
/// `.map` is the whole defect.
#[tokio::test]
async fn a_lease_whose_job_completed_is_retired_even_though_the_job_still_exists() {
    let (service, repository) = service(1).await;
    launching_invocation(&service, "inv-1").await;
    assert_eq!(
        repository
            .get(&invocation_key("inv-1"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Launching,
        "the lease must be stranded in `launching`, the production state"
    );

    // The namespace still holds the Job. It is Complete.
    let job = owning_job_name();
    let report = reclaimer(&repository, NamespaceInventory::holding_finished(&[&job]))
        .reclaim()
        .await;

    assert!(
        report.blockers.is_empty(),
        "blockers: {:?}",
        report.blockers
    );
    assert!(
        report.failures.is_empty(),
        "failures: {:?}",
        report.failures
    );
    assert_eq!(
        report.finished_object, 1,
        "the proof must be `the object finished`, not `the object is absent` — \
         the Job is right there in the listing"
    );
    assert_eq!(report.reclaimed, 1);

    let row = repository
        .get(&invocation_key("inv-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, BuildLeaseState::Terminal);
    assert_eq!(
        row.terminal_reason.as_deref(),
        Some("reclaimed_finished"),
        "an operator triaging the ledger must be able to tell `its Job was \
         collected` from `its Job finished and nobody released`"
    );

    // And the capacity is genuinely back: a fresh invocation is granted.
    assert!(
        matches!(
            service.queue(invocation_request("inv-2")).await,
            LeaseResult::Granted(_)
        ),
        "retiring the stranded lease must actually free the slot it held"
    );
}

/// The other direction, and the one that matters more: a Job that is listed and
/// has NOT reached a terminal condition is live work, and no proof retires it.
///
/// Releasing capacity out from under a running compile is strictly worse than
/// leaking it. The mutation this fails on: treat every listed object as
/// finished, or derive `terminal` from the `succeeded`/`failed` Pod counters.
#[tokio::test]
async fn a_lease_whose_job_is_still_running_is_never_retired() {
    let (service, repository) = service(1).await;
    launching_invocation(&service, "inv-1").await;

    let job = owning_job_name();
    // `holding` builds records with `terminal: false` — a live Job.
    let report = reclaimer(&repository, NamespaceInventory::holding(&[&job]))
        .reclaim()
        .await;

    assert_eq!(report.examined, 1, "the sweep must have seen the lease");
    assert_eq!(report.absent, 0);
    assert_eq!(report.finished_object, 0);
    assert_eq!(report.reclaimed, 0);
    assert_eq!(
        repository
            .get(&invocation_key("inv-1"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Launching,
        "a live holder must keep its lease"
    );
}

/// A degraded API server proves nothing, and that must stay true on the new
/// path too: an `Uncertain` probe against a Job that is not in the listing
/// leaves the lease exactly where it is.
#[tokio::test]
async fn an_unanswerable_probe_never_retires_an_invocation_lease() {
    let (service, repository) = service(1).await;
    launching_invocation(&service, "inv-1").await;

    let report = reclaimer(&repository, NamespaceInventory::uncertain())
        .reclaim()
        .await;

    assert_eq!(report.reclaimed, 0);
    assert_eq!(
        repository
            .get(&invocation_key("inv-1"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Launching
    );
}

/// **The fourth stranded row.**
///
/// A `queued` lease holds no capacity, so it is not in `OCCUPYING`, so the
/// sweep's own listing used to filter it out — and the only other thing that
/// retires a queued row is `expire_queued_tx` inside `grant_next`, which never
/// runs while dispatch is paused. `preflight.sh` fences the cutover on
/// `state <> 'terminal'`, so this row blocked the cutover with no code path
/// anywhere able to clear it.
///
/// The mutation this fails on: narrow the sweep back to occupying states only.
#[tokio::test]
async fn a_queued_invocation_lease_whose_task_run_finished_is_retired() {
    let (service, repository) = service(1).await;
    // Fill the cap so the second invocation is QUEUED rather than granted.
    launching_invocation(&service, "inv-holder").await;
    assert!(
        matches!(
            service.queue(invocation_request("inv-queued")).await,
            LeaseResult::Queued(_)
        ),
        "the second invocation must queue behind the full cap"
    );
    assert_eq!(
        repository
            .get(&invocation_key("inv-queued"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Queued
    );

    // Both leases belong to the same task-run, whose Job has completed.
    let job = owning_job_name();
    let report = reclaimer(&repository, NamespaceInventory::holding_finished(&[&job]))
        .reclaim()
        .await;

    assert_eq!(
        report.examined, 2,
        "the sweep must examine the `queued` row, not just the occupying one"
    );
    assert_eq!(report.reclaimed, 2);
    assert_eq!(
        repository
            .get(&invocation_key("inv-queued"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Terminal,
        "a queued row whose task-run provably finished must reach terminal \
         without a human writing SQL"
    );
}

/// The guard that keeps widening the sweep to `queued` from making the absence
/// proof vacuous.
///
/// A `graph_warm` lease is what AUTHORIZES the warm Job POST, so a queued warm
/// lease has no Kubernetes object and never did. Probing for it finds nothing —
/// and a warm request can legitimately sit untouched behind a full cap for
/// hours, so no settle window rescues it either. Retiring it would silently
/// cancel a queued warm.
///
/// The mutation this fails on: delete `object_predates_lease`, or make it
/// return `true` unconditionally.
#[tokio::test]
async fn a_queued_warm_lease_is_never_retired_on_an_object_proof() {
    let (service, repository) = service(1).await;
    // `holder` occupies the cap; `waiting` queues behind it and stays queued.
    let granted = service.queue(queue_request("holder")).await;
    let LeaseResult::Granted(grant) = granted else {
        panic!("the first queue against a free cap must grant: {granted:?}");
    };
    assert!(matches!(
        service
            .grant(LeaseGrantRequest {
                identity: warm("holder"),
                fencing_token: grant.fencing_token,
            })
            .await,
        LeaseResult::Status(_)
    ));
    assert!(matches!(
        service.queue(queue_request("waiting")).await,
        LeaseResult::Queued(_)
    ));

    // An EMPTY namespace: the strongest possible absence evidence. The queued
    // warm still must not be touched, because its object was never going to
    // exist yet.
    let report = reclaimer(&repository, NamespaceInventory::empty())
        .reclaim()
        .await;

    assert_eq!(
        repository
            .get(&key("waiting"))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Queued,
        "a queued warm lease has no object yet; absence is not evidence about it"
    );
    assert!(
        report.examined >= 2,
        "the queued warm must still be EXAMINED — it is skipped by proof, not by \
         being invisible: {report:?}"
    );
}

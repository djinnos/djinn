//! Proof that build capacity has exactly ONE authority.
//!
//! Every test here drives the real `BuildAdmissionController`, the real
//! `BuildLeaseService`, and the real journal/lease repositories against a fresh
//! Postgres database. Nothing is faked: the whole class of defect these guard
//! against is "the fixture supplied what production does not", and a fake
//! capacity authority would reproduce it exactly.
//!
//! # What was wrong
//!
//! `DJINN_MAX_BUILD_TASKRUNS` was read by two independent authorities which
//! each enforced it over a DISJOINT population -- the v0 journal over task-runs,
//! the v1 lease over graph warming -- so at enforce the node ran 2x the
//! configured concurrency. Each test below states which half of that it pins.

use std::sync::Arc;

use djinn_db::{
    AdmissionDomain, AdmissionJournalRepository, BuildLeaseConsumerKind, BuildLeaseRepository,
    BuildLeaseState, Database,
};
use djinn_k8s::{WarmAdmission, WarmAdmissionRequest, WarmAdmissionTransition};
use djinn_runtime::{BuildSlotWeight, RoleResourceClass};
use djinn_supervisor::services::{
    LeaseDeadlines, LeaseIdentity, LeaseQueueRequest, LeaseResult, TaskInvocationLeaseIdentity,
};

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, BuildSlotAuthority,
};
use crate::build_lease::{BuildLeaseService, BuildSlotWeights};
use crate::build_slot_authority::BuildLeaseDispatchAuthority;

const EPOCH: &str = "unified-capacity-epoch";

/// The production composition, assembled exactly as `AppState` assembles it:
/// ONE lease service, handed to the controller as its capacity authority.
struct Harness {
    controller: Arc<BuildAdmissionController>,
    lease: Arc<BuildLeaseService>,
    journal: Arc<AdmissionJournalRepository>,
    leases: Arc<BuildLeaseRepository>,
}

async fn harness(cap: i64) -> Harness {
    let db = Database::open_in_memory().unwrap();
    let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));

    let lease = Arc::new(
        BuildLeaseService::new(Arc::clone(&leases), cap)
            .with_slot_weights(BuildSlotWeights::default()),
    );
    assert!(matches!(lease.recover().await, LeaseResult::Status(_)));
    assert!(matches!(lease.set_cap(cap).await, LeaseResult::Status(_)));
    // Arm layer-1 dispatch admission. In production this is the durable
    // admission epoch reaching invocation-primary; nothing else can arm it.
    lease.set_dispatch_enforcing_for_test(true);

    let authority: Arc<dyn BuildSlotAuthority> =
        Arc::new(BuildLeaseDispatchAuthority::new(Arc::clone(&lease)));
    let controller = Arc::new(
        BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Enforce,
            cap,
            EPOCH,
        )
        .with_slot_authority(authority),
    );
    controller.mark_ready();

    Harness {
        controller,
        lease,
        journal,
        leases,
    }
}

/// Take a graph-warm lease the way the warmer does: queue, then acknowledge.
async fn acquire_warm_lease(lease: &BuildLeaseService, request: &str) -> bool {
    let identity = LeaseIdentity::GraphWarm(djinn_supervisor::services::GraphWarmLeaseIdentity {
        project_id: "unified-project".into(),
        warm_request_id: request.into(),
        graph_revision: "unified-revision".into(),
    });
    matches!(
        lease
            .queue(LeaseQueueRequest {
                identity,
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await,
        LeaseResult::Granted(_)
    )
}

async fn occupancy(leases: &BuildLeaseRepository) -> i64 {
    leases.snapshot().await.unwrap().occupied
}

/// THE defect: N admissions spread across BOTH domains at cap N must yield
/// exactly N grants, not 2N.
///
/// Against the previous design this admits 2N: the warm Jobs occupy the v1
/// lease's N slots while the task-runs independently occupy the v0 journal's N,
/// because neither authority counted the other's population.
#[tokio::test]
async fn both_domains_share_one_cap_and_admit_n_not_two_n() {
    const CAP: i64 = 3;
    let h = harness(CAP).await;

    // Saturate with graph warming alone.
    let mut warm_granted = 0;
    for index in 0..CAP {
        if acquire_warm_lease(&h.lease, &format!("warm-{index}")).await {
            warm_granted += 1;
        }
    }
    assert_eq!(warm_granted, CAP, "warm must be able to fill the pool");
    assert_eq!(occupancy(&h.leases).await, CAP);

    // The pool is now full. Task dispatch draws from the SAME pool, so every
    // build-capable task-run must be denied -- previously they had their own N.
    for index in 0..CAP {
        let decision = h
            .controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                format!("task-{index}"),
                0,
                format!("djinn-taskrun-task-{index}"),
            )
            .await
            .unwrap();
        assert!(
            matches!(decision, BuildAdmissionDecision::Denied { .. }),
            "task-run {index} was admitted past a full shared pool: {decision:?} \
             -- the two authorities are counting separate populations again"
        );
    }

    assert_eq!(
        occupancy(&h.leases).await,
        CAP,
        "total occupancy across BOTH domains must never exceed the one cap"
    );
}

/// The complement: with the pool empty, task dispatch really does occupy it,
/// and warm is then the one that is refused. Without this, the test above could
/// pass by task dispatch simply never acquiring anything.
#[tokio::test]
async fn task_dispatch_occupies_the_shared_pool_and_blocks_warm() {
    const CAP: i64 = 2;
    let h = harness(CAP).await;

    for index in 0..CAP {
        let decision = h
            .controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                format!("filler-{index}"),
                0,
                format!("djinn-taskrun-filler-{index}"),
            )
            .await
            .unwrap();
        assert!(matches!(decision, BuildAdmissionDecision::Permitted { .. }));
    }
    assert_eq!(
        occupancy(&h.leases).await,
        CAP,
        "build-capable dispatch must occupy real, shared capacity"
    );

    assert!(
        !acquire_warm_lease(&h.lease, "warm-after-tasks").await,
        "graph warming must contend with task dispatch, not run beside it"
    );
}

/// A leased warm Job must still leave a journal row.
///
/// The warmer used to match `(Some(lease_grant), _) => None`, so once the lease
/// was armed a warm Job wrote NOTHING to the lifecycle ledger -- which is what
/// records its Kubernetes UID and what `reclaim_absent_object` (#2597) reads to
/// retire occupancy whose object is provably gone. This drives the real
/// `WarmAdmission` trait implementation the warmer calls.
#[tokio::test]
async fn a_leased_warm_job_still_writes_its_lifecycle_ledger_row() {
    let h = harness(3).await;
    assert!(acquire_warm_lease(&h.lease, "ledger-warm").await);

    let permit = WarmAdmission::admit(
        h.controller.as_ref(),
        WarmAdmissionRequest {
            domain: "warm_build".into(),
            work_id: "ledger-warm".into(),
            generation: 0,
            object_name: "djinn-warm-ledger".into(),
        },
    )
    .await
    .expect("a lease-granted warm Job must still be admitted to the ledger");

    WarmAdmission::transition(
        h.controller.as_ref(),
        &permit,
        WarmAdmissionTransition::CreateStarted,
    )
    .await
    .unwrap();
    WarmAdmission::transition(
        h.controller.as_ref(),
        &permit,
        WarmAdmissionTransition::Live {
            uid: "warm-uid-7f91".into(),
        },
    )
    .await
    .unwrap();

    let history = h
        .journal
        .list_history(AdmissionDomain::WarmBuild, "ledger-warm")
        .await
        .unwrap();
    assert_eq!(
        history.len(),
        1,
        "a leased warm Job wrote no ledger row -- it is invisible to reclamation"
    );
    assert_eq!(
        history[0].object_uid.as_deref(),
        Some("warm-uid-7f91"),
        "the ledger must carry the UID reclamation fences on"
    );
}

/// The ledger row must not ALSO buy capacity. Warm holds one lease; admitting it
/// to the journal must not turn that into two units.
#[tokio::test]
async fn journaling_a_leased_warm_job_does_not_charge_capacity_twice() {
    let h = harness(3).await;
    assert!(acquire_warm_lease(&h.lease, "single-charge").await);
    let before = occupancy(&h.leases).await;
    assert_eq!(before, 1);

    WarmAdmission::admit(
        h.controller.as_ref(),
        WarmAdmissionRequest {
            domain: "warm_build".into(),
            work_id: "single-charge".into(),
            generation: 0,
            object_name: "djinn-warm-single".into(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        occupancy(&h.leases).await,
        before,
        "the ledger append must be capacity-neutral; warm already paid once"
    );
}

/// The non-double-charge rule, end to end.
///
/// A build-capable task-run pre-charges a dispatch slot for exactly the compile
/// it is expected to run. When that compile escalates to an invocation lease,
/// it must NOT buy a second unit -- one physical compile, charged once. This is
/// the trap a naive "warm and task both call one admit" would have fallen into.
#[tokio::test]
async fn an_escalating_invocation_reuses_its_task_runs_dispatch_slot() {
    let h = harness(2).await;

    let decision = h
        .controller
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            "reentrant".into(),
            0,
            "djinn-taskrun-reentrant".into(),
        )
        .await
        .unwrap();
    assert!(matches!(decision, BuildAdmissionDecision::Permitted { .. }));
    assert_eq!(occupancy(&h.leases).await, 1);

    // The pod's build now crosses the measured CPU threshold and escalates.
    let result = h
        .lease
        .queue(LeaseQueueRequest {
            identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                task_id: "reentrant".into(),
                task_run_id: "run-1".into(),
                invocation_id: "invocation-1".into(),
            }),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await;
    assert!(
        matches!(result, LeaseResult::Granted(_)),
        "an escalation whose slot is already held must never queue: {result:?}"
    );

    assert_eq!(
        occupancy(&h.leases).await,
        1,
        "one physical compile was charged twice -- the 2x defect, one layer down"
    );

    // And it is a real, fenced lease, not a bypass: it holds a token.
    let row = h
        .leases
        .get(&djinn_db::BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
            consumer_id: "invocation-1".into(),
        })
        .await
        .unwrap()
        .expect("the escalation must still be a durable, fenced row");
    assert_eq!(row.weight, 0, "re-entrant escalation must weigh zero");
    assert!(
        row.fencing_token.is_some(),
        "zero weight must not mean unfenced -- the quota lift still needs a token"
    );
}

/// A LIGHT task-run holds no dispatch slot, so its escalation is the one that
/// pays. Light is a dispatch-admission prior, not a capability boundary: the
/// ~5% of light sessions that really compile are governed by measurement.
#[tokio::test]
async fn a_light_task_runs_escalation_pays_full_weight() {
    let h = harness(2).await;

    let decision = h
        .controller
        .admit_task_run(
            Some("reviewer"),
            AdmissionDomain::TaskObservation,
            "light-task".into(),
            0,
            "djinn-taskrun-light".into(),
        )
        .await
        .unwrap();
    assert!(matches!(decision, BuildAdmissionDecision::Permitted { .. }));
    assert_eq!(
        occupancy(&h.leases).await,
        0,
        "a Light role must take no DISPATCH slot"
    );

    // The reviewer runs `cargo check`. It contends now, at full weight.
    assert!(matches!(
        h.lease
            .queue(LeaseQueueRequest {
                identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                    task_id: "light-task".into(),
                    task_run_id: "light-run".into(),
                    invocation_id: "light-invocation".into(),
                }),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await,
        LeaseResult::Granted(_)
    ));
    assert_eq!(
        occupancy(&h.leases).await,
        1,
        "a Light compile is not free -- it is charged when it actually happens"
    );
}

/// Light roles take no dispatch slot even when the pool is completely full.
/// This is the throughput property #2600 measured: pre-charging a slot 100% of
/// the time for a ~5% event would queue 56% of task-runs behind builds they
/// almost never compete with.
#[tokio::test]
async fn light_roles_are_admitted_against_a_saturated_pool() {
    const CAP: i64 = 1;
    let h = harness(CAP).await;

    assert!(acquire_warm_lease(&h.lease, "saturate").await);
    assert_eq!(occupancy(&h.leases).await, CAP);

    for role in [
        "planner",
        "reviewer",
        "lead",
        "advocate",
        "adversary",
        "judge",
    ] {
        assert_eq!(
            RoleResourceClass::for_role_name(role),
            RoleResourceClass::Light,
            "{role} must be classed Light for this assertion to mean anything"
        );
        let decision = h
            .controller
            .admit_task_run(
                Some(role),
                AdmissionDomain::TaskObservation,
                format!("light-{role}"),
                0,
                format!("djinn-taskrun-light-{role}"),
            )
            .await
            .unwrap();
        assert!(
            matches!(decision, BuildAdmissionDecision::Permitted { .. }),
            "{role} was blocked by a full BUILD pool it does not draw from"
        );
    }
    assert_eq!(
        occupancy(&h.leases).await,
        CAP,
        "no Light admission may have consumed capacity"
    );
}

/// A terminal task-run hands its dispatch slot back -- exactly once.
#[tokio::test]
async fn a_terminal_task_run_returns_its_slot_once() {
    let h = harness(1).await;

    let BuildAdmissionDecision::Permitted { permit, .. } = h
        .controller
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            "releasing".into(),
            0,
            "djinn-taskrun-releasing".into(),
        )
        .await
        .unwrap()
    else {
        panic!("the first build-capable dispatch must be admitted");
    };
    assert_eq!(occupancy(&h.leases).await, 1);

    h.controller
        .transition(&permit, WarmAdmissionTransition::CreateStarted)
        .await
        .unwrap();
    h.controller
        .transition(
            &permit,
            WarmAdmissionTransition::Live {
                uid: "taskrun-uid-1".into(),
            },
        )
        .await
        .unwrap();
    h.controller
        .transition(
            &permit,
            WarmAdmissionTransition::Terminal {
                uid: "taskrun-uid-1".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        occupancy(&h.leases).await,
        0,
        "a terminal task-run must return its slot or the pool leaks shut"
    );

    // A duplicate terminal signal must not release a second time -- that would
    // free capacity belonging to whoever took the slot next.
    assert!(acquire_warm_lease(&h.lease, "after-release").await);
    let _ = h
        .controller
        .transition(
            &permit,
            WarmAdmissionTransition::Terminal {
                uid: "taskrun-uid-1".into(),
            },
        )
        .await;
    assert_eq!(
        occupancy(&h.leases).await,
        1,
        "a replayed terminal released capacity it no longer owned"
    );
}

/// Strict FIFO by weight: a heavy head is never skipped for a lighter row
/// behind it. Skipping would starve a heavy consumer behind the unbroken stream
/// of weight-1 dispatches that arrives most often.
#[tokio::test]
async fn a_heavy_queue_head_is_not_starved_by_lighter_rows_behind_it() {
    use djinn_db::{BuildLeaseKey, GrantNextBuildLeaseResult, QueueBuildLeaseInput};

    let db = Database::open_in_memory().unwrap();
    let leases = BuildLeaseRepository::new(db);
    let now = "2026-07-25T00:00:00Z";

    let queue = async |kind, id: &str, weight| {
        leases
            .queue(&QueueBuildLeaseInput {
                key: BuildLeaseKey {
                    consumer_kind: kind,
                    consumer_id: id.into(),
                },
                immutable_identity: format!("identity:{id}"),
                queue_deadline: None,
                launch_deadline: None,
                weight,
            })
            .await
            .unwrap()
    };

    // FIFO order matters, so the occupant is enqueued FIRST. Then a weight-2
    // warm Job (a deployment that raised DJINN_K8S_WARM_CPU_REQUEST), then a
    // light dispatch behind it.
    queue(BuildLeaseConsumerKind::TaskDispatch, "occupant", 1).await;
    queue(BuildLeaseConsumerKind::GraphWarm, "heavy-head", 2).await;
    queue(BuildLeaseConsumerKind::TaskDispatch, "light-behind", 1).await;

    // Cap 2: the occupant takes one unit, so the weight-2 head no longer fits
    // while the weight-1 row behind it would. Strict FIFO must grant neither.
    assert!(matches!(
        leases.grant_next(2, now, None).await.unwrap(),
        GrantNextBuildLeaseResult::Granted(_)
    ));

    match leases.grant_next(2, now, None).await.unwrap() {
        GrantNextBuildLeaseResult::Empty { occupancy, cap } => {
            assert_eq!((occupancy, cap), (1, 2));
        }
        GrantNextBuildLeaseResult::Granted(row) => panic!(
            "granted {:?} past a heavy head that did not fit -- the head starves",
            row.key.consumer_id
        ),
    }
}

/// Occupancy is a weighted SUM, not a row COUNT. A COUNT would make the
/// zero-weight re-entry occupy a full slot and reintroduce double-charging.
#[tokio::test]
async fn occupancy_sums_weight_rather_than_counting_rows() {
    use djinn_db::{BuildLeaseKey, GrantNextBuildLeaseResult, QueueBuildLeaseInput};

    let db = Database::open_in_memory().unwrap();
    let leases = BuildLeaseRepository::new(db);
    let now = "2026-07-25T00:00:00Z";

    for (id, weight) in [("zero-a", 0), ("zero-b", 0), ("real", 1)] {
        leases
            .queue(&QueueBuildLeaseInput {
                key: BuildLeaseKey {
                    consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
                    consumer_id: id.into(),
                },
                immutable_identity: format!("identity:{id}"),
                queue_deadline: None,
                launch_deadline: None,
                weight,
            })
            .await
            .unwrap();
    }
    for _ in 0..3 {
        assert!(matches!(
            leases.grant_next(1, now, None).await.unwrap(),
            GrantNextBuildLeaseResult::Granted(_)
        ));
    }

    let snapshot = leases.snapshot().await.unwrap();
    assert_eq!(
        snapshot.occupied, 1,
        "three occupying rows weighing 0+0+1 must occupy ONE slot, not three"
    );
    assert_eq!(
        snapshot
            .rows
            .iter()
            .filter(|row| row.state != BuildLeaseState::Terminal)
            .count(),
        3
    );
}

/// Weight is derived from the rendered manifests, not typed into admission.
///
/// The premise this replaces was that a graph-warm Job (a full workspace
/// compile) must outweigh a task-run that only might compile briefly. Measured
/// against the real render that is false: both request 4000m. A concurrency
/// semaphore governs RATE, not duration, and while they run these two cost the
/// node the same. If the render ever diverges, this fails.
#[test]
fn build_slot_weight_is_derived_from_the_real_rendered_cpu() {
    let config = djinn_k8s::KubernetesConfig::for_testing();
    let slot = djinn_k8s::launcher::launcher_leased_millicores(&config);
    let warm = djinn_k8s::launcher::warm_job_millicores(&config);

    assert_eq!(slot, 4_000, "the leased task-run quota changed");
    assert_eq!(warm, 4_000, "the warm Job CPU request changed");

    let weights = BuildSlotWeights {
        slot_millicores: slot,
        warm_millicores: warm,
    };
    assert_eq!(weights.warm().slots(), 1);
    assert_eq!(weights.dispatch().slots(), 1);
    assert_eq!(
        weights.invocation(false).slots(),
        1,
        "an invocation with no dispatch slot behind it pays in full"
    );
    assert_eq!(
        weights.invocation(true).slots(),
        0,
        "an invocation whose slot is already held must be free"
    );

    // The derivation is real, not a constant: doubling the warm request
    // reweights it with no code change.
    let doubled = BuildSlotWeights {
        slot_millicores: slot,
        warm_millicores: warm * 2,
    };
    assert_eq!(doubled.warm().slots(), 2);

    // Rounding is UP, so a workload asking for more than a slot occupies more
    // than a slot rather than being made cheap by truncation.
    assert_eq!(BuildSlotWeight::for_millicores(4_001, 4_000).slots(), 2);
    assert_eq!(BuildSlotWeight::for_millicores(1, 4_000).slots(), 1);
    assert_eq!(BuildSlotWeight::for_millicores(0, 4_000).slots(), 0);
}

/// Light roles buy nothing at dispatch; build-capable roles buy one slot.
#[test]
fn dispatch_weight_follows_the_role_resource_class() {
    assert_eq!(
        BuildSlotWeight::for_dispatch(RoleResourceClass::Light, 4_000).slots(),
        0
    );
    assert_eq!(
        BuildSlotWeight::for_dispatch(RoleResourceClass::BuildCapable, 4_000).slots(),
        1
    );
}

/// Until the durable epoch arms it, layer-1 dispatch admission must observe and
/// report, never deny. Landing the unified pool must not itself flip
/// enforcement on -- that is a deliberate operator action.
#[tokio::test]
async fn unarmed_dispatch_admission_observes_without_denying_or_occupying() {
    const CAP: i64 = 1;
    let h = harness(CAP).await;
    // Return to the real, unarmed default.
    h.lease.set_dispatch_enforcing_for_test(false);

    assert!(acquire_warm_lease(&h.lease, "shadow-saturate").await);
    assert_eq!(occupancy(&h.leases).await, CAP);

    for index in 0..4 {
        let decision = h
            .controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                format!("shadow-{index}"),
                0,
                format!("djinn-taskrun-shadow-{index}"),
            )
            .await
            .unwrap();
        assert!(
            matches!(decision, BuildAdmissionDecision::Permitted { .. }),
            "shadow must never deny; landing this change cannot alter enforcement"
        );
    }

    assert_eq!(
        occupancy(&h.leases).await,
        CAP,
        "a shadow probe must acquire nothing -- a shadow row would deny warming"
    );
    assert!(
        h.controller.would_defer_observation_count().await > 0,
        "shadow must still report what enforcement WOULD have done; that signal \
         is what the operator reads before arming the epoch"
    );
}

/// A queue position must not outlive the task that wanted it.
///
/// A queued row occupies nothing, which makes an orphan look harmless. It is
/// not: `grant_next` selects the oldest queued row, so a position whose task is
/// gone is eventually GRANTED, occupies a slot, and is released by nobody --
/// a permanent capacity leak that surfaces only as a pool that mysteriously
/// shrinks. A closed task must surrender its position explicitly.
#[tokio::test]
async fn a_closed_task_surrenders_its_queue_position_instead_of_leaking_a_slot() {
    const CAP: i64 = 1;
    let h = harness(CAP).await;

    // Saturate, so the next dispatch can only queue.
    assert!(acquire_warm_lease(&h.lease, "occupy").await);

    let decision = h
        .controller
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            "abandoned-task".into(),
            0,
            "djinn-taskrun-abandoned".into(),
        )
        .await
        .unwrap();
    assert!(matches!(decision, BuildAdmissionDecision::Denied { .. }));

    let queued = h
        .leases
        .get(&djinn_db::BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::TaskDispatch,
            consumer_id: "abandoned-task:0".into(),
        })
        .await
        .unwrap()
        .expect("a denied dispatch holds a durable FIFO position");
    assert_eq!(queued.state, BuildLeaseState::Queued);
    assert!(
        queued.queue_deadline.is_some(),
        "a queue position must carry a deadline so no missed hook leaks it forever"
    );

    // The task closes without ever dispatching.
    h.controller.cancel_deferred_task("abandoned-task").await;

    let after = h
        .leases
        .get(&djinn_db::BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::TaskDispatch,
            consumer_id: "abandoned-task:0".into(),
        })
        .await
        .unwrap()
        .expect("the row is retained for replay");
    assert_eq!(
        after.state,
        BuildLeaseState::Terminal,
        "a closed task's queue position must be surrendered, not left to be granted later"
    );

    // Releasing the warm lease must now hand capacity to real work, not to the
    // orphan. Draining happens on the release itself.
    let occupied_before_release = occupancy(&h.leases).await;
    assert_eq!(occupied_before_release, CAP);
    h.lease
        .release(djinn_supervisor::services::LeaseReleaseRequest {
            identity: LeaseIdentity::GraphWarm(
                djinn_supervisor::services::GraphWarmLeaseIdentity {
                    project_id: "unified-project".into(),
                    warm_request_id: "occupy".into(),
                    graph_revision: "unified-revision".into(),
                },
            ),
            fencing_token: djinn_supervisor::services::LeaseFencingToken(1),
            candidate_cleanup: false,
        })
        .await;
    assert_eq!(
        occupancy(&h.leases).await,
        0,
        "the surrendered position must not have been granted the freed slot"
    );
}

/// Recovery must not fail the node closed because the capacity authority has
/// not recovered YET.
///
/// This is the production startup order, not a contrived one:
/// `initialize_build_admission_recovery` runs before `initialize_graph_warmer`,
/// and the lease service recovers inside the latter — so on every boot the
/// authority's occupancy is unknown at the moment recovery seeds the
/// controller. Latching `over_cap` on unknown occupancy set the gate on every
/// startup, and the gate is sticky: it is re-read only when a durable terminal
/// transition releases a permit, which cannot happen while readiness reports
/// `SeededOccupancyAboveCap` and denies everything. Enforce stayed shut
/// permanently, for a reason that was not true.
#[tokio::test]
async fn an_unrecovered_authority_is_unknown_capacity_not_over_cap() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));

    // Deliberately NOT recovered: exactly what the controller sees at the point
    // `initialize_build_admission_recovery` runs.
    let lease = Arc::new(BuildLeaseService::new(Arc::clone(&leases), 3));
    assert!(!lease.is_ready());
    let authority: Arc<dyn BuildSlotAuthority> =
        Arc::new(BuildLeaseDispatchAuthority::new(Arc::clone(&lease)));
    assert!(
        authority.occupancy().await.is_none(),
        "an unrecovered authority reports occupancy as unknown, never as zero"
    );

    let controller =
        BuildAdmissionController::new_closed(Arc::clone(&journal), 3, "unrecovered-epoch")
            .with_slot_authority(authority);
    let report = controller
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();

    assert_eq!(
        report.readiness,
        crate::build_admission::BuildAdmissionReadiness::InventoryPending,
        "unknown occupancy is not over-cap; the real startup gates keep Enforce closed"
    );

    // Fail-closed is preserved where it is not sticky: an unreachable authority
    // denies at admission, and stops denying by itself once it is readable.
    controller.mark_inventory_ready();
    controller.mark_topology_ready();
    assert!(controller.is_ready());
    assert!(
        matches!(
            controller
                .admit_task_run(
                    Some("worker"),
                    AdmissionDomain::TaskObservation,
                    "before-lease-recovery".into(),
                    0,
                    "run-before".into(),
                )
                .await
                .unwrap(),
            BuildAdmissionDecision::Denied { .. }
        ),
        "capacity that cannot be read is never permission"
    );

    // The warmer's startup phase recovers the lease. Admission opens with no
    // further intervention — there is no sticky gate left to clear.
    assert!(matches!(lease.recover().await, LeaseResult::Status(_)));
    lease.set_dispatch_enforcing_for_test(true);
    assert!(
        matches!(
            controller
                .admit_task_run(
                    Some("worker"),
                    AdmissionDomain::TaskObservation,
                    "after-lease-recovery".into(),
                    0,
                    "run-after".into(),
                )
                .await
                .unwrap(),
            BuildAdmissionDecision::Permitted { .. }
        ),
        "a recovered authority admits without any gate needing to be cleared"
    );
}

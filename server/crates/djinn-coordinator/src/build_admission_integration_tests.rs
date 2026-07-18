//! Deterministic, controller-level integration coverage for combined admission.
//!
//! These tests deliberately use barriers and durable journal inspection rather
//! than wall-clock ordering or a Kubernetes cluster.

use std::sync::Arc;

use djinn_db::{
    AdmissionDomain, AdmissionHandoffAuthority, AdmissionHandoffPhase, AdmissionHandoffRepository,
    AdmissionJournalKey, AdmissionJournalRepository, AdmissionState, AdmissionWorkloadKind,
    Database, ReserveAdmissionInput,
};
use djinn_k8s::{
    WarmAdmission, WarmAdmissionError, WarmAdmissionPermit, WarmAdmissionRequest,
    WarmAdmissionTransition,
};
use tokio::sync::Barrier;

use crate::build_admission::{
    AdmissionSeedReport, BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode,
    BuildAdmissionReadiness,
};
use crate::build_admission_handoff::{
    EmergencyAuthorityDecision, HandoffState, HandoffWarningGauges, InvocationAuthorityObservation,
    evaluate_handoff,
};

fn controller(mode: BuildAdmissionMode, cap: i64) -> BuildAdmissionController {
    BuildAdmissionController::new(
        Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        )),
        mode,
        cap,
        "integration-test-epoch",
    )
}

fn warm(id: &str) -> WarmAdmissionRequest {
    warm_generation(id, 0)
}

fn warm_generation(id: &str, generation: i64) -> WarmAdmissionRequest {
    WarmAdmissionRequest {
        domain: "ignored".into(),
        work_id: id.into(),
        generation,
        object_name: format!("warm-{id}-{generation}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn cap_three_six_ready_tasks_admit_three_then_one_per_fenced_release() {
    let controller = Arc::new(controller(BuildAdmissionMode::Enforce, 3));
    let barrier = Arc::new(Barrier::new(7));
    let mut attempts = Vec::new();
    for index in 0..6 {
        let controller = Arc::clone(&controller);
        let barrier = Arc::clone(&barrier);
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            let id = format!("ready-{index}");
            let decision = controller
                .admit_task_run(
                    Some("worker"),
                    AdmissionDomain::TaskObservation,
                    id.clone(),
                    0,
                    format!("task-run-{index}"),
                )
                .await;
            (id, decision)
        }));
    }
    barrier.wait().await;

    let mut admitted = Vec::new();
    let mut denied = Vec::new();
    for attempt in attempts {
        let (id, decision) = attempt.await.unwrap();
        match decision.unwrap() {
            BuildAdmissionDecision::Permitted { permit, .. } => admitted.push((id, permit)),
            BuildAdmissionDecision::Denied { .. } => denied.push(id),
            BuildAdmissionDecision::Unclassified => panic!("worker must classify"),
        }
    }
    assert_eq!(admitted.len(), 3);
    assert_eq!(denied.len(), 3);
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        3
    );

    for (index, (_, permit)) in admitted.into_iter().enumerate() {
        let uid = format!("uid-{index}");
        controller
            .transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(&permit, WarmAdmissionTransition::Live { uid: uid.clone() })
            .await
            .unwrap();
        controller
            .transition(&permit, WarmAdmissionTransition::Terminal { uid })
            .await
            .unwrap();
        let next = denied.remove(0);
        assert!(matches!(
            controller
                .admit_task_run(
                    Some("worker"),
                    AdmissionDomain::TaskObservation,
                    next.clone(),
                    0,
                    format!("task-run-{next}"),
                )
                .await
                .unwrap(),
            BuildAdmissionDecision::Permitted { .. }
        ));
        assert_eq!(
            controller
                .journal()
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            3
        );
    }
    assert!(denied.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_warm_races_and_cap_matrix_bound_combined_durable_occupancy() {
    for cap in [1_i64, 2, 3, 5] {
        let controller = Arc::new(controller(BuildAdmissionMode::Enforce, cap));
        let barrier = Arc::new(Barrier::new(3));
        let task = {
            let controller = Arc::clone(&controller);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                controller
                    .admit_task_run(
                        Some("worker"),
                        AdmissionDomain::TaskObservation,
                        "task-racer".into(),
                        0,
                        "task-racer-run".into(),
                    )
                    .await
            })
        };
        let warmer = {
            let controller = Arc::clone(&controller);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                WarmAdmission::admit(controller.as_ref(), warm("warm-racer")).await
            })
        };
        barrier.wait().await;
        let task_won = matches!(
            task.await.unwrap().unwrap(),
            BuildAdmissionDecision::Permitted { .. }
        );
        let warm_won = warmer.await.unwrap().is_ok();
        assert!(task_won || warm_won, "one race contender wins cap {cap}");

        for index in 0..cap + 2 {
            if index % 2 == 0 {
                let _ = controller
                    .admit_task_run(
                        Some("worker"),
                        AdmissionDomain::TaskObservation,
                        format!("task-{index}"),
                        0,
                        format!("task-run-{index}"),
                    )
                    .await
                    .unwrap();
            } else {
                let result =
                    WarmAdmission::admit(controller.as_ref(), warm(&format!("warm-{index}"))).await;
                assert!(result.is_ok() || matches!(result, Err(WarmAdmissionError::Denied { .. })));
            }
            assert!(
                controller
                    .journal()
                    .count_task_or_warm_occupancy()
                    .await
                    .unwrap()
                    <= cap
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_post_reconciliation_and_callbacks_keep_fenced_capacity_correct() {
    let controller = Arc::new(controller(BuildAdmissionMode::Enforce, 1));
    let reserved = Arc::new(Barrier::new(2));
    let (allow_post_result, post_result_gate) = tokio::sync::oneshot::channel();
    let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let create = {
        let controller = Arc::clone(&controller);
        let reserved = Arc::clone(&reserved);
        let posts = Arc::clone(&posts);
        tokio::spawn(async move {
            let permit = WarmAdmission::admit(controller.as_ref(), warm("deterministic-name"))
                .await
                .unwrap();
            controller
                .transition(&permit, WarmAdmissionTransition::CreateStarted)
                .await
                .unwrap();
            reserved.wait().await;
            post_result_gate.await.unwrap();
            posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            controller
                .transition(
                    &permit,
                    WarmAdmissionTransition::CreateUnknown {
                        diagnostic: "POST response lost".into(),
                    },
                )
                .await
                .unwrap();
            permit
        })
    };
    reserved.wait().await;
    // A reconciliation pass for another epoch cannot retire current in-flight POST.
    controller
        .journal()
        .recover_predecessor_epoch("other-epoch")
        .await
        .unwrap();
    assert!(matches!(
        WarmAdmission::admit(controller.as_ref(), warm("cannot-reconcile-away")).await,
        Err(WarmAdmissionError::Denied { .. })
    ));
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1
    );
    allow_post_result.send(()).unwrap();
    let permit = create.await.unwrap();
    assert_eq!(posts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let retry = WarmAdmission::admit(controller.as_ref(), warm("deterministic-name"))
        .await
        .unwrap();
    assert_eq!(permit, retry);
    controller
        .transition(
            &retry,
            WarmAdmissionTransition::Live {
                uid: "looked-up-by-name".into(),
            },
        )
        .await
        .unwrap();
    let rows = controller
        .journal()
        .list_history(AdmissionDomain::WarmBuild, "deterministic-name")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, AdmissionState::Live);
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1
    );
    assert!(
        controller
            .transition(
                &retry,
                WarmAdmissionTransition::Terminal {
                    uid: "stale-uid".into()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1
    );
    controller
        .transition(
            &retry,
            WarmAdmissionTransition::Terminal {
                uid: "looked-up-by-name".into(),
            },
        )
        .await
        .unwrap();
    // A delayed terminal for generation zero cannot release the successor.
    let next_generation = WarmAdmission::admit(
        controller.as_ref(),
        warm_generation("deterministic-name", 1),
    )
    .await
    .unwrap();
    controller
        .transition(&next_generation, WarmAdmissionTransition::CreateStarted)
        .await
        .unwrap();
    controller
        .transition(
            &next_generation,
            WarmAdmissionTransition::Live {
                uid: "next-generation-uid".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        controller
            .transition(
                &retry,
                WarmAdmissionTransition::Terminal {
                    uid: "looked-up-by-name".into(),
                },
            )
            .await
            .is_err(),
        "stale generation callback must not release current occupancy"
    );
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1
    );
    controller
        .transition(
            &next_generation,
            WarmAdmissionTransition::Terminal {
                uid: "next-generation-uid".into(),
            },
        )
        .await
        .unwrap();

    // A barrier holds cancellation after reservation and before POST. Its
    // definitive failure releases once; duplicate cancellation cannot leak or
    // double-release capacity.
    let cancellation_reserved = Arc::new(Barrier::new(2));
    let (cancel_post, cancel_gate) = tokio::sync::oneshot::channel();
    let cancelled_create = {
        let controller = Arc::clone(&controller);
        let cancellation_reserved = Arc::clone(&cancellation_reserved);
        tokio::spawn(async move {
            let permit = WarmAdmission::admit(controller.as_ref(), warm("cancelled-before-post"))
                .await
                .unwrap();
            controller
                .transition(&permit, WarmAdmissionTransition::CreateStarted)
                .await
                .unwrap();
            cancellation_reserved.wait().await;
            cancel_gate.await.unwrap();
            controller
                .transition(
                    &permit,
                    WarmAdmissionTransition::DefinitiveFailure {
                        diagnostic: "dispatch cancelled before POST".into(),
                    },
                )
                .await
                .unwrap();
            permit
        })
    };
    cancellation_reserved.wait().await;
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1,
        "paused cancellation retains its reservation"
    );
    cancel_post.send(()).unwrap();
    let cancelled = cancelled_create.await.unwrap();
    controller
        .transition(
            &cancelled,
            WarmAdmissionTransition::DefinitiveFailure {
                diagnostic: "duplicate cancellation".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        0,
        "cancellation and duplicate cancellation release capacity exactly once"
    );
}

#[tokio::test]
async fn invocation_children_are_durable_but_do_not_self_block_the_parent_cap() {
    let controller = controller(BuildAdmissionMode::Enforce, 1);
    assert!(matches!(
        controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "parent".into(),
                0,
                "parent-run".into(),
            )
            .await
            .unwrap(),
        BuildAdmissionDecision::Permitted { .. }
    ));
    for child in ["child-a", "child-b", "child-c"] {
        let BuildAdmissionDecision::Permitted { permit, .. } = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::InvocationBuild,
                child.into(),
                0,
                format!("{child}-job"),
            )
            .await
            .unwrap()
        else {
            panic!("child must not self-block its parent");
        };
        assert_eq!(
            controller
                .journal()
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1
        );
        controller
            .transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &permit,
                WarmAdmissionTransition::Live {
                    uid: format!("{child}-uid"),
                },
            )
            .await
            .unwrap();
        controller
            .transition(
                &permit,
                WarmAdmissionTransition::Terminal {
                    uid: format!("{child}-uid"),
                },
            )
            .await
            .unwrap();
        let rows = controller
            .journal()
            .list_history(AdmissionDomain::InvocationBuild, child)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "children retain separate durable views");
        assert_eq!(
            rows[0].state,
            AdmissionState::Terminal,
            "child release is durable"
        );
        assert_eq!(
            controller
                .journal()
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1
        );
    }
    assert!(matches!(
        WarmAdmission::admit(&controller, warm("real-warm-is-blocked")).await,
        Err(WarmAdmissionError::Denied { .. })
    ));
}

/// Deterministic cap-one forced-restart harness using barriers/channels.
///
/// Scenario (acceptance criterion 4):
/// 1. The predecessor process reserves a warm slot, commits CreateInFlight, and
///    issues a paused POST (the process is killed before the POST result
///    resolves).
/// 2. The replacement process starts with EMPTY initial inventory (no
///    Kubernetes list has run yet) and recovers the durable journal.
/// 3. Recovery converts the predecessor CreateInFlight row to occupying
///    CreateUnknown.
/// 4. A competitor warm is DENIED while the predecessor CreateUnknown occupies
///    the single cap-one slot.
/// 5. The late predecessor create resolves (the Kubernetes object exists) and
///    is adopted into the SAME generation (Live with the looked-up UID).
/// Occupancy never exceeds one throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_one_forced_restart_denies_competitor_and_adopts_late_create() {
    let journal = Arc::new(AdmissionJournalRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let work_id = "restart-warm";
    let predecessor_epoch = "predecessor-epoch";
    let replacement_epoch = "replacement-epoch";

    // --- Phase 1: predecessor commits CreateInFlight and issues a paused POST ---
    // The predecessor is a healthy controller whose own startup gates completed
    // long ago. It reserves the cap-one slot, durably commits CreateStarted
    // (the pre-POST state that survives forced process loss), and issues the
    // Kubernetes POST — modelled as a spawned task that signals "CreateInFlight
    // committed, POST issued" on a barrier and then parks on a oneshot gate so
    // the POST result never resolves for the predecessor process.
    let predecessor = Arc::new(BuildAdmissionController::new(
        Arc::clone(&journal),
        BuildAdmissionMode::Enforce,
        1,
        predecessor_epoch,
    ));
    let create_committed = Arc::new(Barrier::new(2));
    let (post_result, post_result_gate) = tokio::sync::oneshot::channel::<String>();
    let paused_post = {
        let predecessor = Arc::clone(&predecessor);
        let create_committed = Arc::clone(&create_committed);
        tokio::spawn(async move {
            let permit = WarmAdmission::admit(predecessor.as_ref(), warm(work_id))
                .await
                .expect("predecessor reserves the single cap-one slot");
            predecessor
                .transition(&permit, WarmAdmissionTransition::CreateStarted)
                .await
                .expect("predecessor durably commits CreateInFlight before POST");
            // The POST is issued at this point. The barrier proves the durable
            // pre-POST state is committed before the replacement may start;
            // the oneshot keeps the POST unresolved while the process is lost.
            create_committed.wait().await;
            // Forced process loss: the predecessor never records the create
            // result. Only the Kubernetes object — its UID — survives, which
            // the replacement later discovers through the authoritative GET.
            post_result_gate.await.expect("the late POST resolves")
        })
    };
    create_committed.wait().await;
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "predecessor CreateInFlight occupies the single slot"
    );
    let committed = journal
        .list_history(AdmissionDomain::WarmBuild, work_id)
        .await
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(
        committed[0].state,
        AdmissionState::CreateInFlight,
        "the durable pre-POST state survives the forced loss"
    );
    assert_eq!(committed[0].creator_server_epoch, predecessor_epoch);

    // --- Phase 2: replacement starts with EMPTY initial inventory and recovers ---
    // The replacement controller is constructed closed (JournalRecoveryIncomplete)
    // with a fresh, unique epoch. It has not performed any Kubernetes inventory.
    let replacement =
        BuildAdmissionController::new_closed(Arc::clone(&journal), 1, replacement_epoch);
    assert_eq!(
        replacement.readiness(),
        BuildAdmissionReadiness::JournalRecoveryIncomplete,
        "replacement Enforce starts fail-closed before recovery"
    );

    // --- Phase 3: recovery converts CreateInFlight to occupying CreateUnknown ---
    let report: AdmissionSeedReport = replacement
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();
    assert_eq!(report.retired_reserved, 0, "no predecessor Reserved rows");
    assert_eq!(
        report.marked_create_unknown, 1,
        "predecessor CreateInFlight converted to CreateUnknown"
    );
    assert_eq!(report.seeded_rows, 1, "the CreateUnknown row is seeded");
    assert_eq!(
        report.readiness,
        BuildAdmissionReadiness::CreateUnknownHealth,
        "CreateUnknown gates readiness until the late create is adopted"
    );
    assert!(!replacement.is_ready());
    let history = journal
        .list_history(AdmissionDomain::WarmBuild, work_id)
        .await
        .unwrap();
    assert_eq!(
        history[0].state,
        AdmissionState::CreateUnknown,
        "recovery converted the durable row to CreateUnknown"
    );
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "CreateUnknown still occupies the single slot"
    );

    // --- Phase 4: a competitor is DENIED while predecessor CreateUnknown occupies ---
    // Even though the replacement's Kubernetes inventory is empty, the durable
    // journal occupancy is one, so a competitor warm cannot reserve.
    let competitor =
        WarmAdmission::admit(&replacement, warm_generation("competitor-warm", 0)).await;
    // The controller is not ready (CreateUnknownHealth), so admission fails
    // closed. Even if it were ready, the durable occupancy would deny.
    assert!(
        matches!(competitor, Err(WarmAdmissionError::Denied { .. })),
        "competitor must be denied while predecessor CreateUnknown occupies the cap-one slot"
    );
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "denied competitor does not consume a slot"
    );

    // --- Phase 5: the late predecessor create is adopted into the same generation ---
    // The paused POST finally resolves cluster-side: the Kubernetes object the
    // predecessor created actually exists. The predecessor process is gone, so
    // it cannot record the result; the replacement discovers the object UID
    // through the authoritative GET (here: the resolved POST response) and
    // adopts the row into the SAME generation (CreateUnknown -> Live) through
    // the seeded permit, reached via the domain-appropriate recovered-permit
    // lookup keyed on `AdmissionDomain::WarmBuild`.
    post_result
        .send("looked-up-late-create-uid".to_string())
        .expect("release the paused POST result");
    let object_uid = paused_post
        .await
        .expect("the paused POST task reports the created object UID");
    let seeded_permit: WarmAdmissionPermit = replacement
        .permit_for_key(AdmissionDomain::WarmBuild, work_id, 0)
        .await
        .expect("the recovered CreateUnknown row seeded an addressable permit");
    assert!(
        replacement.task_run_permit(work_id, 0).await.is_none(),
        "the task-run accessor deliberately filters to TaskObservation and must \
         not return this warm-build row"
    );

    // Adoption does not wait for readiness: resolving CreateUnknown rows is
    // what lets readiness advance. CreateUnknown -> Live with the looked-up
    // UID adopts the predecessor create without allocating a new slot.
    replacement
        .transition(
            &seeded_permit,
            WarmAdmissionTransition::Live {
                uid: object_uid.clone(),
            },
        )
        .await
        .unwrap();
    let history = journal
        .list_history(AdmissionDomain::WarmBuild, work_id)
        .await
        .unwrap();
    assert_eq!(
        history[0].state,
        AdmissionState::Live,
        "late predecessor create adopted into the same generation"
    );
    assert_eq!(
        history[0].key.generation, 0,
        "the adopted create is in generation zero (same generation)"
    );
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "adopting the late create does not add occupancy"
    );

    // Adoption cleared the CreateUnknown gate; readiness falls through to the
    // still-pending inventory gate, so admission is STILL fail-closed.
    assert_eq!(
        replacement.readiness(),
        BuildAdmissionReadiness::InventoryPending,
        "adoption advances readiness to inventory-pending, never straight to healthy"
    );
    assert!(matches!(
        WarmAdmission::admit(
            &replacement,
            warm_generation("competitor-before-inventory", 0)
        )
        .await,
        Err(WarmAdmissionError::Denied { .. })
    ));

    // The real startup gates then complete in order: the broad Kubernetes
    // inventory LIST succeeds, then the single-active topology check passes.
    replacement.mark_inventory_ready();
    assert_eq!(
        replacement.readiness(),
        BuildAdmissionReadiness::TopologyPending
    );
    assert!(matches!(
        WarmAdmission::admit(
            &replacement,
            warm_generation("competitor-before-topology", 0)
        )
        .await,
        Err(WarmAdmissionError::Denied { .. })
    ));
    replacement.mark_topology_ready();
    assert_eq!(replacement.readiness(), BuildAdmissionReadiness::Healthy);

    // Healthy now, the single slot is Live: a competitor is still denied, this
    // time by the durable cap (occupancy one) rather than by a closed gate.
    assert!(matches!(
        WarmAdmission::admit(&replacement, warm_generation("competitor-after-adopt", 0)).await,
        Err(WarmAdmissionError::Denied { .. })
    ));
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "occupancy is never above one"
    );

    // Releasing the adopted create frees the slot.
    replacement
        .transition(
            &seeded_permit,
            WarmAdmissionTransition::Terminal { uid: object_uid },
        )
        .await
        .unwrap();
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        0,
        "terminal release frees the single slot"
    );
    assert!(
        WarmAdmission::admit(&replacement, warm_generation("competitor-after-release", 0))
            .await
            .is_ok(),
        "the freed slot admits at most one successor"
    );
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "occupancy returns to exactly one, never above"
    );
}

/// Forced process loss safety: the durable pre-POST state (CreateInFlight
/// committed before POST) is what survives, not cooperative shutdown. This test
/// proves that a predecessor killed mid-POST leaves a recoverable CreateUnknown
/// row that a replacement adopts, without relying on any graceful shutdown.
#[tokio::test]
async fn forced_loss_safety_depends_on_durable_pre_post_state_not_cooperative_shutdown() {
    let journal = Arc::new(AdmissionJournalRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let work_id = "forced-loss-warm";
    let predecessor_epoch = "forced-predecessor";

    // Predecessor commits CreateInFlight durably, then is KILLED (no graceful
    // shutdown, no POST result, no CreateUnknown transition from the
    // predecessor itself). Only the durable pre-POST state survives.
    journal
        .reserve(
            &ReserveAdmissionInput {
                key: AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: work_id.into(),
                    generation: 0,
                },
                workload_kind: AdmissionWorkloadKind::Warm,
                creator_server_epoch: predecessor_epoch.into(),
                object_name: format!("warm-{work_id}-0"),
            },
            1,
        )
        .await
        .unwrap();
    journal
        .mark_create_started(&djinn_db::CreateStartedInput {
            key: AdmissionJournalKey {
                domain: AdmissionDomain::WarmBuild,
                work_id: work_id.into(),
                generation: 0,
            },
            creator_server_epoch: predecessor_epoch.into(),
            object_name: format!("warm-{work_id}-0"),
        })
        .await
        .unwrap();

    // Replacement recovers WITHOUT any cooperative shutdown from the
    // predecessor. The CreateInFlight row is converted to CreateUnknown.
    let replacement =
        BuildAdmissionController::new_closed(Arc::clone(&journal), 1, "forced-replacement");
    let report = replacement
        .recover_all_predecessors_and_seed()
        .await
        .unwrap();
    assert_eq!(report.marked_create_unknown, 1);
    assert_eq!(report.seeded_rows, 1);
    assert_eq!(
        report.readiness,
        BuildAdmissionReadiness::CreateUnknownHealth
    );
    // The durable occupancy is one: a competitor cannot slip in during the gap.
    assert_eq!(
        journal.count_task_or_warm_occupancy().await.unwrap(),
        1,
        "forced loss leaves durable occupancy that a replacement recovers"
    );
    assert!(
        matches!(
            WarmAdmission::admit(&replacement, warm("competitor-forced-loss")).await,
            Err(WarmAdmissionError::Denied { .. })
        ),
        "safety depends on durable state, not cooperative shutdown"
    );
}

#[derive(Clone, Copy, Debug)]
enum HandoffMatrixAction {
    Acknowledge(AdmissionHandoffAuthority),
    Commit(AdmissionHandoffPhase),
}

#[derive(Clone, Copy, Debug)]
struct HandoffMatrixExpectation {
    phase: AdmissionHandoffPhase,
    state: HandoffState,
    emergency_acknowledged: bool,
    invocation_acknowledged: bool,
    // The scenario table owns each authority's required enforcement.
    emergency_enforcing: bool,
    invocation_enforcing: bool,
    legal_next: AdmissionHandoffPhase,
    advance_allowed: bool,
}

fn handoff_next(phase: AdmissionHandoffPhase) -> AdmissionHandoffPhase {
    match phase {
        AdmissionHandoffPhase::EmergencyPrimary => AdmissionHandoffPhase::ForwardOverlap,
        AdmissionHandoffPhase::ForwardOverlap => AdmissionHandoffPhase::InvocationPrimary,
        AdmissionHandoffPhase::InvocationPrimary => AdmissionHandoffPhase::RollbackOverlap,
        AdmissionHandoffPhase::RollbackOverlap => AdmissionHandoffPhase::EmergencyPrimary,
    }
}

async fn assert_handoff_restart_snapshot(
    repo: &AdmissionHandoffRepository,
    expected: HandoffMatrixExpectation,
) {
    let row = repo.read().await.unwrap().unwrap();
    assert_eq!(row.phase, expected.phase);
    assert_eq!(row.emergency_ack_epoch, expected.emergency_acknowledged.then_some(row.epoch));
    assert_eq!(row.invocation_ack_epoch, expected.invocation_acknowledged.then_some(row.epoch));
    let snapshot = evaluate_handoff(
        Ok(Some(row.clone())),
        BuildAdmissionMode::Enforce,
        expected.emergency_enforcing,
        BuildAdmissionReadiness::Healthy,
        InvocationAuthorityObservation {
            enforcing: expected.invocation_enforcing,
        },
    );
    assert_eq!(snapshot.state, expected.state);
    assert_eq!(
        snapshot.emergency == EmergencyAuthorityDecision::RequiredFailClosed,
        expected.emergency_enforcing,
        "emergency authority requirement must match the matrix"
    );
    assert_eq!(
        InvocationAuthorityObservation {
            enforcing: expected.invocation_enforcing,
        }
        .enforcing,
        expected.invocation_enforcing,
        "invocation authority observation must match the matrix"
    );
    assert!(
        expected.emergency_enforcing || expected.invocation_enforcing,
        "every crash/restart state retains at least one enforcing authority"
    );
    assert_eq!(
        snapshot.emergency_acknowledgement_allowed,
        expected.emergency_enforcing
            && !expected.emergency_acknowledged
            && snapshot.emergency == EmergencyAuthorityDecision::RequiredFailClosed,
    );
    assert_eq!(
        snapshot.emergency == EmergencyAuthorityDecision::MayDisable,
        !expected.emergency_enforcing,
    );
    assert_eq!(
        snapshot.warning_gauges(
            expected.emergency_enforcing,
            InvocationAuthorityObservation { enforcing: expected.invocation_enforcing },
        ),
        if snapshot.state == HandoffState::IncompleteEpoch {
            HandoffWarningGauges {
                stale_epoch: 1,
                ..HandoffWarningGauges::default()
            }
        } else {
            HandoffWarningGauges::default()
        },
    );
    for authority in [AdmissionHandoffAuthority::Emergency, AdmissionHandoffAuthority::Invocation] {
        assert!(matches!(
            repo.acknowledge(authority, row.epoch - 1).await,
            Err(djinn_db::Error::InvalidTransition(_))
        ));
    }
    assert!(matches!(
        repo.advance(row.epoch, handoff_next(expected.legal_next)).await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));
    if !expected.advance_allowed {
        assert!(matches!(
            repo.advance(row.epoch, expected.legal_next).await,
            Err(djinn_db::Error::InvalidTransition(_))
        ), "current-epoch acknowledgement guard rejects phase advance");
    }
}

/// Table-driven crash/restart proof for the complete forward and rollback cycle.
/// Its invocation authority is a typed observation, never a deployed service.
#[tokio::test]
async fn handoff_crash_matrix_preserves_authority_and_epoch_guards() {
    let repo = AdmissionHandoffRepository::new(Database::open_in_memory().unwrap());
    let expectations = [
        (AdmissionHandoffPhase::EmergencyPrimary, HandoffState::IncompleteEpoch, false, false, true, false, false),
        (AdmissionHandoffPhase::EmergencyPrimary, HandoffState::EmergencyPrimary, true, false, true, false, true),
        (AdmissionHandoffPhase::ForwardOverlap, HandoffState::IncompleteEpoch, false, false, true, true, false),
        (AdmissionHandoffPhase::ForwardOverlap, HandoffState::IncompleteEpoch, true, false, true, true, false),
        (AdmissionHandoffPhase::ForwardOverlap, HandoffState::ForwardOverlap, true, true, true, true, true),
        (AdmissionHandoffPhase::InvocationPrimary, HandoffState::IncompleteEpoch, false, false, true, true, false),
        (AdmissionHandoffPhase::InvocationPrimary, HandoffState::InvocationPrimary, false, true, false, true, true),
        (AdmissionHandoffPhase::RollbackOverlap, HandoffState::IncompleteEpoch, false, false, true, true, false),
        (AdmissionHandoffPhase::RollbackOverlap, HandoffState::IncompleteEpoch, true, false, true, true, false),
        (AdmissionHandoffPhase::RollbackOverlap, HandoffState::RollbackOverlap, true, true, true, true, true),
        (AdmissionHandoffPhase::EmergencyPrimary, HandoffState::IncompleteEpoch, false, false, true, false, false),
    ]
    .map(|(phase, state, emergency_acknowledged, invocation_acknowledged, emergency_enforcing, invocation_enforcing, advance_allowed)| HandoffMatrixExpectation {
        phase, state, emergency_acknowledged, invocation_acknowledged, emergency_enforcing, invocation_enforcing,
        legal_next: handoff_next(phase), advance_allowed,
    });
    let actions = [
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Emergency),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::ForwardOverlap),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Emergency),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Invocation),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::InvocationPrimary),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Invocation),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::RollbackOverlap),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Emergency),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Invocation),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::EmergencyPrimary),
    ];
    for (index, expected) in expectations.into_iter().enumerate() {
        // Crash/restart before every action and, on the next iteration, after it.
        assert_handoff_restart_snapshot(&repo, expected).await;
        let Some(action) = actions.get(index).copied() else {
            continue;
        };
        let before = repo.read().await.unwrap().unwrap();
        match action {
            HandoffMatrixAction::Acknowledge(authority) => {
                repo.acknowledge(authority, before.epoch).await.unwrap();
            }
            HandoffMatrixAction::Commit(next) => {
                assert_eq!(next, handoff_next(before.phase));
                repo.advance(before.epoch, next).await.unwrap();
            }
        }
    }

    let current = repo.read().await.unwrap().unwrap();
    assert_eq!(current.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert!(matches!(
        repo.acknowledge(AdmissionHandoffAuthority::Emergency, current.epoch - 1)
            .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));
    assert!(matches!(
        repo.advance(current.epoch, AdmissionHandoffPhase::InvocationPrimary)
            .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));
    assert!(matches!(
        repo.advance(current.epoch - 1, AdmissionHandoffPhase::ForwardOverlap)
            .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));

    let warning_cases = [
        (
            "read failure",
            Err(()),
            true,
            true,
            HandoffWarningGauges {
                epoch_unreadable: 1,
                ..HandoffWarningGauges::default()
            },
        ),
        (
            "missing row",
            Ok(None),
            true,
            false,
            HandoffWarningGauges::default(),
        ),
        (
            "steady overlap",
            Ok(None),
            true,
            true,
            HandoffWarningGauges {
                unexpected_overlap: 1,
                ..HandoffWarningGauges::default()
            },
        ),
        (
            "emergency primary overlap",
            Ok(Some(djinn_db::AdmissionHandoffRow { phase: AdmissionHandoffPhase::EmergencyPrimary, epoch: 9, emergency_ack_epoch: Some(9), invocation_ack_epoch: None, updated_at: "test".into() })),
            true, true, HandoffWarningGauges { unexpected_overlap: 1, ..HandoffWarningGauges::default() },
        ),
        (
            "invocation primary overlap",
            Ok(Some(djinn_db::AdmissionHandoffRow { phase: AdmissionHandoffPhase::InvocationPrimary, epoch: 9, emergency_ack_epoch: None, invocation_ack_epoch: Some(9), updated_at: "test".into() })),
            true, true, HandoffWarningGauges { unexpected_overlap: 1, ..HandoffWarningGauges::default() },
        ),
        (
            "recovered emergency primary",
            Ok(Some(djinn_db::AdmissionHandoffRow { phase: AdmissionHandoffPhase::EmergencyPrimary, epoch: 9, emergency_ack_epoch: Some(9), invocation_ack_epoch: None, updated_at: "test".into() })),
            true, false, HandoffWarningGauges::default(),
        ),
        (
            "valid overlap",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::ForwardOverlap,
                epoch: 9,
                emergency_ack_epoch: Some(9),
                invocation_ack_epoch: Some(9),
                updated_at: "test".into(),
            })),
            true,
            true,
            HandoffWarningGauges::default(),
        ),
        (
            "stale acknowledgement",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::RollbackOverlap,
                epoch: 9,
                emergency_ack_epoch: Some(8),
                invocation_ack_epoch: Some(9),
                updated_at: "test".into(),
            })),
            true,
            true,
            HandoffWarningGauges {
                stale_epoch: 1,
                ..HandoffWarningGauges::default()
            },
        ),
    ];
    for (name, row, emergency, invocation, expected) in warning_cases {
        let snapshot = evaluate_handoff(
            row,
            BuildAdmissionMode::Enforce,
            emergency,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation {
                enforcing: invocation,
            },
        );
        assert_eq!(
            snapshot.warning_gauges(
                emergency,
                InvocationAuthorityObservation { enforcing: invocation },
            ),
            expected,
            "{name}"
        );
    }
    assert_eq!(
        evaluate_handoff(
            Ok(Some(current)),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation::default(),
        )
        .state,
        HandoffState::IncompleteEpoch,
    );
}

#[tokio::test]
async fn concurrent_invocation_children_share_parent_observation_without_v0_occupancy() {
    let controller = controller(BuildAdmissionMode::Enforce, 1);
    let BuildAdmissionDecision::Permitted { permit: parent, .. } = controller
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            "shared-identity".into(),
            0,
            "parent-observation".into(),
        )
        .await
        .unwrap()
    else {
        panic!("parent observation reserves the one v0 slot");
    };
    let mut children = Vec::new();
    for child in ["shared-identity", "invocation-a", "invocation-b"] {
        let BuildAdmissionDecision::Permitted { permit, .. } = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::InvocationBuild,
                child.into(),
                0,
                format!("{child}-job"),
            )
            .await
            .unwrap()
        else {
            panic!("concurrent invocation child must not self-block parent");
        };
        children.push((child, permit));
    }
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1,
        "three simultaneously acquired invocation leases never double-count parent occupancy"
    );
    assert_eq!(
        controller.journal().list_history(AdmissionDomain::TaskObservation, "shared-identity").await.unwrap().len(),
        1,
        "same work identity retains its parent observation view while children coexist"
    );
    assert_eq!(
        controller.journal().list_history(AdmissionDomain::InvocationBuild, "shared-identity").await.unwrap().len(),
        1,
        "same work identity retains its invocation view while the parent is held"
    );
    for (child, permit) in children {
        controller
            .transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &permit,
                WarmAdmissionTransition::Live {
                    uid: format!("{child}-uid"),
                },
            )
            .await
            .unwrap();
        controller
            .transition(
                &permit,
                WarmAdmissionTransition::Terminal {
                    uid: format!("{child}-uid"),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            controller
                .journal()
                .list_history(AdmissionDomain::InvocationBuild, child)
                .await
                .unwrap()
                .len(),
            1,
            "invocation identity retains a separate durable view"
        );
    }
    assert_eq!(
        controller
            .journal()
            .count_task_or_warm_occupancy()
            .await
            .unwrap(),
        1,
        "child release cannot release or collide with the parent observation"
    );
    assert_eq!(
        controller.journal().list_history(AdmissionDomain::TaskObservation, "shared-identity").await.unwrap().len(),
        1,
        "releasing the same-identity child cannot replace the parent view"
    );
    controller
        .transition(
            &parent,
            WarmAdmissionTransition::DefinitiveFailure {
                diagnostic: "parent cleanup".into(),
            },
        )
        .await
        .unwrap();
}

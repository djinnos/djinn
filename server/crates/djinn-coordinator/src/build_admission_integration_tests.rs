//! Deterministic, controller-level integration coverage for combined admission.
//!
//! These tests deliberately use barriers and durable journal inspection rather
//! than wall-clock ordering or a Kubernetes cluster.

use std::sync::Arc;

use djinn_db::{AdmissionDomain, AdmissionJournalRepository, AdmissionState, Database};
use djinn_k8s::{WarmAdmission, WarmAdmissionError, WarmAdmissionRequest, WarmAdmissionTransition};
use tokio::sync::Barrier;

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode,
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

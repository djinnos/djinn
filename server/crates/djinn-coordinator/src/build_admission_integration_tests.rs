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
    WarmAdmissionRequest {
        domain: "ignored".into(),
        work_id: id.into(),
        generation: 0,
        object_name: format!("warm-{id}"),
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

#[tokio::test]
async fn paused_and_ambiguous_create_retain_capacity_and_deterministic_retry_resolves_live() {
    let controller = controller(BuildAdmissionMode::Enforce, 1);
    let permit = WarmAdmission::admit(&controller, warm("deterministic-name"))
        .await
        .unwrap();
    // Reservation is durable before the controlled fake POST is allowed to run.
    assert!(matches!(
        WarmAdmission::admit(&controller, warm("cannot-reconcile-away")).await,
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

    controller
        .transition(&permit, WarmAdmissionTransition::CreateStarted)
        .await
        .unwrap();
    controller
        .transition(
            &permit,
            WarmAdmissionTransition::CreateUnknown {
                diagnostic: "POST response lost".into(),
            },
        )
        .await
        .unwrap();
    let retry = WarmAdmission::admit(&controller, warm("deterministic-name"))
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
        assert!(matches!(
            controller
                .admit_task_run(
                    Some("worker"),
                    AdmissionDomain::InvocationBuild,
                    child.into(),
                    0,
                    format!("{child}-job"),
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
            1
        );
        assert_eq!(
            controller
                .journal()
                .list_history(AdmissionDomain::InvocationBuild, child)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    assert!(matches!(
        WarmAdmission::admit(&controller, warm("real-warm-is-blocked")).await,
        Err(WarmAdmissionError::Denied { .. })
    ));
}

//! PR-lane counterpart of the privileged 40-cycle kind proof.

use djinn_db::{
    AcquireBuildPodPermitResult, BuildPodPermitRepository, BuildPodPermitState,
    BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, CreateTaskRunParams, Database,
    TaskRunRepository, TransitionBuildPodResizeLifecycleResult,
};
use djinn_k8s::pod_resize_fixture::StoredTaskRunPod;

const RUN: &str = "01983f00-0000-7000-8000-00000000d091";
const POD_UID: &str = "91eq-hermetic-pod";

async fn seed_task_run(db: &Database) {
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(db, &project_id, &format!("proj-{project_id}")).await;
    let task_id = djinn_db::test_support::seed_task_row(
        db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: RUN,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn resize_cycle_hermetic_three_distinct_invocations_return_to_birth_limit() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    seed_task_run(&db).await;
    let permits = BuildPodPermitRepository::new(db);
    let pod = StoredTaskRunPod::resize_v2(POD_UID, "4000m");

    let AcquireBuildPodPermitResult::Acquired { row, .. } = permits.acquire(RUN, 3).await else {
        panic!("permit acquired")
    };
    permits
        .bind_or_refresh_job_uid(RUN, &row.permit_id, row.fencing_token, "job-uid")
        .await
        .unwrap();
    let observed = pod.observe_launcher().unwrap().unwrap();
    let identity = BuildPodResizeIdentity {
        pod_namespace: observed.namespace,
        pod_name: observed.pod_name,
        pod_uid: observed.pod_uid,
        launcher_container_name: observed.launcher_container_name,
        launcher_container_id: observed.launcher_container_id.unwrap(),
        image_digest: observed.image_digest.unwrap(),
        observed_launcher_protocol: observed.observed_protocol.clone().unwrap(),
        effective_launcher_protocol: observed.observed_protocol.unwrap(),
        admitted_cpu_millicores: observed.admitted_cpu_millicores.unwrap() as i64,
    };
    assert!(matches!(
        permits
            .capture_resize_identity(RUN, &row.permit_id, row.fencing_token, &identity)
            .await
            .unwrap(),
        CaptureBuildPodResizeIdentityResult::Captured(_)
    ));
    pod.resize_launcher_cpu(&identity.pod_name, 250)
        .await
        .unwrap();

    for cycle in 0..3 {
        let invocation = format!("01983f00-0000-7000-8000-00000000f09{cycle}");
        assert!(matches!(
            permits
                .begin_resize_invocation(
                    RUN,
                    &row.permit_id,
                    row.fencing_token,
                    POD_UID,
                    &invocation
                )
                .await
                .unwrap(),
            TransitionBuildPodResizeLifecycleResult::Transitioned(_)
        ));
        pod.resize_launcher_cpu(&identity.pod_name, 4_000)
            .await
            .unwrap();
        for (from, to) in [
            (
                BuildPodPermitState::LiftApplying,
                BuildPodPermitState::Lifted,
            ),
            (
                BuildPodPermitState::Lifted,
                BuildPodPermitState::DropRequired,
            ),
            (
                BuildPodPermitState::DropRequired,
                BuildPodPermitState::DropApplying,
            ),
        ] {
            assert!(matches!(
                permits
                    .transition_resize_lifecycle(
                        RUN,
                        &row.permit_id,
                        row.fencing_token,
                        POD_UID,
                        Some(&invocation),
                        from,
                        to
                    )
                    .await
                    .unwrap(),
                TransitionBuildPodResizeLifecycleResult::Transitioned(_)
            ));
        }
        pod.resize_launcher_cpu(&identity.pod_name, 250)
            .await
            .unwrap();
        assert_eq!(pod.launcher_status_cpu().as_deref(), Some("250m"));
        assert!(matches!(
            permits
                .transition_resize_lifecycle(
                    RUN,
                    &row.permit_id,
                    row.fencing_token,
                    POD_UID,
                    Some(&invocation),
                    BuildPodPermitState::DropApplying,
                    BuildPodPermitState::BirthConfirmed
                )
                .await
                .unwrap(),
            TransitionBuildPodResizeLifecycleResult::Transitioned(_)
        ));
        let active = permits.active(RUN).await.unwrap().unwrap();
        assert_eq!(
            active.resize_invocation_id.as_deref(),
            Some(invocation.as_str())
        );
        assert_eq!(active.state, BuildPodPermitState::BirthConfirmed);
    }
    assert_eq!(
        pod.resize_patches(),
        7,
        "birth clamp plus lift/drop for all three cycles"
    );
}

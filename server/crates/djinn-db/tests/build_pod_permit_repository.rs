use std::sync::Arc;

use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildPodPermitRepository,
    BuildPodPermitState, BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, Database,
    ReleaseBuildPodPermitResult, TransitionBuildPodResizeLifecycleResult,
};
use tokio::sync::Barrier;

async fn seed_runs(db: &Database, ids: &[&str]) {
    db.ensure_initialized().await.unwrap();
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('00000000-0000-7000-8000-000000000163', 9000000163, 'permit-repository')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('permit-repository-project', 'permit-repository-project', 'djinnos', 'permit-repository')",
    )
    .execute(pool)
    .await
    .unwrap();
    for (index, id) in ids.iter().enumerate() {
        let task_id = format!("permit-repository-task-{index}");
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ($1, 'permit-repository-project', $2, 'title', 'description', 'design', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '00000000-0000-7000-8000-000000000163')",
        )
        .bind(&task_id)
        .bind(format!("permit-{index}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
             VALUES ($1, 'permit-repository-project', $2, 'manual', 'running')",
        )
        .bind(id)
        .bind(&task_id)
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn resize_identity_is_write_once_and_lifecycle_is_uid_fenced() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["resize"]).await;
    let repo = BuildPodPermitRepository::new(db.clone());
    let permit = match repo.acquire("resize", 1).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    assert!(matches!(
        repo.bind_or_refresh_job_uid("resize", &permit.permit_id, permit.fencing_token, "job")
            .await
            .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));
    let identity = BuildPodResizeIdentity {
        pod_namespace: "ns".into(),
        pod_name: "pod".into(),
        pod_uid: "pod-uid".into(),
        launcher_container_name: "launcher".into(),
        launcher_container_id: "containerd://launcher".into(),
        image_digest: "sha256:abc".into(),
        observed_launcher_protocol: "resize-v2".into(),
        effective_launcher_protocol: "resize-v2".into(),
        admitted_cpu_millicores: 1000,
    };
    assert!(matches!(
        repo.capture_resize_identity("resize", &permit.permit_id, permit.fencing_token, &identity)
            .await
            .unwrap(),
        CaptureBuildPodResizeIdentityResult::Captured(_)
    ));
    assert!(matches!(
        repo.capture_resize_identity("resize", &permit.permit_id, permit.fencing_token, &identity)
            .await
            .unwrap(),
        CaptureBuildPodResizeIdentityResult::AlreadyCaptured(_)
    ));
    let mut conflicting = identity.clone();
    conflicting.pod_uid = "other-pod".into();
    assert!(matches!(
        repo.capture_resize_identity(
            "resize",
            &permit.permit_id,
            permit.fencing_token,
            &conflicting
        )
        .await
        .unwrap(),
        CaptureBuildPodResizeIdentityResult::Rejected
    ));
    for (from, to) in [
        (
            BuildPodPermitState::BirthConfirmed,
            BuildPodPermitState::LiftApplying,
        ),
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
        (
            BuildPodPermitState::DropApplying,
            BuildPodPermitState::BirthConfirmed,
        ),
        (
            BuildPodPermitState::BirthConfirmed,
            BuildPodPermitState::Quarantined,
        ),
    ] {
        assert!(matches!(
            repo.transition_resize_lifecycle(
                "resize",
                &permit.permit_id,
                permit.fencing_token,
                "pod-uid",
                from,
                to
            )
            .await
            .unwrap(),
            TransitionBuildPodResizeLifecycleResult::Transitioned(_)
        ));
    }
    assert!(matches!(
        repo.transition_resize_lifecycle(
            "resize",
            &permit.permit_id,
            permit.fencing_token + 1,
            "pod-uid",
            BuildPodPermitState::Quarantined,
            BuildPodPermitState::DropRequired
        )
        .await
        .unwrap(),
        TransitionBuildPodResizeLifecycleResult::Rejected
    ));
    assert!(matches!(
        repo.transition_resize_lifecycle(
            "resize",
            &permit.permit_id,
            permit.fencing_token,
            "stale-uid",
            BuildPodPermitState::Quarantined,
            BuildPodPermitState::DropRequired
        )
        .await
        .unwrap(),
        TransitionBuildPodResizeLifecycleResult::Rejected
    ));
    assert_eq!(
        BuildPodPermitRepository::new(db)
            .list_nonterminal_resize()
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_unit_is_acquired_by_exactly_one_synchronized_contender() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["existing", "contender-a", "contender-b"]).await;
    let repo = Arc::new(BuildPodPermitRepository::new(db));
    assert!(matches!(
        repo.acquire("existing", 2).await,
        AcquireBuildPodPermitResult::Acquired {
            idempotent: false,
            ..
        }
    ));

    let barrier = Arc::new(Barrier::new(3));
    let mut contenders = Vec::new();
    for task_run_id in ["contender-a", "contender-b"] {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        contenders.push(tokio::spawn(async move {
            barrier.wait().await;
            repo.acquire(task_run_id, 2).await
        }));
    }
    barrier.wait().await;
    let results = futures::future::join_all(contenders).await;
    let acquired = results
        .into_iter()
        .map(Result::unwrap)
        .filter(|result| matches!(result, AcquireBuildPodPermitResult::Acquired { .. }))
        .count();
    assert_eq!(acquired, 1);
    assert_eq!(repo.active_count().await.unwrap(), 2);
}

#[tokio::test]
async fn permit_lifecycle_is_idempotent_fenced_and_never_revoked_by_lower_limit() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["first", "second"]).await;
    let repo = BuildPodPermitRepository::new(db.clone());
    let first = match repo.acquire("first", 2).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    assert!(matches!(
        repo.acquire("first", 1).await,
        AcquireBuildPodPermitResult::Acquired {
            idempotent: true,
            ..
        }
    ));
    assert!(matches!(
        repo.acquire("second", 1).await,
        AcquireBuildPodPermitResult::PoolFull { .. }
    ));
    assert_eq!(
        repo.active("first").await.unwrap().unwrap().state,
        BuildPodPermitState::Acquired
    );

    assert!(matches!(
        repo.bind_or_refresh_job_uid("first", &first.permit_id, first.fencing_token, "job-a")
            .await
            .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));
    assert!(matches!(
        repo.bind_or_refresh_job_uid("first", &first.permit_id, first.fencing_token, "job-a")
            .await
            .unwrap(),
        BindBuildPodPermitResult::AlreadyBound(_)
    ));
    assert!(matches!(
        repo.release("first", &first.permit_id, first.fencing_token + 1, "absent")
            .await
            .unwrap(),
        ReleaseBuildPodPermitResult::Rejected
    ));
    assert!(matches!(
        repo.release("first", &first.permit_id, first.fencing_token, "absent")
            .await
            .unwrap(),
        ReleaseBuildPodPermitResult::Released(_)
    ));
    assert!(matches!(
        repo.release("first", &first.permit_id, first.fencing_token, "absent")
            .await
            .unwrap(),
        ReleaseBuildPodPermitResult::AlreadyReleased(_)
    ));
}

#[tokio::test]
async fn non_positive_limits_and_missing_pool_fail_closed() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["invalid-limit", "missing-pool"]).await;
    let repo = BuildPodPermitRepository::new(db.clone());

    assert!(matches!(
        repo.acquire("invalid-limit", 0).await,
        AcquireBuildPodPermitResult::InvalidLimit { limit: 0 }
    ));
    assert_eq!(repo.active_count().await.unwrap(), 0);

    sqlx::query("DELETE FROM build_pod_permit_pools WHERE pool_key = 'global'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        !repo
            .global_pool_is_readable()
            .await
            .expect("a missing singleton pool is a readable false result")
    );
    assert!(matches!(
        repo.acquire("missing-pool", 1).await,
        AcquireBuildPodPermitResult::Unavailable
    ));
    assert_eq!(repo.active_count().await.unwrap(), 0);

    sqlx::query("DROP TABLE build_pod_permit_pools")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(repo.global_pool_is_readable().await.is_err());
}

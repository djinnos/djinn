use std::sync::Arc;
use std::time::Duration;

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
    seed_runs(&db, &["resize", "resize-malformed"]).await;
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
    // Write-once is per field, not merely per Pod UID: every component of the
    // captured identity must reject a conflicting second capture, and none of
    // them may mutate the durable row on the way to being rejected.
    let conflicting_identities = [
        ("pod uid", {
            let mut it = identity.clone();
            it.pod_uid = "other-pod".into();
            it
        }),
        ("pod coordinates", {
            let mut it = identity.clone();
            it.pod_namespace = "other-ns".into();
            it.pod_name = "other-pod-name".into();
            it
        }),
        ("launcher container identity", {
            let mut it = identity.clone();
            it.launcher_container_id = "containerd://other".into();
            it
        }),
        ("image digest", {
            let mut it = identity.clone();
            it.image_digest = "sha256:replacement".into();
            it
        }),
        ("effective protocol", {
            let mut it = identity.clone();
            it.effective_launcher_protocol = "leaf-v1".into();
            it
        }),
        ("admitted ceiling", {
            let mut it = identity.clone();
            it.admitted_cpu_millicores = 2000;
            it
        }),
    ];
    for (label, conflicting) in &conflicting_identities {
        assert!(
            matches!(
                repo.capture_resize_identity(
                    "resize",
                    &permit.permit_id,
                    permit.fencing_token,
                    conflicting
                )
                .await
                .unwrap(),
                CaptureBuildPodResizeIdentityResult::Rejected
            ),
            "a conflicting {label} must fail closed"
        );
        assert_eq!(
            repo.active("resize")
                .await
                .unwrap()
                .unwrap()
                .resize_identity,
            Some(identity.clone()),
            "a rejected {label} capture must leave the first writer's row intact"
        );
    }
    // A replay of the *correct* identity under a stale permit or fence is still
    // not an owner, so it cannot be reported as an idempotent replay.
    assert!(matches!(
        repo.capture_resize_identity(
            "resize",
            &permit.permit_id,
            permit.fencing_token + 1,
            &identity
        )
        .await
        .unwrap(),
        CaptureBuildPodResizeIdentityResult::Rejected
    ));
    assert!(matches!(
        repo.capture_resize_identity(
            "resize",
            "00000000-0000-4000-8000-000000000000",
            permit.fencing_token,
            &identity
        )
        .await
        .unwrap(),
        CaptureBuildPodResizeIdentityResult::Rejected
    ));
    assert_eq!(
        repo.active("resize")
            .await
            .unwrap()
            .unwrap()
            .resize_identity,
        Some(identity.clone()),
        "stale-permit and stale-fence captures must not mutate the row"
    );

    // A never-captured permit proves the malformed-identity guard actually
    // rejects rather than the row simply being locked by an earlier capture.
    let fresh = match repo.acquire("resize-malformed", 2).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    assert!(matches!(
        repo.bind_or_refresh_job_uid(
            "resize-malformed",
            &fresh.permit_id,
            fresh.fencing_token,
            "job-malformed"
        )
        .await
        .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));
    for (label, malformed) in [
        ("blank pod uid", {
            let mut it = identity.clone();
            it.pod_uid = "   ".into();
            it
        }),
        ("unknown protocol", {
            let mut it = identity.clone();
            it.observed_launcher_protocol = "unknown-v3".into();
            it
        }),
        ("nonpositive ceiling", {
            let mut it = identity.clone();
            it.admitted_cpu_millicores = 0;
            it
        }),
    ] {
        assert!(
            matches!(
                repo.capture_resize_identity(
                    "resize-malformed",
                    &fresh.permit_id,
                    fresh.fencing_token,
                    &malformed
                )
                .await
                .unwrap(),
                CaptureBuildPodResizeIdentityResult::Rejected
            ),
            "a malformed identity ({label}) must fail closed"
        );
    }
    let untouched = repo.active("resize-malformed").await.unwrap().unwrap();
    assert_eq!(untouched.state, BuildPodPermitState::JobCreated);
    assert_eq!(
        untouched.resize_identity, None,
        "a rejected malformed capture must not enter the resize lifecycle"
    );

    // Migration 168: the lifecycle may only be entered by CLAIMING an
    // invocation, so the `birth_confirmed -> lift_applying` edge goes through
    // `begin_resize_invocation` rather than the generic transition.
    const INVOCATION: &str = "invocation-a";
    let claimed = repo
        .begin_resize_invocation(
            "resize",
            &permit.permit_id,
            permit.fencing_token,
            "pod-uid",
            INVOCATION,
        )
        .await
        .unwrap();
    let TransitionBuildPodResizeLifecycleResult::Transitioned(row) = claimed else {
        panic!("the first invocation must win the claim, got {claimed:?}");
    };
    assert_eq!(row.state, BuildPodPermitState::LiftApplying);
    assert_eq!(row.resize_invocation_id.as_deref(), Some(INVOCATION));

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
        (
            BuildPodPermitState::DropApplying,
            BuildPodPermitState::BirthConfirmed,
        ),
        (
            BuildPodPermitState::BirthConfirmed,
            BuildPodPermitState::Quarantined,
        ),
    ] {
        let outcome = repo
            .transition_resize_lifecycle(
                "resize",
                &permit.permit_id,
                permit.fencing_token,
                "pod-uid",
                Some(INVOCATION),
                from,
                to,
            )
            .await
            .unwrap();
        let TransitionBuildPodResizeLifecycleResult::Transitioned(row) = outcome else {
            panic!("{from:?} -> {to:?} must be a legal transition, got {outcome:?}");
        };
        // The returned row, not just the variant, has to carry the new state,
        // and the identity must survive the transition untouched.
        assert_eq!(row.state, to);
        assert_eq!(row.resize_identity, Some(identity.clone()));
        assert_eq!(
            repo.active("resize").await.unwrap().unwrap().state,
            to,
            "the durable row must agree with the returned row"
        );
    }
    // A matching UID/fence is not sufficient: stale observers must also echo
    // the actual durable lifecycle state.
    assert!(matches!(
        repo.transition_resize_lifecycle(
            "resize",
            &permit.permit_id,
            permit.fencing_token,
            "pod-uid",
            Some(INVOCATION),
            BuildPodPermitState::Lifted,
            BuildPodPermitState::DropRequired
        )
        .await
        .unwrap(),
        TransitionBuildPodResizeLifecycleResult::Rejected
    ));
    assert_eq!(
        repo.active("resize").await.unwrap().unwrap().state,
        BuildPodPermitState::Quarantined,
        "a stale expected state must not mutate the row"
    );
    assert!(matches!(
        repo.transition_resize_lifecycle(
            "resize",
            &permit.permit_id,
            permit.fencing_token + 1,
            "pod-uid",
            Some(INVOCATION),
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
            Some(INVOCATION),
            BuildPodPermitState::Quarantined,
            BuildPodPermitState::DropRequired
        )
        .await
        .unwrap(),
        TransitionBuildPodResizeLifecycleResult::Rejected
    ));
    // A freshly constructed repository — the restart case — must recover the
    // nonterminal row from the database alone. The still-`job_created`
    // `resize-malformed` permit is the negative control: it is active for
    // capacity but owes no resize work, so it must not appear here.
    let recovered = BuildPodPermitRepository::new(db)
        .list_nonterminal_resize()
        .await
        .unwrap();
    assert_eq!(
        recovered
            .iter()
            .map(|row| row.task_run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["resize"],
        "only nonterminal resize rows are recoverable after a restart"
    );
    assert_eq!(recovered[0].state, BuildPodPermitState::Quarantined);
    assert_eq!(recovered[0].resize_identity, Some(identity));
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

/// Seed one permit at `birth_confirmed` with a captured identity.
async fn birth_confirmed(db: &Database, task_run_id: &str, pod_uid: &str) -> (String, i64) {
    let repo = BuildPodPermitRepository::new(db.clone());
    let permit = match repo.acquire(task_run_id, 16).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    assert!(matches!(
        repo.bind_or_refresh_job_uid(
            task_run_id,
            &permit.permit_id,
            permit.fencing_token,
            &format!("job-{pod_uid}")
        )
        .await
        .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));
    let identity = BuildPodResizeIdentity {
        pod_namespace: "ns".into(),
        pod_name: format!("pod-{pod_uid}"),
        pod_uid: pod_uid.into(),
        launcher_container_name: "cgroup-launcher".into(),
        launcher_container_id: "containerd://launcher".into(),
        image_digest: "sha256:abc".into(),
        observed_launcher_protocol: "resize-v2".into(),
        effective_launcher_protocol: "resize-v2".into(),
        admitted_cpu_millicores: 4000,
    };
    assert!(matches!(
        repo.capture_resize_identity(
            task_run_id,
            &permit.permit_id,
            permit.fencing_token,
            &identity
        )
        .await
        .unwrap(),
        CaptureBuildPodResizeIdentityResult::Captured(_)
    ));
    (permit.permit_id, permit.fencing_token)
}

/// AC1. The invocation half of the fence exists, is written only on the edge
/// that starts a lift, and REJECTS a transition presented with a stale
/// invocation id — leaving the row byte-identical.
///
/// NAMED FAILING MUTATION: delete
/// `AND resize_invocation_id IS NOT DISTINCT FROM $6` from
/// `transition_resize_lifecycle`'s predicate, leaving the `pod_uid` fence in
/// place. The stale transition below then succeeds and this test fails.
///
/// Against REAL Postgres, deliberately: the compare-and-swap, migration 164's
/// transition trigger and migration 168's invocation write-window are Postgres
/// behaviours. A fake repository would assert this test's opinion of them.
#[tokio::test]
async fn a_stale_invocation_id_is_rejected_and_leaves_the_row_unchanged() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["fence-run"]).await;
    let repo = BuildPodPermitRepository::new(db.clone());
    let (permit_id, fence) = birth_confirmed(&db, "fence-run", "pod-fence").await;

    // Invocation A claims the lifecycle.
    let claimed = repo
        .begin_resize_invocation("fence-run", &permit_id, fence, "pod-fence", "invocation-a")
        .await
        .unwrap();
    let TransitionBuildPodResizeLifecycleResult::Transitioned(row) = claimed else {
        panic!("the claim must succeed, got {claimed:?}");
    };
    assert_eq!(row.resize_invocation_id.as_deref(), Some("invocation-a"));
    let before = repo.active("fence-run").await.unwrap().unwrap();

    // Invocation B presents a legal edge on the right Pod under the right
    // permit and fencing token. ONLY the invocation is wrong.
    let stale = repo
        .transition_resize_lifecycle(
            "fence-run",
            &permit_id,
            fence,
            "pod-fence",
            Some("invocation-b"),
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
        )
        .await
        .unwrap();
    assert!(
        matches!(stale, TransitionBuildPodResizeLifecycleResult::Rejected),
        "a stale invocation must be rejected even with a matching Pod UID: {stale:?}"
    );
    assert_eq!(
        repo.active("fence-run").await.unwrap().unwrap(),
        before,
        "a rejected transition must leave the row byte-identical"
    );

    // `None` is a fence value, not "do not check": a caller claiming the row
    // carries no invocation must lose against a row that carries one.
    let unfenced = repo
        .transition_resize_lifecycle(
            "fence-run",
            &permit_id,
            fence,
            "pod-fence",
            None,
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
        )
        .await
        .unwrap();
    assert!(
        matches!(unfenced, TransitionBuildPodResizeLifecycleResult::Rejected),
        "an absent invocation must not match a claimed row: {unfenced:?}"
    );

    // The owner still wins.
    assert!(matches!(
        repo.transition_resize_lifecycle(
            "fence-run",
            &permit_id,
            fence,
            "pod-fence",
            Some("invocation-a"),
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
        )
        .await
        .unwrap(),
        TransitionBuildPodResizeLifecycleResult::Transitioned(_)
    ));
}

/// AC2. Two invocations of ONE task run CAN be in flight simultaneously —
/// nothing in `djinn_agent::process`'s `'invocation: loop` serializes them, the
/// runner is `Arc`-shared behind `&self`, and its journal keeps a `HashSet` of
/// live invocation ids precisely because more than one can be live. A single-row
/// lifecycle cannot represent two simultaneous lifts, so the second claimant
/// must FAIL CLOSED.
///
/// Both claims race against real Postgres through a `Barrier`. Exactly one wins.
///
/// NAMED FAILING MUTATION: remove `AND state = 'birth_confirmed'` from
/// `begin_resize_invocation`'s predicate — the guard the answer rests on — and
/// both claims succeed, so the row ends up carrying the loser's invocation id
/// and this test fails.
#[tokio::test]
async fn only_one_of_two_concurrent_invocations_can_claim_the_lifecycle() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["concurrent-run"]).await;
    let (permit_id, fence) = birth_confirmed(&db, "concurrent-run", "pod-concurrent").await;

    let repo = Arc::new(BuildPodPermitRepository::new(db.clone()));
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for invocation in ["invocation-a", "invocation-b"] {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let permit_id = permit_id.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            (
                invocation,
                repo.begin_resize_invocation(
                    "concurrent-run",
                    &permit_id,
                    fence,
                    "pod-concurrent",
                    invocation,
                )
                .await
                .unwrap(),
            )
        }));
    }
    let mut winners = Vec::new();
    let mut losers = Vec::new();
    for handle in handles {
        match handle.await.unwrap() {
            (id, TransitionBuildPodResizeLifecycleResult::Transitioned(_)) => winners.push(id),
            (id, TransitionBuildPodResizeLifecycleResult::Rejected) => losers.push(id),
        }
    }
    assert_eq!(
        winners.len(),
        1,
        "exactly one invocation may own a single-row lifecycle; won: {winners:?}"
    );
    assert_eq!(losers.len(), 1, "the second claimant must fail closed");

    let row = repo.active("concurrent-run").await.unwrap().unwrap();
    assert_eq!(row.state, BuildPodPermitState::LiftApplying);
    assert_eq!(
        row.resize_invocation_id.as_deref(),
        Some(winners[0]),
        "the row must carry the WINNER's invocation, never the loser's"
    );
}

/// The write window is exactly one edge: `birth_confirmed -> lift_applying`.
/// Migration 168's trigger refuses to let any other transition re-key the row,
/// so a drop or a quarantine cannot silently change ownership underneath a
/// caller that is mid-sequence.
#[tokio::test]
async fn the_invocation_id_may_only_change_on_entry_to_lift_applying() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["window-run"]).await;
    let repo = BuildPodPermitRepository::new(db.clone());
    let (permit_id, fence) = birth_confirmed(&db, "window-run", "pod-window").await;
    repo.begin_resize_invocation(
        "window-run",
        &permit_id,
        fence,
        "pod-window",
        "invocation-a",
    )
    .await
    .unwrap();

    // The trigger, not the repository, is the authority. Reach past the
    // repository's own predicate and try to re-key on a non-lift edge.
    let direct = sqlx::query(
        "UPDATE build_pod_permits SET state = 'lifted', resize_invocation_id = 'invocation-b' \
         WHERE task_run_id = 'window-run'",
    )
    .execute(db.pool())
    .await;
    let error = direct.expect_err("the trigger must refuse a re-key off the lift edge");
    assert!(
        error
            .to_string()
            .contains("resize invocation id may only change on entry to lift_applying"),
        "unexpected error: {error}"
    );
    assert_eq!(
        repo.active("window-run")
            .await
            .unwrap()
            .unwrap()
            .resize_invocation_id
            .as_deref(),
        Some("invocation-a")
    );

    // And the next invocation DOES get to re-key, once the previous one's drop
    // has returned the row to `birth_confirmed`. Without that the lifecycle
    // could serve exactly one invocation per task run, ever.
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
        (
            BuildPodPermitState::DropApplying,
            BuildPodPermitState::BirthConfirmed,
        ),
    ] {
        assert!(matches!(
            repo.transition_resize_lifecycle(
                "window-run",
                &permit_id,
                fence,
                "pod-window",
                Some("invocation-a"),
                from,
                to,
            )
            .await
            .unwrap(),
            TransitionBuildPodResizeLifecycleResult::Transitioned(_)
        ));
    }
    let second = repo
        .begin_resize_invocation(
            "window-run",
            &permit_id,
            fence,
            "pod-window",
            "invocation-b",
        )
        .await
        .unwrap();
    let TransitionBuildPodResizeLifecycleResult::Transitioned(row) = second else {
        panic!("the next invocation must be able to claim, got {second:?}");
    };
    assert_eq!(row.resize_invocation_id.as_deref(), Some("invocation-b"));
}

/// Migration 170: every state change restamps `state_changed_at`, and nothing
/// else does.
///
/// This is the fact `task_run_resize_reconcile`'s strand predicate rests on.
/// Without it the reconciler cannot tell a row a live worker moved one second
/// ago from a row a dead worker abandoned an hour ago, and production on
/// 2026-08-01 showed what it does when it guesses: it resumes a drop that is
/// already under way, and the real driver's `release_lease` then answers
/// `LeaseUnavailable` for a drop that had succeeded.
///
/// NAMED FAILING MUTATION: delete
/// `IF NEW.state IS DISTINCT FROM OLD.state THEN NEW.state_changed_at := now();`
/// from migration 170's trigger body. The row stays 600 seconds old across a
/// real transition and the third assertion block fails.
///
/// NAMED FAILING MUTATION 2: stamp unconditionally (drop the `IS DISTINCT FROM`
/// guard) and the SECOND block fails: a write that changes no state would reset
/// the age, which is how a driver stuck re-attempting one drop would renew its
/// own grace forever and never be reconciled at all.
///
/// Against REAL Postgres, deliberately: the stamp is a trigger behaviour, and a
/// fake repository would assert this test's opinion of it.
#[tokio::test]
async fn a_state_change_restamps_the_row_age_and_nothing_else_does() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["age-run"]).await;
    let repo = BuildPodPermitRepository::new(db.clone());
    let (permit_id, fence) = birth_confirmed(&db, "age-run", "pod-age").await;

    let fresh = repo.active("age-run").await.unwrap().unwrap();
    assert!(
        fresh.state_age() < Duration::from_secs(5),
        "a row that just reached birth_confirmed must read as young, got {:?}",
        fresh.state_age()
    );

    // A write that changes no state. The age must survive it.
    repo.backdate_state_change_for_test("age-run", Duration::from_secs(600))
        .await
        .unwrap();
    let aged = repo.active("age-run").await.unwrap().unwrap();
    assert!(
        aged.state_age() >= Duration::from_secs(595),
        "a non-state write must not restamp the age, got {:?}",
        aged.state_age()
    );
    assert_eq!(
        aged.state, fresh.state,
        "precondition: the backdate changed no state"
    );

    // THE STAMP. A real lifecycle edge, through the production repository.
    assert!(matches!(
        repo.begin_resize_invocation("age-run", &permit_id, fence, "pod-age", "invocation-age")
            .await
            .unwrap(),
        TransitionBuildPodResizeLifecycleResult::Transitioned(_)
    ));
    let restamped = repo.active("age-run").await.unwrap().unwrap();
    assert_eq!(restamped.state, BuildPodPermitState::LiftApplying);
    assert!(
        restamped.state_age() < Duration::from_secs(5),
        "a state change must restamp the age; the row still reads {:?} old, so \
         the reconciler would treat a drop a live worker just started as \
         abandoned",
        restamped.state_age()
    );

    // And a caller cannot forge a young row into an old one across a state
    // change: the trigger overwrites whatever the statement supplied. Without
    // this the stamp would be advisory, and any future write path could hand
    // the reconciler a row that lies about its age.
    sqlx::query(
        "UPDATE build_pod_permits \
         SET state = 'lifted', state_changed_at = now() - interval '3 hours' \
         WHERE task_run_id = 'age-run'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let forged = repo.active("age-run").await.unwrap().unwrap();
    assert_eq!(forged.state, BuildPodPermitState::Lifted);
    assert!(
        forged.state_age() < Duration::from_secs(5),
        "the trigger, not the caller, owns the stamp on a state change; got {:?}",
        forged.state_age()
    );
}

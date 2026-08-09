//! Repository-level contract tests for attempt lifecycle and canonical ownership.

use djinn_core::models::{DirectDeliveryParkReason, ProposalBuildAttemptLifecycle};
use djinn_db::{
    AcquireProposalBuildAttemptLeaseInput, AcquireProposalBuildAttemptLeaseResult,
    ActivateProposalBuildAttemptInput, ActivateProposalBuildAttemptResult, Database,
    PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository, ReconcileAttemptBranchHeadInput,
    ReconcileAttemptBranchHeadResult, ReserveProposalBuildAttemptInput,
    ReserveProposalBuildAttemptResult, ResolveTaskActiveAttemptResult,
    RetireProposalBuildAttemptInput,
};

async fn db() -> Database {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE direct_delivery_epochs SET state = 'active', generation = 1")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) VALUES ('user', 900100001, 'user')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO projects (id, name, github_owner, github_repo) VALUES ('project', 'project', 'owner', 'repo')")
        .execute(db.pool()).await.unwrap();
    db
}

async fn proposal(db: &Database, id: &str, breakdown: Option<&str>) {
    sqlx::query("INSERT INTO proposals (id, short_id, title, build_breakdown_task_id) VALUES ($1, $2, 'proposal', $3)")
        .bind(id).bind(format!("short-{id}")).bind(breakdown).execute(db.pool()).await.unwrap();
}
async fn task(db: &Database, id: &str, epic: Option<&str>) {
    sqlx::query("INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ($1, 'project', $2, $3, 'task', '', '', '[]', '[]', '[]', 'user')")
        .bind(id).bind(format!("short-{id}")).bind(epic).execute(db.pool()).await.unwrap();
}
async fn reserve(repo: &ProposalBuildAttemptRepository, proposal_id: &str, id: &str) {
    assert!(matches!(
        repo.reserve(&ReserveProposalBuildAttemptInput {
            proposal_id: proposal_id.into(),
            proposal_short_id: format!("short-{proposal_id}"),
            build_attempt_id: id.into(),
            build_attempt_short_id: format!("short-{id}"),
            observed_base_sha: "base".into()
        })
        .await
        .unwrap(),
        ReserveProposalBuildAttemptResult::Reserved(_)
    ));
}
async fn activate(repo: &ProposalBuildAttemptRepository, id: &str) {
    assert!(matches!(
        repo.activate(&ActivateProposalBuildAttemptInput {
            build_attempt_id: id.into(),
            expected_lifecycle: ProposalBuildAttemptLifecycle::Reserved,
            expected_branch_head_sha: None,
            branch_head_sha: "base".into()
        })
        .await
        .unwrap(),
        ActivateProposalBuildAttemptResult::Activated(_)
    ));
}

#[tokio::test]
async fn lifecycle_cas_replay_regraduation_and_retirement_are_immutable() {
    let db = db().await;
    proposal(&db, "p", None).await;
    let repo = ProposalBuildAttemptRepository::new(db.clone());
    reserve(&repo, "p", "a1").await;
    assert!(matches!(
        repo.reserve(&ReserveProposalBuildAttemptInput {
            proposal_id: "p".into(),
            proposal_short_id: "short-p".into(),
            build_attempt_id: "a1".into(),
            build_attempt_short_id: "short-a1".into(),
            observed_base_sha: "base".into()
        })
        .await
        .unwrap(),
        ReserveProposalBuildAttemptResult::Replayed(_)
    ));
    activate(&repo, "a1").await;
    assert!(matches!(
        repo.activate(&ActivateProposalBuildAttemptInput {
            build_attempt_id: "a1".into(),
            expected_lifecycle: ProposalBuildAttemptLifecycle::Reserved,
            expected_branch_head_sha: None,
            branch_head_sha: "other".into()
        })
        .await
        .unwrap(),
        ActivateProposalBuildAttemptResult::Stale { .. }
    ));
    let initial_lease = AcquireProposalBuildAttemptLeaseInput {
        build_attempt_id: "a1".into(),
        owner_incarnation_id: "one".into(),
        expected_generation: 0,
        expires_at: "2099-01-01T00:00:00.000Z".into(),
    };
    assert!(matches!(
        repo.acquire_lease(&initial_lease).await.unwrap(),
        AcquireProposalBuildAttemptLeaseResult::Acquired(_)
    ));
    assert!(matches!(
        repo.acquire_lease(&initial_lease).await.unwrap(),
        AcquireProposalBuildAttemptLeaseResult::Replayed(ref lease) if lease.generation == 1
    ));
    assert!(matches!(
        repo.acquire_lease(&AcquireProposalBuildAttemptLeaseInput {
            build_attempt_id: "a1".into(),
            owner_incarnation_id: "two".into(),
            expected_generation: 1,
            expires_at: "2099-01-01T00:00:00.000Z".into()
        })
        .await
        .unwrap(),
        AcquireProposalBuildAttemptLeaseResult::Stale { .. }
    ));
    sqlx::query(
        "UPDATE proposal_build_attempt_leases SET expires_at = '2000-01-01T00:00:00.000Z' WHERE build_attempt_id = 'a1'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let takeover_lease = AcquireProposalBuildAttemptLeaseInput {
        build_attempt_id: "a1".into(),
        owner_incarnation_id: "three".into(),
        expected_generation: 1,
        expires_at: "2099-02-01T00:00:00.000Z".into(),
    };
    assert!(matches!(
        repo.acquire_lease(&takeover_lease).await.unwrap(),
        AcquireProposalBuildAttemptLeaseResult::Acquired(ref lease) if lease.generation == 2
    ));
    let lease_before_retry: (String, i64, String) = sqlx::query_as(
        "SELECT owner_incarnation_id, generation, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM proposal_build_attempt_leases WHERE build_attempt_id = 'a1'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(matches!(
        repo.acquire_lease(&takeover_lease).await.unwrap(),
        AcquireProposalBuildAttemptLeaseResult::Replayed(ref lease) if lease.generation == 2
    ));
    let lease_after_retry: (String, i64, String) = sqlx::query_as(
        "SELECT owner_incarnation_id, generation, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM proposal_build_attempt_leases WHERE build_attempt_id = 'a1'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(lease_after_retry, lease_before_retry);
    repo.persist_pr_identity(&PersistAttemptPrIdentityInput {
        build_attempt_id: "a1".into(),
        proposal_pr_number: 7,
        proposal_pr_url: "https://example.test/7".into(),
    })
    .await
    .unwrap();
    repo.retire(&RetireProposalBuildAttemptInput {
        build_attempt_id: "a1".into(),
    })
    .await
    .unwrap();
    assert!(matches!(
        repo.activate(&ActivateProposalBuildAttemptInput {
            build_attempt_id: "a1".into(),
            expected_lifecycle: ProposalBuildAttemptLifecycle::Retired,
            expected_branch_head_sha: Some("base".into()),
            branch_head_sha: "new".into()
        })
        .await
        .unwrap(),
        ActivateProposalBuildAttemptResult::Stale { .. }
    ));
    assert!(matches!(
        repo.reconcile_branch_head(&ReconcileAttemptBranchHeadInput {
            build_attempt_id: "a1".into(),
            expected_branch_head_sha: Some("base".into()),
            observed_branch_head_sha: "new".into()
        })
        .await
        .unwrap(),
        ReconcileAttemptBranchHeadResult::Stale { .. }
    ));
    assert!(matches!(
        repo.persist_pr_identity(&PersistAttemptPrIdentityInput {
            build_attempt_id: "a1".into(),
            proposal_pr_number: 8,
            proposal_pr_url: "https://example.test/8".into()
        })
        .await
        .unwrap(),
        djinn_db::PersistAttemptPrIdentityResult::Parked { .. }
    ));
    reserve(&repo, "p", "a2").await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_build_attempts WHERE proposal_id = 'p'"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        2
    );
}

#[tokio::test]
async fn reconciliation_parks_adopted_branch_and_resolver_routes_only_authorized_owners() {
    let db = db().await;
    proposal(&db, "p1", None).await;
    reserve(&ProposalBuildAttemptRepository::new(db.clone()), "p1", "a1").await;
    let repo = ProposalBuildAttemptRepository::new(db.clone());
    assert!(matches!(
        repo.reconcile_branch_head(&ReconcileAttemptBranchHeadInput {
            build_attempt_id: "a1".into(),
            expected_branch_head_sha: None,
            observed_branch_head_sha: "foreign".into()
        })
        .await
        .unwrap(),
        ReconcileAttemptBranchHeadResult::Parked {
            reason: DirectDeliveryParkReason::BranchIdentityMismatch,
            ..
        }
    ));
    let parked: Option<String> =
        sqlx::query_scalar("SELECT park_reason FROM proposal_build_attempts WHERE id = 'a1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(parked.as_deref(), Some("branch_identity_mismatch"));

    proposal(&db, "p2", None).await;
    sqlx::query("INSERT INTO epics (id, project_id, short_id, title, description, memory_refs, created_by_user_id, proposal_id) VALUES ('e', 'project', 'e', 'epic', '', '[]', 'user', 'p2')").execute(db.pool()).await.unwrap();
    task(&db, "ordinary", Some("e")).await;
    assert!(
        matches!(repo.resolve_task_active_attempt("ordinary").await.unwrap(), ResolveTaskActiveAttemptResult::NoActiveAttempt { proposal_id, .. } if proposal_id == "p2")
    );
    reserve(&repo, "p2", "a2").await;
    activate(&repo, "a2").await;
    assert!(matches!(
        repo.resolve_task_active_attempt("ordinary").await.unwrap(),
        ResolveTaskActiveAttemptResult::Resolved(_)
    ));
    task(&db, "none", None).await;
    assert!(matches!(
        repo.resolve_task_active_attempt("none").await.unwrap(),
        ResolveTaskActiveAttemptResult::NoProposalOwner { .. }
    ));
    task(&db, "breakdown", None).await;
    proposal(&db, "p3", Some("breakdown")).await;
    reserve(&repo, "p3", "a3").await;
    activate(&repo, "a3").await;
    assert!(
        matches!(repo.resolve_task_active_attempt("breakdown").await.unwrap(), ResolveTaskActiveAttemptResult::Resolved(ref value) if value.proposal_id == "p3")
    );
    proposal(&db, "p4", Some("breakdown")).await;
    assert!(matches!(
        repo.resolve_task_active_attempt("breakdown").await.unwrap(),
        ResolveTaskActiveAttemptResult::AmbiguousProposalOwner { .. }
    ));
}

#[tokio::test]
async fn capability_mismatches_reject_writes_before_attempt_mutation() {
    let db = db().await;
    proposal(&db, "p", None).await;
    let repo = ProposalBuildAttemptRepository::new(db.clone());
    sqlx::query("UPDATE direct_delivery_epochs SET state = 'disabled'")
        .execute(db.pool())
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM proposal_build_attempts")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(
        repo.activate(&ActivateProposalBuildAttemptInput {
            build_attempt_id: "x".into(),
            expected_lifecycle: ProposalBuildAttemptLifecycle::Reserved,
            expected_branch_head_sha: None,
            branch_head_sha: "head".into()
        })
        .await
        .is_err()
    );
    assert!(
        repo.acquire_lease(&AcquireProposalBuildAttemptLeaseInput {
            build_attempt_id: "x".into(),
            owner_incarnation_id: "owner".into(),
            expected_generation: 0,
            expires_at: "2099-01-01T00:00:00Z".into()
        })
        .await
        .is_err()
    );
    assert!(
        repo.reconcile_branch_head(&ReconcileAttemptBranchHeadInput {
            build_attempt_id: "x".into(),
            expected_branch_head_sha: None,
            observed_branch_head_sha: "head".into()
        })
        .await
        .is_err()
    );
    assert!(
        repo.persist_pr_identity(&PersistAttemptPrIdentityInput {
            build_attempt_id: "x".into(),
            proposal_pr_number: 1,
            proposal_pr_url: "https://example.test/1".into()
        })
        .await
        .is_err()
    );
    assert!(
        repo.retire(&RetireProposalBuildAttemptInput {
            build_attempt_id: "x".into()
        })
        .await
        .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proposal_build_attempts")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        before
    );
    sqlx::query(
        "ALTER TABLE direct_delivery_epochs DROP CONSTRAINT direct_delivery_epochs_state_check",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE direct_delivery_epochs SET state = 'unknown'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        repo.reserve(&ReserveProposalBuildAttemptInput {
            proposal_id: "p".into(),
            proposal_short_id: "short-p".into(),
            build_attempt_id: "unknown".into(),
            build_attempt_short_id: "unknown".into(),
            observed_base_sha: "base".into()
        })
        .await
        .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proposal_build_attempts")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        before
    );
}

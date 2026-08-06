//! Acceptance tests for the `nafu` wave-1 durable CI routing layer.
//!
//! The proposal's database acceptance row asks for: migration round trip;
//! unique evidence reservation; `reserved`/`calling` transitions; immutable
//! owner incarnation; reservation eligibility; monotonic budgets; owner-scoped
//! finalization; exclusive owner-handoff compare-and-set; exactly-once
//! terminalization; unique current-evidence Tier-2 lease; obsolete-route
//! suppression across reload.
//!
//! Two things every test here does deliberately:
//!
//! * **It asserts the side effect, not the return label.** A test that only
//!   checked `matches!(outcome, Resumed { .. })` would stay green if the body
//!   charged twice, or zero times. So the assertions are on the counter rows
//!   the mechanism WRITES: `budget_counts`, `owner_incarnation_id`,
//!   `terminal_outcome`, `pre_call_resumptions`.
//! * **Recovery tests go through a genuinely new `Database` handle**
//!   (`reopen_test`), not a second call on the live object. A restart that is
//!   only a second method call proves the opposite of durability.

use crate::database::Database;
use crate::repositories::ci_route_attempt::{
    CI_CALLING_RECOVERY_TIMEOUT_SECS, CI_HEAD_BUDGET_LIMIT, CI_SIGNATURE_BUDGET_LIMIT, CiAction,
    CiActionPhase, CiCallingRecovery, CiCallingRecoveryAuthority, CiCallingRecoveryReason,
    CiChargeOutcome, CiClass, CiDiagnosticReason, CiEvidenceIdentity, CiLane, CiOriginState,
    CiQuiescenceProof, CiReopenMode, CiReserveOutcome, CiReservedRecovery, CiRouteAttempt,
    CiRouteAttemptRepository, CiRouteOutcome, CiRouteReservation, CiTier2LeaseOutcome,
    CiTier2LeaseState, CiTier2Reason, CiTier2Resolution,
};
use crate::repositories::coordinator_incarnation::CoordinatorIncarnationRepository;
use crate::repositories::test_support::{UsageTestTaskSeed, make_project, seed_task_row};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    project_id: String,
    task_id: String,
}

async fn fixture() -> Fixture {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project = make_project(&db, std::path::Path::new("ci-route")).await;
    let task_id = seed_task_row(
        &db,
        UsageTestTaskSeed {
            project_id: &project.id,
            status: "pr_draft",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    Fixture {
        db,
        project_id: project.id,
        task_id,
    }
}

fn repo(db: &Database) -> CiRouteAttemptRepository {
    CiRouteAttemptRepository::new(db.clone())
}

/// A brand-new repository built from nothing but the connection string — the
/// only honest way to express "the process died and came back".
fn reopened(db: &Database) -> (Database, CiRouteAttemptRepository) {
    let dsn = db.test_dsn().expect("ephemeral database exposes a DSN");
    let handle = Database::reopen_test(&dsn).expect("reopen after simulated restart");
    let repository = CiRouteAttemptRepository::new(handle.clone());
    (handle, repository)
}

fn incarnation() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn pr_head_identity(run_id: i64, head: &str) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: 4242,
        pr_head_sha: head.to_owned(),
        run_id,
        run_head_sha: head.to_owned(),
        dequeue_id: None,
    }
}

fn merge_group_identity(run_id: i64, head: &str, dequeue: &str) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: 4242,
        pr_head_sha: head.to_owned(),
        run_id,
        run_head_sha: head.to_owned(),
        dequeue_id: Some(dequeue.to_owned()),
    }
}

/// Distinct evidence, one shared signature budget and one shared head budget —
/// which is the arrangement the ceilings exist to bound.
fn reservation(
    task_id: &str,
    key: &str,
    identity: CiEvidenceIdentity,
    fingerprint: &str,
) -> CiRouteReservation {
    let head_budget_key = format!("head:{}:{}", identity.pr_number, identity.pr_head_sha);
    let retry_budget_key = format!(
        "sig:{}:{}:{}:{fingerprint}",
        identity.lane.as_str(),
        identity.pr_number,
        identity.pr_head_sha
    );
    let origin_state = match identity.lane {
        CiLane::PrHead => CiOriginState::PrDraft,
        CiLane::MergeGroup => CiOriginState::PrReview,
    };
    let action = match identity.lane {
        CiLane::PrHead => CiAction::RerunRun,
        CiLane::MergeGroup => CiAction::Reenqueue,
    };
    CiRouteReservation {
        provider_action_key: key.to_owned(),
        identity,
        task_id: task_id.to_owned(),
        origin_state,
        class: CiClass::Inconclusive,
        action,
        transient_fingerprint: fingerprint.to_owned(),
        retry_budget_key,
        head_budget_key,
    }
}

fn unwrap_reserved(outcome: CiReserveOutcome) -> CiRouteAttempt {
    match outcome {
        CiReserveOutcome::Reserved(attempt) => *attempt,
        other => panic!("expected a fresh reservation, got {other:?}"),
    }
}

/// Age a row's `reserved_at`/`calling_at` backwards so an eligibility floor is
/// crossed without the test sleeping. The clock being moved is the DATABASE's,
/// which is the same clock the eligibility predicate reads.
async fn age_reserved(db: &Database, key: &str, seconds: i64) {
    sqlx::query("UPDATE ci_route_attempts SET reserved_at = now() - make_interval(secs => $2::double precision) WHERE provider_action_key = $1")
        .bind(key)
        .bind(seconds as f64)
        .execute(db.pool())
        .await
        .expect("age reserved_at");
}

async fn age_calling(db: &Database, key: &str, seconds: i64) {
    sqlx::query("UPDATE ci_route_attempts SET calling_at = now() - make_interval(secs => $2::double precision) WHERE provider_action_key = $1")
        .bind(key)
        .bind(seconds as f64)
        .execute(db.pool())
        .await
        .expect("age calling_at");
}

/// Register an incarnation that has completed the full graceful drain, so a
/// handoff test is exercising the real predicate rather than a bare boolean.
async fn drained_incarnation(db: &Database) -> String {
    let id = incarnation();
    let repository = CoordinatorIncarnationRepository::new(db.clone());
    repository
        .register(&id)
        .await
        .expect("register incarnation");
    assert!(repository.mark_draining(&id).await.expect("mark draining"));
    assert!(
        repository
            .mark_provider_actions_drained(&id)
            .await
            .expect("mark drained")
    );
    id
}

fn authority(
    former: &str,
    recovering: &str,
    proof: CiQuiescenceProof,
    lock: bool,
) -> CiCallingRecoveryAuthority {
    CiCallingRecoveryAuthority {
        recovering_incarnation: recovering.to_owned(),
        former_owner_incarnation: former.to_owned(),
        holds_exclusive_lock: lock,
        quiescence_proof: proof,
        calling_recovery_timeout_secs: CI_CALLING_RECOVERY_TIMEOUT_SECS,
    }
}

// ---------------------------------------------------------------------------
// Migration round trip
// ---------------------------------------------------------------------------

/// Migration 191 round trip: every column the repository binds survives a
/// write and a read back through a *different* connection pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_round_trips_every_route_attempt_field() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = merge_group_identity(9001, "headsha-round-trip", "dequeue-77");
    let input = reservation(&f.task_id, "key-round-trip", identity.clone(), "fp-a");
    let reserved = unwrap_reserved(repository.reserve(&input).await.unwrap());

    assert_eq!(reserved.action_phase, CiActionPhase::Reserved);
    assert!(reserved.terminal_outcome.is_none());
    assert!(reserved.owner_incarnation_id.is_none());
    assert!(!reserved.reserved_at.is_empty());

    // Read back through a genuinely new handle: the row is on disk, not in an
    // object we are still holding.
    let (handle, restarted) = reopened(&f.db);
    let loaded = restarted
        .get("key-round-trip")
        .await
        .unwrap()
        .expect("row survives reload");

    assert_eq!(loaded.identity, identity);
    assert_eq!(loaded.task_id, f.task_id);
    assert_eq!(loaded.origin_state, CiOriginState::PrReview);
    assert_eq!(loaded.class, CiClass::Inconclusive);
    assert_eq!(loaded.action, CiAction::Reenqueue);
    assert_eq!(loaded.transient_fingerprint, "fp-a");
    assert_eq!(loaded.retry_budget_key, input.retry_budget_key);
    assert_eq!(loaded.head_budget_key, input.head_budget_key);
    assert_eq!(loaded.pre_call_resumptions, 0);
    assert!(loaded.charged_signature_count.is_none());
    assert!(loaded.tier2_lease_id.is_none());

    // And the two additive coordinator-incarnation drain columns round trip.
    let drained = drained_incarnation(&f.db).await;
    let lease = CoordinatorIncarnationRepository::new(handle.clone())
        .get(&drained)
        .await
        .unwrap()
        .expect("incarnation survives reload");
    assert!(lease.draining_at.is_some());
    assert!(lease.provider_actions_drained_at.is_some());
}

/// `mark_provider_actions_drained` refuses an incarnation that never entered
/// `draining`. Otherwise a caller could mint the exclusion proof the next
/// incarnation relies on without ever closing action admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_proof_requires_admission_to_have_closed_first() {
    let f = fixture().await;
    let repository = CoordinatorIncarnationRepository::new(f.db.clone());
    let id = incarnation();
    repository.register(&id).await.unwrap();

    assert!(
        !repository.mark_provider_actions_drained(&id).await.unwrap(),
        "drain proof must not be writable before draining starts"
    );
    let row = repository.get(&id).await.unwrap().unwrap();
    assert!(row.provider_actions_drained_at.is_none());

    assert!(repository.mark_draining(&id).await.unwrap());
    assert!(repository.mark_provider_actions_drained(&id).await.unwrap());
    // Both are write-once.
    assert!(!repository.mark_draining(&id).await.unwrap());
    assert!(!repository.mark_provider_actions_drained(&id).await.unwrap());
}

// ---------------------------------------------------------------------------
// Unique evidence reservation
// ---------------------------------------------------------------------------

/// A duplicate poll for the same evidence identity gets the existing row, not
/// a second one — and critically, not a second charge opportunity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_poll_reserves_one_row_for_one_evidence_identity() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let input = reservation(
        &f.task_id,
        "key-dup",
        pr_head_identity(1, "headsha-dup"),
        "fp-a",
    );

    let first = repository.reserve(&input).await.unwrap();
    assert!(matches!(first, CiReserveOutcome::Reserved(_)));

    // A second poller, and then a restarted one, both collide on the key.
    let second = repository.reserve(&input).await.unwrap();
    let (_handle, restarted) = reopened(&f.db);
    let third = restarted.reserve(&input).await.unwrap();
    assert!(matches!(second, CiReserveOutcome::AlreadyPresent(_)));
    assert!(matches!(third, CiReserveOutcome::AlreadyPresent(_)));

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_route_attempts")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1, "one evidence identity must own exactly one row");

    // Reserving never charges. Only `calling` does.
    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(counts.signature, 0);
    assert_eq!(counts.head, 0);
}

// ---------------------------------------------------------------------------
// reserved -> calling, and owner identity
// ---------------------------------------------------------------------------

/// The happy path, asserted on state rather than on the returned variant: one
/// winner, one charge on each budget, and an owner recorded on the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_advances_to_calling_once_and_charges_both_budgets() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-happy");
    let input = reservation(&f.task_id, "key-happy", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    let owner = incarnation();
    let charged = repository
        .charge_and_begin_calling("key-happy", &owner, &identity)
        .await
        .unwrap();
    let CiChargeOutcome::Charged { attempt, counts } = charged else {
        panic!("expected a charge, got {charged:?}");
    };
    assert_eq!(attempt.action_phase, CiActionPhase::Calling);
    assert_eq!(
        attempt.owner_incarnation_id.as_deref(),
        Some(owner.as_str())
    );
    assert!(attempt.calling_at.is_some());
    assert_eq!(counts.signature, 1);
    assert_eq!(counts.head, 1);
    assert_eq!(attempt.charged_signature_count, Some(1));
    assert_eq!(attempt.charged_head_count, Some(1));

    // A second caller — a duplicate poll — finds the row already advanced and
    // must not charge again.
    let loser = incarnation();
    let again = repository
        .charge_and_begin_calling("key-happy", &loser, &identity)
        .await
        .unwrap();
    assert!(matches!(again, CiChargeOutcome::NotReserved(_)));

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "a losing caller must not charge"
    );
    let row = repository.get("key-happy").await.unwrap().unwrap();
    assert_eq!(
        row.owner_incarnation_id.as_deref(),
        Some(owner.as_str()),
        "the owner incarnation is immutable except through owner handoff"
    );
}

/// A PR head that moved between the reservation and the call: no charge, no
/// lease, terminal `superseded_pre_call`, and the defeating evidence recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_change_before_calling_supersedes_uncharged() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-old");
    let input = reservation(&f.task_id, "key-stale", identity, "fp-a");
    repository.reserve(&input).await.unwrap();

    let observed = pr_head_identity(2, "headsha-new");
    let outcome = repository
        .charge_and_begin_calling("key-stale", &incarnation(), &observed)
        .await
        .unwrap();
    let CiChargeOutcome::SupersededPreCall(attempt) = outcome else {
        panic!("expected supersession, got {outcome:?}");
    };

    assert_eq!(
        attempt.terminal_outcome,
        Some(CiRouteOutcome::SupersededPreCall)
    );
    assert!(attempt.owner_incarnation_id.is_none(), "no call ownership");
    assert!(attempt.charged_signature_count.is_none(), "no charge");
    assert!(!attempt.holds_open_tier2_lease(), "no Tier-2 lease");
    let evidence = attempt
        .superseded_by_evidence
        .expect("the defeating evidence is recorded");
    assert!(evidence.contains("headsha-new"));

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!((counts.signature, counts.head), (0, 0));
}

// ---------------------------------------------------------------------------
// Pre-call recovery — the core of the wave
// ---------------------------------------------------------------------------

/// A reservation younger than the timeout is not recoverable, and recovery
/// leaves it exactly as it found it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reservation_below_the_timeout_is_not_eligible_for_recovery() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-young");
    let input = reservation(&f.task_id, "key-young", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    let outcome = repository
        .recover_reserved("key-young", &identity, &incarnation(), 300, "lease-young")
        .await
        .unwrap();
    let CiReservedRecovery::NotEligible(attempt) = outcome else {
        panic!("expected ineligibility, got {outcome:?}");
    };
    assert_eq!(attempt.action_phase, CiActionPhase::Reserved);
    assert_eq!(attempt.pre_call_resumptions, 0);

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!((counts.signature, counts.head), (0, 0));
}

/// **The wave's central invariant.** Three recoveries — two of them through
/// fresh `Database` handles standing in for restarts — race for a still-current
/// `reserved` row with budget remaining. Exactly one wins, the row is resumed
/// rather than replaced, each budget is charged exactly once, and no Tier-2
/// lease or Lead session is created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_recovery_of_current_reserved_row_resumes_one_winner_and_charges_once() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-resume");
    let input = reservation(&f.task_id, "key-resume", identity.clone(), "fp-a");
    let reserved = unwrap_reserved(repository.reserve(&input).await.unwrap());
    age_reserved(&f.db, "key-resume", 600).await;

    let (_h1, restart_one) = reopened(&f.db);
    let (_h2, restart_two) = reopened(&f.db);
    let winner_incarnation = incarnation();

    let first = restart_one
        .recover_reserved(
            "key-resume",
            &identity,
            &winner_incarnation,
            60,
            "lease-resume",
        )
        .await
        .unwrap();
    let second = restart_two
        .recover_reserved("key-resume", &identity, &incarnation(), 60, "lease-resume")
        .await
        .unwrap();
    let third = repository
        .recover_reserved("key-resume", &identity, &incarnation(), 60, "lease-resume")
        .await
        .unwrap();

    let CiReservedRecovery::Resumed { attempt, counts } = first else {
        panic!("first recovery must resume, got {first:?}");
    };
    assert_eq!(
        attempt.provider_action_key, reserved.provider_action_key,
        "resumption advances the SAME row, it does not create a new one"
    );
    assert_eq!((counts.signature, counts.head), (1, 1));
    assert!(matches!(second, CiReservedRecovery::NotEligible(_)));
    assert!(matches!(third, CiReservedRecovery::NotEligible(_)));

    let final_counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (final_counts.signature, final_counts.head),
        (1, 1),
        "three recoveries must charge exactly once"
    );

    let row = repository.get("key-resume").await.unwrap().unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);
    assert_eq!(
        row.owner_incarnation_id.as_deref(),
        Some(winner_incarnation.as_str()),
        "the single winner owns the provider-call episode"
    );
    assert_eq!(row.pre_call_resumptions, 1);
    assert!(row.tier2_lease_id.is_none(), "no Tier-2 lease");
    assert!(row.lead_session_id.is_none(), "no Lead session");
    assert_eq!(
        rows_in_state(&f.db, "calling").await,
        1,
        "exactly one provider-call episode is authorized"
    );
}

/// The same invariant under genuine concurrency rather than sequencing.
///
/// The previous test runs its recoveries one after another, so it proves the
/// phase check. This one launches them *simultaneously* on separate pools, so
/// the only thing standing between them is the row lock inside the
/// transaction. If that lock were missing, both would read `reserved`, both
/// would charge, and the counter would read 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recoveries_of_one_reserved_row_charge_exactly_once() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-race-resume");
    let input = reservation(&f.task_id, "key-race-resume", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    age_reserved(&f.db, "key-race-resume", 600).await;

    let (_h1, a) = reopened(&f.db);
    let (_h2, b) = reopened(&f.db);
    let (_h3, c) = reopened(&f.db);
    let id_a = identity.clone();
    let id_b = identity.clone();
    let id_c = identity.clone();
    let inc_a = incarnation();
    let inc_b = incarnation();
    let inc_c = incarnation();

    let (ra, rb, rc) = tokio::join!(
        a.recover_reserved("key-race-resume", &id_a, &inc_a, 60, "lease-race"),
        b.recover_reserved("key-race-resume", &id_b, &inc_b, 60, "lease-race"),
        c.recover_reserved("key-race-resume", &id_c, &inc_c, 60, "lease-race"),
    );

    let resumed = [ra.unwrap(), rb.unwrap(), rc.unwrap()]
        .into_iter()
        .filter(|outcome| matches!(outcome, CiReservedRecovery::Resumed { .. }))
        .count();
    assert_eq!(resumed, 1, "exactly one concurrent recovery may resume");

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "three concurrent recoveries must charge exactly once"
    );
    let row = repository.get("key-race-resume").await.unwrap().unwrap();
    assert_eq!(row.pre_call_resumptions, 1);
    assert_eq!(rows_in_state(&f.db, "calling").await, 1);
}

/// Concurrent duplicate polls for one evidence identity: one row, one charge
/// opportunity. Without the pre-insert lock both would insert and one would
/// surface a raw unique-violation error instead of `AlreadyPresent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reservations_of_one_identity_yield_one_row() {
    let f = fixture().await;
    let input = reservation(
        &f.task_id,
        "key-race-reserve",
        pr_head_identity(1, "headsha-race-reserve"),
        "fp-a",
    );
    let (_h1, a) = reopened(&f.db);
    let (_h2, b) = reopened(&f.db);

    let (ra, rb) = tokio::join!(a.reserve(&input), b.reserve(&input));
    let outcomes = [ra.unwrap(), rb.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, CiReserveOutcome::Reserved(_)))
            .count(),
        1,
        "exactly one concurrent poll creates the row"
    );

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_route_attempts")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

/// Recovery of an obsolete `reserved` row: terminal, uncharged, no lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_of_obsolete_reserved_row_supersedes_without_cost() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let input = reservation(
        &f.task_id,
        "key-obsolete",
        pr_head_identity(1, "headsha-old"),
        "fp-a",
    );
    repository.reserve(&input).await.unwrap();
    age_reserved(&f.db, "key-obsolete", 600).await;

    let (_handle, restarted) = reopened(&f.db);
    let outcome = restarted
        .recover_reserved(
            "key-obsolete",
            &pr_head_identity(2, "headsha-new"),
            &incarnation(),
            60,
            "lease-obsolete",
        )
        .await
        .unwrap();
    let CiReservedRecovery::SupersededPreCall(attempt) = outcome else {
        panic!("expected supersession, got {outcome:?}");
    };
    assert_eq!(
        attempt.terminal_outcome,
        Some(CiRouteOutcome::SupersededPreCall)
    );
    assert!(attempt.charged_signature_count.is_none());
    assert!(!attempt.holds_open_tier2_lease());

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!((counts.signature, counts.head), (0, 0));
    assert_eq!(rows_in_state(&f.db, "calling").await, 0);
}

/// Recovery of a still-current `reserved` row whose budget is spent: no charge
/// and no call, but at most one Tier-2 lease — however many recoveries run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_of_exhausted_reserved_row_routes_to_one_tier_two_lease() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(3, "headsha-exhausted");
    let input = reservation(&f.task_id, "key-exhausted", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    age_reserved(&f.db, "key-exhausted", 600).await;

    // Spend the signature budget through two other, distinct runs.
    spend_signature_budget(&f, &identity, "fp-a").await;
    let before = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(before.signature, CI_SIGNATURE_BUDGET_LIMIT);

    let (_handle, restarted) = reopened(&f.db);
    let first = restarted
        .recover_reserved(
            "key-exhausted",
            &identity,
            &incarnation(),
            60,
            "lease-exhausted",
        )
        .await
        .unwrap();
    let CiReservedRecovery::RetryExhausted {
        attempt,
        counts,
        tier2_lease_id,
    } = first
    else {
        panic!("expected retry exhaustion, got {first:?}");
    };
    assert_eq!(counts.signature, CI_SIGNATURE_BUDGET_LIMIT);
    assert!(attempt.retry_exhausted_at.is_some());
    assert!(
        tier2_lease_id.is_some(),
        "exhaustion opens the Tier-2 lease"
    );
    assert_eq!(
        attempt.tier2_lease_reason,
        Some(CiTier2Reason::RetryExhausted)
    );
    assert!(attempt.owner_incarnation_id.is_none(), "no call ownership");

    // A second recovery must not open a second lease or charge anything.
    let second = repository
        .recover_reserved(
            "key-exhausted",
            &identity,
            &incarnation(),
            60,
            "lease-exhausted",
        )
        .await
        .unwrap();
    let CiReservedRecovery::RetryExhausted { tier2_lease_id, .. } = second else {
        panic!("expected retry exhaustion again");
    };
    assert!(tier2_lease_id.is_none(), "at most one Tier-2 lease");

    let after = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (after.signature, after.head),
        (before.signature, before.head),
        "exhausted recovery charges nothing"
    );
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ci_route_attempts WHERE tier2_lease_state = 'open'",
    )
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(open, 1);
}

// ---------------------------------------------------------------------------
// Monotonic budgets
// ---------------------------------------------------------------------------

/// The signature ceiling: two charged actions per retry-budget key, then the
/// third distinct run is refused a reservation entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signature_budget_stops_at_two_and_never_decrements() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-sig");
    let input = reservation(&f.task_id, "key-sig-1", identity.clone(), "fp-a");

    for run in 1..=CI_SIGNATURE_BUDGET_LIMIT {
        let id = pr_head_identity(run, "headsha-sig");
        let key = format!("key-sig-{run}");
        let res = reservation(&f.task_id, &key, id.clone(), "fp-a");
        assert!(matches!(
            repository.reserve(&res).await.unwrap(),
            CiReserveOutcome::Reserved(_)
        ));
        let charged = repository
            .charge_and_begin_calling(&key, &incarnation(), &id)
            .await
            .unwrap();
        assert!(matches!(charged, CiChargeOutcome::Charged { .. }));
        // The provider answered with an error; the slot is NOT returned.
        // (`ignored` is not the owner, so this finalization writes nothing —
        // the point here is only that no code path exists that decrements.)
        assert!(
            !repository
                .finalize_calling(&key, "ignored", CiRouteOutcome::ActionFailed, None)
                .await
                .unwrap()
        );
    }

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(counts.signature, CI_SIGNATURE_BUDGET_LIMIT);
    assert!(counts.is_exhausted());

    let third = reservation(
        &f.task_id,
        "key-sig-3",
        pr_head_identity(3, "headsha-sig"),
        "fp-a",
    );
    let outcome = repository.reserve(&third).await.unwrap();
    let CiReserveOutcome::BudgetExhausted(reported) = outcome else {
        panic!("expected exhaustion, got {outcome:?}");
    };
    assert_eq!(reported.signature, CI_SIGNATURE_BUDGET_LIMIT);
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ci_route_attempts WHERE provider_action_key = 'key-sig-3'",
    )
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(rows, 0, "an exhausted budget reserves nothing");
}

/// The head ceiling is shared **across both lanes**: a changed fingerprint
/// starts a fresh signature budget but still counts against the same head, and
/// the fourth charge closes it for every lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_budget_is_shared_across_lanes_and_survives_reload() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let head = "headsha-shared";

    // Two PR-head charges under fingerprint A, then two merge-group charges
    // under fingerprint B: four distinct signature slots, one head budget.
    let plan: [(CiEvidenceIdentity, &str); 4] = [
        (pr_head_identity(1, head), "fp-a"),
        (pr_head_identity(2, head), "fp-a"),
        (merge_group_identity(3, head, "dq-1"), "fp-b"),
        (merge_group_identity(4, head, "dq-2"), "fp-b"),
    ];
    for (index, (identity, fingerprint)) in plan.iter().enumerate() {
        let key = format!("key-head-{index}");
        let res = reservation(&f.task_id, &key, identity.clone(), fingerprint);
        assert!(
            matches!(
                repository.reserve(&res).await.unwrap(),
                CiReserveOutcome::Reserved(_)
            ),
            "charge {index} must reserve"
        );
        assert!(matches!(
            repository
                .charge_and_begin_calling(&key, &incarnation(), identity)
                .await
                .unwrap(),
            CiChargeOutcome::Charged { .. }
        ));
    }

    let head_key = format!("head:4242:{head}");
    let (_handle, restarted) = reopened(&f.db);
    let counts = restarted
        .budget_counts("sig:pr_head:4242:headsha-shared:fp-a", &head_key)
        .await
        .unwrap();
    assert_eq!(
        counts.head, CI_HEAD_BUDGET_LIMIT,
        "the head budget counts both lanes and survives a restart"
    );

    // A fifth attempt on a brand-new fingerprint has a fresh signature budget
    // and is still refused by the head ceiling.
    let fresh = reservation(
        &f.task_id,
        "key-head-4",
        pr_head_identity(5, head),
        "fp-fresh",
    );
    let counts = restarted
        .budget_counts(&fresh.retry_budget_key, &fresh.head_budget_key)
        .await
        .unwrap();
    assert_eq!(counts.signature, 0, "a changed fingerprint starts fresh");
    let outcome = restarted.reserve(&fresh).await.unwrap();
    assert!(matches!(outcome, CiReserveOutcome::BudgetExhausted(_)));

    // A changed PR head starts both budgets over.
    let new_head = reservation(
        &f.task_id,
        "key-newhead",
        pr_head_identity(6, "headsha-moved"),
        "fp-a",
    );
    assert!(matches!(
        restarted.reserve(&new_head).await.unwrap(),
        CiReserveOutcome::Reserved(_)
    ));
}

// ---------------------------------------------------------------------------
// Owner-scoped finalization and exactly-once terminalization
// ---------------------------------------------------------------------------

/// Finalization is fenced to the exact owner: a former owner's late result
/// writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalization_is_scoped_to_the_exact_calling_owner() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-final");
    let input = reservation(&f.task_id, "key-final", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling("key-final", &owner, &identity)
        .await
        .unwrap();

    let impostor = incarnation();
    assert!(
        !repository
            .finalize_calling("key-final", &impostor, CiRouteOutcome::Retriggered, None)
            .await
            .unwrap(),
        "a non-owner cannot finalize"
    );
    let row = repository.get("key-final").await.unwrap().unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);

    assert!(
        repository
            .finalize_calling("key-final", &owner, CiRouteOutcome::Retriggered, None)
            .await
            .unwrap()
    );
    let row = repository.get("key-final").await.unwrap().unwrap();
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Retriggered));

    // Exactly-once: the owner's own retry finds nothing left to write.
    assert!(
        !repository
            .finalize_calling("key-final", &owner, CiRouteOutcome::ActionFailed, None)
            .await
            .unwrap()
    );
    let row = repository.get("key-final").await.unwrap().unwrap();
    assert_eq!(
        row.terminal_outcome,
        Some(CiRouteOutcome::Retriggered),
        "a terminal outcome is never rewritten"
    );
}

/// `terminalize` writes once and reports honestly to every later caller,
/// including one that arrives after a simulated restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminalization_happens_exactly_once_across_reload() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let input = reservation(
        &f.task_id,
        "key-term",
        pr_head_identity(1, "headsha-term"),
        "fp-a",
    );
    repository.reserve(&input).await.unwrap();

    assert!(
        repository
            .terminalize("key-term", CiRouteOutcome::Held, None)
            .await
            .unwrap()
    );
    assert!(
        !repository
            .terminalize("key-term", CiRouteOutcome::Held, None)
            .await
            .unwrap()
    );

    let (_handle, restarted) = reopened(&f.db);
    assert!(
        !restarted
            .terminalize("key-term", CiRouteOutcome::DiagnosticReopened, None)
            .await
            .unwrap(),
        "a startup sweep must not re-close a terminal row"
    );
    let row = restarted.get("key-term").await.unwrap().unwrap();
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Held));
    assert!(row.terminalized_at.is_some());
}

// ---------------------------------------------------------------------------
// Calling owner handoff
// ---------------------------------------------------------------------------

/// A live `calling` owner is untouchable. Every illegal recovery shape — no
/// lock, no quiescence proof, timeout not elapsed, wrong former owner — leaves
/// the row unchanged and opens no Tier-2 lease, and every one of them is
/// recorded so the deferral is auditable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_calling_owner_is_never_recovered() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-live");
    let input = reservation(&f.task_id, "key-live", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling("key-live", &owner, &identity)
        .await
        .unwrap();

    let recovering = incarnation();
    let (_handle, sweeper) = reopened(&f.db);

    // 1. A periodic sweep that does not hold the exclusive lock.
    let deferred = sweeper
        .recover_calling_owner(
            "key-live",
            Some(&identity),
            &authority(
                &owner,
                &recovering,
                CiQuiescenceProof::ProcessTerminated,
                false,
            ),
            "lease-live",
        )
        .await
        .unwrap();
    assert_deferred(&deferred, CiCallingRecoveryReason::LockNotHeld);

    // 2. Lock held, but the owner is alive: no quiescence proof exists.
    let deferred = sweeper
        .recover_calling_owner(
            "key-live",
            Some(&identity),
            &authority(&owner, &recovering, CiQuiescenceProof::None, true),
            "lease-live",
        )
        .await
        .unwrap();
    assert_deferred(&deferred, CiCallingRecoveryReason::LiveOwnerDeferred);

    // 3. A *claimed* graceful drain that the former incarnation never recorded
    //    is not a proof. This is the case that makes the drain column load
    //    bearing rather than decorative.
    CoordinatorIncarnationRepository::new(f.db.clone())
        .register(&owner)
        .await
        .unwrap();
    let deferred = sweeper
        .recover_calling_owner(
            "key-live",
            Some(&identity),
            &authority(&owner, &recovering, CiQuiescenceProof::GracefulDrain, true),
            "lease-live",
        )
        .await
        .unwrap();
    assert_deferred(&deferred, CiCallingRecoveryReason::LiveOwnerDeferred);

    // 4. Process death is proven, but `calling_at` has not aged past the floor.
    let deferred = sweeper
        .recover_calling_owner(
            "key-live",
            Some(&identity),
            &authority(
                &owner,
                &recovering,
                CiQuiescenceProof::ProcessTerminated,
                true,
            ),
            "lease-live",
        )
        .await
        .unwrap();
    assert_deferred(&deferred, CiCallingRecoveryReason::TimeoutNotElapsed);

    // 5. Aged out, but fenced to the wrong former owner.
    age_calling(&f.db, "key-live", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;
    let deferred = sweeper
        .recover_calling_owner(
            "key-live",
            Some(&identity),
            &authority(
                &incarnation(),
                &recovering,
                CiQuiescenceProof::ProcessTerminated,
                true,
            ),
            "lease-live",
        )
        .await
        .unwrap();
    assert_deferred(&deferred, CiCallingRecoveryReason::OwnerMismatch);

    // The row is exactly as the owner left it, and nothing escalated.
    let row = repository.get("key-live").await.unwrap().unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);
    assert_eq!(row.owner_incarnation_id.as_deref(), Some(owner.as_str()));
    assert!(row.tier2_lease_id.is_none());
    assert!(row.lead_session_id.is_none());

    // Five deferrals, five audit rows, none of them claiming a win.
    let audit = repository.calling_recovery_audit("key-live").await.unwrap();
    assert_eq!(audit.len(), 5);
    assert!(audit.iter().all(|r| !r.cas_won));

    // And the owner can still finalize, because nothing took its row.
    assert!(
        repository
            .finalize_calling("key-live", &owner, CiRouteOutcome::Retriggered, None)
            .await
            .unwrap()
    );
}

/// A legal startup handoff: exclusive lock, proven drain, aged out, exact
/// former owner. It runs twice; the second run loses because the row is no
/// longer `calling`. One handoff, one retained charge, no provider replay, and
/// at most one Tier-2 lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiescent_owner_handoff_recovers_calling_once() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-handoff");
    let input = reservation(&f.task_id, "key-handoff", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    // The former owner completed the full graceful drain before releasing the
    // advisory lock.
    let former = drained_incarnation(&f.db).await;
    repository
        .charge_and_begin_calling("key-handoff", &former, &identity)
        .await
        .unwrap();
    age_calling(&f.db, "key-handoff", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;

    let recovering = incarnation();
    let (_handle, restarted) = reopened(&f.db);
    let first = restarted
        .recover_calling_owner(
            "key-handoff",
            Some(&identity),
            &authority(&former, &recovering, CiQuiescenceProof::GracefulDrain, true),
            "lease-handoff",
        )
        .await
        .unwrap();
    let CiCallingRecovery::Recovered {
        attempt,
        outcome,
        tier2_lease_id,
    } = first
    else {
        panic!("expected a handoff, got {first:?}");
    };
    assert_eq!(outcome, CiRouteOutcome::OutcomeUnknown);
    assert_eq!(
        attempt.owner_incarnation_id.as_deref(),
        Some(recovering.as_str())
    );
    assert_eq!(
        attempt.charged_signature_count,
        Some(1),
        "the handoff retains the charge"
    );
    assert!(tier2_lease_id.is_some());

    // A second startup sweep finds nothing to take.
    let second = restarted
        .recover_calling_owner(
            "key-handoff",
            Some(&identity),
            &authority(
                &former,
                &incarnation(),
                CiQuiescenceProof::GracefulDrain,
                true,
            ),
            "lease-handoff",
        )
        .await
        .unwrap();
    assert_deferred(&second, CiCallingRecoveryReason::NotCalling);

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "two handoffs must not double-charge"
    );
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ci_route_attempts WHERE tier2_lease_state = 'open'",
    )
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(open, 1, "at most one Tier-2 lease");

    let audit = repository
        .calling_recovery_audit("key-handoff")
        .await
        .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(
        audit[0].recovery_reason,
        CiCallingRecoveryReason::StartupOwnerHandoff
    );
    assert!(audit[0].cas_won);
    assert_eq!(
        audit[0].former_owner_incarnation.as_deref(),
        Some(former.as_str())
    );
    assert_eq!(audit[0].quiescence_proof, CiQuiescenceProof::GracefulDrain);
    assert!(!audit[1].cas_won);
}

/// A provider result that committed before the handoff wins. Recovery cannot
/// overwrite it and records the loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_finalizer_wins_the_owner_handoff_race() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-race");
    let input = reservation(&f.task_id, "key-race", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let former = drained_incarnation(&f.db).await;
    repository
        .charge_and_begin_calling("key-race", &former, &identity)
        .await
        .unwrap();
    age_calling(&f.db, "key-race", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;

    // The old owner's finalizer commits first.
    assert!(
        repository
            .finalize_calling("key-race", &former, CiRouteOutcome::Reenqueued, None)
            .await
            .unwrap()
    );

    let outcome = repository
        .recover_calling_owner(
            "key-race",
            Some(&identity),
            &authority(
                &former,
                &incarnation(),
                CiQuiescenceProof::GracefulDrain,
                true,
            ),
            "lease-race",
        )
        .await
        .unwrap();
    assert_deferred(&outcome, CiCallingRecoveryReason::NotCalling);

    let row = repository.get("key-race").await.unwrap().unwrap();
    assert_eq!(
        row.terminal_outcome,
        Some(CiRouteOutcome::Reenqueued),
        "a committed provider outcome is authoritative"
    );
    assert_eq!(row.owner_incarnation_id.as_deref(), Some(former.as_str()));
    assert!(row.tier2_lease_id.is_none());
}

/// After a legal handoff of an **obsolete** row: no Tier-2 lease, no board or
/// worker consequence, charge retained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn obsolete_row_handoff_closes_superseded_after_call() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-gone");
    let input = reservation(&f.task_id, "key-gone", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let former = drained_incarnation(&f.db).await;
    repository
        .charge_and_begin_calling("key-gone", &former, &identity)
        .await
        .unwrap();
    age_calling(&f.db, "key-gone", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;

    let outcome = repository
        .recover_calling_owner(
            "key-gone",
            Some(&pr_head_identity(2, "headsha-moved")),
            &authority(
                &former,
                &incarnation(),
                CiQuiescenceProof::ProcessTerminated,
                true,
            ),
            "lease-gone",
        )
        .await
        .unwrap();
    let CiCallingRecovery::Recovered {
        attempt,
        outcome,
        tier2_lease_id,
    } = outcome
    else {
        panic!("expected a handoff");
    };
    assert_eq!(outcome, CiRouteOutcome::SupersededAfterCall);
    assert!(tier2_lease_id.is_none(), "an obsolete row opens no Tier 2");
    assert_eq!(attempt.charged_signature_count, Some(1));

    let counts = repository
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!((counts.signature, counts.head), (1, 1));
}

// ---------------------------------------------------------------------------
// Tier-2 lease
// ---------------------------------------------------------------------------

/// Two routes on the same current evidence contend for one lease. Exactly one
/// gets it; the loser opens nothing, and the winner's lease survives a reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_evidence_tier_two_leases_are_mutually_exclusive() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let head = "headsha-lease";
    let first_identity = pr_head_identity(1, head);
    let second_identity = pr_head_identity(2, head);
    repository
        .reserve(&reservation(
            &f.task_id,
            "key-lease-1",
            first_identity.clone(),
            "fp-a",
        ))
        .await
        .unwrap();
    repository
        .reserve(&reservation(
            &f.task_id,
            "key-lease-2",
            second_identity.clone(),
            "fp-b",
        ))
        .await
        .unwrap();

    let lease_key = format!("tier2:pr_head:4242:{head}");
    let first = repository
        .open_tier2_lease(
            "key-lease-1",
            &first_identity,
            &lease_key,
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = first else {
        panic!("expected the first opener to win, got {first:?}");
    };

    let (_handle, restarted) = reopened(&f.db);
    let second = restarted
        .open_tier2_lease(
            "key-lease-2",
            &second_identity,
            &lease_key,
            CiTier2Reason::EvidenceUnknown,
        )
        .await
        .unwrap();
    assert!(
        matches!(second, CiTier2LeaseOutcome::AlreadyLeased(_)),
        "a second route on the same current evidence cannot open a Lead adjudication"
    );

    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ci_route_attempts WHERE tier2_lease_state = 'open'",
    )
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(open, 1);

    // The dispatched Lead session binds to the exact lease.
    assert!(
        restarted
            .attach_lead_session("key-lease-1", &lease_id, "session-alpha")
            .await
            .unwrap()
    );
    assert!(
        !restarted
            .attach_lead_session("key-lease-1", "not-the-lease", "session-beta")
            .await
            .unwrap()
    );

    let quiescence = restarted.quiescence_counts().await.unwrap();
    assert_eq!(quiescence.open_tier2_leases, 1);
    assert_eq!(quiescence.unapplied_lead_results, 1);
    assert!(!quiescence.is_quiescent());

    // Resolving releases the current-evidence key for genuinely newer evidence.
    assert!(
        restarted
            .resolve_tier2_lease(
                "key-lease-1",
                &lease_id,
                &first_identity,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );
    let row = restarted.get("key-lease-1").await.unwrap().unwrap();
    assert_eq!(row.tier2_lease_state, Some(CiTier2LeaseState::Resolved));
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::RepairReopened));
    assert_eq!(row.reopen_mode, Some(CiReopenMode::Repair));
    assert!(row.diagnostic_reason.is_none());

    let third = restarted
        .open_tier2_lease(
            "key-lease-2",
            &second_identity,
            &lease_key,
            CiTier2Reason::EvidenceUnknown,
        )
        .await
        .unwrap();
    assert!(matches!(third, CiTier2LeaseOutcome::Opened { .. }));
}

/// An identity change before Lead dispatch closes the route
/// `superseded_before_lead` and opens no lease; an identity change while Lead
/// is pending closes it `superseded_before_apply` and applies nothing. Both
/// survive a reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn obsolete_routes_are_suppressed_before_lead_and_before_apply() {
    let f = fixture().await;
    let repository = repo(&f.db);

    // (a) before dispatch
    let stale = pr_head_identity(1, "headsha-before-lead");
    repository
        .reserve(&reservation(
            &f.task_id,
            "key-before-lead",
            stale.clone(),
            "fp-a",
        ))
        .await
        .unwrap();
    let outcome = repository
        .open_tier2_lease(
            "key-before-lead",
            &pr_head_identity(2, "headsha-moved"),
            "tier2:before-lead",
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::SupersededBeforeLead(attempt) = outcome else {
        panic!("expected pre-dispatch suppression, got {outcome:?}");
    };
    assert_eq!(
        attempt.terminal_outcome,
        Some(CiRouteOutcome::SupersededBeforeLead)
    );
    assert!(attempt.tier2_lease_id.is_none(), "no lease was opened");
    assert!(attempt.lead_session_id.is_none(), "no Lead session");

    // (b) after dispatch, before apply
    let pending = merge_group_identity(3, "headsha-pending", "dq-1");
    repository
        .reserve(&reservation(
            &f.task_id,
            "key-before-apply",
            pending.clone(),
            "fp-b",
        ))
        .await
        .unwrap();
    let opened = repository
        .open_tier2_lease(
            "key-before-apply",
            &pending,
            "tier2:before-apply",
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = opened else {
        panic!("expected an opened lease");
    };
    repository
        .attach_lead_session("key-before-apply", &lease_id, "session-pending")
        .await
        .unwrap();

    // The merge group re-formed under a different dequeue while Lead thought.
    let (_handle, restarted) = reopened(&f.db);
    let applied = restarted
        .resolve_tier2_lease(
            "key-before-apply",
            &lease_id,
            &merge_group_identity(3, "headsha-pending", "dq-2"),
            &CiTier2Resolution::diagnose(CiDiagnosticReason::EvidenceIncomplete),
        )
        .await
        .unwrap();
    assert!(!applied, "a failed guard applies nothing");

    let row = restarted.get("key-before-apply").await.unwrap().unwrap();
    assert_eq!(
        row.terminal_outcome,
        Some(CiRouteOutcome::SupersededBeforeApply)
    );
    assert_eq!(row.tier2_lease_state, Some(CiTier2LeaseState::Resolved));
    assert!(
        row.reopen_mode.is_none() && row.diagnostic_reason.is_none(),
        "a suppressed result leaves no reopen payload behind"
    );
    assert!(restarted.quiescence_counts().await.unwrap().is_quiescent());
}

/// A newer passing observation closes every open route for the PR without
/// refunding a charge, and a Lead result that arrives afterwards applies
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newer_passing_outcome_closes_routes_without_refunding_charges() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-pass");
    let input = reservation(&f.task_id, "key-pass", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling("key-pass", &owner, &identity)
        .await
        .unwrap();
    repository
        .finalize_calling("key-pass", &owner, CiRouteOutcome::ActionFailed, None)
        .await
        .unwrap();
    let opened = repository
        .open_tier2_lease(
            "key-pass",
            &identity,
            "tier2:pass",
            CiTier2Reason::ProviderActionFailed,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = opened else {
        panic!("action_failed must still be able to open Tier 2");
    };

    // A second, still-reserved route on the same PR.
    let other = reservation(
        &f.task_id,
        "key-pass-2",
        pr_head_identity(2, "headsha-pass"),
        "fp-b",
    );
    repository.reserve(&other).await.unwrap();

    // And a route with the SAME PR number belonging to a different task, i.e.
    // a different repository. A PR number is unique only within one repo, and
    // this table holds every project's rows.
    let foreign_task = seed_task_row(
        &f.db,
        UsageTestTaskSeed {
            project_id: &f.project_id,
            status: "pr_draft",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    repository
        .reserve(&reservation(
            &foreign_task,
            "key-pass-foreign",
            pr_head_identity(9, "headsha-foreign"),
            "fp-c",
        ))
        .await
        .unwrap();

    let closed = repository
        .close_routes_for_newer_outcome(&f.task_id, 4242, CiRouteOutcome::Passed, None)
        .await
        .unwrap();
    assert_eq!(closed, 1, "only the non-terminal route needed closing");
    let foreign = repository.get("key-pass-foreign").await.unwrap().unwrap();
    assert!(
        !foreign.is_terminal(),
        "another task's identically numbered PR must be untouched"
    );

    let (_handle, restarted) = reopened(&f.db);
    let other_row = restarted.get("key-pass-2").await.unwrap().unwrap();
    assert_eq!(other_row.terminal_outcome, Some(CiRouteOutcome::Passed));

    let counts = restarted
        .budget_counts(&input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "passing does not refund a spent slot"
    );

    // The delayed Lead result now has no open lease to apply to.
    assert!(
        !restarted
            .resolve_tier2_lease(
                "key-pass",
                &lease_id,
                &identity,
                &CiTier2Resolution::repair()
            )
            .await
            .unwrap()
    );
    let row = restarted.get("key-pass").await.unwrap().unwrap();
    assert_eq!(
        row.terminal_outcome,
        Some(CiRouteOutcome::ActionFailed),
        "the provider outcome remains the route's single terminal fact"
    );

    // This PR's own work is drained. The one remaining `reserved` row is the
    // other task's, which is exactly what the rollback gate should still see.
    let quiescence = restarted.quiescence_counts().await.unwrap();
    assert_eq!(quiescence.calling_rows, 0);
    assert_eq!(quiescence.open_tier2_leases, 0);
    assert_eq!(quiescence.unapplied_lead_results, 0);
    assert_eq!(quiescence.reserved_rows, 1);
    assert!(!quiescence.is_quiescent());
}

/// Contradictory Tier-2 payloads are rejected before they can be persisted:
/// a repair carrying a diagnostic reason, a diagnose without one, a park
/// without a citation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_two_resolution_payloads_are_mutually_exclusive() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-payload");
    repository
        .reserve(&reservation(
            &f.task_id,
            "key-payload",
            identity.clone(),
            "fp-a",
        ))
        .await
        .unwrap();
    let opened = repository
        .open_tier2_lease(
            "key-payload",
            &identity,
            "tier2:payload",
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = opened else {
        panic!("expected a lease");
    };

    let contradictions = [
        CiTier2Resolution {
            outcome: CiRouteOutcome::RepairReopened,
            reopen_mode: Some(CiReopenMode::Repair),
            diagnostic_reason: Some(CiDiagnosticReason::NoGroundedRemedy),
            park_justification: None,
        },
        CiTier2Resolution {
            outcome: CiRouteOutcome::DiagnosticReopened,
            reopen_mode: Some(CiReopenMode::Diagnose),
            diagnostic_reason: None,
            park_justification: None,
        },
        CiTier2Resolution {
            outcome: CiRouteOutcome::Parked,
            reopen_mode: None,
            diagnostic_reason: None,
            park_justification: Some("   ".to_owned()),
        },
    ];
    for resolution in &contradictions {
        assert!(
            repository
                .resolve_tier2_lease("key-payload", &lease_id, &identity, resolution)
                .await
                .is_err(),
            "contradictory payload {:?} must be rejected",
            resolution.outcome
        );
    }
    let row = repository.get("key-payload").await.unwrap().unwrap();
    assert!(row.terminal_outcome.is_none(), "nothing was persisted");

    // The valid park does land, with its citation.
    assert!(
        repository
            .resolve_tier2_lease(
                "key-payload",
                &lease_id,
                &identity,
                &CiTier2Resolution::park("runner image pull is failing fleet-wide"),
            )
            .await
            .unwrap()
    );
    let row = repository.get("key-payload").await.unwrap().unwrap();
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Parked));
    assert!(
        row.park_justification
            .as_deref()
            .is_some_and(|j| j.contains("fleet-wide"))
    );
}

// ---------------------------------------------------------------------------
// Helpers used by more than one test
// ---------------------------------------------------------------------------

async fn rows_in_state(db: &Database, phase: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM ci_route_attempts WHERE action_phase = $1")
        .bind(phase)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Spend the whole signature budget for `identity`'s key through distinct
/// runs, so a later attempt on the same signature is genuinely exhausted.
async fn spend_signature_budget(f: &Fixture, identity: &CiEvidenceIdentity, fingerprint: &str) {
    let repository = repo(&f.db);
    for run in 100..(100 + CI_SIGNATURE_BUDGET_LIMIT) {
        let mut spent = identity.clone();
        spent.run_id = run;
        let key = format!("key-spend-{run}");
        repository
            .reserve(&reservation(&f.task_id, &key, spent.clone(), fingerprint))
            .await
            .unwrap();
        let charged = repository
            .charge_and_begin_calling(&key, &incarnation(), &spent)
            .await
            .unwrap();
        assert!(matches!(charged, CiChargeOutcome::Charged { .. }));
    }
}

fn assert_deferred(outcome: &CiCallingRecovery, expected: CiCallingRecoveryReason) {
    match outcome {
        CiCallingRecovery::Deferred { reason, .. } => assert_eq!(*reason, expected),
        other => panic!("expected deferral {expected:?}, got {other:?}"),
    }
}

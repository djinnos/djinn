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
    CiRouteAttemptRepository, CiRouteOutcome, CiRouteReservation, CiRouteSubject,
    CiTier2LeaseOutcome, CiTier2LeaseState, CiTier2Reason, CiTier2Resolution,
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
    subject: CiRouteSubject,
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
    let subject = CiRouteSubject::task(&task_id);
    Fixture {
        db,
        project_id: project.id,
        task_id,
        subject,
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

/// The canonical Tier-2 lease key wave 2 must derive.
///
/// **(PR number, PR-head SHA)** and nothing else. No lane, no run id, no
/// dequeue id: the hold is per PR head across both lanes, and the subject
/// scoping already supplies repository identity.
fn tier2_lease_key(pr_number: i64, head_sha: &str) -> String {
    format!("tier2:{pr_number}:{head_sha}")
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
    subject: &CiRouteSubject,
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
        subject: subject.clone(),
        provider_action_key: key.to_owned(),
        identity,
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
    let input = reservation(&f.subject, "key-round-trip", identity.clone(), "fp-a");
    let reserved = unwrap_reserved(repository.reserve(&input).await.unwrap());

    assert_eq!(reserved.action_phase, CiActionPhase::Reserved);
    assert!(reserved.terminal_outcome.is_none());
    assert!(reserved.owner_incarnation_id.is_none());
    assert!(!reserved.reserved_at.is_empty());

    // Read back through a genuinely new handle: the row is on disk, not in an
    // object we are still holding.
    let (handle, restarted) = reopened(&f.db);
    let loaded = restarted
        .get(&f.subject, "key-round-trip")
        .await
        .unwrap()
        .expect("row survives reload");

    assert_eq!(loaded.identity, identity);
    assert_eq!(loaded.task_id.as_deref(), Some(f.task_id.as_str()));
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
        &f.subject,
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
    let input = reservation(&f.subject, "key-happy", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    let owner = incarnation();
    let charged = repository
        .charge_and_begin_calling(&f.subject, "key-happy", &owner, &identity)
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
        .charge_and_begin_calling(&f.subject, "key-happy", &loser, &identity)
        .await
        .unwrap();
    assert!(matches!(again, CiChargeOutcome::NotReserved(_)));

    let counts = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "a losing caller must not charge"
    );
    let row = repository
        .get(&f.subject, "key-happy")
        .await
        .unwrap()
        .unwrap();
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
    let input = reservation(&f.subject, "key-stale", identity, "fp-a");
    repository.reserve(&input).await.unwrap();

    let observed = pr_head_identity(2, "headsha-new");
    let outcome = repository
        .charge_and_begin_calling(&f.subject, "key-stale", &incarnation(), &observed)
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
    let input = reservation(&f.subject, "key-young", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    let outcome = repository
        .recover_reserved(
            &f.subject,
            "key-young",
            &identity,
            &incarnation(),
            300,
            "lease-young",
        )
        .await
        .unwrap();
    let CiReservedRecovery::NotEligible(attempt) = outcome else {
        panic!("expected ineligibility, got {outcome:?}");
    };
    assert_eq!(attempt.action_phase, CiActionPhase::Reserved);
    assert_eq!(attempt.pre_call_resumptions, 0);

    let counts = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
    let input = reservation(&f.subject, "key-resume", identity.clone(), "fp-a");
    let reserved = unwrap_reserved(repository.reserve(&input).await.unwrap());
    age_reserved(&f.db, "key-resume", 600).await;

    let (_h1, restart_one) = reopened(&f.db);
    let (_h2, restart_two) = reopened(&f.db);
    let winner_incarnation = incarnation();

    let first = restart_one
        .recover_reserved(
            &f.subject,
            "key-resume",
            &identity,
            &winner_incarnation,
            60,
            "lease-resume",
        )
        .await
        .unwrap();
    let second = restart_two
        .recover_reserved(
            &f.subject,
            "key-resume",
            &identity,
            &incarnation(),
            60,
            "lease-resume",
        )
        .await
        .unwrap();
    let third = repository
        .recover_reserved(
            &f.subject,
            "key-resume",
            &identity,
            &incarnation(),
            60,
            "lease-resume",
        )
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (final_counts.signature, final_counts.head),
        (1, 1),
        "three recoveries must charge exactly once"
    );

    let row = repository
        .get(&f.subject, "key-resume")
        .await
        .unwrap()
        .unwrap();
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
/// phase check. This one launches them *simultaneously* on separate pools.
///
/// Be precise about what fails without the row lock, because it is not a
/// double charge. Two mechanisms are stacked, and the outer one is about
/// ergonomics rather than correctness:
///
/// * remove `FOR UPDATE` and the losers get past the phase read, increment the
///   counters, then **lose the `action_phase = 'reserved'` compare-and-set**.
///   The transaction rolls back, so the counter still reads `(1, 1)` — the
///   phase CAS plus rollback is what actually prevents the double charge.
///   What the caller sees instead is `Err("reserved->calling resumption lost
///   under a held row lock")`, which is why this test's `unwrap` is the thing
///   that fails.
/// * the row lock turns that lost race into a clean `NotEligible` for the
///   losers, which is what lets a duplicate poll be a no-op rather than an
///   error the coordinator has to interpret.
///
/// So: asserting `(1, 1)` proves the charge is exactly once; asserting that
/// exactly one caller returns `Resumed` without erroring proves the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recoveries_of_one_reserved_row_charge_exactly_once() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-race-resume");
    let input = reservation(&f.subject, "key-race-resume", identity.clone(), "fp-a");
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
        a.recover_reserved(
            &f.subject,
            "key-race-resume",
            &id_a,
            &inc_a,
            60,
            "lease-race"
        ),
        b.recover_reserved(
            &f.subject,
            "key-race-resume",
            &id_b,
            &inc_b,
            60,
            "lease-race"
        ),
        c.recover_reserved(
            &f.subject,
            "key-race-resume",
            &id_c,
            &inc_c,
            60,
            "lease-race"
        ),
    );

    let resumed = [ra.unwrap(), rb.unwrap(), rc.unwrap()]
        .into_iter()
        .filter(|outcome| matches!(outcome, CiReservedRecovery::Resumed { .. }))
        .count();
    assert_eq!(resumed, 1, "exactly one concurrent recovery may resume");

    let counts = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "three concurrent recoveries must charge exactly once"
    );
    let row = repository
        .get(&f.subject, "key-race-resume")
        .await
        .unwrap()
        .unwrap();
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
        &f.subject,
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
        &f.subject,
        "key-obsolete",
        pr_head_identity(1, "headsha-old"),
        "fp-a",
    );
    repository.reserve(&input).await.unwrap();
    age_reserved(&f.db, "key-obsolete", 600).await;

    let (_handle, restarted) = reopened(&f.db);
    let outcome = restarted
        .recover_reserved(
            &f.subject,
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
    let input = reservation(&f.subject, "key-exhausted", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    age_reserved(&f.db, "key-exhausted", 600).await;

    // Spend the signature budget through two other, distinct runs.
    spend_signature_budget(&f, &identity, "fp-a").await;
    let before = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(before.signature, CI_SIGNATURE_BUDGET_LIMIT);

    let (_handle, restarted) = reopened(&f.db);
    let first = restarted
        .recover_reserved(
            &f.subject,
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
            &f.subject,
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
    let input = reservation(&f.subject, "key-sig-1", identity.clone(), "fp-a");

    for run in 1..=CI_SIGNATURE_BUDGET_LIMIT {
        let id = pr_head_identity(run, "headsha-sig");
        let key = format!("key-sig-{run}");
        let res = reservation(&f.subject, &key, id.clone(), "fp-a");
        assert!(matches!(
            repository.reserve(&res).await.unwrap(),
            CiReserveOutcome::Reserved(_)
        ));
        let charged = repository
            .charge_and_begin_calling(&f.subject, &key, &incarnation(), &id)
            .await
            .unwrap();
        assert!(matches!(charged, CiChargeOutcome::Charged { .. }));
        // The provider answered with an error; the slot is NOT returned.
        // (`ignored` is not the owner, so this finalization writes nothing —
        // the point here is only that no code path exists that decrements.)
        assert!(
            !repository
                .finalize_calling(
                    &f.subject,
                    &key,
                    "ignored",
                    CiRouteOutcome::ActionFailed,
                    None
                )
                .await
                .unwrap()
        );
    }

    let counts = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(counts.signature, CI_SIGNATURE_BUDGET_LIMIT);
    assert!(counts.is_exhausted());

    let third = reservation(
        &f.subject,
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
        let res = reservation(&f.subject, &key, identity.clone(), fingerprint);
        assert!(
            matches!(
                repository.reserve(&res).await.unwrap(),
                CiReserveOutcome::Reserved(_)
            ),
            "charge {index} must reserve"
        );
        assert!(matches!(
            repository
                .charge_and_begin_calling(&f.subject, &key, &incarnation(), identity)
                .await
                .unwrap(),
            CiChargeOutcome::Charged { .. }
        ));
    }

    let head_key = format!("head:4242:{head}");
    let (_handle, restarted) = reopened(&f.db);
    let counts = restarted
        .budget_counts(
            &f.subject,
            "sig:pr_head:4242:headsha-shared:fp-a",
            &head_key,
        )
        .await
        .unwrap();
    assert_eq!(
        counts.head, CI_HEAD_BUDGET_LIMIT,
        "the head budget counts both lanes and survives a restart"
    );

    // A fifth attempt on a brand-new fingerprint has a fresh signature budget
    // and is still refused by the head ceiling.
    let fresh = reservation(
        &f.subject,
        "key-head-4",
        pr_head_identity(5, head),
        "fp-fresh",
    );
    let counts = restarted
        .budget_counts(&f.subject, &fresh.retry_budget_key, &fresh.head_budget_key)
        .await
        .unwrap();
    assert_eq!(counts.signature, 0, "a changed fingerprint starts fresh");
    let outcome = restarted.reserve(&fresh).await.unwrap();
    assert!(matches!(outcome, CiReserveOutcome::BudgetExhausted(_)));

    // A changed PR head starts both budgets over.
    let new_head = reservation(
        &f.subject,
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
    let input = reservation(&f.subject, "key-final", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-final", &owner, &identity)
        .await
        .unwrap();

    let impostor = incarnation();
    assert!(
        !repository
            .finalize_calling(
                &f.subject,
                "key-final",
                &impostor,
                CiRouteOutcome::Retriggered,
                None
            )
            .await
            .unwrap(),
        "a non-owner cannot finalize"
    );
    let row = repository
        .get(&f.subject, "key-final")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);

    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-final",
                &owner,
                CiRouteOutcome::Retriggered,
                None
            )
            .await
            .unwrap()
    );
    let row = repository
        .get(&f.subject, "key-final")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Retriggered));

    // Exactly-once: the owner's own retry finds nothing left to write.
    assert!(
        !repository
            .finalize_calling(
                &f.subject,
                "key-final",
                &owner,
                CiRouteOutcome::ActionFailed,
                None
            )
            .await
            .unwrap()
    );
    let row = repository
        .get(&f.subject, "key-final")
        .await
        .unwrap()
        .unwrap();
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
        &f.subject,
        "key-term",
        pr_head_identity(1, "headsha-term"),
        "fp-a",
    );
    repository.reserve(&input).await.unwrap();

    assert!(
        repository
            .terminalize(&f.subject, "key-term", CiRouteOutcome::Held, None)
            .await
            .unwrap()
    );
    assert!(
        !repository
            .terminalize(&f.subject, "key-term", CiRouteOutcome::Held, None)
            .await
            .unwrap()
    );

    let (_handle, restarted) = reopened(&f.db);
    assert!(
        !restarted
            .terminalize(
                &f.subject,
                "key-term",
                CiRouteOutcome::DiagnosticReopened,
                None
            )
            .await
            .unwrap(),
        "a startup sweep must not re-close a terminal row"
    );
    let row = restarted
        .get(&f.subject, "key-term")
        .await
        .unwrap()
        .unwrap();
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
    let input = reservation(&f.subject, "key-live", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-live", &owner, &identity)
        .await
        .unwrap();

    let recovering = incarnation();
    let (_handle, sweeper) = reopened(&f.db);

    // 1. A periodic sweep that does not hold the exclusive lock.
    let deferred = sweeper
        .recover_calling_owner(
            &f.subject,
            "key-live",
            &identity,
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
            &f.subject,
            "key-live",
            &identity,
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
            &f.subject,
            "key-live",
            &identity,
            &authority(&owner, &recovering, CiQuiescenceProof::GracefulDrain, true),
            "lease-live",
        )
        .await
        .unwrap();
    assert_deferred(&deferred, CiCallingRecoveryReason::LiveOwnerDeferred);

    // 4. Process death is proven, but `calling_at` has not aged past the floor.
    let deferred = sweeper
        .recover_calling_owner(
            &f.subject,
            "key-live",
            &identity,
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
            &f.subject,
            "key-live",
            &identity,
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
    let row = repository
        .get(&f.subject, "key-live")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);
    assert_eq!(row.owner_incarnation_id.as_deref(), Some(owner.as_str()));
    assert!(row.tier2_lease_id.is_none());
    assert!(row.lead_session_id.is_none());

    // Five deferrals, five audit rows, none of them claiming a win.
    let audit = repository
        .calling_recovery_audit(&f.subject, "key-live")
        .await
        .unwrap();
    assert_eq!(audit.len(), 5);
    assert!(audit.iter().all(|r| !r.cas_won));

    // And the owner can still finalize, because nothing took its row.
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-live",
                &owner,
                CiRouteOutcome::Retriggered,
                None
            )
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
    let input = reservation(&f.subject, "key-handoff", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    // The former owner completed the full graceful drain before releasing the
    // advisory lock.
    let former = drained_incarnation(&f.db).await;
    repository
        .charge_and_begin_calling(&f.subject, "key-handoff", &former, &identity)
        .await
        .unwrap();
    age_calling(&f.db, "key-handoff", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;

    let recovering = incarnation();
    let (_handle, restarted) = reopened(&f.db);
    let first = restarted
        .recover_calling_owner(
            &f.subject,
            "key-handoff",
            &identity,
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
            &f.subject,
            "key-handoff",
            &identity,
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
        .calling_recovery_audit(&f.subject, "key-handoff")
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
    let input = reservation(&f.subject, "key-race", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let former = drained_incarnation(&f.db).await;
    repository
        .charge_and_begin_calling(&f.subject, "key-race", &former, &identity)
        .await
        .unwrap();
    age_calling(&f.db, "key-race", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;

    // The old owner's finalizer commits first.
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-race",
                &former,
                CiRouteOutcome::Reenqueued,
                None
            )
            .await
            .unwrap()
    );

    let outcome = repository
        .recover_calling_owner(
            &f.subject,
            "key-race",
            &identity,
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

    let row = repository
        .get(&f.subject, "key-race")
        .await
        .unwrap()
        .unwrap();
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
    let input = reservation(&f.subject, "key-gone", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let former = drained_incarnation(&f.db).await;
    repository
        .charge_and_begin_calling(&f.subject, "key-gone", &former, &identity)
        .await
        .unwrap();
    age_calling(&f.db, "key-gone", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;

    let outcome = repository
        .recover_calling_owner(
            &f.subject,
            "key-gone",
            &pr_head_identity(2, "headsha-moved"),
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
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!((counts.signature, counts.head), (1, 1));
}

// ---------------------------------------------------------------------------
// Tier-2 lease
// ---------------------------------------------------------------------------

/// Two routes on the same PR head contend for one Lead adjudication, and they
/// are deliberately **in different lanes**.
///
/// That is the head-level hold: at most one Lead adjudication may be open per
/// PR head across both lanes. It works only because the lease key excludes
/// the lane — a lane-scoped key would give two concurrent adjudications for one
/// head and defeat the retry-storm safeguard entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_evidence_tier_two_leases_are_mutually_exclusive_across_lanes() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let head = "headsha-lease";
    let first_identity = pr_head_identity(1, head);
    let second_identity = merge_group_identity(2, head, "dq-lease");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-lease-1",
            first_identity.clone(),
            "fp-a",
        ))
        .await
        .unwrap();
    repository
        .reserve(&reservation(
            &f.subject,
            "key-lease-2",
            second_identity.clone(),
            "fp-b",
        ))
        .await
        .unwrap();

    // One key for both lanes. Adding the lane here is the mistake this test
    // exists to catch.
    let lease_key = tier2_lease_key(4242, head);
    assert!(
        !lease_key.contains("pr_head") && !lease_key.contains("merge_group"),
        "the Tier-2 lease key must not carry the lane"
    );
    let first = repository
        .open_tier2_lease(
            &f.subject,
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
            &f.subject,
            "key-lease-2",
            &second_identity,
            &lease_key,
            CiTier2Reason::EvidenceUnknown,
        )
        .await
        .unwrap();
    assert!(
        matches!(second, CiTier2LeaseOutcome::KeyHeldElsewhere(_)),
        "a merge-group route on the same PR head cannot open a second Lead \
         adjudication while the PR-head route holds the hold, got {second:?}"
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
            .attach_lead_session(&f.subject, "key-lease-1", &lease_id, "session-alpha")
            .await
            .unwrap()
    );
    assert!(
        !restarted
            .attach_lead_session(&f.subject, "key-lease-1", "not-the-lease", "session-beta")
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
                &f.subject,
                "key-lease-1",
                &lease_id,
                &first_identity,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );
    let row = restarted
        .get(&f.subject, "key-lease-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.tier2_lease_state, Some(CiTier2LeaseState::Resolved));
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::RepairReopened));
    assert_eq!(row.reopen_mode, Some(CiReopenMode::Repair));
    assert!(row.diagnostic_reason.is_none());

    let third = restarted
        .open_tier2_lease(
            &f.subject,
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
            &f.subject,
            "key-before-lead",
            stale.clone(),
            "fp-a",
        ))
        .await
        .unwrap();
    let outcome = repository
        .open_tier2_lease(
            &f.subject,
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
            &f.subject,
            "key-before-apply",
            pending.clone(),
            "fp-b",
        ))
        .await
        .unwrap();
    let opened = repository
        .open_tier2_lease(
            &f.subject,
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
        .attach_lead_session(&f.subject, "key-before-apply", &lease_id, "session-pending")
        .await
        .unwrap();

    // The merge group re-formed under a different dequeue while Lead thought.
    let (_handle, restarted) = reopened(&f.db);
    let applied = restarted
        .resolve_tier2_lease(
            &f.subject,
            "key-before-apply",
            &lease_id,
            &merge_group_identity(3, "headsha-pending", "dq-2"),
            &CiTier2Resolution::diagnose(CiDiagnosticReason::EvidenceIncomplete),
        )
        .await
        .unwrap();
    assert!(!applied, "a failed guard applies nothing");

    let row = restarted
        .get(&f.subject, "key-before-apply")
        .await
        .unwrap()
        .unwrap();
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
    let input = reservation(&f.subject, "key-pass", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-pass", &owner, &identity)
        .await
        .unwrap();
    repository
        .finalize_calling(
            &f.subject,
            "key-pass",
            &owner,
            CiRouteOutcome::ActionFailed,
            None,
        )
        .await
        .unwrap();
    let opened = repository
        .open_tier2_lease(
            &f.subject,
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
        &f.subject,
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
    let foreign_subject = CiRouteSubject::task(&foreign_task);
    repository
        .reserve(&reservation(
            &foreign_subject,
            "key-pass-foreign",
            pr_head_identity(9, "headsha-foreign"),
            "fp-c",
        ))
        .await
        .unwrap();

    let closed = repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Passed, None)
        .await
        .unwrap();
    assert_eq!(closed, 1, "only the non-terminal route needed closing");
    let foreign = repository
        .get(&foreign_subject, "key-pass-foreign")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !foreign.is_terminal(),
        "another task's identically numbered PR must be untouched"
    );

    let (_handle, restarted) = reopened(&f.db);
    let other_row = restarted
        .get(&f.subject, "key-pass-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other_row.terminal_outcome, Some(CiRouteOutcome::Passed));

    let counts = restarted
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
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
                &f.subject,
                "key-pass",
                &lease_id,
                &identity,
                &CiTier2Resolution::repair()
            )
            .await
            .unwrap()
    );
    let row = restarted
        .get(&f.subject, "key-pass")
        .await
        .unwrap()
        .unwrap();
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
            &f.subject,
            "key-payload",
            identity.clone(),
            "fp-a",
        ))
        .await
        .unwrap();
    let opened = repository
        .open_tier2_lease(
            &f.subject,
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
                .resolve_tier2_lease(&f.subject, "key-payload", &lease_id, &identity, resolution)
                .await
                .is_err(),
            "contradictory payload {:?} must be rejected",
            resolution.outcome
        );
    }
    let row = repository
        .get(&f.subject, "key-payload")
        .await
        .unwrap()
        .unwrap();
    assert!(row.terminal_outcome.is_none(), "nothing was persisted");

    // The valid park does land, with its citation.
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-payload",
                &lease_id,
                &identity,
                &CiTier2Resolution::park("runner image pull is failing fleet-wide"),
            )
            .await
            .unwrap()
    );
    let row = repository
        .get(&f.subject, "key-payload")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Parked));
    assert!(
        row.park_justification
            .as_deref()
            .is_some_and(|j| j.contains("fleet-wide"))
    );
}

// ---------------------------------------------------------------------------
// The live exhausted path, and the two write-once outcome fields
// ---------------------------------------------------------------------------

/// The budgets can be spent between a reservation and its charge — a peer
/// route on the same signature got there first. This is the LIVE exhausted
/// path and it behaves differently from the recovery one on purpose: it stamps
/// `retry_exhausted_at`, leaves the row `reserved`, charges nothing, and opens
/// no Tier-2 lease, because the caller is still running and owns that
/// decision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn charge_on_a_reserved_row_whose_budget_was_spent_meanwhile_is_refused() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(7, "headsha-live-exhausted");
    let input = reservation(&f.subject, "key-live-exhausted", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    // Two peer routes on the same signature spend the budget after our row
    // was already reserved.
    spend_signature_budget(&f, &identity, "fp-a").await;

    let outcome = repository
        .charge_and_begin_calling(&f.subject, "key-live-exhausted", &incarnation(), &identity)
        .await
        .unwrap();
    let CiChargeOutcome::BudgetExhausted { attempt, counts } = outcome else {
        panic!("expected the live exhausted path, got {outcome:?}");
    };
    assert_eq!(counts.signature, CI_SIGNATURE_BUDGET_LIMIT);
    assert_eq!(
        attempt.action_phase,
        CiActionPhase::Reserved,
        "the live path leaves the row reserved for the caller to route"
    );
    assert!(attempt.retry_exhausted_at.is_some());
    assert!(attempt.owner_incarnation_id.is_none(), "no call ownership");
    assert!(attempt.charged_signature_count.is_none(), "no charge");
    assert!(
        !attempt.has_routed_to_tier2(),
        "the live path opens no lease; that is the recovery path's job"
    );

    // And the budget really did not move.
    let after = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(after.signature, CI_SIGNATURE_BUDGET_LIMIT);
    // The only `calling` rows are the two peers that spent the budget; the
    // refused row never got call ownership, which the phase assertion above
    // already established.
    assert_eq!(
        rows_in_state(&f.db, "calling").await,
        CI_SIGNATURE_BUDGET_LIMIT
    );
}

/// The case the two write-once outcome fields exist for: a route that
/// terminalized on a provider error, then legally routed to Tier 2 once, and
/// came back with a repair reopen.
///
/// `terminal_outcome` keeps `action_failed` — the route really did fail at the
/// provider and that fact is not rewritable. The adjudication lands in
/// `tier2_resolution`. Anything counting reopens must read
/// `adjudicated_outcome()`, or it silently drops every reopen that followed a
/// provider failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failure_then_tier_two_records_both_outcomes() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-both");
    let input = reservation(&f.subject, "key-both", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-both", &owner, &identity)
        .await
        .unwrap();
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-both",
                &owner,
                CiRouteOutcome::ActionFailed,
                Some(r#"{"status":422,"message":"workflow run is not re-runnable"}"#),
            )
            .await
            .unwrap()
    );

    let opened = repository
        .open_tier2_lease(
            &f.subject,
            "key-both",
            &identity,
            "tier2:both",
            CiTier2Reason::ProviderActionFailed,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = opened else {
        panic!("an action_failed row may route once to Tier 2");
    };
    assert!(
        repository
            .attach_lead_session(&f.subject, "key-both", &lease_id, "session-both")
            .await
            .unwrap()
    );
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-both",
                &lease_id,
                &identity,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );

    let (_handle, restarted) = reopened(&f.db);
    let row = restarted
        .get(&f.subject, "key-both")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        row.terminal_outcome,
        Some(CiRouteOutcome::ActionFailed),
        "the provider failure is the route's single terminal fact"
    );
    assert_eq!(
        row.tier2_resolution,
        Some(CiRouteOutcome::RepairReopened),
        "the adjudication lands in the second write-once field"
    );
    assert_eq!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened),
        "downstream reopen/park queries MUST read this, not terminal_outcome"
    );
    assert_eq!(row.reopen_mode, Some(CiReopenMode::Repair));
    assert_eq!(row.tier2_lease_state, Some(CiTier2LeaseState::Resolved));
    assert!(
        row.provider_error
            .as_deref()
            .is_some_and(|e| e.contains("not re-runnable"))
    );

    // The route has used its one trip. A second adjudication is refused even
    // though the lease is no longer open — uniqueness here is once-ever, not
    // merely concurrent.
    let again = restarted
        .open_tier2_lease(
            &f.subject,
            "key-both",
            &identity,
            "tier2:both",
            CiTier2Reason::ProviderActionFailed,
        )
        .await
        .unwrap();
    assert!(
        matches!(again, CiTier2LeaseOutcome::AlreadyRoutedToTier2(_)),
        "`may route once to Tier 2` means once ever, got {again:?}"
    );
    let row = restarted
        .get(&f.subject, "key-both")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.tier2_lease_state, Some(CiTier2LeaseState::Resolved));
    assert_eq!(row.tier2_lease_id.as_deref(), Some(lease_id.as_str()));
}

/// An exhausted recovery that cannot get the head lease because another route
/// already holds it. This leaves the row `reserved` with no route out of its
/// own accord: it is **not** self-healing, and W2 must re-drive recovery once
/// the holding lease resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_recovery_blocked_by_a_peer_head_lease_strands_the_row() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let head = "headsha-strand";
    let lease_key = tier2_lease_key(4242, head);

    // A peer route on the same PR head takes the one head lease.
    let peer_identity = pr_head_identity(1, head);
    repository
        .reserve(&reservation(
            &f.subject,
            "key-strand-peer",
            peer_identity.clone(),
            "fp-peer",
        ))
        .await
        .unwrap();
    let peer_lease = repository
        .open_tier2_lease(
            &f.subject,
            "key-strand-peer",
            &peer_identity,
            &lease_key,
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened {
        lease_id: peer_lease_id,
        ..
    } = peer_lease
    else {
        panic!("the peer must take the head lease");
    };

    // Our row reserves, then its signature budget is spent by other runs.
    let identity = pr_head_identity(2, head);
    let input = reservation(&f.subject, "key-strand", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    age_reserved(&f.db, "key-strand", 600).await;
    spend_signature_budget(&f, &identity, "fp-a").await;

    let outcome = repository
        .recover_reserved(
            &f.subject,
            "key-strand",
            &identity,
            &incarnation(),
            60,
            &lease_key,
        )
        .await
        .unwrap();
    let CiReservedRecovery::RetryExhausted {
        attempt,
        tier2_lease_id,
        ..
    } = outcome
    else {
        panic!("expected retry exhaustion, got {outcome:?}");
    };
    assert!(
        tier2_lease_id.is_none(),
        "the peer holds the one head lease, so no lease is granted"
    );
    assert!(!attempt.has_routed_to_tier2());

    // THE STRAND. The row is exhausted, current, non-terminal, and holds no
    // lease. Nothing in this layer will move it again on its own.
    assert_eq!(attempt.action_phase, CiActionPhase::Reserved);
    assert!(attempt.retry_exhausted_at.is_some());
    let quiescence = repository.quiescence_counts().await.unwrap();
    assert!(
        quiescence.reserved_rows >= 1,
        "a stranded row keeps the rollback gate open, which is the point"
    );

    // W2's obligation: once the holding adjudication resolves, re-drive
    // recovery. Only then does the stranded row get its Tier-2 lease.
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-strand-peer",
                &peer_lease_id,
                &peer_identity,
                &CiTier2Resolution::diagnose(CiDiagnosticReason::NoGroundedRemedy),
            )
            .await
            .unwrap()
    );
    let retried = repository
        .recover_reserved(
            &f.subject,
            "key-strand",
            &identity,
            &incarnation(),
            60,
            &lease_key,
        )
        .await
        .unwrap();
    let CiReservedRecovery::RetryExhausted { tier2_lease_id, .. } = retried else {
        panic!("expected retry exhaustion again");
    };
    assert!(
        tier2_lease_id.is_some(),
        "re-driving recovery after the peer released the key must now lease"
    );
}

/// `terminalize` is the unfenced-looking sibling of the owner-scoped
/// `finalize_calling`, so it carries two fences of its own. Without them it
/// would be a way to claim the provider was called, or to close a route whose
/// provider future is still in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminalize_refuses_provider_outcomes_and_live_calling_rows() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-fence");
    let input = reservation(&f.subject, "key-fence", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    // Fence 1: a provider-finalization outcome is refused even on a plain
    // `reserved` row. Only the calling owner may assert a provider call.
    for outcome in [
        CiRouteOutcome::Retriggered,
        CiRouteOutcome::Reenqueued,
        CiRouteOutcome::ActionFailed,
    ] {
        assert!(
            repository
                .terminalize(&f.subject, "key-fence", outcome, None)
                .await
                .is_err(),
            "terminalize must not be able to write `{}`",
            outcome.as_str()
        );
    }
    let row = repository
        .get(&f.subject, "key-fence")
        .await
        .unwrap()
        .unwrap();
    assert!(row.terminal_outcome.is_none(), "nothing was written");

    // Fence 2: a row owned in `calling` may not be closed out from under its
    // owner. Its provider future may be in flight right now.
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-fence", &owner, &identity)
        .await
        .unwrap();
    let refused = repository
        .terminalize(&f.subject, "key-fence", CiRouteOutcome::Held, None)
        .await;
    assert!(refused.is_err(), "a live calling row must not be closable");
    let row = repository
        .get(&f.subject, "key-fence")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);
    assert_eq!(row.owner_incarnation_id.as_deref(), Some(owner.as_str()));

    // The owner can still finalize, because nothing stole its row.
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-fence",
                &owner,
                CiRouteOutcome::Retriggered,
                None
            )
            .await
            .unwrap()
    );

    // A pass/merge observation likewise leaves a `calling` row alone.
    let live = pr_head_identity(3, "headsha-fence");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-fence-live",
            live.clone(),
            "fp-b",
        ))
        .await
        .unwrap();
    let live_owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-fence-live", &live_owner, &live)
        .await
        .unwrap();
    let closed = repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();
    assert_eq!(closed, 0, "there was no `reserved` row left to close");
    let row = repository
        .get(&f.subject, "key-fence-live")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.action_phase,
        CiActionPhase::Calling,
        "a merge observation must not steal a row from a live provider call"
    );
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-fence-live",
                &live_owner,
                CiRouteOutcome::Reenqueued,
                None
            )
            .await
            .unwrap(),
        "the owner's finalizer must still win after the merge observation"
    );
}

/// Every key on this table is subject-scoped, so a route on one subject can
/// never swallow an identically keyed route on another.
///
/// Before the scoping, a colliding `provider_action_key` made `reserve` answer
/// `AlreadyPresent` for foreign evidence and that route silently never
/// existed; a colliding `tier2_lease_key` made the Lead adjudication never
/// open. Both are exercised here with deliberately identical keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_keys_on_two_subjects_do_not_collide() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let other_task = seed_task_row(
        &f.db,
        UsageTestTaskSeed {
            project_id: &f.project_id,
            status: "pr_draft",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let other = CiRouteSubject::task(&other_task);

    // The SAME action key, budget keys, and lease key on both subjects.
    let identity = pr_head_identity(1, "headsha-collide");
    let mine = reservation(&f.subject, "key-collide", identity.clone(), "fp-a");
    let theirs = reservation(&other, "key-collide", identity.clone(), "fp-a");
    assert_eq!(mine.provider_action_key, theirs.provider_action_key);
    assert_eq!(mine.retry_budget_key, theirs.retry_budget_key);

    assert!(matches!(
        repository.reserve(&mine).await.unwrap(),
        CiReserveOutcome::Reserved(_)
    ));
    assert!(
        matches!(
            repository.reserve(&theirs).await.unwrap(),
            CiReserveOutcome::Reserved(_)
        ),
        "a foreign subject's identical key must still create its own route"
    );

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_route_attempts")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 2);

    // Budgets are independent too.
    repository
        .charge_and_begin_calling(&f.subject, "key-collide", &incarnation(), &identity)
        .await
        .unwrap();
    let theirs_counts = repository
        .budget_counts(&other, &theirs.retry_budget_key, &theirs.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        (theirs_counts.signature, theirs_counts.head),
        (0, 0),
        "one subject's charge must not spend another's budget"
    );

    // And so is the Tier-2 head lease.
    let lease_key = tier2_lease_key(4242, "headsha-collide");
    let theirs_lease = repository
        .open_tier2_lease(
            &other,
            "key-collide",
            &identity,
            &lease_key,
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    assert!(
        matches!(theirs_lease, CiTier2LeaseOutcome::Opened { .. }),
        "a foreign subject must get its own Lead adjudication, got {theirs_lease:?}"
    );

    // The generated task_id column tracks the subject and carries the real
    // foreign key; the database derives it and refuses a direct write.
    let row = repository
        .get(&other, "key-collide")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.task_id.as_deref(), Some(other_task.as_str()));
    assert_eq!(row.subject, other);
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
            .reserve(&reservation(&f.subject, &key, spent.clone(), fingerprint))
            .await
            .unwrap();
        let charged = repository
            .charge_and_begin_calling(&f.subject, &key, &incarnation(), &spent)
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

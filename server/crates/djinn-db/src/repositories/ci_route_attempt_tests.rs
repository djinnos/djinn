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
use crate::repositories::ci_incomplete_hold::{
    CI_INCOMPLETE_HOLD_MAX_POLLS, CiHoldApply, CiHoldEscalationRoute, CiHoldIdentity,
    CiIncompleteHoldRepository,
};
use crate::repositories::ci_route_attempt::{
    CI_CALLING_RECOVERY_TIMEOUT_SECS, CI_HEAD_BUDGET_LIMIT, CI_SIGNATURE_BUDGET_LIMIT, CiAction,
    CiActionPhase, CiCallingRecovery, CiCallingRecoveryAuthority, CiCallingRecoveryReason,
    CiChargeOutcome, CiClass, CiDiagnosticReason, CiEvidenceIdentity, CiLane, CiOriginState,
    CiQuiescenceProof, CiReopenMode, CiReserveOutcome, CiReservedRecovery, CiRouteAttempt,
    CiRouteAttemptRepository, CiRouteOutcome, CiRouteReservation, CiRouteSubject,
    CiTier2LeaseOutcome, CiTier2LeaseState, CiTier2Reason, CiTier2Resolution,
};
use crate::repositories::ci_route_attempt::{CiLeadRejection, CiLeadSessionAttachment};
use crate::repositories::ci_route_report::{CiRouteQuiescenceAttestation, CiRouteReportFilter};
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
        run_id: Some(run_id),
        run_head_sha: head.to_owned(),
        dequeue_id: None,
    }
}

fn merge_group_identity(run_id: i64, head: &str, dequeue: &str) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: 4242,
        pr_head_sha: head.to_owned(),
        run_id: Some(run_id),
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

/// Migration 193 round trip: every column the repository binds survives a
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
    // NULL is the honest spelling of "no merge observed". A reserved route has
    // seen none, and the column must survive reload as absent rather than as a
    // zero timestamp.
    assert!(loaded.pr_merged_at.is_none());

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

/// The quiescence vocabulary is exactly one proof and one absence, in Rust and
/// in the schema alike.
///
/// Migration 196 retired `process_terminated`. It was never producible: the
/// only trace an abrupt death leaves behind is the automatic release of the
/// coordinator advisory lock, and Postgres performs that release at backend
/// termination — before the dying (or merely disconnected) client can react —
/// so it proves the connection went away and never that the process did.
/// Closing that gap needs elapsed time, which AC5 forbids.
///
/// A dead value that the schema still calls legal is not inert: the next reader
/// wires a producer to it, which is exactly the elapsed-time inference this
/// wave removed. So both halves are pinned here.
///
/// NAMED FAILING MUTATIONS. (a) Re-add `ProcessTerminated => "process_terminated"`
/// to the `CiQuiescenceProof` `durable_enum!`: `parse` starts answering `Ok`
/// and the first assertion fails. (b) Restore migration 193's wider CHECK (or
/// drop 196's narrowed one): the INSERT succeeds and the second assertion
/// fails. Neither mutation is caught by any other fixture, because every other
/// caller of this repository now spells only `GracefulDrain` and `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_death_is_not_a_quiescence_proof_in_rust_or_in_the_schema() {
    let f = fixture().await;
    let repository = repo(&f.db);

    assert!(
        CiQuiescenceProof::parse("process_terminated").is_err(),
        "`process_terminated` must not round trip: a spelling the enum accepts \
         is a spelling a witness can be written for"
    );
    // Vacuity: `parse` really does accept the vocabulary that survives, so the
    // assertion above is about the retired value rather than about `parse`
    // rejecting everything.
    assert_eq!(
        CiQuiescenceProof::parse("graceful_drain").unwrap(),
        CiQuiescenceProof::GracefulDrain
    );
    assert_eq!(
        CiQuiescenceProof::parse("none").unwrap(),
        CiQuiescenceProof::None
    );

    // And the database refuses it independently of Rust. The audit table has a
    // foreign key onto the route, so a real reserved row goes in first —
    // otherwise the INSERT would fail for the wrong reason and this would pass
    // vacuously.
    let input = reservation(
        &f.subject,
        "key-vocabulary",
        pr_head_identity(1, "headsha-vocabulary"),
        "fp-a",
    );
    repository.reserve(&input).await.unwrap();

    let insert = |proof: &'static str| {
        let db = f.db.clone();
        let subject_id = f.subject.id.clone();
        async move {
            sqlx::query(
                r#"INSERT INTO ci_route_calling_recoveries
                     (id, subject_kind, subject_id, provider_action_key,
                      recovering_incarnation, holds_exclusive_lock, quiescence_proof,
                      recovery_reason, calling_recovery_timeout_secs, cas_won)
                   VALUES ($1, 'task', $2, 'key-vocabulary', $3, TRUE, $4,
                           'live_owner_deferred', 300, FALSE)"#,
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(subject_id)
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(proof)
            .execute(db.pool())
            .await
        }
    };

    assert!(
        insert("process_terminated").await.is_err(),
        "the CHECK must refuse the retired spelling"
    );
    // Vacuity again: the same INSERT with a surviving spelling succeeds, so the
    // refusal above is the CHECK and not a malformed statement.
    insert("none")
        .await
        .expect("the surviving vocabulary still inserts");
}

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
    //
    // The proof passed here and in cases 4 and 5 is the *strongest* one the
    // vocabulary has. That is the point: each of those deferrals must come
    // from its own named predicate, so handing the call a proof that would
    // otherwise be sufficient is what stops the assertion passing because the
    // quiescence gate happened to refuse first.
    let deferred = sweeper
        .recover_calling_owner(
            &f.subject,
            "key-live",
            &identity,
            &authority(&owner, &recovering, CiQuiescenceProof::GracefulDrain, false),
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

    // 4. The drain is now genuinely recorded, so the quiescence gate is
    //    satisfied — but `calling_at` has not aged past the floor.
    //
    //    Completing the owner's drain here is what makes this case test the
    //    floor rather than re-test case 3: without it the call would defer on
    //    `LiveOwnerDeferred` again and `TimeoutNotElapsed` would be
    //    unreachable from this fixture.
    let incarnations = CoordinatorIncarnationRepository::new(f.db.clone());
    assert!(incarnations.mark_draining(&owner).await.unwrap());
    assert!(
        incarnations
            .mark_provider_actions_drained(&owner)
            .await
            .unwrap()
    );
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
    assert_deferred(&deferred, CiCallingRecoveryReason::TimeoutNotElapsed);

    // 5. Aged out and provably drained, but fenced to the wrong former owner.
    age_calling(&f.db, "key-live", CI_CALLING_RECOVERY_TIMEOUT_SECS + 60).await;
    let deferred = sweeper
        .recover_calling_owner(
            &f.subject,
            "key-live",
            &identity,
            &authority(
                &incarnation(),
                &recovering,
                CiQuiescenceProof::GracefulDrain,
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
                CiQuiescenceProof::GracefulDrain,
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
    assert_eq!(
        restarted
            .attach_lead_session(&f.subject, "key-lease-1", &lease_id, "session-alpha")
            .await
            .unwrap(),
        CiLeadSessionAttachment::Attached { session_count: 1 }
    );
    assert_eq!(
        restarted
            .attach_lead_session(&f.subject, "key-lease-1", "not-the-lease", "session-beta")
            .await
            .unwrap(),
        CiLeadSessionAttachment::NotFound
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
            rejection: None,
        },
        CiTier2Resolution {
            outcome: CiRouteOutcome::DiagnosticReopened,
            reopen_mode: Some(CiReopenMode::Diagnose),
            diagnostic_reason: None,
            park_justification: None,
            rejection: None,
        },
        CiTier2Resolution {
            outcome: CiRouteOutcome::Parked,
            reopen_mode: None,
            diagnostic_reason: None,
            park_justification: Some("   ".to_owned()),
            rejection: None,
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
    assert_eq!(
        repository
            .attach_lead_session(&f.subject, "key-both", &lease_id, "session-both")
            .await
            .unwrap(),
        CiLeadSessionAttachment::Attached { session_count: 1 }
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
// A live `calling` row belongs to its owner, whichever door you knock on
// ---------------------------------------------------------------------------

/// The Tier-2 doors must not steal a row whose provider call is in flight.
///
/// `open_tier2_lease` used to: handed a superseding observed identity, its
/// guard branch terminalized the row `superseded_before_lead` even in
/// `calling`, and the owner's later `finalize_calling` then returned `false`
/// and dropped a real provider result on the floor with nothing reported. That
/// is the same steal already fenced out of `terminalize` and
/// `close_routes_for_newer_outcome`; this proves the third and fourth doors
/// are shut too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_two_doors_never_close_a_live_calling_row() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-inflight");
    let input = reservation(&f.subject, "key-inflight", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-inflight", &owner, &identity)
        .await
        .unwrap();

    // The PR head moved while the provider call was in flight. This is the
    // exact input that used to steal the row.
    let superseding = pr_head_identity(2, "headsha-moved");
    let outcome = repository
        .open_tier2_lease(
            &f.subject,
            "key-inflight",
            &superseding,
            &tier2_lease_key(4242, "headsha-moved"),
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, CiTier2LeaseOutcome::OwnedByProviderCall(_)),
        "a row in calling belongs to its owner, got {outcome:?}"
    );

    // Nothing moved: not the phase, not the owner, not the lease.
    let row = repository
        .get(&f.subject, "key-inflight")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.action_phase, CiActionPhase::Calling);
    assert!(row.terminal_outcome.is_none());
    assert_eq!(row.owner_incarnation_id.as_deref(), Some(owner.as_str()));
    assert!(!row.has_routed_to_tier2());

    // The specific durable lie this prevents. The row is CHARGED — a provider
    // call really was authorized and really did execute — so recording it as
    // `superseded_before_lead` would mean the table says "we never called the
    // provider" about a row whose own counters say we did. Being authoritative
    // about that one fact is the entire reason this table exists.
    assert_eq!(row.charged_signature_count, Some(1));
    assert_eq!(row.charged_head_count, Some(1));
    assert!(
        row.superseded_by_evidence.is_none(),
        "a charged, in-flight call must never be recorded as a supersession"
    );

    // THE POINT: the owner's provider result still lands. Before the fix this
    // returned `false` and the result was lost.
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-inflight",
                &owner,
                CiRouteOutcome::Retriggered,
                None,
            )
            .await
            .unwrap(),
        "the owner's finalizer must still win; a stolen row makes it a silent no-op"
    );
    let row = repository
        .get(&f.subject, "key-inflight")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Retriggered));
}

/// The same fence in the helper every non-owner terminalization funnels
/// through, exercised directly rather than via one public method.
///
/// A row that legally holds an open Tier-2 lease is charged to `calling` out
/// from under the adjudication, and the delayed Lead result then arrives with
/// a superseding identity — the branch that terminalizes
/// `superseded_before_apply`. It must apply nothing and close nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delayed_lead_result_cannot_close_a_row_that_is_now_calling() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(1, "headsha-delayed");
    let input = reservation(&f.subject, "key-delayed", identity.clone(), "fp-a");
    repository.reserve(&input).await.unwrap();

    let opened = repository
        .open_tier2_lease(
            &f.subject,
            "key-delayed",
            &identity,
            &tier2_lease_key(4242, "headsha-delayed"),
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = opened else {
        panic!("expected a lease on a reserved row");
    };
    repository
        .attach_lead_session(&f.subject, "key-delayed", &lease_id, "session-delayed")
        .await
        .unwrap();

    // The row is charged to `calling` while Lead is thinking.
    let owner = incarnation();
    let charged = repository
        .charge_and_begin_calling(&f.subject, "key-delayed", &owner, &identity)
        .await
        .unwrap();
    assert!(matches!(charged, CiChargeOutcome::Charged { .. }));

    // Lead answers late, against evidence that has since moved.
    let applied = repository
        .resolve_tier2_lease(
            &f.subject,
            "key-delayed",
            &lease_id,
            &pr_head_identity(9, "headsha-elsewhere"),
            &CiTier2Resolution::repair(),
        )
        .await
        .unwrap();
    assert!(
        !applied,
        "a delayed result applies nothing to a calling row"
    );

    let row = repository
        .get(&f.subject, "key-delayed")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.action_phase,
        CiActionPhase::Calling,
        "the row must still belong to its provider-call owner"
    );
    assert!(row.terminal_outcome.is_none());
    assert!(row.reopen_mode.is_none());
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-delayed",
                &owner,
                CiRouteOutcome::ActionFailed,
                None,
            )
            .await
            .unwrap(),
        "the owner's finalizer must still win"
    );
}

/// The Tier-2 head hold is **per subject**, not global — the documented
/// consequence of scoping every key by subject.
///
/// This is not a bug to fix; it is the price of making a key collision unable
/// to swallow a foreign route, and it is invisible while every subject is a
/// task and one PR maps to one task. The test exists so the boundary is
/// pinned: if someone later widens the index to make the hold global, this
/// fails and they are forced to notice the lost-route class reopening.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tier_two_head_hold_stops_at_the_subject_boundary() {
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

    let head = "headsha-shared-hold";
    let identity = pr_head_identity(1, head);
    let lease_key = tier2_lease_key(4242, head);

    for (subject, key) in [(&f.subject, "key-hold-mine"), (&other, "key-hold-theirs")] {
        repository
            .reserve(&reservation(subject, key, identity.clone(), "fp-a"))
            .await
            .unwrap();
        let outcome = repository
            .open_tier2_lease(
                subject,
                key,
                &identity,
                &lease_key,
                CiTier2Reason::CausalFailure,
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, CiTier2LeaseOutcome::Opened { .. }),
            "the hold is per subject, so `{key}` must get its own adjudication"
        );
    }

    // Two Lead adjudications, one PR head, two subjects. Documented in
    // migration 193 as what the first non-task subject inherits.
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ci_route_attempts WHERE tier2_lease_state = 'open'",
    )
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(open, 2);

    // And the head budget is per subject too: the same head key charges
    // independently on each side.
    let head_key = format!("head:4242:{head}");
    for subject in [&f.subject, &other] {
        let counts = repository
            .budget_counts(subject, "sig:unused", &head_key)
            .await
            .unwrap();
        assert_eq!(
            counts.head, 0,
            "each subject starts its own head budget for the same PR head"
        );
    }
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
        spent.run_id = Some(run);
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

// ---------------------------------------------------------------------------
// Wave 5: reporting and the rollback quiescence report
// ---------------------------------------------------------------------------

/// Reopen and park counts must union **both** outcome columns.
///
/// Terminalization is write-once, so a route that already terminalized on its
/// provider result keeps `terminal_outcome = action_failed` and records the
/// Lead adjudication in `tier2_resolution`. A report keyed on
/// `terminal_outcome` alone therefore drops **every reopen that followed a
/// provider failure** — silently, and exactly the population an operator is
/// looking at when they ask why routing is spending worker sessions.
///
/// This fixture builds one of each so the union is load-bearing: mutate
/// `COALESCE(tier2_resolution, terminal_outcome)` back to `terminal_outcome`
/// and `repair_reopens` drops from 2 to 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_counts_union_both_outcome_columns() {
    let f = fixture().await;
    let repository = repo(&f.db);

    // Route A: a plain Tier-2 route whose terminal outcome IS the adjudication.
    let a = pr_head_identity(401, "headaaa");
    repository
        .reserve(&reservation(&f.subject, "key-a", a.clone(), "fp-a"))
        .await
        .unwrap();
    let lease_a = open_lease(&repository, &f.subject, "key-a", &a, "lease-a").await;
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-a",
                &lease_a,
                &a,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );

    // Route B: charged, called, failed at the provider — so it terminalized on
    // `action_failed` BEFORE Lead ever saw it — and then reopened.
    let b = pr_head_identity(402, "headaaa");
    repository
        .reserve(&reservation(&f.subject, "key-b", b.clone(), "fp-b"))
        .await
        .unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-b", &owner, &b)
        .await
        .unwrap();
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-b",
                &owner,
                CiRouteOutcome::ActionFailed,
                Some(r#"{"status":500}"#),
            )
            .await
            .unwrap()
    );
    let lease_b = open_lease(&repository, &f.subject, "key-b", &b, "lease-b").await;
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-b",
                &lease_b,
                &b,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );

    let b_row = repository
        .get(&f.subject, "key-b")
        .await
        .unwrap()
        .expect("route b");
    assert_eq!(
        b_row.terminal_outcome,
        Some(CiRouteOutcome::ActionFailed),
        "the provider outcome is write-once and must survive the adjudication"
    );
    assert_eq!(
        b_row.tier2_resolution,
        Some(CiRouteOutcome::RepairReopened),
        "so the reopen lives in the OTHER column — this is the whole trap"
    );

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        report.repair_reopens, 2,
        "both reopens must be counted; reading terminal_outcome alone finds one"
    );
    assert_eq!(
        report.worker_reopens, 2,
        "two reopens are two worker dispatches, wherever they were recorded"
    );
    assert_eq!(
        report.provider_action_failures, 1,
        "and the provider failure is still counted on its own column"
    );
}

/// The three obsolete-suppression boundaries are reported **separately**.
///
/// They are different costs: pre-call spent nothing, before-lead wasted a
/// lease, before-apply wasted a whole Lead session. Collapsing them into one
/// "stale" number hides which of the three is happening.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_three_suppression_boundaries_are_reported_separately() {
    let f = fixture().await;
    let repository = repo(&f.db);

    // Before the provider call: the head moved between reserve and charge.
    let pre = pr_head_identity(501, "oldhead");
    repository
        .reserve(&reservation(&f.subject, "key-pre", pre.clone(), "fp"))
        .await
        .unwrap();
    let moved = pr_head_identity(501, "newhead");
    repository
        .charge_and_begin_calling(&f.subject, "key-pre", &incarnation(), &moved)
        .await
        .unwrap();

    // Before Lead dispatch: the lease request itself loses the compare-and-set.
    let lead = pr_head_identity(502, "oldhead");
    repository
        .reserve(&reservation(&f.subject, "key-lead", lead.clone(), "fp2"))
        .await
        .unwrap();
    let lead_moved = pr_head_identity(502, "newhead");
    let outcome = repository
        .open_tier2_lease(
            &f.subject,
            "key-lead",
            &lead_moved,
            "lease-key-502",
            CiTier2Reason::CausalFailure,
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        CiTier2LeaseOutcome::SupersededBeforeLead(_)
    ));

    // Before supervisor apply: the lease opened, Lead answered, and the head
    // moved in between.
    let apply = pr_head_identity(503, "oldhead");
    repository
        .reserve(&reservation(&f.subject, "key-apply", apply.clone(), "fp3"))
        .await
        .unwrap();
    let lease = open_lease(&repository, &f.subject, "key-apply", &apply, "lease-503").await;
    let apply_moved = pr_head_identity(503, "newhead");
    assert!(
        !repository
            .resolve_tier2_lease(
                &f.subject,
                "key-apply",
                &lease,
                &apply_moved,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(report.suppressed_before_provider_call, 1);
    assert_eq!(report.suppressed_before_lead_dispatch, 1);
    assert_eq!(report.suppressed_before_supervisor_apply, 1);
    assert_eq!(report.total_obsolete_suppressions(), 3);
    assert_eq!(
        report.worker_reopens, 0,
        "not one of the three may dispatch a worker"
    );
}

/// A Lead timeout and a delivered `no_grounded_remedy` write the same
/// `diagnostic_reason` and must still be countable apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reporting_separates_an_absent_lead_result_from_a_delivered_diagnosis() {
    let f = fixture().await;
    let repository = repo(&f.db);

    let delivered = pr_head_identity(601, "headddd");
    repository
        .reserve(&reservation(&f.subject, "key-d", delivered.clone(), "fp-d"))
        .await
        .unwrap();
    let lease_d = open_lease(&repository, &f.subject, "key-d", &delivered, "lease-d").await;
    repository
        .resolve_tier2_lease(
            &f.subject,
            "key-d",
            &lease_d,
            &delivered,
            &CiTier2Resolution::diagnose(CiDiagnosticReason::NoGroundedRemedy),
        )
        .await
        .unwrap();

    let absent = pr_head_identity(602, "headddd");
    repository
        .reserve(&reservation(&f.subject, "key-t", absent.clone(), "fp-t"))
        .await
        .unwrap();
    let lease_t = open_lease(&repository, &f.subject, "key-t", &absent, "lease-t").await;
    repository
        .resolve_tier2_lease(
            &f.subject,
            "key-t",
            &lease_t,
            &absent,
            &CiTier2Resolution::diagnose(CiDiagnosticReason::NoGroundedRemedy)
                .rejected_as(CiLeadRejection::TimedOut),
        )
        .await
        .unwrap();

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(report.diagnostic_reopens, 2);
    assert_eq!(
        report.diagnostic_reopens_from_absent_result, 1,
        "the timeout must be countable as `Lead never answered`"
    );
    assert_eq!(
        report.diagnostic_reopens_from_rejected_result, 0,
        "a timeout is not a refused result"
    );
}

/// A rejection may only ride a diagnostic reopen. A repair that claims one is
/// refused by the repository before the CHECK ever sees it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejection_cannot_ride_a_result_lead_actually_produced() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(701, "headeee");
    repository
        .reserve(&reservation(&f.subject, "key-r", identity.clone(), "fp-r"))
        .await
        .unwrap();
    let lease = open_lease(&repository, &f.subject, "key-r", &identity, "lease-r").await;
    let error = repository
        .resolve_tier2_lease(
            &f.subject,
            "key-r",
            &lease,
            &identity,
            &CiTier2Resolution::repair().rejected_as(CiLeadRejection::TimedOut),
        )
        .await
        .expect_err("a repair carrying a rejection is a contradiction");
    assert!(
        error.to_string().contains("rejection"),
        "the refusal must name the reason, got {error}"
    );
    let row = repository.get(&f.subject, "key-r").await.unwrap().unwrap();
    assert!(
        row.holds_open_tier2_lease(),
        "a refused resolution must not have half-applied"
    );
}

/// The lane and time-window filters actually scope the report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_report_is_scoped_by_lane_and_window() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let head = pr_head_identity(801, "headfff");
    repository
        .reserve(&reservation(&f.subject, "key-h", head, "fp-h"))
        .await
        .unwrap();
    let group = merge_group_identity(802, "headfff", "dq-802");
    repository
        .reserve(&reservation(&f.subject, "key-g", group, "fp-g"))
        .await
        .unwrap();

    let all = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(all.action_rerun_run + all.action_reenqueue, 2);

    let head_only = repository
        .route_report(&CiRouteReportFilter::all().lane(CiLane::PrHead))
        .await
        .unwrap();
    assert_eq!(head_only.action_rerun_run, 1);
    assert_eq!(
        head_only.action_reenqueue, 0,
        "the lane filter must exclude the merge-group route"
    );

    // A window entirely in the past contains neither.
    let empty = repository
        .route_report(&CiRouteReportFilter::all().until("2000-01-01T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(empty.action_rerun_run, 0);
    assert_eq!(empty.action_reenqueue, 0);
}

/// Rollback is blocked while anything is in flight, and the report says which
/// thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollback_report_blocks_on_every_non_zero_count() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(901, "headggg");
    repository
        .reserve(&reservation(&f.subject, "key-q", identity.clone(), "fp-q"))
        .await
        .unwrap();

    // A reserved row alone blocks.
    let blocked = repository
        .record_rollback_quiescence_report(
            "quiescing",
            "inc-1",
            CiRouteQuiescenceAttestation {
                registered_provider_futures: 0,
                current_failed_identities: 0,
            },
        )
        .await
        .unwrap();
    assert!(!blocked.permits_rollback);
    assert_eq!(blocked.reserved_rows, 1);
    assert!(
        blocked
            .blocking_reasons()
            .iter()
            .any(|reason| reason.contains("reserved rows")),
        "the report must name what is blocking, got {:?}",
        blocked.blocking_reasons()
    );

    // Terminalize it, but attest a live provider future. Still blocked, and by
    // a count SQL cannot see.
    repository
        .terminalize(&f.subject, "key-q", CiRouteOutcome::Held, None)
        .await
        .unwrap();
    let still_blocked = repository
        .record_rollback_quiescence_report(
            "quiescing",
            "inc-1",
            CiRouteQuiescenceAttestation {
                registered_provider_futures: 1,
                current_failed_identities: 0,
            },
        )
        .await
        .unwrap();
    assert!(
        !still_blocked.permits_rollback,
        "a live provider-action future blocks a rollback even with a clean table"
    );
    assert_eq!(still_blocked.reserved_rows, 0);
    assert!(
        still_blocked
            .blocking_reasons()
            .iter()
            .any(|reason| reason.contains("provider-action futures"))
    );
}

/// An `enabled` gate can never permit a rollback, whatever the counts say.
///
/// **Mutation target.** Drop the `gate_state != "enabled"` conjunct and this
/// goes red; without it a report taken mid-flight would authorize a rollback
/// against a table that is still being written to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enabled_gate_never_permits_a_rollback() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let clean = CiRouteQuiescenceAttestation {
        registered_provider_futures: 0,
        current_failed_identities: 0,
    };
    let enabled = repository
        .record_rollback_quiescence_report("enabled", "inc-1", clean)
        .await
        .unwrap();
    assert!(
        !enabled.permits_rollback,
        "new routes are still being admitted; the counts are a moving target"
    );
    assert!(!enabled.recomputed_verdict());

    let quiescing = repository
        .record_rollback_quiescence_report("quiescing", "inc-1", clean)
        .await
        .unwrap();
    assert!(quiescing.permits_rollback, "the same counts, draining");
    assert!(quiescing.recomputed_verdict());

    // The report is durable and the latest one wins.
    let latest = repository
        .latest_rollback_quiescence_report()
        .await
        .unwrap()
        .expect("a report was recorded");
    assert_eq!(latest.id, quiescing.id);
    assert!(latest.permits_rollback);
}

/// The six counts a rollback report stores, in the order the table declares
/// them.
const ROLLBACK_COUNT_LABELS: [&str; 6] = [
    "reserved_rows",
    "calling_rows",
    "open_tier2_leases",
    "unapplied_lead_results",
    "registered_provider_futures",
    "current_failed_identities",
];

/// Insert one rollback report **directly**, bypassing the writer that computes
/// the verdict, so the database's own refusal is what is under test.
async fn insert_rollback_report(
    db: &Database,
    gate_state: &str,
    counts: [i64; 6],
    permits_rollback: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ci_route_rollback_reports (
             id, gate_state, reserved_rows, calling_rows, open_tier2_leases,
             unapplied_lead_results, registered_provider_futures,
             current_failed_identities, permits_rollback, recorded_by_incarnation
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'inc-verdict-check')",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(gate_state)
    .bind(counts[0])
    .bind(counts[1])
    .bind(counts[2])
    .bind(counts[3])
    .bind(counts[4])
    .bind(counts[5])
    .bind(permits_rollback)
    .execute(db.pool())
    .await
    .map(|_| ())
}

#[track_caller]
fn assert_verdict_check_refused(result: &Result<(), sqlx::Error>, what: &str) {
    let error = match result {
        Ok(()) => panic!(
            "{what}: the database accepted a rollback report whose stored verdict \
             disagrees with its own counts — the report an operator points at can \
             now say `permits_rollback` over a live route"
        ),
        Err(error) => error,
    };
    let constraint = error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint);
    assert_eq!(
        constraint,
        Some("ci_route_rollback_reports_verdict_check"),
        "{what}: refused, but by something other than the verdict CHECK: {error}",
    );
}

/// The verdict is enforced by the **database**, not only by the code that
/// computes it.
///
/// # Why this is not covered by the writer's own fixtures
///
/// `record_rollback_quiescence_report` derives `permits_rollback` from the six
/// counts and then stores both, so every test that goes through it necessarily
/// agrees with itself. The CHECK exists for the case where something else
/// writes the row — a repair script, a backfill, a future writer that computes
/// the verdict from five of the six counts — and until now the only thing
/// standing behind it was `migrations_immutable`'s file-hash pin. A later
/// migration issuing `ALTER TABLE … DROP CONSTRAINT` changes no hashed file and
/// would have gone unnoticed, and the constraint is precisely what stops a
/// green report being persisted over a live `calling` row.
///
/// Migration 195 is merged and immutable; this tests it as it stands.
///
/// NAMED FAILING MUTATIONS. Add a migration that drops
/// `ci_route_rollback_reports_verdict_check`, or weakens it from an equality to
/// an implication (`permits_rollback = false OR (…)`): the first six cases and
/// the `enabled` case still fail on `Ok(())`, and the "clean counts, `false`
/// verdict" case is the one that specifically kills the implication form.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_verdict_check_refuses_a_report_that_disagrees_with_its_counts() {
    let f = fixture().await;
    const CLEAN: [i64; 6] = [0; 6];

    // Vacuity: the honest rows this constraint is shaped around are accepted,
    // so the refusals below are about the disagreement and not about the
    // statement being malformed.
    insert_rollback_report(&f.db, "quiescing", CLEAN, true)
        .await
        .expect("a clean drain with a `true` verdict is the row the gate exists to produce");
    insert_rollback_report(&f.db, "quiescing", [1, 0, 0, 0, 0, 0], false)
        .await
        .expect("and a blocked drain with a `false` verdict is equally legal");

    // Each count, one at a time: a `true` verdict over a non-zero count is the
    // green report that would authorize stranding whatever that count names.
    for (index, label) in ROLLBACK_COUNT_LABELS.iter().enumerate() {
        let mut counts = CLEAN;
        counts[index] = 1;
        let result = insert_rollback_report(&f.db, "quiescing", counts, true).await;
        assert_verdict_check_refused(&result, &format!("a `true` verdict over 1 {label}"));
    }

    // The gate posture is part of the same function: routes are still being
    // admitted, so the counts are a snapshot of a moving target.
    let enabled = insert_rollback_report(&f.db, "enabled", CLEAN, true).await;
    assert_verdict_check_refused(
        &enabled,
        "a `true` verdict taken while the gate is `enabled`",
    );

    // And it is an EQUALITY, not an implication. A `false` verdict over clean
    // counts is just as refused — otherwise a writer could weaken the
    // constraint to a one-way check and the stored verdict would stop being a
    // function of the row at all.
    let understated = insert_rollback_report(&f.db, "quiescing", CLEAN, false).await;
    assert_verdict_check_refused(&understated, "a `false` verdict over clean counts");

    // Only the two honest rows survived.
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_route_rollback_reports")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(
        stored, 2,
        "every refused insert must have been refused, not merely reported"
    );
}

/// The evidence-advance high-watermark: a routed identity that is still the
/// current failed evidence for its lane blocks a rollback, and a superseded or
/// passed one does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_evidence_advance_watermark_counts_only_unadvanced_identities() {
    let f = fixture().await;
    let repository = repo(&f.db);

    let old = pr_head_identity(1001, "oldhead");
    repository
        .reserve(&reservation(&f.subject, "key-old", old.clone(), "fp"))
        .await
        .unwrap();
    assert_eq!(
        repository.current_failed_identity_count().await.unwrap(),
        1,
        "one routed identity, nothing newer, not passed: it is still current"
    );

    // The head moves and a newer route is reserved for the same lane and PR.
    let new = pr_head_identity(1002, "newhead");
    // (Both rows carry PR 4242; only the head SHA and the run id differ, which
    // is exactly the "advanced to distinct newer provider evidence" the
    // watermark is asking about.)
    repository
        .reserve(&reservation(&f.subject, "key-new", new.clone(), "fp"))
        .await
        .unwrap();
    assert_eq!(
        repository.current_failed_identity_count().await.unwrap(),
        1,
        "the OLD identity advanced; the NEW one is now the current failed evidence"
    );

    // The PR goes green, which closes every reserved route on it as `passed`.
    // (`pr_head_identity` fixes the PR number at 4242; the varying argument is
    // the run id, which is what makes the two identities distinct.)
    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Passed, None)
        .await
        .unwrap();
    assert_eq!(
        repository.current_failed_identity_count().await.unwrap(),
        0,
        "every routed identity has advanced or reached a passing state"
    );
}

/// Helper: open a Tier-2 lease and return its id.
async fn open_lease(
    repository: &CiRouteAttemptRepository,
    subject: &CiRouteSubject,
    key: &str,
    identity: &CiEvidenceIdentity,
    lease_key: &str,
) -> String {
    match repository
        .open_tier2_lease(
            subject,
            key,
            identity,
            lease_key,
            CiTier2Reason::CausalFailure,
        )
        .await
        .expect("open lease")
    {
        CiTier2LeaseOutcome::Opened { lease_id, .. } => lease_id,
        other => panic!("expected an opened lease, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Wave 5, revision 58: the run-absent identity, the append-only Lead session
// count, the database-side guards, and the bounded incomplete-evidence hold.
//
// Every fixture below was mutation-proven: the guard it names was temporarily
// broken (trigger dropped, CHECK dropped, comparison inverted, overwrite
// restored) and the fixture was confirmed to FAIL before the guard was
// restored. A test that survives the deletion of the thing it tests is not
// evidence.
// ---------------------------------------------------------------------------

/// An evidence identity that **names no run**: the honest encoding for a lane
/// capture that failed closed before runs were attributed.
fn run_absent_identity(lane: CiLane, head: &str, dequeue: Option<&str>) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane,
        pr_number: 4242,
        pr_head_sha: head.to_owned(),
        run_id: None,
        run_head_sha: head.to_owned(),
        dequeue_id: dequeue.map(str::to_owned),
    }
}

/// A reservation whose action is `ask_lead` — the only action a run-absent
/// route may carry, since there is nothing to re-run.
fn ask_lead_reservation(
    subject: &CiRouteSubject,
    key: &str,
    identity: CiEvidenceIdentity,
    fingerprint: &str,
) -> CiRouteReservation {
    let mut input = reservation(subject, key, identity, fingerprint);
    input.action = CiAction::AskLead;
    input.class = CiClass::Unknown;
    input
}

fn hold_identity(subject: &CiRouteSubject, head: &str) -> CiHoldIdentity {
    CiHoldIdentity {
        subject: subject.clone(),
        repository_id: "djinnos/djinn".to_owned(),
        pr_number: 4242,
        pr_head_sha: head.to_owned(),
        lane: CiLane::PrHead,
        dequeue_id: None,
    }
}

/// The one diagnose-only run-absent route an escalating hold inserts.
fn escalation_route(subject: &CiRouteSubject, head: &str) -> CiHoldEscalationRoute {
    CiHoldEscalationRoute {
        reservation: ask_lead_reservation(
            subject,
            &format!("key-escalation-{head}"),
            run_absent_identity(CiLane::PrHead, head, None),
            "fp-hold",
        ),
        tier2_lease_id: format!("lease-hold-{head}"),
        tier2_lease_key: tier2_lease_key(4242, head),
        tier2_reason: CiTier2Reason::EvidenceUnknown,
    }
}

/// One complete poll: reserve, (pretend to enumerate), apply.
async fn one_poll(
    holds: &CiIncompleteHoldRepository,
    identity: &CiHoldIdentity,
    escalation: &CiHoldEscalationRoute,
    complete: bool,
) -> CiHoldApply {
    let poll_id = uuid::Uuid::now_v7().to_string();
    holds
        .reserve_poll(identity, &poll_id)
        .await
        .expect("reserve a poll sequence");
    holds
        .apply_poll(identity, identity, &poll_id, complete, escalation)
        .await
        .expect("apply a poll")
}

async fn count_rows(db: &Database, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(db.pool())
        .await
        .expect("count rows")
}

// ---------------------------------------------------------------------------
// The run-absent identity
// ---------------------------------------------------------------------------

/// The `run_id = 0` sentinel is unrepresentable, and two run-absent captures of
/// one lane/PR/head/dequeue collapse onto **one** row.
///
/// The collapse is the `NULLS NOT DISTINCT` claim. Under the default
/// `NULLS DISTINCT` each run-absent row is unique to itself, so two different
/// irrecoverable reasons would each open a route, each take a Tier-2 lease, and
/// "at most one provider-call episode per evidence identity" would hold only
/// for identities that happened to name a run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_absent_identities_reject_the_sentinel_and_collapse_onto_one_row() {
    let f = fixture().await;
    let repository = repo(&f.db);

    // (a) The sentinel and every other non-positive run id are refused before
    //     the CHECK fires, with a message that names the field.
    for sentinel in [0_i64, -1] {
        let mut identity = pr_head_identity(1, "headsha-sentinel");
        identity.run_id = Some(sentinel);
        let err = repository
            .reserve(&reservation(
                &f.subject,
                &format!("key-sentinel-{sentinel}"),
                identity,
                "fp-sentinel",
            ))
            .await
            .expect_err("a non-positive run id is not a provider run");
        let message = err.to_string();
        assert!(
            message.contains("run_id") && message.contains("None"),
            "the refusal must name the field and the correct encoding, got: {message}"
        );
    }
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_route_attempts").await,
        0,
        "a refused reservation writes nothing"
    );

    // (b) The database refuses the sentinel too, whatever writes it. This is
    //     the enforcement; the Rust guard above is only the readable error.
    let raw = sqlx::query(
        "INSERT INTO ci_route_attempts (subject_kind, subject_id, provider_action_key, lane, \
           pr_number, pr_head_sha, run_id, run_head_sha, origin_state, class, action, \
           transient_fingerprint, retry_budget_key, head_budget_key, action_phase) \
         VALUES ('task', $1, 'key-raw-sentinel', 'pr_head', 4242, 'h', 0, 'h', 'pr_draft', \
                 'unknown', 'ask_lead', 'fp', 'sig', 'head', 'reserved')",
    )
    .bind(&f.task_id)
    .execute(f.db.pool())
    .await
    .expect_err("ci_route_attempts_run_id_positive_check");
    assert!(
        raw.to_string().contains("run_id_positive"),
        "expected the positivity CHECK, got: {raw}"
    );

    // (c) Two DIFFERENT irrecoverable reasons on one lane/PR/head/dequeue.
    //     Different caller-computed keys, one identity.
    let identity = run_absent_identity(CiLane::PrHead, "headsha-absent", None);
    let first = repository
        .reserve(&ask_lead_reservation(
            &f.subject,
            "key-reason-truncated",
            identity.clone(),
            "fp-absent",
        ))
        .await
        .unwrap();
    let first = match first {
        CiReserveOutcome::Reserved(attempt) => *attempt,
        other => panic!("expected a fresh reservation, got {other:?}"),
    };
    assert!(first.is_run_absent());

    let second = repository
        .reserve(&ask_lead_reservation(
            &f.subject,
            "key-reason-missing-timestamp",
            identity.clone(),
            "fp-absent",
        ))
        .await
        .unwrap();
    match second {
        CiReserveOutcome::AlreadyPresent(attempt) => assert_eq!(
            attempt.provider_action_key, "key-reason-truncated",
            "the second reason collapses onto the FIRST reason's row"
        ),
        other => panic!("two run-absent captures of one identity are one identity, got {other:?}"),
    }

    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_route_attempts").await,
        1,
        "NULLS NOT DISTINCT: one identity, one row, one route"
    );

    // A genuinely different identity still gets its own row, so the collapse is
    // not simply "run-absent rows all collide".
    let elsewhere = run_absent_identity(CiLane::PrHead, "headsha-elsewhere", None);
    assert!(matches!(
        repository
            .reserve(&ask_lead_reservation(
                &f.subject,
                "key-other-head",
                elsewhere,
                "fp-absent",
            ))
            .await
            .unwrap(),
        CiReserveOutcome::Reserved(_)
    ));
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_route_attempts").await,
        2
    );
}

/// A run-absent route is **diagnose-only**: the repair reopen is refused and
/// the diagnose reopen is accepted, on the same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repair_reopen_on_a_run_absent_route_is_rejected() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = run_absent_identity(CiLane::PrHead, "headsha-diagnose-only", None);
    repository
        .reserve(&ask_lead_reservation(
            &f.subject,
            "key-absent",
            identity.clone(),
            "fp-absent",
        ))
        .await
        .unwrap();
    let lease_id = open_lease(
        &repository,
        &f.subject,
        "key-absent",
        &identity,
        &tier2_lease_key(4242, "headsha-diagnose-only"),
    )
    .await;

    let err = repository
        .resolve_tier2_lease(
            &f.subject,
            "key-absent",
            &lease_id,
            &identity,
            &CiTier2Resolution::repair(),
        )
        .await
        .expect_err("a run-absent route has nothing to re-run");
    let message = err.to_string();
    assert!(
        message.contains("diagnose-only"),
        "expected the diagnose-only refusal, got: {message}"
    );

    // Nothing was written: the lease is still open and the row is not terminal.
    let untouched = repository
        .get(&f.subject, "key-absent")
        .await
        .unwrap()
        .expect("row survives a refused resolution");
    assert_eq!(untouched.tier2_lease_state, Some(CiTier2LeaseState::Open));
    assert!(untouched.tier2_resolution.is_none());
    assert!(!untouched.is_terminal());

    // The diagnose resolution on the identical row is accepted.
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-absent",
                &lease_id,
                &identity,
                &CiTier2Resolution::diagnose(CiDiagnosticReason::EvidenceIncomplete)
                    .rejected_as(CiLeadRejection::RepairUnavailableForRoute),
            )
            .await
            .unwrap()
    );
    let resolved = repository
        .get(&f.subject, "key-absent")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        resolved.adjudicated_outcome(),
        Some(CiRouteOutcome::DiagnosticReopened)
    );
    assert_eq!(
        resolved.lead_rejection,
        Some(CiLeadRejection::RepairUnavailableForRoute),
        "the durable record says WHY the repair was replaced"
    );

    // And a route that DOES name a run accepts the repair, so the refusal is
    // scoped to run-absence rather than to repairs generally.
    let with_run = pr_head_identity(9, "headsha-has-a-run");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-with-run",
            with_run.clone(),
            "fp-run",
        ))
        .await
        .unwrap();
    let lease_id = open_lease(
        &repository,
        &f.subject,
        "key-with-run",
        &with_run,
        &tier2_lease_key(4242, "headsha-has-a-run"),
    )
    .await;
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-with-run",
                &lease_id,
                &with_run,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );
}

/// The evidence-advance watermark is NULL-safe over run ids.
///
/// `(pr_head_sha, run_id) IS DISTINCT FROM (...)` over a row constructor, not a
/// plain `<>`: with NULL run ids a plain comparison evaluates to NULL, the
/// `WHERE` drops the row, and a run-absent identity would never be seen to
/// advance — so the rollback gate would stay red forever. Asserted, not
/// assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_evidence_advance_watermark_is_null_safe_over_run_ids() {
    let f = fixture().await;
    let repository = repo(&f.db);

    let absent = run_absent_identity(CiLane::PrHead, "headsha-absent-1", None);
    repository
        .reserve(&ask_lead_reservation(
            &f.subject,
            "key-absent-1",
            absent,
            "fp",
        ))
        .await
        .unwrap();
    assert_eq!(
        repository.current_failed_identity_count().await.unwrap(),
        1,
        "one run-absent routed identity, nothing newer: still current"
    );

    // A SECOND run-absent identity on a new head. `(sha, NULL)` vs
    // `(other_sha, NULL)` must read as DISTINCT, or the first never advances.
    let advanced = run_absent_identity(CiLane::PrHead, "headsha-absent-2", None);
    repository
        .reserve(&ask_lead_reservation(
            &f.subject,
            "key-absent-2",
            advanced,
            "fp",
        ))
        .await
        .unwrap();
    assert_eq!(
        repository.current_failed_identity_count().await.unwrap(),
        1,
        "the older run-absent identity advanced; only the newer one is current"
    );

    // And a run-PRESENT row on a third head: `(sha, 7)` vs `(sha, NULL)` must
    // also read as distinct in both directions.
    repository
        .reserve(&reservation(
            &f.subject,
            "key-present",
            pr_head_identity(7, "headsha-absent-3"),
            "fp",
        ))
        .await
        .unwrap();
    assert_eq!(
        repository.current_failed_identity_count().await.unwrap(),
        1,
        "a run-absent identity is advanced by a later run-present one"
    );
}

// ---------------------------------------------------------------------------
// Database-side guards
// ---------------------------------------------------------------------------

/// Budget monotonicity is enforced by a **trigger**, not by the repository's
/// habit of never writing a decrement.
///
/// The decrement here is raw SQL that never touches Rust, which is the point:
/// 193's `charged_count >= 0` is satisfied by 2 -> 1, so before the trigger the
/// only thing standing between a refund path and a silently reset budget was
/// every future writer remembering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn charged_count_decrease_is_rejected() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(700, "headsha-monotonic");
    let input = reservation(&f.subject, "key-mono", identity.clone(), "fp-mono");
    repository.reserve(&input).await.unwrap();
    repository
        .charge_and_begin_calling(&f.subject, "key-mono", &incarnation(), &identity)
        .await
        .unwrap();
    let before = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(before.signature, 1);
    assert_eq!(before.head, 1);

    let err = sqlx::query("UPDATE ci_route_budget_counters SET charged_count = charged_count - 1")
        .execute(f.db.pool())
        .await
        .expect_err("the trigger refuses any decrease of charged_count");
    let message = err.to_string();
    assert!(
        message.contains("monotonic"),
        "expected the monotonicity trigger, got: {message}"
    );
    assert_eq!(
        err.as_database_error()
            .and_then(sqlx::error::DatabaseError::code),
        Some(std::borrow::Cow::Borrowed("23514")),
        "the trigger raises check_violation, so callers classify it exactly as \
         they classify the CHECKs beside it"
    );

    let after = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "the refused decrement wrote nothing to either counter"
    );

    // An INCREASE is still legal, so the trigger bounds direction rather than
    // freezing the row.
    sqlx::query("UPDATE ci_route_budget_counters SET charged_count = charged_count + 1")
        .execute(f.db.pool())
        .await
        .expect("a monotonic increment is unaffected");
    let raised = repository
        .budget_counts(&f.subject, &input.retry_budget_key, &input.head_budget_key)
        .await
        .unwrap();
    assert_eq!(raised.signature, 2);
}

/// A park must cite its cause, and the CHECK is what proves it — not the Rust
/// guard, and not the report's own filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_without_justification_is_rejected() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(800, "headsha-park");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-park",
            identity.clone(),
            "fp-park",
        ))
        .await
        .unwrap();
    let lease_id = open_lease(
        &repository,
        &f.subject,
        "key-park",
        &identity,
        &tier2_lease_key(4242, "headsha-park"),
    )
    .await;

    // (a) The repository refusal, for the readable message.
    for blank in ["", "   "] {
        let err = repository
            .resolve_tier2_lease(
                &f.subject,
                "key-park",
                &lease_id,
                &identity,
                &CiTier2Resolution::park(blank),
            )
            .await
            .expect_err("a park without a cited cause is not a park");
        assert!(
            err.to_string().contains("cited justification"),
            "got: {err}"
        );
    }

    // (b) The CHECK, which is the actual enforcement. Raw SQL, no Rust guard in
    //     the path: this is what makes `parks_with_cited_cause` a measurement
    //     rather than a restatement of the `parked` label.
    let update_err = sqlx::query(
        "UPDATE ci_route_attempts SET tier2_resolution = 'parked' \
         WHERE provider_action_key = 'key-park'",
    )
    .execute(f.db.pool())
    .await
    .expect_err("ci_route_attempts_park_cited_check");
    assert!(
        update_err.to_string().contains("park_cited"),
        "expected the park CHECK, got: {update_err}"
    );

    let blank_err = sqlx::query(
        "UPDATE ci_route_attempts SET tier2_resolution = 'parked', park_justification = '   ' \
         WHERE provider_action_key = 'key-park'",
    )
    .execute(f.db.pool())
    .await
    .expect_err("btrim: whitespace is not a citation");
    assert!(
        blank_err.to_string().contains("park_cited"),
        "expected the park CHECK, got: {blank_err}"
    );

    let insert_err = sqlx::query(
        "INSERT INTO ci_route_attempts (subject_kind, subject_id, provider_action_key, lane, \
           pr_number, pr_head_sha, run_id, run_head_sha, origin_state, class, action, \
           transient_fingerprint, retry_budget_key, head_budget_key, action_phase, \
           terminal_outcome) \
         VALUES ('task', $1, 'key-park-raw', 'pr_head', 4242, 'h2', 5, 'h2', 'pr_draft', \
                 'unknown', 'ask_lead', 'fp', 'sig2', 'head2', 'terminal', 'parked')",
    )
    .bind(&f.task_id)
    .execute(f.db.pool())
    .await
    .expect_err("an uncited park cannot be inserted either");
    assert!(
        insert_err.to_string().contains("park_cited"),
        "expected the park CHECK, got: {insert_err}"
    );

    // (c) A cited park is accepted, and the report counts it.
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-park",
                &lease_id,
                &identity,
                &CiTier2Resolution::park("runner image pull failed for 6h; infra dead end"),
            )
            .await
            .unwrap()
    );
    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(report.parks_with_cited_cause, 1);

    // (d) The metric is an INDEPENDENT witness, not a restatement of the label.
    //     With the CHECK in place an uncited park cannot exist, so the only way
    //     to show the report filter is doing work is to simulate the CHECK
    //     regressing and confirm the metric still refuses to count the park.
    //     A filter of `adjudicated = 'parked'` alone would count it.
    sqlx::query("ALTER TABLE ci_route_attempts DROP CONSTRAINT ci_route_attempts_park_cited_check")
        .execute(f.db.pool())
        .await
        .expect("simulate the CHECK regressing");
    sqlx::query(
        "INSERT INTO ci_route_attempts (subject_kind, subject_id, provider_action_key, lane, \
           pr_number, pr_head_sha, run_id, run_head_sha, origin_state, class, action, \
           transient_fingerprint, retry_budget_key, head_budget_key, action_phase, \
           terminal_outcome) \
         VALUES ('task', $1, 'key-uncited-park', 'pr_head', 4243, 'h3', 6, 'h3', 'pr_draft', \
                 'unknown', 'ask_lead', 'fp', 'sig3', 'head3', 'terminal', 'parked')",
    )
    .bind(&f.task_id)
    .execute(f.db.pool())
    .await
    .expect("the CHECK is gone, so the uncited park lands");

    let regressed = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        regressed.parks_with_cited_cause, 1,
        "the uncited park is a `parked` row and is still NOT counted: the metric \
         measures the cited cause, not the label"
    );
}

/// A second Lead session on one route **increments** rather than overwriting.
///
/// The old behaviour was a blind `SET lead_session_id = $5`, which made
/// `lead_invocations` structurally incapable of exceeding one per route — so
/// the proposal's cost bound could never observe its own overrun.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_lead_session_increments_rather_than_overwrites() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(900, "headsha-two-sessions");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-sessions",
            identity.clone(),
            "fp-sessions",
        ))
        .await
        .unwrap();
    let lease_id = open_lease(
        &repository,
        &f.subject,
        "key-sessions",
        &identity,
        &tier2_lease_key(4242, "headsha-two-sessions"),
    )
    .await;

    assert_eq!(
        repository
            .attach_lead_session(&f.subject, "key-sessions", &lease_id, "session-a")
            .await
            .unwrap(),
        CiLeadSessionAttachment::Attached { session_count: 1 },
        "the first attach reports one"
    );
    assert_eq!(
        repository
            .attach_lead_session(&f.subject, "key-sessions", &lease_id, "session-b")
            .await
            .unwrap(),
        CiLeadSessionAttachment::Attached { session_count: 2 },
        "the second attach is an ADDITIONAL session, not a correction of the first"
    );

    let row = repository
        .get(&f.subject, "key-sessions")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.lead_session_count, 2);
    assert_eq!(
        row.lead_session_id.as_deref(),
        Some("session-a"),
        "the FIRST session id is the audit handle and is never overwritten"
    );

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        report.lead_invocations, 2,
        "two Lead sessions on one route cost two Lead sessions"
    );

    // The fence is unchanged: a stale lease id attaches nothing and counts
    // nothing.
    assert_eq!(
        repository
            .attach_lead_session(&f.subject, "key-sessions", "not-the-lease", "session-c")
            .await
            .unwrap(),
        CiLeadSessionAttachment::NotFound
    );
    assert_eq!(
        repository
            .get(&f.subject, "key-sessions")
            .await
            .unwrap()
            .expect("row")
            .lead_session_count,
        2,
        "a refused attach does not increment"
    );
}

/// A merge recorded only in `tier2_resolution` still counts toward the
/// per-merged-PR denominator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merged_prs_counts_a_merge_that_landed_in_the_tier_two_column() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(950, "headsha-merged-late");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-merged",
            identity.clone(),
            "fp-merged",
        ))
        .await
        .unwrap();
    let owner = incarnation();
    repository
        .charge_and_begin_calling(&f.subject, "key-merged", &owner, &identity)
        .await
        .unwrap();
    // The route terminalizes on its PROVIDER result, so `terminal_outcome` is
    // write-once and spent.
    assert!(
        repository
            .finalize_calling(
                &f.subject,
                "key-merged",
                &owner,
                CiRouteOutcome::ActionFailed,
                None,
            )
            .await
            .unwrap()
    );
    // It routes to Tier 2 — which `action_failed` legally may — and the PR
    // merges while that adjudication is open, so the merge lands in
    // `tier2_resolution` and `terminal_outcome` keeps the provider failure.
    open_lease(
        &repository,
        &f.subject,
        "key-merged",
        &identity,
        &tier2_lease_key(4242, "headsha-merged-late"),
    )
    .await;
    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();

    let row = repository
        .get(&f.subject, "key-merged")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::ActionFailed));
    assert_eq!(row.tier2_resolution, Some(CiRouteOutcome::Merged));

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        report.merged_prs, 1,
        "reading only `terminal_outcome` would miss every merge that followed a \
         provider failure and inflate every per-merged-PR ratio in the report"
    );
}

// ---------------------------------------------------------------------------
// The per-merged-PR denominator, after the follow-up defect
// ---------------------------------------------------------------------------
//
// The four fixtures below exist because the denominator used to be derived
// from `COALESCE(tier2_resolution, terminal_outcome)`, and
// `close_routes_for_newer_outcome` cannot write that column once Lead has
// resolved the lease: its `WHERE` demands `tier2_lease_state = 'open'` and its
// `COALESCE` preserves whatever is already there. So `merged_prs` counted only
// routes that never reached Lead while `lead_invocations` counted only routes
// that did — two disjoint populations, and a ratio that was uncomputable
// rather than merely wrong. Production read `merged_prs = 0` with 13 merges
// and 7 Lead routes behind it.
//
// `pr_merged_at` is the separate fact. Each fixture below names the mutation
// it kills.

/// **The production scenario.** A route reaches Lead, Lead adjudicates a
/// repair, and the PR merges afterwards — and the cost ratios are computable.
///
/// Kills: reverting `merged_prs` to `FILTER (WHERE adjudicated = 'merged')`,
/// and adding any lease-state or action-phase predicate to the `pr_merged_at`
/// stamp in `close_routes_for_newer_outcome`. Either one puts this route's
/// merge back out of reach of the denominator, `merged_prs` returns to 0 and
/// both ratios collapse to `None`.
///
/// The `lead_invocations`/`worker_reopens` assertions are the vacuity guards:
/// without them a stamp that also wiped the adjudication would satisfy
/// `Some(..)` with a numerator of zero, which reads as the *best* possible cost
/// and is the one wrong answer nobody would investigate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merged_prs_counts_a_pr_that_merged_after_lead_adjudicated_the_route() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(960, "headsha-merged-after-lead");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-after-lead",
            identity.clone(),
            "fp-after-lead",
        ))
        .await
        .unwrap();
    let lease_id = open_lease(
        &repository,
        &f.subject,
        "key-after-lead",
        &identity,
        &tier2_lease_key(4242, "headsha-merged-after-lead"),
    )
    .await;
    repository
        .attach_lead_session(
            &f.subject,
            "key-after-lead",
            &lease_id,
            "session-after-lead",
        )
        .await
        .unwrap();
    // Lead answers. The lease is now `resolved` and `tier2_resolution` is
    // spent — this is the state the old denominator could never see a merge
    // reach.
    assert!(
        repository
            .resolve_tier2_lease(
                &f.subject,
                "key-after-lead",
                &lease_id,
                &identity,
                &CiTier2Resolution::repair(),
            )
            .await
            .unwrap()
    );

    let before_merge = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        before_merge.merged_prs, 0,
        "vacuity: nothing has merged yet, so a denominator that already reads 1 \
         is counting something other than a merge"
    );
    assert_eq!(before_merge.lead_sessions_per_merged_pr(), None);

    // The PR merges some time later. Nothing about this route is `open` or
    // `reserved` any more.
    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();

    let row = repository
        .get(&f.subject, "key-after-lead")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        row.tier2_lease_state,
        Some(CiTier2LeaseState::Resolved),
        "vacuity: the lease must already be resolved, or this fixture is the \
         open-lease case and proves nothing about the defect"
    );
    assert!(
        row.pr_merged_at.is_some(),
        "the merge is a fact about the PR and must be recorded on a route Lead \
         already adjudicated"
    );

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        report.merged_prs, 1,
        "the PR merged, so the denominator of both cost ratios is 1"
    );
    assert_eq!(
        report.lead_invocations, 1,
        "vacuity: one Lead session ran, so a ratio built on this numerator is \
         describing real cost"
    );
    assert_eq!(
        report.worker_reopens, 1,
        "vacuity: the repair reopen dispatched one worker"
    );
    assert_eq!(
        report.lead_sessions_per_merged_pr(),
        Some(1.0),
        "one Lead session per merged PR — the exact number that read `None` in \
         production while 13 PRs merged past 7 Lead routes"
    );
    assert_eq!(report.worker_reopens_per_merged_pr(), Some(1.0));
}

/// The merge does not clobber the adjudication it arrives after.
///
/// Kills: writing the merge into `tier2_resolution` (dropping the `COALESCE`,
/// or widening the `WHERE` past `tier2_lease_state = 'open'`) as a shortcut to
/// fixing the denominator. That would repair `merged_prs` by destroying
/// `repair_reopens`, `worker_reopens`, and the audit record of a Lead session
/// that actually ran — five numerators traded for one denominator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_after_lead_leaves_the_adjudication_intact() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(961, "headsha-adjudication-intact");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-intact",
            identity.clone(),
            "fp-intact",
        ))
        .await
        .unwrap();
    let lease_id = open_lease(
        &repository,
        &f.subject,
        "key-intact",
        &identity,
        &tier2_lease_key(4242, "headsha-adjudication-intact"),
    )
    .await;
    repository
        .resolve_tier2_lease(
            &f.subject,
            "key-intact",
            &lease_id,
            &identity,
            &CiTier2Resolution::repair(),
        )
        .await
        .unwrap();

    let adjudicated = repository
        .get(&f.subject, "key-intact")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        adjudicated.tier2_resolution,
        Some(CiRouteOutcome::RepairReopened),
        "vacuity: the adjudication must be present before the merge, or the \
         assertion below is comparing nothing to nothing"
    );
    assert_eq!(adjudicated.reopen_mode, Some(CiReopenMode::Repair));

    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();

    let after = repository
        .get(&f.subject, "key-intact")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        after.tier2_resolution,
        Some(CiRouteOutcome::RepairReopened),
        "how Lead decided this route is not what the PR later did with itself"
    );
    assert_eq!(after.reopen_mode, Some(CiReopenMode::Repair));
    assert!(after.pr_merged_at.is_some());

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        report.repair_reopens, 1,
        "the repair is still counted as a repair after the merge"
    );
    assert_eq!(report.worker_reopens, 1);
    assert_eq!(report.merged_prs, 1);
}

/// The open-lease close is byte-for-byte the behaviour it always had, plus the
/// stamp.
///
/// Kills: replacing the two existing statements with the new one, i.e. "fixing"
/// the denominator by making the merge stop resolving the lease and
/// terminalizing the reserved row. That regression frees no current-evidence
/// key and leaves a delayed Lead result something to apply to, and every
/// assertion here except `pr_merged_at` would have passed before this change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_before_lead_resolves_still_closes_the_open_lease() {
    let f = fixture().await;
    let repository = repo(&f.db);
    let identity = pr_head_identity(962, "headsha-merged-while-open");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-while-open",
            identity.clone(),
            "fp-while-open",
        ))
        .await
        .unwrap();
    open_lease(
        &repository,
        &f.subject,
        "key-while-open",
        &identity,
        &tier2_lease_key(4242, "headsha-merged-while-open"),
    )
    .await;

    assert_eq!(
        repository
            .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
            .await
            .unwrap(),
        1,
        "the `reserved` row is still terminalized by the merge and still \
         counted as closed"
    );

    let row = repository
        .get(&f.subject, "key-while-open")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.tier2_lease_state, Some(CiTier2LeaseState::Resolved));
    assert_eq!(
        row.tier2_resolution,
        Some(CiRouteOutcome::Merged),
        "an UNADJUDICATED lease still takes the merge as its resolution"
    );
    assert_eq!(row.terminal_outcome, Some(CiRouteOutcome::Merged));
    assert!(row.is_terminal());
    assert!(row.pr_merged_at.is_some());

    assert_eq!(
        repository
            .quiescence_counts()
            .await
            .unwrap()
            .open_tier2_leases,
        0,
        "the current-evidence key is released, so a delayed Lead result finds \
         nothing to apply to"
    );
    assert_eq!(
        repository
            .route_report(&CiRouteReportFilter::all())
            .await
            .unwrap()
            .merged_prs,
        1
    );
}

/// The stamp is write-once, and a pass is not a merge.
///
/// Kills: dropping `AND pr_merged_at IS NULL` from the stamp (a merged PR is
/// re-polled, so every later poll would move the recorded merge instant and the
/// column would degrade into a boolean that lies about *when*), and stamping on
/// `CiRouteOutcome::Passed` (which would make green CI count as a merged PR and
/// silently divide the cost bounds by the wrong population).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_merge_stamp_is_write_once_and_a_pass_never_sets_it() {
    let f = fixture().await;
    let repository = repo(&f.db);

    // A passing close on its own PR.
    let passing = CiEvidenceIdentity {
        pr_number: 5150,
        ..pr_head_identity(963, "headsha-passed-not-merged")
    };
    repository
        .reserve(&reservation(
            &f.subject,
            "key-passed",
            passing.clone(),
            "fp-passed",
        ))
        .await
        .unwrap();
    repository
        .close_routes_for_newer_outcome(&f.subject, 5150, CiRouteOutcome::Passed, None)
        .await
        .unwrap();
    let passed_row = repository
        .get(&f.subject, "key-passed")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        passed_row.terminal_outcome,
        Some(CiRouteOutcome::Passed),
        "vacuity: the passing close must actually have landed"
    );
    assert!(
        passed_row.pr_merged_at.is_none(),
        "CI going green is not the PR merging"
    );

    // A merging close, polled twice.
    let merging = pr_head_identity(964, "headsha-merged-twice");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-twice",
            merging.clone(),
            "fp-twice",
        ))
        .await
        .unwrap();
    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();
    // Rewind the stamp an hour before re-polling. Two closes a millisecond
    // apart would compare equal whether or not the guard exists — `to_char`
    // renders milliseconds — so the assertion below would be vacuous exactly
    // when the mutation it kills is present. An hour is unmistakable.
    sqlx::query(
        "UPDATE ci_route_attempts SET pr_merged_at = now() - interval '1 hour' \
                 WHERE provider_action_key = 'key-twice'",
    )
    .execute(f.db.pool())
    .await
    .expect("rewind the stamp to a distinguishable instant");
    let first = repository
        .get(&f.subject, "key-twice")
        .await
        .unwrap()
        .expect("row")
        .pr_merged_at
        .expect("the merge is stamped");

    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();
    assert_eq!(
        repository
            .get(&f.subject, "key-twice")
            .await
            .unwrap()
            .expect("row")
            .pr_merged_at
            .as_deref(),
        Some(first.as_str()),
        "a re-poll of an already-merged PR records the FIRST observation, not \
         the latest one"
    );

    let report = repository
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        report.merged_prs, 1,
        "one PR merged and one only passed, across two closes of the merged one"
    );
    assert_eq!(
        report.passed, 1,
        "vacuity: the passing route is in the window, so `merged_prs = 1` is \
         excluding it rather than never having seen it"
    );
}

/// Migration 202's backfill recovers the merges the OLD reading could see.
///
/// Every merge that landed while its lease was still open did reach
/// `tier2_resolution`, and those rows exist in production. Switching the
/// denominator to a column the migration introduced would zero them at the
/// cutover — the report would say fewer PRs merged the day of the deploy than
/// the day before, which is the shape of a regression an operator would chase
/// into the routing layer rather than into the schema.
///
/// The statement under test is read out of the SHIPPED migration file rather
/// than restated here, so a predicate typo in the file fails this fixture
/// instead of passing a copy of itself. Kills: dropping the backfill, and
/// narrowing its predicate to `terminal_outcome = 'merged'` (which is the
/// misreading the original defect comment was written to warn about).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_202_backfills_merges_that_reached_the_adjudication_column() {
    let f = fixture().await;
    let repository = repo(&f.db);

    // One route whose PR merged while its Tier-2 lease was open — the only
    // shape the pre-202 denominator could ever count — and one that never
    // merged at all.
    let merged = pr_head_identity(965, "headsha-backfill-merged");
    repository
        .reserve(&reservation(
            &f.subject,
            "key-backfill-merged",
            merged.clone(),
            "fp-backfill-merged",
        ))
        .await
        .unwrap();
    open_lease(
        &repository,
        &f.subject,
        "key-backfill-merged",
        &merged,
        &tier2_lease_key(4242, "headsha-backfill-merged"),
    )
    .await;
    repository
        .close_routes_for_newer_outcome(&f.subject, 4242, CiRouteOutcome::Merged, None)
        .await
        .unwrap();

    let never_merged = CiEvidenceIdentity {
        pr_number: 7070,
        ..pr_head_identity(966, "headsha-backfill-open")
    };
    repository
        .reserve(&reservation(
            &f.subject,
            "key-backfill-open",
            never_merged,
            "fp-backfill-open",
        ))
        .await
        .unwrap();

    // Rewind the stamp to reproduce a row written before 202 existed. Its
    // `tier2_resolution` still says `merged`, which is exactly the production
    // state the backfill has to recognise.
    sqlx::query("UPDATE ci_route_attempts SET pr_merged_at = NULL")
        .execute(f.db.pool())
        .await
        .expect("simulate rows written before migration 202");
    assert_eq!(
        repository
            .route_report(&CiRouteReportFilter::all())
            .await
            .unwrap()
            .merged_prs,
        0,
        "vacuity: with the stamp cleared the denominator must read 0, or the \
         report is not reading the column this test is about"
    );

    let migration = include_str!("../../migrations_postgres/202_ci_route_pr_merged_fact.sql");
    let backfill = migration
        .split(';')
        .map(|statement| {
            statement
                .lines()
                .filter(|line| !line.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_owned()
        })
        .find(|statement| statement.starts_with("UPDATE ci_route_attempts"))
        .expect("migration 202 still carries a backfill UPDATE");
    sqlx::query(&backfill)
        .execute(f.db.pool())
        .await
        .expect("the shipped backfill applies");

    assert!(
        repository
            .get(&f.subject, "key-backfill-merged")
            .await
            .unwrap()
            .expect("row")
            .pr_merged_at
            .is_some(),
        "a merge already recorded in `tier2_resolution` is carried into the \
         new column rather than lost at the cutover"
    );
    assert!(
        repository
            .get(&f.subject, "key-backfill-open")
            .await
            .unwrap()
            .expect("row")
            .pr_merged_at
            .is_none(),
        "vacuity: a route whose PR never merged stays NULL, so the backfill is \
         a predicate and not `WHERE true`"
    );
    assert_eq!(
        repository
            .route_report(&CiRouteReportFilter::all())
            .await
            .unwrap()
            .merged_prs,
        1,
        "the denominator survives the migration boundary"
    );
}

// ---------------------------------------------------------------------------
// The bounded incomplete-evidence hold
// ---------------------------------------------------------------------------

/// A retry of the SAME logical poll reads back its sequence instead of
/// reserving a second one — across a genuinely new repository over the same
/// database.
///
/// Without this, a crash-loop between reserve and apply burns sequence space,
/// and every observation still in flight looks superseded because the
/// watermark race has moved on without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_observation_is_idempotent_across_restart() {
    let f = fixture().await;
    let identity = hold_identity(&f.subject, "headsha-idempotent");
    let poll_id = uuid::Uuid::now_v7().to_string();

    let first = {
        let holds = CiIncompleteHoldRepository::new(f.db.clone());
        holds.reserve_poll(&identity, &poll_id).await.unwrap()
    };
    assert!(!first.replayed, "the first reservation is not a replay");
    assert_eq!(
        first.poll_sequence, 1,
        "sequences start at one, so zero can mean `nothing applied yet`"
    );

    // The process died and came back: a brand-new repository over a brand-new
    // pool, built from nothing but the connection string.
    let dsn = f.db.test_dsn().expect("ephemeral database exposes a DSN");
    let handle = Database::reopen_test(&dsn).expect("reopen after simulated restart");
    let restarted = CiIncompleteHoldRepository::new(handle.clone());

    let second = restarted.reserve_poll(&identity, &poll_id).await.unwrap();
    assert!(
        second.replayed,
        "the same poll id is a replay, not a new poll"
    );
    assert_eq!(second.poll_sequence, first.poll_sequence);
    assert_eq!(second.streak_id, first.streak_id);

    let streak = restarted
        .get(&identity)
        .await
        .unwrap()
        .expect("the streak survives the restart");
    assert_eq!(
        streak.next_poll_sequence, 1,
        "the retry reserved NO second sequence"
    );
    assert_eq!(streak.poll_count, 0, "reserving is not counting");
    assert_eq!(streak.last_applied_poll_sequence, 0);
    assert_eq!(
        count_rows(
            &f.db,
            "SELECT COUNT(*) FROM ci_incomplete_hold_observations"
        )
        .await,
        1,
        "one logical poll, one observation row"
    );

    // A DIFFERENT poll id does reserve a second sequence.
    let other = restarted
        .reserve_poll(&identity, &uuid::Uuid::now_v7().to_string())
        .await
        .unwrap();
    assert!(!other.replayed);
    assert_eq!(other.poll_sequence, 2);
}

/// One hold streak per `(repository, PR, head, lane, dequeue)` identity — on
/// the PR-head lane, where `dequeue_id IS NULL`.
///
/// # Why NULL is the whole story here
///
/// `lock_or_create_streak` creates with `INSERT … ON CONFLICT DO NOTHING` and
/// **no conflict target**, so the only thing standing between two simultaneous
/// creations and two rows is `NULLS NOT DISTINCT` on
/// `ci_incomplete_hold_streaks_identity_uniq` (migration 195). Drop that one
/// clause and, under the default `NULLS DISTINCT`, every PR-head streak is
/// unique to itself no matter how many of them exist: two concurrent
/// coordinator polls mint two streaks for one head, each counts alone, and
/// `CI_INCOMPLETE_HOLD_MAX_POLLS` is reached at twice the wall-clock — or
/// never, if the polls keep splitting. The bounded hold silently becomes an
/// unbounded one, which is the stall the bound exists to prevent.
///
/// Every other hold fixture in this file polls **sequentially**, and a
/// sequential second poll finds the first row under `SELECT … FOR UPDATE`
/// before it ever attempts an insert. So none of them touches the clause, and
/// dropping it leaves the whole `nafu` command list at its baseline counts.
///
/// # The two halves
///
/// 1. **The race**, driven through the production repository: two reservations
///    for one identity, concurrently, from a state where no row exists.
/// 2. **The collapse**, deterministic: the creation statement
///    `lock_or_create_streak` issues, replayed verbatim against the identity
///    columns of the row that already exists. Under the index it is absorbed
///    (`rows_affected() == 0`); without it, it silently mints a second streak.
///    This half does not depend on how the runtime happened to interleave, so
///    it holds the contract even if (1) is scheduled sequentially.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_pr_head_identity_holds_one_streak_across_concurrent_polls() {
    let f = fixture().await;
    let identity = hold_identity(&f.subject, "headsha-concurrent-identity");
    assert!(
        identity.dequeue_id.is_none(),
        "vacuity: this fixture is about the PR-head lane, whose dequeue id is \
         NULL — that is the column the identity index has to collapse",
    );
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_incomplete_hold_streaks").await,
        0,
        "vacuity: no streak exists yet, so both reservations below race to \
         CREATE one rather than finding it",
    );

    // ── (1) Two concurrent polls of one PR head ────────────────────────────
    //
    // Two repositories, as two coordinator ticks would hold: nothing is shared
    // between them but the database.
    let left = CiIncompleteHoldRepository::new(f.db.clone());
    let right = CiIncompleteHoldRepository::new(f.db.clone());
    let left_poll = uuid::Uuid::now_v7().to_string();
    let right_poll = uuid::Uuid::now_v7().to_string();
    let (first, second) = tokio::join!(
        left.reserve_poll(&identity, &left_poll),
        right.reserve_poll(&identity, &right_poll),
    );
    let first = first.expect("the first concurrent reservation");
    let second = second.expect("the second concurrent reservation");

    assert_eq!(
        first.streak_id, second.streak_id,
        "two polls of ONE PR head are one identity and must share one streak; \
         separate streaks each count alone and the bound is never reached",
    );
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_incomplete_hold_streaks").await,
        1,
        "exactly one hold row per repository/PR/head/lane/dequeue identity",
    );
    let mut sequences = [first.poll_sequence, second.poll_sequence];
    sequences.sort_unstable();
    assert_eq!(
        sequences,
        [1, 2],
        "and both polls draw from that ONE streak's sequence space, so neither \
         is silently ordering itself against a private counter",
    );

    // ── (2) The collapse, without relying on the scheduler ─────────────────
    //
    // The identity columns are read back off the surviving row so this replays
    // the production statement against the production values rather than a
    // hand-built approximation of them.
    let (subject_kind, subject_id, repository_id, pr_number, pr_head_sha, lane, dequeue_id): (
        String,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT subject_kind, subject_id, repository_id, pr_number, pr_head_sha, lane, dequeue_id \
         FROM ci_incomplete_hold_streaks",
    )
    .fetch_one(f.db.pool())
    .await
    .expect("the single surviving streak");
    assert!(
        dequeue_id.is_none(),
        "vacuity: the surviving row really does carry a NULL dequeue id",
    );

    // Verbatim from `lock_or_create_streak`: no conflict target, so the index
    // is the entire guard.
    let absorbed = sqlx::query(
        "INSERT INTO ci_incomplete_hold_streaks \
           (id, subject_kind, subject_id, repository_id, pr_number, pr_head_sha, lane, dequeue_id) \
         VALUES ($8, $1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
    )
    .bind(&subject_kind)
    .bind(&subject_id)
    .bind(&repository_id)
    .bind(pr_number)
    .bind(&pr_head_sha)
    .bind(&lane)
    .bind(dequeue_id)
    .bind(uuid::Uuid::now_v7().to_string())
    .execute(f.db.pool())
    .await
    .expect("a duplicate creation is absorbed, never an error");

    assert_eq!(
        absorbed.rows_affected(),
        0,
        "the creation statement is `ON CONFLICT DO NOTHING` with NO target, so \
         `NULLS NOT DISTINCT` on the identity index is the only thing that can \
         absorb a duplicate PR-head streak. Without it this insert succeeds and \
         one PR head owns two streaks",
    );
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_incomplete_hold_streaks").await,
        1,
        "and the row count is what that means operationally",
    );
}

/// Sequence reservation, not arrival time, is the authority order.
///
/// A poll that reserved earlier but lands later is **superseded**: it writes
/// its own marker and nothing else. Without that, a late incomplete answer
/// arriving after an on-time complete one resurrects a streak that had
/// legitimately reset — the lane is green and the hold escalates anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_poll_is_superseded_and_changes_nothing() {
    let f = fixture().await;
    let holds = CiIncompleteHoldRepository::new(f.db.clone());
    let identity = hold_identity(&f.subject, "headsha-ordering");
    let escalation = escalation_route(&f.subject, "headsha-ordering");

    // A reserves first...
    let a = uuid::Uuid::now_v7().to_string();
    let reserved_a = holds.reserve_poll(&identity, &a).await.unwrap();
    assert_eq!(reserved_a.poll_sequence, 1);

    // ...but B reserves second and lands FIRST, with complete evidence.
    let b = uuid::Uuid::now_v7().to_string();
    let reserved_b = holds.reserve_poll(&identity, &b).await.unwrap();
    assert_eq!(reserved_b.poll_sequence, 2);
    assert_eq!(
        holds
            .apply_poll(&identity, &identity, &b, true, &escalation)
            .await
            .unwrap(),
        CiHoldApply::Reset { poll_sequence: 2 }
    );

    let after_reset = holds.get(&identity).await.unwrap().expect("streak");
    assert_eq!(after_reset.last_applied_poll_sequence, 2);
    assert_eq!(after_reset.poll_count, 0);
    assert!(
        !after_reset.has_escalated(),
        "a complete enumeration clears the escalation marker"
    );

    // Now A lands. Its sequence is BEHIND the watermark, so it is superseded.
    assert_eq!(
        holds
            .apply_poll(&identity, &identity, &a, false, &escalation)
            .await
            .unwrap(),
        CiHoldApply::Superseded,
        "an overtaken observation applies nothing, whatever it observed"
    );

    let after_late = holds.get(&identity).await.unwrap().expect("streak");
    assert_eq!(
        after_late.poll_count, 0,
        "the superseded incomplete answer did NOT increment"
    );
    assert_eq!(
        after_late.last_applied_poll_sequence, 2,
        "and did NOT move the retained high-watermark backwards"
    );
    assert!(!after_late.has_escalated());
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_route_attempts").await,
        0,
        "and dispatched nothing"
    );

    // The loser is still auditable: an ordering contract whose losers leave no
    // trace cannot be checked.
    let observation = holds.observation(&a).await.unwrap().expect("observation");
    assert_eq!(
        observation.apply_outcome.as_deref(),
        Some("superseded_observation")
    );
    assert!(observation.applied_at.is_some());

    // A genuinely newer incomplete poll counts normally: the streak was reset,
    // not broken.
    let c = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        holds
            .reserve_poll(&identity, &c)
            .await
            .unwrap()
            .poll_sequence,
        3
    );
    assert_eq!(
        holds
            .apply_poll(&identity, &identity, &c, false, &escalation)
            .await
            .unwrap(),
        CiHoldApply::Held { poll_count: 1 }
    );
    assert_eq!(
        holds
            .get(&identity)
            .await
            .unwrap()
            .expect("streak")
            .last_applied_poll_sequence,
        3
    );
}

/// A head advance leaves the old streak ineligible to apply, and the new head
/// starts a fresh streak at one.
///
/// The identity check runs BEFORE the sequence comparison, deliberately: a
/// delayed apply for a head that has since moved must not increment — let alone
/// escalate — the old head's streak, because that would open a diagnose route
/// for a head nobody is on any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_advance_clears_hold_streak() {
    let f = fixture().await;
    let holds = CiIncompleteHoldRepository::new(f.db.clone());
    let old = hold_identity(&f.subject, "headsha-old");
    let new = hold_identity(&f.subject, "headsha-new");
    let escalation = escalation_route(&f.subject, "headsha-old");

    // One real incomplete poll on the old head, so the streak is non-trivial.
    assert_eq!(
        one_poll(&holds, &old, &escalation, false).await,
        CiHoldApply::Held { poll_count: 1 }
    );

    // A poll reserves against the old head, and the head moves during the
    // provider enumeration.
    let stale = uuid::Uuid::now_v7().to_string();
    holds.reserve_poll(&old, &stale).await.unwrap();
    assert_eq!(
        holds
            .apply_poll(&old, &new, &stale, false, &escalation)
            .await
            .unwrap(),
        CiHoldApply::IdentityAdvanced
    );

    let old_streak = holds.get(&old).await.unwrap().expect("old streak");
    assert_eq!(
        old_streak.poll_count, 1,
        "the stale apply did not increment the old head's streak"
    );
    assert_eq!(
        old_streak.last_applied_poll_sequence, 1,
        "nor advance its watermark"
    );
    assert!(!old_streak.has_escalated());
    assert_eq!(
        holds
            .observation(&stale)
            .await
            .unwrap()
            .expect("observation")
            .apply_outcome
            .as_deref(),
        Some("identity_advanced")
    );

    // The new head is a fresh row that counts from one.
    assert!(
        holds.get(&new).await.unwrap().is_none(),
        "the new head has no streak until it is polled"
    );
    assert_eq!(
        one_poll(
            &holds,
            &new,
            &escalation_route(&f.subject, "headsha-new"),
            false
        )
        .await,
        CiHoldApply::Held { poll_count: 1 }
    );
    let new_streak = holds.get(&new).await.unwrap().expect("new streak");
    assert_eq!(new_streak.poll_count, 1);
    assert_eq!(new_streak.next_poll_sequence, 1);
    assert_ne!(new_streak.id, old_streak.id);
}

/// The twelfth consecutive incomplete poll escalates **exactly once**, under a
/// real race, and inserts **exactly one** diagnose-only run-absent route in the
/// same transaction.
///
/// `poll_count` reaches 11 by driving eleven real applies rather than by a raw
/// UPDATE: a streak fabricated with SQL proves nothing about the mechanism that
/// is supposed to produce it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_twelfth_incomplete_poll_escalates_exactly_once_under_a_race() {
    let f = fixture().await;
    let holds = CiIncompleteHoldRepository::new(f.db.clone());
    let identity = hold_identity(&f.subject, "headsha-escalate");
    let escalation = escalation_route(&f.subject, "headsha-escalate");

    for expected in 1..CI_INCOMPLETE_HOLD_MAX_POLLS {
        assert_eq!(
            one_poll(&holds, &identity, &escalation, false).await,
            CiHoldApply::Held {
                poll_count: expected
            },
            "poll {expected} is below the bound and holds"
        );
    }
    let seeded = holds.get(&identity).await.unwrap().expect("streak");
    assert_eq!(seeded.poll_count, CI_INCOMPLETE_HOLD_MAX_POLLS - 1);
    assert!(!seeded.has_escalated());
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_route_attempts").await,
        0,
        "eleven consecutive incomplete polls have dispatched nothing"
    );

    // The report sees the hold even though it has written no route row — which
    // is the reason `recoverable_holds` is not derivable from `held`.
    let holding = repo(&f.db)
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(holding.recoverable_holds, 1);
    assert_eq!(holding.bounded_hold_escalations, 0);
    assert_eq!(
        holding.held, 0,
        "a recoverable hold is NOT a `held` route: it is disjoint, not a subset"
    );

    // Two pollers now reserve, then race their applies. Both observed
    // incomplete evidence; both are entitled to try.
    let (first, second) = (
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
    );
    holds.reserve_poll(&identity, &first).await.unwrap();
    holds.reserve_poll(&identity, &second).await.unwrap();

    let left = CiIncompleteHoldRepository::new(f.db.clone());
    let right = CiIncompleteHoldRepository::new(f.db.clone());
    let (a, b) = tokio::join!(
        left.apply_poll(&identity, &identity, &first, false, &escalation),
        right.apply_poll(&identity, &identity, &second, false, &escalation),
    );
    let outcomes = [a.unwrap(), b.unwrap()];

    let escalations = outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                CiHoldApply::Escalated {
                    poll_count: CI_INCOMPLETE_HOLD_MAX_POLLS,
                    route_inserted: true
                }
            )
        })
        .count();
    assert_eq!(
        escalations, 1,
        "exactly one poller escalates; got {outcomes:?}"
    );
    let losers = outcomes
        .iter()
        .filter(|o| matches!(o, CiHoldApply::Superseded | CiHoldApply::AlreadyEscalated))
        .count();
    assert_eq!(
        losers, 1,
        "the other poller is overtaken or finds the streak already escalated; got {outcomes:?}"
    );

    let streak = holds.get(&identity).await.unwrap().expect("streak");
    assert!(streak.has_escalated(), "escalated_at is set");
    assert_eq!(streak.poll_count, CI_INCOMPLETE_HOLD_MAX_POLLS);
    assert_eq!(
        count_rows(
            &f.db,
            "SELECT COUNT(*) FROM ci_incomplete_hold_observations WHERE apply_outcome = 'escalated'"
        )
        .await,
        1,
        "escalated_at was reached through exactly one observation"
    );

    // One route, one lease, one Lead session's worth of work — and it is the
    // run-absent diagnose-only route.
    assert_eq!(
        count_rows(
            &f.db,
            "SELECT COUNT(*) FROM ci_route_attempts WHERE run_id IS NULL"
        )
        .await,
        1,
        "the escalation inserts exactly one diagnose-only run-absent route"
    );
    let route = repo(&f.db)
        .get(&f.subject, "key-escalation-headsha-escalate")
        .await
        .unwrap()
        .expect("the escalation route");
    assert!(route.is_run_absent());
    assert_eq!(route.action, CiAction::AskLead);
    assert_eq!(route.tier2_lease_state, Some(CiTier2LeaseState::Open));
    assert_eq!(
        route.tier2_lease_reason,
        Some(CiTier2Reason::EvidenceUnknown)
    );

    let escalated = repo(&f.db)
        .route_report(&CiRouteReportFilter::all())
        .await
        .unwrap();
    assert_eq!(
        escalated.bounded_hold_escalations, 1,
        "the escalation is reportable"
    );
    assert_eq!(
        escalated.recoverable_holds, 0,
        "and it is no longer a recoverable hold: the two are mutually exclusive"
    );

    // A thirteenth incomplete poll neither counts nor dispatches again.
    assert_eq!(
        one_poll(&holds, &identity, &escalation, false).await,
        CiHoldApply::AlreadyEscalated
    );
    let after = holds.get(&identity).await.unwrap().expect("streak");
    assert_eq!(
        after.poll_count, CI_INCOMPLETE_HOLD_MAX_POLLS,
        "an escalated streak stops counting"
    );
    assert_eq!(
        after.escalated_at, streak.escalated_at,
        "and escalated_at is stamped once, never re-stamped"
    );
    assert_eq!(
        count_rows(&f.db, "SELECT COUNT(*) FROM ci_route_attempts").await,
        1,
        "and opens no second lease and no second Lead session"
    );
}

/// The escalation marker and the route it authorizes commit **together**.
///
/// Proven by making the route insert fail inside the apply transaction: if the
/// two were sequential, `escalated_at` would survive the failure and the hold
/// would be permanently silent with nothing dispatched — the worst of both
/// outcomes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_escalation_route_rolls_back_the_escalation_marker() {
    let f = fixture().await;
    let holds = CiIncompleteHoldRepository::new(f.db.clone());
    let identity = hold_identity(&f.subject, "headsha-atomic");
    let escalation = escalation_route(&f.subject, "headsha-atomic");

    for _ in 1..CI_INCOMPLETE_HOLD_MAX_POLLS {
        one_poll(&holds, &identity, &escalation, false).await;
    }

    // Make the route insert impossible, without touching the streak table.
    sqlx::query("ALTER TABLE ci_route_attempts ADD CONSTRAINT reject_all_for_test CHECK (false)")
        .execute(f.db.pool())
        .await
        .expect("install the injected refusal");

    let poll_id = uuid::Uuid::now_v7().to_string();
    holds.reserve_poll(&identity, &poll_id).await.unwrap();
    let err = holds
        .apply_poll(&identity, &identity, &poll_id, false, &escalation)
        .await
        .expect_err("the route insert fails, so the whole apply fails");
    assert!(
        err.to_string().contains("reject_all_for_test"),
        "expected the injected refusal, got: {err}"
    );

    let streak = holds.get(&identity).await.unwrap().expect("streak");
    assert!(
        !streak.has_escalated(),
        "escalated_at rolled back with the route insert: a hold that could not \
         dispatch has NOT escalated"
    );
    assert_eq!(
        streak.poll_count,
        CI_INCOMPLETE_HOLD_MAX_POLLS - 1,
        "and the increment rolled back too"
    );
    assert_eq!(
        streak.last_applied_poll_sequence,
        CI_INCOMPLETE_HOLD_MAX_POLLS - 1,
        "and the high-watermark did not advance, so the poll can be retried"
    );

    // With the refusal removed, the same bound is reached and dispatched.
    sqlx::query("ALTER TABLE ci_route_attempts DROP CONSTRAINT reject_all_for_test")
        .execute(f.db.pool())
        .await
        .expect("remove the injected refusal");
    assert_eq!(
        one_poll(&holds, &identity, &escalation, false).await,
        CiHoldApply::Escalated {
            poll_count: CI_INCOMPLETE_HOLD_MAX_POLLS,
            route_inserted: true
        }
    );
}

/// The escalation route may not fabricate an identity, and an apply that has
/// nothing to apply to is a no-op rather than an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escalation_route_that_names_a_run_is_refused() {
    let f = fixture().await;
    let holds = CiIncompleteHoldRepository::new(f.db.clone());
    let identity = hold_identity(&f.subject, "headsha-validate");

    let mut fabricated = escalation_route(&f.subject, "headsha-validate");
    fabricated.reservation.identity.run_id = Some(77);
    let poll_id = uuid::Uuid::now_v7().to_string();
    holds.reserve_poll(&identity, &poll_id).await.unwrap();
    let err = holds
        .apply_poll(&identity, &identity, &poll_id, false, &fabricated)
        .await
        .expect_err("a hold escalates because no run was resolved");
    assert!(err.to_string().contains("run_id = 77"), "got: {err}");

    let mut wrong_action = escalation_route(&f.subject, "headsha-validate");
    wrong_action.reservation.action = CiAction::RerunRun;
    let err = holds
        .apply_poll(&identity, &identity, &poll_id, false, &wrong_action)
        .await
        .expect_err("an escalation authorizes an adjudication, not a rerun");
    assert!(err.to_string().contains("rerun_run"), "got: {err}");

    // The refusals happened at the API boundary, so nothing was applied.
    assert_eq!(
        holds
            .get(&identity)
            .await
            .unwrap()
            .expect("streak")
            .poll_count,
        0
    );

    // An apply against an identity that has no streak is NotFound, never a side
    // effect.
    let never_polled = hold_identity(&f.subject, "headsha-never-polled");
    assert_eq!(
        holds
            .apply_poll(
                &never_polled,
                &never_polled,
                &uuid::Uuid::now_v7().to_string(),
                false,
                &escalation_route(&f.subject, "headsha-never-polled"),
            )
            .await
            .unwrap(),
        CiHoldApply::NotFound
    );
    assert!(holds.get(&never_polled).await.unwrap().is_none());

    // And an apply for an unknown observation on a KNOWN streak is NotFound too.
    assert_eq!(
        holds
            .apply_poll(
                &identity,
                &identity,
                "no-such-observation",
                false,
                &escalation_route(&f.subject, "headsha-validate"),
            )
            .await
            .unwrap(),
        CiHoldApply::NotFound
    );
}

/// The sequence high-watermarks never retreat, whatever writes them.
///
/// The repository never issues a decrease, which is exactly why the trigger is
/// here: "retained high-watermark" is the property the entire ordering contract
/// rests on, and an operational `UPDATE ... SET last_applied_poll_sequence = 0`
/// to "unstick" a streak would silently re-admit every superseded observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_sequence_watermarks_never_retreat() {
    let f = fixture().await;
    let holds = CiIncompleteHoldRepository::new(f.db.clone());
    let identity = hold_identity(&f.subject, "headsha-watermark");
    let escalation = escalation_route(&f.subject, "headsha-watermark");
    one_poll(&holds, &identity, &escalation, false).await;
    one_poll(&holds, &identity, &escalation, false).await;
    let before = holds.get(&identity).await.unwrap().expect("streak");
    assert_eq!(before.next_poll_sequence, 2);
    assert_eq!(before.last_applied_poll_sequence, 2);

    for (column, needle) in [
        ("last_applied_poll_sequence", "high-watermark"),
        ("next_poll_sequence", "monotonic"),
    ] {
        let err = sqlx::query(&format!(
            "UPDATE ci_incomplete_hold_streaks SET {column} = 0"
        ))
        .execute(f.db.pool())
        .await
        .expect_err("the monotonicity trigger refuses a retreat");
        assert!(
            err.to_string().contains(needle),
            "expected the {column} guard, got: {err}"
        );
    }

    let after = holds.get(&identity).await.unwrap().expect("streak");
    assert_eq!(after.next_poll_sequence, before.next_poll_sequence);
    assert_eq!(
        after.last_applied_poll_sequence,
        before.last_applied_poll_sequence
    );
}

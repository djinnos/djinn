//! Proposal 4etb: rung-1 planner remediation is retired and every in-scope
//! stuck-task trigger routes DIRECTLY to the forensic arbiter.
//!
//! These tests cover the acceptance matrix the proposal specifies. The load
//! bearing properties, in the proposal's own order:
//!
//! 1. Each in-scope trigger stamps a pending `escalation_evidence_at`,
//!    preserves its trigger evidence, and creates an arbiter child directly —
//!    and repeated ticks retain ONE epoch, ONE arbitration row and ONE child.
//! 2. Park evidence uses the inclusive canonical floor
//!    `max(escalation_evidence_at, last_intervention_at, human_review_resolved_at)`
//!    with a bounded, one-time, PERSISTED legacy fallback.
//! 3. A guard-declined park consumes its open arbitration row and its
//!    `park_redispatch` marker exactly once and does not read
//!    `intervention_count`.
//! 4. Promotion is derived from `task_arbitrations.hold_cycle`: rows 0/1/2 are
//!    ordinary cycles, row 3 is exactly one final arbiter, row 4 is never
//!    created.
//! 5. The terminal rung permits blocker-derived rounds 1/2/3 and never a 4th,
//!    and the unchanged close of round 3 applies the exhausted-ladder ownership
//!    contract.
//! 6. Trigger evidence reaches the arbiter VERBATIM — this proposal changes
//!    which rung the payload reaches, never what it says.
//!
//! Every assertion below reads a durable side effect (a row, a column, an
//! activity payload), never a log line: a fix that changes the consequence
//! without changing the trigger must fail these.

use super::*;
use djinn_core::models::TransitionAction;
use djinn_db::repositories::task::{
    ADJUDICATION_OUTCOME_EVENT, LADDER_EXHAUSTED_CLOSE_REASON, SOURCE_CHANGED, SOURCE_UNCHANGED,
    TERMINAL_CLOSE_REASON_CLASS, is_adjudication_child,
};
use djinn_db::repositories::task_arbitration::TaskArbitrationRepository;

// ── Fixtures ────────────────────────────────────────────────────────────────

/// A worker task that has crossed the quality-strike threshold — i.e. exactly
/// what trigger A hands to the router. Deliberately does NOT touch
/// `intervention_count`: under 4etb the arbiter rung is unconditional and a
/// fixture that pre-seeds that counter would hide a regression that reinstates
/// the old `intervention_count >= MAX_PLANNER_INTERVENTIONS` gate.
async fn stuck_worker_task(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> djinn_core::models::Task {
    let task = make_task_with_reopen_count(db, tx, REOPEN_INTERVENTION_THRESHOLD).await;
    assert_eq!(
        task.intervention_count, 0,
        "the 4etb fixture must reach the arbiter with a ZERO intervention_count — \
         a non-zero value here would let a reinstated counter gate pass silently"
    );
    assert!(
        task.escalation_evidence_at.is_none(),
        "a task that has never been escalated must have no evidence epoch"
    );
    task
}

async fn arbitration_rows(db: &Database, task_id: &str) -> Vec<i32> {
    let mut cycles: Vec<i32> = TaskArbitrationRepository::new(db.clone())
        .list_for_task(task_id)
        .await
        .expect("read arbitration ledger")
        .into_iter()
        .map(|record| record.hold_cycle)
        .collect();
    cycles.sort_unstable();
    cycles
}

async fn arbiter_dispatch_payloads(repo: &TaskRepository, task_id: &str) -> Vec<serde_json::Value> {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(ARBITER_DISPATCHED_MARKER.to_string()),
        limit: 100,
        ..ActivityQuery::default()
    })
    .await
    .unwrap()
    .into_iter()
    .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap())
    .collect()
}

/// Every remediation child the coordinator created for this source, by label.
async fn remediation_child_labels(repo: &TaskRepository, source_id: &str) -> Vec<String> {
    let mut labels = Vec::new();
    for blocker in repo.list_blockers(source_id).await.unwrap() {
        if let Ok(Some(child)) = repo.get(&blocker.task_id).await {
            labels.push(child.labels.clone());
        }
    }
    labels
}

// ── AC1: direct routing, one epoch / one row / one child ────────────────────

/// **AC1.** The FIRST trigger on a task whose `intervention_count` is zero must
/// reach the arbiter: it stamps the evidence epoch, opens arbitration row 0,
/// and creates an arbiter child. It must NOT create a rung-1 planner
/// remediation — that rung is gone, and the `planner-remediation` label is the
/// durable proof it did not come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_trigger_routes_directly_to_the_arbiter_and_stamps_the_epoch() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = stuck_worker_task(&db, &tx).await;
    let handled = actor
        .route_arbiter_adjudication(&task, "worker", "trigger A: quality strikes", None, 5)
        .await;
    assert!(
        handled,
        "the first trigger must be handled by the arbiter rung"
    );

    let after = repo.get(&task.id).await.unwrap().unwrap();
    let epoch = after
        .escalation_evidence_at
        .clone()
        .expect("the trigger must stamp a pending escalation_evidence_at");
    assert!(
        !epoch.is_empty(),
        "the stamped epoch must be a real timestamp, not an empty marker"
    );

    assert_eq!(
        arbitration_rows(&db, &task.id).await,
        vec![0],
        "the first trigger opens exactly arbitration row 0"
    );
    assert_eq!(
        arbiter_dispatch_payloads(&repo, &task.id).await.len(),
        1,
        "exactly one arbiter child is dispatched"
    );
    for labels in remediation_child_labels(&repo, &task.id).await {
        assert!(
            !labels.contains("planner-remediation"),
            "rung 1 is retired: no path may create a first-response planner \
             remediation child (found labels {labels})"
        );
    }
}

/// **AC1.** A repeated coordinator tick while the SAME escalation is pending
/// must not rewrite the epoch, must not open a second arbitration row, and must
/// not dispatch a second child. The epoch + unconsumed row ARE the idempotency
/// boundary — there is no activity-marker guard behind them any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_ticks_retain_one_epoch_one_row_and_one_child() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = stuck_worker_task(&db, &tx).await;
    actor
        .route_arbiter_adjudication(&task, "worker", "first tick", None, 5)
        .await;
    let first = repo.get(&task.id).await.unwrap().unwrap();
    let first_epoch = first.escalation_evidence_at.clone().unwrap();

    for tick in 0..3 {
        let refreshed = repo.get(&task.id).await.unwrap().unwrap();
        actor
            .route_arbiter_adjudication(&refreshed, "worker", "repeat tick", None, 5)
            .await;
        let now = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            now.escalation_evidence_at.as_deref(),
            Some(first_epoch.as_str()),
            "tick {tick}: a pending escalation must never restamp its epoch"
        );
    }

    assert_eq!(
        arbitration_rows(&db, &task.id).await,
        vec![0],
        "repeated ticks must not open a second arbitration row"
    );
    assert_eq!(
        arbiter_dispatch_payloads(&repo, &task.id).await.len(),
        1,
        "repeated ticks must not emit a second arbiter_dispatched event"
    );
}

// ── AC9: trigger evidence passthrough ───────────────────────────────────────

/// **AC9.** CI/merge-queue classification stays out of scope: the escalation
/// reason and its `ci_failure_sections` reach the arbiter dossier VERBATIM.
/// This asserts the payload the arbiter reads, not the log line — a rung that
/// silently reinterpreted or dropped the classification would fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_evidence_reaches_the_arbiter_dossier_unchanged() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);

    let task = stuck_worker_task(&db, &tx).await;
    const REASON: &str = "CI loop: required check `Quality Gate` failing on the current head";
    const SECTIONS: &str = "- `cargo clippy` failed (ci_job_log(job_id=4242))";

    actor
        .route_arbiter_adjudication(&task, "worker", REASON, Some(SECTIONS), 5)
        .await;

    let record = TaskArbitrationRepository::new(db.clone())
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .unwrap()
        .expect("arbitration row 0 must exist");
    let dossier = record.dossier.expect("the arbiter must receive a dossier");

    let trigger_reason = dossier["trigger_reason"]
        .as_str()
        .expect("the dossier must carry the trigger reason verbatim");
    assert!(
        trigger_reason.contains(REASON),
        "the trigger's own reason must reach the arbiter unchanged; got {trigger_reason}"
    );
    assert!(
        trigger_reason.contains(SECTIONS),
        "the CI failure sections must reach the arbiter unchanged; got {trigger_reason}"
    );
    assert_eq!(
        dossier["ci_failure_sections"].as_str().map(str::to_owned),
        Some(SECTIONS.to_owned()),
        "ci_failure_sections must be preserved as its own field, not reinterpreted"
    );
    assert_eq!(
        record.failing_ci_job_ids,
        serde_json::json!([4242]),
        "the ledger's failing job ids come from the SAME sections text"
    );
}

// ── AC2: the canonical evidence floor ───────────────────────────────────────

/// **AC2.** The floor is `max(escalation_evidence_at, last_intervention_at,
/// human_review_resolved_at)` — a later intervention or human review RAISES it,
/// so evidence predating that reset cannot justify a later park.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_floor_is_the_max_of_all_three_instants() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "floor fixture").await;
    repo.stamp_escalation_evidence_epoch(&task.id)
        .await
        .unwrap();
    let with_epoch = repo.get(&task.id).await.unwrap().unwrap();
    let epoch = with_epoch.escalation_evidence_at.clone().unwrap();

    // Epoch alone.
    assert_eq!(
        actor.canonical_evidence_floor(&with_epoch).await.as_deref(),
        Some(epoch.as_str()),
        "with no intervention or hold release, the epoch IS the floor"
    );

    // A LATER intervention wins.
    let later = "2999-01-01T00:00:00.000Z".to_owned();
    let mut with_intervention = with_epoch.clone();
    with_intervention.last_intervention_at = Some(later.clone());
    assert_eq!(
        actor
            .canonical_evidence_floor(&with_intervention)
            .await
            .as_deref(),
        Some(later.as_str()),
        "an intervention after the trigger must RAISE the floor"
    );

    // An EARLIER intervention loses.
    let mut with_old_intervention = with_epoch.clone();
    with_old_intervention.last_intervention_at = Some("1999-01-01T00:00:00.000Z".to_owned());
    assert_eq!(
        actor
            .canonical_evidence_floor(&with_old_intervention)
            .await
            .as_deref(),
        Some(epoch.as_str()),
        "an instant older than the epoch must not lower the floor"
    );
}

/// **AC2.** A task that has NEVER been escalated has no floor, so
/// `post_intervention_history` reports nothing — an unescalated worker dispatch
/// must not pick up rotation exclusions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unescalated_task_has_no_floor_and_no_history() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let (task, _path) = create_simple_task(&db, &tx, "task", "never escalated").await;
    assert!(
        actor.canonical_evidence_floor(&task).await.is_none(),
        "a task with no epoch, no intervention and no hold release has no floor"
    );
    let history = actor.post_intervention_history(&task).await;
    assert!(history.evidence_floor.is_none());
    assert!(!history.any_submitted);
    assert!(
        history.rotation_excluded_models().is_empty(),
        "no floor means no evidence, which means no rotation exclusions — this is \
         what keeps the ungated (4etb) rotation block inert for ordinary dispatch"
    );
}

/// **AC2.** The LEGACY fallback: a task that was already awaiting adjudication
/// when 4etb shipped has no epoch and no transition left that would stamp one.
/// It gets `max(last_intervention_at, human_review_resolved_at, updated_at)`
/// ONCE, and that value is PERSISTED — the second read must come from the
/// column, not be re-derived, and there must be no unbounded-history fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_pending_escalation_persists_its_fallback_epoch_once() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "legacy pending").await;
    let task = repo
        .set_status(&task.id, "needs_lead_intervention")
        .await
        .unwrap();
    assert!(
        task.escalation_evidence_at.is_none(),
        "the legacy fixture must start with no epoch"
    );

    let floor = actor.canonical_evidence_floor(&task).await.expect(
        "a legacy pending escalation must get the bounded fallback, never \
                 an unbounded-history read",
    );

    let persisted = repo
        .escalation_evidence_at(&task.id)
        .await
        .unwrap()
        .expect("the fallback must be PERSISTED, not computed per evaluation");
    assert_eq!(
        persisted, floor,
        "the value handed to the guards must be the one written to the column"
    );

    // Second evaluation must not move it.
    let refreshed = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        actor.canonical_evidence_floor(&refreshed).await.as_deref(),
        Some(persisted.as_str()),
        "the one-time fallback must be exactly one-time"
    );
}

// ── AC2: the INCLUSIVE floor comparison and the pre-epoch exclusion ─────────

/// Seed one SUBMITTED worker attempt on `task_id` and return the durable row.
///
/// `post_intervention_history` compares the row's `created_at` against the
/// evidence floor, and counts it in `qualifying_submission_count` /
/// `any_submitted` when it qualifies — so this row is the only thing the
/// floor-boundary tests below need.
async fn seed_submitted_worker_attempt(
    db: &Database,
    task_id: &str,
    tag: &str,
) -> djinn_core::models::task_attempt::TaskAttempt {
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("evidence-floor-{tag}-{id}");
    let attempt = attempt_repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id,
            role: "worker",
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed a pending worker attempt");
    attempt_repo
        .advance_to_submitted(
            djinn_db::repositories::task_attempt::SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            },
        )
        .await
        .expect("advance the seeded worker attempt to submitted")
}

/// **AC2.** The floor comparison is INCLUSIVE. The proposal is explicit: "Only
/// attempts and submissions with `created_at >= evidence_floor` qualify. The
/// comparison is inclusive so an attempt written in the same transaction
/// timestamp is not lost." Task timestamps are millisecond text, and the epoch
/// stamp and the attempt insert genuinely share a transaction instant in
/// production, so an exclusive `>` would silently discard the one piece of
/// evidence uv3p needs and re-park forever.
///
/// This is the ONLY test that pins the boundary itself: every other fixture
/// sleeps so its evidence sorts strictly after the floor, and an auditor who
/// changed `>=` to `>` in `post_intervention_history` left all 2083 tests
/// green. Both floor terms that can win at the boundary are exercised — the
/// epoch and `last_intervention_at` — because the filter reads whatever
/// `canonical_evidence_floor` returned, not a particular column.
///
/// **If the `role == "worker" && created_at >= floor` filter body were deleted**
/// and every attempt qualified unconditionally, this test would still pass: it
/// is a boundary test, not an over-inclusion test. What it does catch, and
/// nothing else does, is the boundary moving off the floor by one instant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attempt_at_exactly_the_evidence_floor_qualifies() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let (task, _path) = create_simple_task(&db, &tx, "task", "inclusive floor boundary").await;
    let attempt = seed_submitted_worker_attempt(&db, &task.id, "equal").await;

    // (a) the EPOCH lands on exactly the attempt's instant — the same
    // transaction timestamp the proposal names.
    let mut epoch_at_the_attempt = task.clone();
    epoch_at_the_attempt.escalation_evidence_at = Some(attempt.created_at.clone());
    assert_eq!(
        actor
            .canonical_evidence_floor(&epoch_at_the_attempt)
            .await
            .as_deref(),
        Some(attempt.created_at.as_str()),
        "fixture: the floor must sit on exactly the attempt's created_at"
    );

    let history = actor.post_intervention_history(&epoch_at_the_attempt).await;
    assert_eq!(
        history.evidence_floor.as_deref(),
        Some(attempt.created_at.as_str())
    );
    assert_eq!(
        history.qualifying_submission_count, 1,
        "an attempt written AT the evidence floor must qualify — `>=`, not `>`"
    );
    assert!(
        history.any_submitted,
        "the same-instant submission is the evidence uv3p measures; losing it \
         re-declines the park on every tick forever"
    );
    assert!(
        history.latest_submission_at.is_some(),
        "the qualifying submission must also be visible to the 2vxr freshness guard"
    );

    // (b) `last_intervention_at` wins the max() and lands on the same instant.
    // The filter must read the RESOLVED floor, whichever term produced it.
    let mut intervention_at_the_attempt = task.clone();
    intervention_at_the_attempt.last_intervention_at = Some(attempt.created_at.clone());
    assert_eq!(
        actor
            .canonical_evidence_floor(&intervention_at_the_attempt)
            .await
            .as_deref(),
        Some(attempt.created_at.as_str())
    );
    assert_eq!(
        actor
            .post_intervention_history(&intervention_at_the_attempt)
            .await
            .qualifying_submission_count,
        1,
        "the inclusive comparison is a property of the FLOOR, not of one column"
    );
}

/// **AC2, truth-table row 7** ("Trigger fires after an older successful
/// submission → new trigger timestamp → older submission excluded → decline;
/// 2vxr cannot use a prior head"). Evidence that predates the epoch belongs to
/// a previous head and a previous adjudication; letting it through is exactly
/// the 2vxr incident, where CI evidence from a prior head justified a park.
///
/// The control assertion is what makes this non-vacuous: the SAME durable
/// attempt row is measured twice, once against a floor at its own instant
/// (qualifies) and once against the newer epoch (excluded). A fixture that
/// simply failed to create the attempt would fail the control.
///
/// **If the `created_at >= floor` filter body were deleted** so every attempt
/// qualified, the control would still pass and this test's second half would
/// fail — which is the point.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_epoch_evidence_is_excluded_from_the_current_adjudication() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "prior-head submission").await;
    let seeded = seed_submitted_worker_attempt(&db, &task.id, "prior").await;
    // Push the submission an hour into the past: it belongs to the head the
    // task carried BEFORE the trigger that is about to fire.
    djinn_db::test_support::backdate_task_attempt_created_at(&db, &seeded.id, "1 hour").await;
    let older = TaskAttemptRepository::new(db.clone())
        .get(&seeded.id)
        .await
        .unwrap()
        .expect("the backdated attempt row must still exist");

    // Control: with the floor at the older submission's own instant it DOES
    // qualify, so the exclusion below cannot be an artefact of a broken seed.
    let mut floor_at_the_older_attempt = task.clone();
    floor_at_the_older_attempt.escalation_evidence_at = Some(older.created_at.clone());
    assert_eq!(
        actor
            .post_intervention_history(&floor_at_the_older_attempt)
            .await
            .qualifying_submission_count,
        1,
        "control: the seeded submission is real and countable"
    );

    // The new trigger stamps a NEW epoch — after the older submission.
    let epoch = repo
        .stamp_escalation_evidence_epoch(&task.id)
        .await
        .unwrap()
        .expect("the trigger must stamp an epoch");
    assert!(
        epoch.as_str() > older.created_at.as_str(),
        "fixture: the new trigger must be strictly newer than the older submission \
         (epoch {epoch}, submission {})",
        older.created_at
    );
    let refreshed = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        refreshed.escalation_evidence_at.as_deref(),
        Some(epoch.as_str()),
        "the epoch under test must be the PERSISTED column, not a fixture-local value"
    );

    let history = actor.post_intervention_history(&refreshed).await;
    assert_eq!(history.evidence_floor.as_deref(), Some(epoch.as_str()));
    assert_eq!(
        history.qualifying_submission_count, 0,
        "a submission that predates the epoch belongs to a prior head and must not \
         count toward the current adjudication"
    );
    assert!(
        !history.any_submitted,
        "uv3p must decline the park: no post-trigger worker evidence exists"
    );
    assert!(
        history.latest_submission_at.is_none(),
        "2vxr must not be handed a prior head's submission as the current one"
    );
    assert!(
        history.non_attempt_models.is_empty() && history.rotation_excluded_models().is_empty(),
        "a pre-epoch attempt must not contribute rotation exclusions either"
    );
}

/// **AC2, truth-table row 5** ("Prior human review resolves after trigger →
/// floor is the human-review resolution timestamp → only attempts at or after
/// review resolution qualify"). `human_review_resolved_at` is the third term of
/// `max(escalation_evidence_at, last_intervention_at, human_review_resolved_at)`
/// and, before this test, was never the WINNING one anywhere in the suite: a
/// floor implemented as `max(epoch, last_intervention_at)` would have passed
/// every other test in this file.
///
/// The durable evidence is the `human_review_resolved_at` column stamped by
/// `TaskRepository::mark_human_review_resolved` when the hold child resolves,
/// plus the attempt row that sits strictly between the epoch and that stamp.
///
/// **If `canonical_evidence_floor`'s body were reduced to just the epoch**, the
/// control assertion would still pass and the post-resolution assertions would
/// fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_human_review_resolution_wins_the_floor_and_excludes_earlier_evidence() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "review resolution floor").await;
    let hold = repo
        .create_fixture_in_project(
            &task.project_id,
            task.epic_id.as_deref(),
            "Human review hold",
            "hold",
            "",
            "review",
            1,
            "",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    repo.add_blocker(&task.id, &hold.id).await.unwrap();

    // 1. the trigger stamps the epoch.
    let epoch = repo
        .stamp_escalation_evidence_epoch(&task.id)
        .await
        .unwrap()
        .expect("the trigger must stamp an epoch");

    // 2. a worker submits AFTER the epoch — at this point it qualifies.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let attempt = seed_submitted_worker_attempt(&db, &task.id, "pre-review").await;
    let mid = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        actor
            .post_intervention_history(&mid)
            .await
            .qualifying_submission_count,
        1,
        "control: before the review resolves, the post-epoch submission qualifies"
    );

    // 3. the human review resolves LAST, raising the floor above everything.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        repo.mark_human_review_resolved(&hold.id).await.unwrap(),
        1,
        "the resolving hold must stamp exactly its own source"
    );
    let resolved_at = repo
        .human_review_resolved_at(&task.id)
        .await
        .unwrap()
        .expect("mark_human_review_resolved must write the durable column");

    let mut after = repo.get(&task.id).await.unwrap().unwrap();
    // `last_intervention_at` sits on the submission's own instant: strictly
    // later than the epoch, and still strictly below the review resolution. So
    // the review resolution is the max of all THREE terms, and if it were
    // dropped the submission would qualify.
    after.last_intervention_at = Some(attempt.created_at.clone());

    assert!(
        resolved_at.as_str() > epoch.as_str() && resolved_at.as_str() > attempt.created_at.as_str(),
        "fixture: the review resolution must be later than both the epoch ({epoch}) \
         and the intervention/submission instant ({})",
        attempt.created_at
    );
    assert_eq!(
        actor.canonical_evidence_floor(&after).await.as_deref(),
        Some(resolved_at.as_str()),
        "human_review_resolved_at is the max of the three terms and therefore IS the floor"
    );

    let history = actor.post_intervention_history(&after).await;
    assert_eq!(
        history.evidence_floor.as_deref(),
        Some(resolved_at.as_str()),
        "the guards must measure against the review-resolution floor"
    );
    assert_eq!(
        history.qualifying_submission_count, 0,
        "evidence between the epoch and the review resolution is discarded: a human \
         already adjudicated it, so it cannot justify a later park"
    );
    assert!(!history.any_submitted);
    assert!(history.latest_submission_at.is_none());
}

// ── AC5: bounded promotion from the arbitration ledger ──────────────────────

/// **AC5.** `MAX_ARBITER_HOLD_CYCLES = 3` has one exact meaning: rows 0, 1 and 2
/// are ordinary forensic cycles and row 3 is exactly ONE final-disposition
/// arbiter. Row 4 is never created, and re-entering after the final arbiter
/// routes to the terminal rung rather than opening another row.
///
/// The assertion reads the arbitration ledger — the durable ledger the design
/// derives promotion from — so a regression that re-introduced a task-level
/// cycle counter would not make this pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_opens_rows_0_1_2_then_one_final_row_3_and_never_row_4() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let arb = TaskArbitrationRepository::new(db.clone());

    let task = stuck_worker_task(&db, &tx).await;

    // Cycle 0 is opened by the trigger itself and is UNCONDITIONAL — the park
    // guards gate a park, not arbiter entry.
    actor
        .route_arbiter_adjudication(&task, "worker", "ordinary cycle 0", None, 5)
        .await;
    assert!(
        arb.get_by_task_and_cycle(&task.id, 0)
            .await
            .unwrap()
            .is_some(),
        "cycle 0 must open its own arbitration row"
    );
    arb.mark_consumed(&task.id, 0).await.unwrap();

    // Cycles 1 and 2 are ORDINARY forensic rounds. Seed them the way the
    // production ladder does (open + consume) rather than driving them through
    // the park guards: whether a given tick passes uv3p/8y3q/2vxr is those
    // guards' own contract and has its own tests, while the property under test
    // here is the exact promotion arithmetic over `task_arbitrations.hold_cycle`.
    for cycle in 1..3 {
        let empty = serde_json::json!([]);
        arb.try_create(
            djinn_db::repositories::task_arbitration::CreateArbitrationParams {
                task_id: &task.id,
                hold_cycle: cycle,
                deadline_at: None,
                mirror_head_sha: None,
                github_head_sha: None,
                pr_url: None,
                failing_ci_job_ids: &empty,
                dossier: None,
                directive: Some(&serde_json::json!({ "decision": "reopen" })),
                verification_command: None,
                excluded_models: &empty,
            },
        )
        .await
        .expect("seed ordinary cycle");
        assert!(
            arb.mark_consumed(&task.id, cycle).await.unwrap(),
            "ordinary cycle {cycle} must be consumable exactly once"
        );
    }

    // Prospective cycle 3: exactly one FINAL arbiter.
    let refreshed = repo.get(&task.id).await.unwrap().unwrap();
    actor
        .route_arbiter_adjudication(&refreshed, "worker", "final cycle", None, 5)
        .await;
    let final_row = arb
        .get_by_task_and_cycle(&task.id, 3)
        .await
        .unwrap()
        .expect("prospective cycle 3 opens the one-shot final arbiter");
    assert_eq!(
        final_row
            .dossier
            .as_ref()
            .and_then(|d| d["final_disposition"].as_bool()),
        Some(true),
        "row 3 must be dispatched as the FINAL disposition, not an ordinary cycle"
    );
    arb.mark_consumed(&task.id, 3).await.unwrap();

    // Any further trigger must NOT open row 4.
    let refreshed = repo.get(&task.id).await.unwrap().unwrap();
    actor
        .route_arbiter_adjudication(&refreshed, "worker", "post-final re-entry", None, 5)
        .await;
    assert_eq!(
        arbitration_rows(&db, &task.id).await,
        vec![0, 1, 2, 3],
        "row 4 must never be created — the ladder ends at the final arbiter"
    );
}

// ── AC6/AC7: terminal rung and exhausted-ladder ownership ───────────────────

/// **AC7.** With the terminal rung spent, a source carrying an OPEN, UNMERGED
/// PR must be handed to the PR poller (`pr_review`) — never left `open`, the
/// status nothing scans, which is exactly how `z8i8`/`zkas` stranded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_ladder_hands_an_open_unmerged_pr_to_the_poller() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "exhausted with a PR").await;
    repo.set_pr_url(&task.id, "https://github.com/o/r/pull/7")
        .await
        .unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    assert!(
        actor
            .apply_exhausted_ladder_ownership(&task, "ladder spent")
            .await,
        "the ownership contract must complete"
    );
    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "pr_review",
        "an open unmerged PR belongs to the PR poller, not to nobody"
    );

    // Idempotency: a repeated tick preserves the already-owned state.
    assert!(
        actor
            .apply_exhausted_ladder_ownership(&after, "ladder spent")
            .await
    );
    let again = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        again.status, "pr_review",
        "an already-owned source must be preserved, not re-dispositioned"
    );
}

/// **AC7.** A source with NO PR is terminal by contract, with the EXACT close
/// reason the ownership contract specifies. Matching on that prefix must stay
/// possible: an operator query for stranded exhausted work keys on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_ladder_force_closes_a_no_pr_source_with_the_contractual_reason() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "exhausted with no PR").await;
    assert!(
        actor
            .apply_exhausted_ladder_ownership(&task, "ladder spent")
            .await
    );

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(after.status, "closed");

    let reason_text = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("status_changed".to_owned()),
            limit: 50,
            ..ActivityQuery::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .filter(|p| p["to_status"] == "closed")
        .filter_map(|p| p["reason"].as_str().map(str::to_owned))
        .next()
        .expect("the terminal close must carry a reason");
    assert!(
        reason_text.starts_with(djinn_db::repositories::task::LADDER_EXHAUSTED_CLOSE_REASON),
        "the contractual reason must be the PREFIX so operator queries match it; \
         got {reason_text}"
    );
}

/// **AC7.** An already-terminal source is preserved verbatim: the contract must
/// not rewrite a disposition somebody else already made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_ladder_preserves_an_already_terminal_source() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let (task, _path) = create_simple_task(&db, &tx, "task", "already closed").await;
    let closed = repo.set_status(&task.id, "closed").await.unwrap();

    assert!(
        actor
            .apply_exhausted_ladder_ownership(&closed, "ladder spent")
            .await,
        "the already-owned branch is a successful no-op"
    );
    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(after.status, "closed");
    assert!(
        after.close_reason.as_deref()
            != Some(djinn_db::repositories::task::LADDER_EXHAUSTED_CLOSE_REASON),
        "an already-terminal source must keep ITS close reason, not acquire the \
         exhausted-ladder one"
    );
}

// ── Negative space: the retired rung stays retired ──────────────────────────

/// The whole point of 4etb: `RemediationKind` no longer has a first-response
/// `Planner` variant, so `create_remediation_task` cannot mint one. This is a
/// compile-time-adjacent guard — it enumerates the surviving kinds so a future
/// change that re-adds the rung has to touch this test deliberately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_held_remediation_kinds_survive() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    for (kind, expected_label) in [
        (
            crate::dispatch::RemediationKind::PlannerEscalation,
            crate::roles::PLANNER_PARK_ESCALATION_LABEL,
        ),
        (
            crate::dispatch::RemediationKind::HumanReview,
            crate::roles::HUMAN_REVIEW_HOLD_LABEL,
        ),
    ] {
        let (source, _path) =
            create_simple_task(&db, &tx, "task", &format!("source for {kind:?}")).await;
        actor
            .create_remediation_task(&source.id, "reason", &source.project_id, kind)
            .await;

        let labels = remediation_child_labels(&repo, &source.id).await;
        assert_eq!(
            labels.len(),
            1,
            "{kind:?} must create exactly one held child"
        );
        assert!(
            labels[0].contains(expected_label),
            "{kind:?} must be labelled {expected_label}; got {}",
            labels[0]
        );
        assert!(
            !labels[0].contains("planner-remediation"),
            "no surviving kind may carry the retired first-response label"
        );
    }
}

// ── AC7/AC8: the child-close contract, driven by its REAL producers ─────────
//
// Everything above this line either calls `apply_exhausted_ladder_ownership`
// (the COORDINATOR-side copy of the contract) directly, or asserts a property
// that never reaches a child close at all. The DB-side copy —
// `djinn_db::repositories::task::apply_adjudication_child_close_tx`, which is
// the one production actually takes when a planner closes an adjudication
// child — had NO coverage in this crate: an audit made its entire body inert
// and all 2092 lib tests here stayed green, because the only tests that
// exercised it were its own djinn-db fixture tests, and that fixture calls
// `record_adjudication_source_snapshot` itself and sets the source's statuses
// by hand.
//
// Every one of the four defects found in this mechanism was "the fixture
// agreed with itself while production did something different", and each one
// turned on the SNAPSHOT-TIME STATUS a particular producer leaves behind. So
// the tests below drive the real producers, three real escalation rounds each,
// and close every round's child through `TaskRepository::transition` — the real
// close path, the one that runs the transaction.
//
// What each test would still pass with the mechanism's body deleted is stated
// on the test itself.

/// Every `adjudication_outcome` payload written for the (source, child) pair.
///
/// Matched on `adjudication_child_id` rather than on position: `query_activity`
/// orders by `created_at DESC` at millisecond resolution, and three rounds of a
/// fast test genuinely tie.
async fn outcome_for_child(
    repo: &TaskRepository,
    source_id: &str,
    child_id: &str,
) -> serde_json::Value {
    let mut found: Vec<serde_json::Value> = repo
        .query_activity(ActivityQuery {
            task_id: Some(source_id.to_owned()),
            event_type: Some(ADJUDICATION_OUTCOME_EVENT.to_owned()),
            limit: 100,
            ..ActivityQuery::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .filter(|payload| payload["adjudication_child_id"] == child_id)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one adjudication_outcome event must exist for child {child_id} on source \
         {source_id}; found {}",
        found.len()
    );
    found.pop().expect("length asserted above")
}

/// Every durable `pr_terminal_handoff` marker on `task_id`. This is the marker
/// operator queries key on to find work the exhausted ladder pushed back to the
/// poller — the DB-side copy of the contract must write it, not just move the
/// status column.
async fn pr_terminal_handoffs(repo: &TaskRepository, task_id: &str) -> Vec<serde_json::Value> {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some("pr_terminal_handoff".to_owned()),
        limit: 100,
        ..ActivityQuery::default()
    })
    .await
    .unwrap()
    .into_iter()
    .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
    .collect()
}

/// The one unresolved adjudication child currently holding `source_id`.
///
/// Derived from the durable blocker rows + the child's own label, exactly the
/// way `planner_escalation_count_tx` derives the round number — so a producer
/// that stopped labelling its child, or stopped linking it, fails here rather
/// than silently dropping out of the ladder.
async fn held_by_one_adjudication_child(repo: &TaskRepository, source_id: &str) -> String {
    let mut open = Vec::new();
    for blocker in repo.list_blockers(source_id).await.unwrap() {
        let child = repo
            .get(&blocker.task_id)
            .await
            .unwrap()
            .expect("a blocker row must point at a real task");
        if child.status != "closed" && is_adjudication_child(&child.labels) {
            open.push(child.id);
        }
    }
    assert_eq!(
        open.len(),
        1,
        "the producer must leave the source held by exactly one unresolved adjudication child; \
         found {open:?}"
    );
    open.pop().expect("length asserted above")
}

/// Close an adjudication child through the REAL close path.
///
/// `TaskRepository::transition` is where `apply_adjudication_child_close_tx`
/// runs, in the child's own transaction. Nothing here pokes the source.
async fn close_adjudication_child(repo: &TaskRepository, child_id: &str, rationale: &str) {
    repo.transition(
        child_id,
        TransitionAction::Close,
        "planner",
        "planner",
        Some(rationale),
        None,
    )
    .await
    .expect("the planner's close of an adjudication child must succeed");
}

/// Assert the non-terminal rounds: a child close that adjudicated nothing is
/// coordinator scaffolding. It releases the source back to `open` and reports
/// `source_unchanged`.
///
/// This is where defect instances 2 and 3 lived. The exclusion that makes a
/// producer's `<snapshot status> -> open` park read as scaffolding was once
/// narrowed to the two Lead-intervention statuses, which silently reinstated
/// `source_changed` for every producer that snapshots from a PR status — and a
/// `source_changed` round-3 close never invokes exhausted ownership at all.
async fn assert_intermediate_round_released_the_source(
    repo: &TaskRepository,
    source_id: &str,
    child_id: &str,
    round: i64,
    producer: &str,
) {
    let source = repo.get(source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "open",
        "{producer} round {round}: rounds below the ceiling release the source, they do not \
         dispose of it"
    );
    assert_eq!(
        source.close_reason, None,
        "{producer} round {round}: a released source carries no terminal close reason"
    );
    let outcome = outcome_for_child(repo, source_id, child_id).await;
    assert_eq!(
        outcome["adjudication_outcome"], SOURCE_UNCHANGED,
        "{producer} round {round}: closing the child, releasing its blocker and letting the next \
         round open is scaffolding — the source's business disposition did not move"
    );
    assert_eq!(
        outcome["terminal_rung_round"], round,
        "{producer} round {round}: the round number is derived from the durable blocker rows"
    );
}

/// Assert the terminal round for a source carrying an OPEN, UNMERGED PR: it
/// lands in `pr_review` (the PR poller's queue) with the durable handoff marker,
/// and the outcome event reports the handoff as a disposition change described
/// by THIS transaction's own before/after pair.
///
/// `z8i8`/`zkas` are the incident: those sources exhausted the ladder with
/// unmerged PRs and sat `open` — a status nothing scans — until a human moved
/// them.
///
/// **Marker parity.** The two copies of this contract write the SAME
/// `pr_terminal_handoff` event, byte for byte: same actor and role, the same
/// `adjudication_ladder_exhausted_pr_handoff` reason, and the same `pr_url` /
/// `head_sha` fields. They diverged at first — the DB copy wrote
/// `actor_role = "system"`, the contractual sentence as its reason, and neither
/// PR field — which meant an operator query keyed on the coordinator's reason
/// string (exactly what
/// `escalation_ceiling::ceiling_pr_handoff_is_invariant_across_pr_snapshot_observations`
/// asserts) matched NO DB-side handoff, and the DB copy is the one production
/// takes for a planner-closed child. The constants themselves are asserted
/// equal by `both_pr_handoff_marker_writers_agree_on_actor_role_and_reason`;
/// this asserts the row actually written carries them.
async fn assert_terminal_round_handed_the_pr_to_the_poller(
    repo: &TaskRepository,
    source_id: &str,
    child_id: &str,
    producer: &str,
) {
    let source = repo.get(source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "pr_review",
        "{producer}: the spent ladder must hand an open unmerged PR to the poller, never leave \
         the source `open` with no owner"
    );
    assert_eq!(
        source.close_reason, None,
        "{producer}: the PR-handoff branch does not terminate the source"
    );

    let outcome = outcome_for_child(repo, source_id, child_id).await;
    assert_eq!(
        outcome["adjudication_outcome"], SOURCE_CHANGED,
        "{producer}: the ownership handoff IS a disposition change (outcome truth-table row 4)"
    );
    assert_eq!(
        outcome["source_status_before"], "open",
        "{producer}: the reported pair describes THIS transaction — the producer's own park had \
         already landed the source at `open` before it ran"
    );
    assert_eq!(
        outcome["source_status_after"], "pr_review",
        "{producer}: ...and this transaction is what moved it to the poller"
    );
    assert_eq!(
        outcome["terminal_rung_round"], MAX_AUTONOMOUS_ESCALATIONS,
        "{producer}: the terminal rung is round {MAX_AUTONOMOUS_ESCALATIONS}"
    );

    let handoffs = pr_terminal_handoffs(repo, source_id).await;
    assert_eq!(
        handoffs.len(),
        1,
        "{producer}: exactly one durable pr_terminal_handoff marker — the operator query for \
         ladder-exhausted PR work reads this row, not the status column"
    );
    assert_eq!(handoffs[0]["from_status"], "open", "{producer}");
    assert_eq!(handoffs[0]["to_status"], "pr_review", "{producer}");
    assert_eq!(
        handoffs[0]["reason"],
        djinn_db::repositories::task::PR_HANDOFF_REASON,
        "{producer}: the marker must carry the same reason the coordinator copy writes, so both \
         answer one query"
    );
}

/// Assert the terminal round for a source with NO PR: terminal by contract,
/// with the fixed-vocabulary class in the `close_reason` COLUMN and the
/// contractual sentence on the timeline (which is where the coordinator-side
/// copy puts it too, so one operator query finds both).
async fn assert_terminal_round_closed_a_no_pr_source(
    repo: &TaskRepository,
    source_id: &str,
    child_id: &str,
    producer: &str,
) {
    let source = repo.get(source_id).await.unwrap().unwrap();
    assert_eq!(
        source.status, "closed",
        "{producer}: no PR means no poller can own it — the ladder ends terminally"
    );
    assert_eq!(
        source.close_reason.as_deref(),
        Some(TERMINAL_CLOSE_REASON_CLASS),
        "{producer}: the close_reason column carries the fixed vocabulary class"
    );

    let reasons: Vec<String> = repo
        .query_activity(ActivityQuery {
            task_id: Some(source_id.to_owned()),
            event_type: Some("status_changed".to_owned()),
            limit: 100,
            ..ActivityQuery::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .filter(|p| p["to_status"] == "closed")
        .filter_map(|p| p["reason"].as_str().map(str::to_owned))
        .collect();
    assert!(
        reasons.iter().any(|r| r == LADDER_EXHAUSTED_CLOSE_REASON),
        "{producer}: the terminal disposition must be on the timeline with its exact contractual \
         reason; got {reasons:?}"
    );

    let outcome = outcome_for_child(repo, source_id, child_id).await;
    assert_eq!(
        outcome["adjudication_outcome"], SOURCE_CHANGED,
        "{producer}"
    );
    assert_eq!(outcome["source_status_before"], "open", "{producer}");
    assert_eq!(outcome["source_status_after"], "closed", "{producer}");
    assert!(
        pr_terminal_handoffs(repo, source_id).await.is_empty(),
        "{producer}: a source with no PR must NOT be advertised to the poller"
    );
}

/// **AC7/AC8, producer 1: the loop-breaker / arbiter ceiling, PR-bearing.**
///
/// `escalate_to_planner_or_terminally_fail` is the no-human bottom of the
/// ladder, reached from the dispatch rung and from the arbiter hold-cycle
/// ceiling. It snapshots the source in whatever in-flight state the trigger
/// caught it in — here the Lead-intervention hold the arbiter ceiling parks
/// from — and then parks it `open` behind a fresh `planner-park-escalation`.
///
/// THREE real rounds are driven, and each round's child is closed through
/// `TaskRepository::transition`. Nothing calls
/// `apply_exhausted_ladder_ownership` (the coordinator-side copy); the round-3
/// disposition below is produced entirely by the DB-side child-close
/// transaction.
///
/// **With `apply_adjudication_child_close_tx`'s body made inert** (early
/// `return Ok(0)`): rounds 1 and 2 lose their outcome events, so
/// `outcome_for_child` fails first; and the round-3 source stays `open` with an
/// unmerged PR and no owner — the exact `z8i8`/`zkas` stranding. What would
/// STILL pass with the body deleted: everything about the producer itself — the
/// three children are still created, still labelled, still linked as blockers,
/// and the source is still parked `open` each round. That is precisely why the
/// producer-side assertions alone were not enough.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_breaker_three_rounds_hand_an_open_pr_source_to_the_poller() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_pr_url(&task.id, "https://github.com/o/r/pull/301")
        .await
        .unwrap();

    let mut terminal_child = String::new();
    for round in 1..=MAX_AUTONOMOUS_ESCALATIONS {
        // The arbiter ceiling parks out of the Lead-intervention hold, so that
        // is the status the producer snapshots. Staging it is the only fixture
        // step; the escalation itself is the production call.
        repo.set_status(&task.id, "in_lead_intervention")
            .await
            .unwrap();
        let observed = repo.get(&task.id).await.unwrap().unwrap();

        assert!(
            actor
                .escalate_to_planner_or_terminally_fail(
                    &observed,
                    &format!("loop-breaker round {round}: still not converging")
                )
                .await,
            "round {round} is below the ceiling and must park behind a fresh escalation"
        );
        let child = held_by_one_adjudication_child(&repo, &task.id).await;

        close_adjudication_child(&repo, &child, "planner reviewed; nothing to change").await;
        if round < MAX_AUTONOMOUS_ESCALATIONS {
            assert_intermediate_round_released_the_source(
                &repo,
                &task.id,
                &child,
                round,
                "loop-breaker",
            )
            .await;
        }
        terminal_child = child;
    }

    assert_terminal_round_handed_the_pr_to_the_poller(
        &repo,
        &task.id,
        &terminal_child,
        "loop-breaker",
    )
    .await;
    assert_eq!(
        repo.list_blockers(&task.id).await.unwrap().len(),
        MAX_AUTONOMOUS_ESCALATIONS as usize,
        "the ladder ends at round {MAX_AUTONOMOUS_ESCALATIONS}: a fourth escalation is forbidden"
    );
}

/// **AC7/AC8, producer 1: the loop-breaker / arbiter ceiling, NO PR.**
///
/// The other branch of the same producer. A source with no PR has no poller
/// that could own it, so the spent ladder ends it terminally — and the
/// contractual reason has to be matchable, because the operator query for
/// stranded exhausted work keys on it.
///
/// **With the mechanism's body made inert**: the source stays `open` forever
/// with three closed escalations against it and no disposition at all, and no
/// outcome event is ever written. What would STILL pass: the producer's own
/// contract — three children created and closed, the ceiling never exceeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_breaker_three_rounds_terminally_close_a_no_pr_source() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // No `set_pr_url`: this source never opened one.
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    assert!(
        task.pr_url.is_none(),
        "fixture: the no-PR branch requires a source with no PR"
    );

    let mut terminal_child = String::new();
    for round in 1..=MAX_AUTONOMOUS_ESCALATIONS {
        // Reached from the dispatch rung, where the source is `open`: the park
        // is a no-op and the snapshot status is `open` too. That makes this the
        // one producer shape where the scaffolding EXCLUSION never fires and
        // plain status equality carries the unchanged reading.
        let observed = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(observed.status, "open", "round {round}");

        assert!(
            actor
                .escalate_to_planner_or_terminally_fail(
                    &observed,
                    &format!("loop-breaker round {round}: dispatch dead end")
                )
                .await,
            "round {round} is below the ceiling and must park behind a fresh escalation"
        );
        let child = held_by_one_adjudication_child(&repo, &task.id).await;

        close_adjudication_child(&repo, &child, "planner reviewed; nothing to change").await;
        if round < MAX_AUTONOMOUS_ESCALATIONS {
            assert_intermediate_round_released_the_source(
                &repo,
                &task.id,
                &child,
                round,
                "loop-breaker (no PR)",
            )
            .await;
        }
        terminal_child = child;
    }

    assert_terminal_round_closed_a_no_pr_source(
        &repo,
        &task.id,
        &terminal_child,
        "loop-breaker (no PR)",
    )
    .await;
}

/// **AC7/AC8, producer 2: the merge-method wedge.**
///
/// `poll_pr_review_tasks` escalates STRAIGHT OFF a `pr_review` row when the
/// repository has rejected every merge method Djinn can attempt. This is the
/// `z8i8`/`zkas` family itself and the producer that hid defect instances 2 and
/// 3: its snapshot status is `pr_review`, so
///
/// - a scaffolding exclusion narrowed to the Lead-intervention pair made its
///   `pr_review -> open` park read `source_changed`, and a changed round-3 close
///   never invokes exhausted ownership; and
/// - reporting the CREATION snapshot as `source_status_before` made the terminal
///   handoff read `pr_review -> pr_review` — no movement — beside a
///   `source_changed` outcome.
///
/// Both are asserted below: rounds 1/2 must read `source_unchanged`, and round
/// 3 must report `open -> pr_review`.
///
/// **Scope note (no mock).** The GitHub half of this producer — the merge
/// attempt, `is_merge_method_not_allowed`, and the failure counter that reaches
/// `MERGE_METHOD_NOT_ALLOWED_ESCALATION_THRESHOLD` — needs a live
/// `GitHubApiClient` resolved from a project installation, and cannot be driven
/// from a test without a fake that would bypass the very thing under test. What
/// IS driven here is everything downstream of that decision and everything the
/// child-close contract can see: the real escalation entry point the wedge
/// calls, invoked with a source loaded from a real `pr_review` row, which is the
/// only property of this producer the mechanism reads.
///
/// **With the mechanism's body made inert**: the round-1 `outcome_for_child`
/// lookup fails, and the round-3 source sits `open` with an unmergeable PR
/// nobody polls. What would STILL pass: the escalation ladder, the blockers, the
/// parks — the producer is entirely healthy in both worlds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_method_wedge_three_rounds_hand_the_pr_row_back_to_the_poller() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_pr_url(&task.id, "https://github.com/o/r/pull/302")
        .await
        .unwrap();

    let mut terminal_child = String::new();
    for round in 1..=MAX_AUTONOMOUS_ESCALATIONS {
        // `poll_pr_review_tasks` iterates `list_by_status("pr_review")`, so the
        // task it hands the escalation is always a `pr_review` row.
        repo.set_status(&task.id, "pr_review").await.unwrap();
        let observed = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            observed.status, "pr_review",
            "round {round}: the merge-method wedge escalates off a pr_review row"
        );

        assert!(
            actor
                .escalate_to_planner_or_terminally_fail(
                    &observed,
                    &format!(
                        "PR #302 cannot be merged: the repository rejected every merge method \
                         Djinn attempted (squash / merge-commit / rebase) as \"not allowed\" \
                         (round {round})."
                    )
                )
                .await,
            "round {round} is below the ceiling and must park behind a fresh escalation"
        );
        let child = held_by_one_adjudication_child(&repo, &task.id).await;

        close_adjudication_child(&repo, &child, "planner cannot fix repo merge settings").await;
        if round < MAX_AUTONOMOUS_ESCALATIONS {
            assert_intermediate_round_released_the_source(
                &repo,
                &task.id,
                &child,
                round,
                "merge-method wedge",
            )
            .await;
        }
        terminal_child = child;
    }

    assert_terminal_round_handed_the_pr_to_the_poller(
        &repo,
        &task.id,
        &terminal_child,
        "merge-method wedge",
    )
    .await;
}

/// **AC7/AC8, producer 3: the CI-loop park.**
///
/// `escalate_ci_failure_and_park` is the PR poller's non-converging-CI rung. It
/// terminalizes the in-flight worker attempt as `PrCiFailed`/`Reopened` FIRST
/// and then routes into the same escalation ladder, so the source it snapshots
/// is the red `pr_draft` row the poller was looking at. That attempt
/// terminalization is a real production write this test drives, and it is the
/// reason this producer needs its own case rather than sharing the loop-breaker
/// one: it reaches the ladder with different durable state around the source.
///
/// **With the mechanism's body made inert**: the round-1/2 outcome lookups fail
/// and the round-3 source keeps its red PR with no owner. What would STILL pass:
/// the CI escalation comment, the attempt terminalization, the blocker and the
/// park — every assertion the existing `escalate_ci_failure_and_park` tests in
/// `tests::intervention` make.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_loop_park_three_rounds_hand_the_red_pr_to_the_poller() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    const PR_URL: &str = "https://github.com/o/r/pull/303";
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_pr_url(&task.id, PR_URL).await.unwrap();

    let mut terminal_child = String::new();
    for round in 1..=MAX_AUTONOMOUS_ESCALATIONS {
        // The CI-loop park is reached with the source on its draft PR.
        repo.set_status(&task.id, "pr_draft").await.unwrap();
        let observed = repo.get(&task.id).await.unwrap().unwrap();

        actor
            .escalate_ci_failure_and_park(
                &observed,
                PR_URL,
                &format!(
                    "Required CI keeps failing on the same fingerprint across round {round}; the \
                     worker is not converging."
                ),
                &["**Failed job:** server-clippy (failure)".to_owned()],
            )
            .await;
        let child = held_by_one_adjudication_child(&repo, &task.id).await;

        close_adjudication_child(&repo, &child, "planner reviewed the CI loop; no rescope").await;
        if round < MAX_AUTONOMOUS_ESCALATIONS {
            assert_intermediate_round_released_the_source(
                &repo,
                &task.id,
                &child,
                round,
                "CI-loop park",
            )
            .await;
        }
        terminal_child = child;
    }

    assert_terminal_round_handed_the_pr_to_the_poller(
        &repo,
        &task.id,
        &terminal_child,
        "CI-loop park",
    )
    .await;
}

/// Build a `Held` tripwire gate result from a migration change (an
/// enforcement-on rule) for the given task/project/head.
///
/// A local copy of the fixture in `tests::tripwire_planner_escalation` — that
/// module's helper is private to it, and duplicating six lines is cheaper than
/// widening its visibility.
fn held_migration_gate(
    task_id: &str,
    project_id: &str,
    head_sha: &str,
) -> crate::pr_poller::tripwire_gate::TripwireGateResult {
    let pr_files = vec![djinn_provider::github_api::PrFile {
        sha: "deadbeef".to_owned(),
        filename: "migrations/20260805_add_column.sql".to_owned(),
        status: "added".to_owned(),
        additions: 12,
        deletions: 0,
        changes: 12,
        patch: None,
    }];
    let result =
        crate::pr_poller::tripwire_gate::run_gate(&crate::tripwires::TripwireEvaluationInput {
            task_id: task_id.to_owned(),
            project_id: project_id.to_owned(),
            pr_number: Some(4343),
            head_sha: head_sha.to_owned(),
            policy: crate::tripwires::TripwirePolicy::default(),
            allowlist_revision: None,
            changed_files: crate::pr_poller::tripwire_gate::convert_pr_files(&pr_files),
        });
    assert_eq!(
        result.decision.outcome,
        crate::tripwires::GateOutcome::Held,
        "fixture must produce a Held gate"
    );
    result
}

/// **AC7/AC8, producer 4: the tripwire human-adjudication escape hatch.**
///
/// `create_tripwire_hold_with_policy`, given a policy whose rule adjudication is
/// `Human`, takes the legacy path: a `human-review-hold` child plus a park out
/// of `pr_draft`. That child carries a DIFFERENT label from every other producer
/// here, and `is_adjudication_child` / `planner_escalation_count_tx` are the only
/// things that make it participate in the terminal-rung round count at all — so
/// this is the one producer whose rounds would silently never reach the ceiling
/// if the label discrimination regressed.
///
/// It also snapshots from `pr_draft`, the second PR-status producer that the
/// Lead-intervention-only exclusion left stranded.
///
/// **With the mechanism's body made inert**: a source whose tripwire hold a
/// human declined three times keeps its held draft PR and sits `open`. What
/// would STILL pass: `human_adjudicated_tripwire_hold_keeps_legacy_human_review_path`
/// and `..._releases_the_hold_on_close` — both assert the hold child and the
/// release, neither of which this mechanism writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_adjudicated_tripwire_hold_three_rounds_reaches_the_exhausted_ladder() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_pr_url(&task.id, "https://github.com/o/r/pull/304")
        .await
        .unwrap();

    // The operator opted the migration rule into human adjudication.
    let mut policy = crate::tripwires::TripwirePolicy::default();
    policy.migration.adjudication = crate::tripwires::Adjudication::Human;

    let mut terminal_child = String::new();
    for round in 1..=MAX_AUTONOMOUS_ESCALATIONS {
        // Each round is a fresh push: a new held head on the draft PR.
        let head = format!("head-tripwire-round-{round}-000000000000");
        repo.set_status(&task.id, "pr_draft").await.unwrap();
        let observed = repo.get(&task.id).await.unwrap().unwrap();
        let gate = held_migration_gate(&observed.id, &observed.project_id, &head);

        actor
            .create_tripwire_hold_with_policy(&observed, &gate, &head, policy.clone())
            .await;
        let child = held_by_one_adjudication_child(&repo, &task.id).await;
        let child_task = repo.get(&child).await.unwrap().unwrap();
        assert!(
            child_task.labels.contains("human-review-hold"),
            "round {round}: the human escape hatch must take the legacy path; labels={}",
            child_task.labels
        );

        close_adjudication_child(&repo, &child, "human reviewed the migration; no change").await;
        if round < MAX_AUTONOMOUS_ESCALATIONS {
            assert_intermediate_round_released_the_source(
                &repo,
                &task.id,
                &child,
                round,
                "tripwire human hatch",
            )
            .await;
        }
        terminal_child = child;
    }

    assert_terminal_round_handed_the_pr_to_the_poller(
        &repo,
        &task.id,
        &terminal_child,
        "tripwire human hatch",
    )
    .await;
}

/// Both copies of the exhausted-ladder PR handoff must write the SAME durable
/// marker, or an operator query finds only one of them — and the one
/// production takes for a planner-closed child is the DB copy, i.e. the one
/// most such queries actually want.
///
/// `djinn-db` cannot depend on `djinn-coordinator`, so its constants are
/// duplicated literals. This is the only place that can see both, so this is
/// where the parity is asserted. It would still pass if the marker were never
/// written; that the DB copy writes it is proven by the producer-level tests
/// above, which read the payload back.
#[test]
fn both_pr_handoff_marker_writers_agree_on_actor_role_and_reason() {
    assert_eq!(
        djinn_db::repositories::task::PR_HANDOFF_ACTOR_ID,
        crate::dispatch::respawn_guard::HANDOFF_ACTOR_ID,
    );
    assert_eq!(
        djinn_db::repositories::task::PR_HANDOFF_ACTOR_ROLE,
        crate::dispatch::respawn_guard::HANDOFF_ACTOR_ROLE,
    );
    assert_eq!(
        djinn_db::repositories::task::PR_HANDOFF_REASON,
        "adjudication_ladder_exhausted_pr_handoff",
        "the reason string is what `escalation_ceiling` and operator queries match on"
    );
}

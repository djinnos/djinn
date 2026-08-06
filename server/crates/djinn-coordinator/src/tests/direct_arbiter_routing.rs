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

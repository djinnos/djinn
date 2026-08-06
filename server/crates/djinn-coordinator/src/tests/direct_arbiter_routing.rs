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
        dossier["failing_ci_job_ids"],
        serde_json::json!([]),
        "job-id parsing is unchanged by 4etb — it reads the sections it was given"
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

    // Cycles 0, 1, 2: each trigger opens the next ordinary row; consuming it
    // (an arbiter decision) is what advances the prospective cycle.
    for cycle in 0..3 {
        let refreshed = repo.get(&task.id).await.unwrap().unwrap();
        actor
            .route_arbiter_adjudication(&refreshed, "worker", "ordinary cycle", None, 5)
            .await;
        assert!(
            arb.get_by_task_and_cycle(&task.id, cycle)
                .await
                .unwrap()
                .is_some(),
            "cycle {cycle} must open its own arbitration row"
        );
        arb.mark_consumed(&task.id, cycle).await.unwrap();
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

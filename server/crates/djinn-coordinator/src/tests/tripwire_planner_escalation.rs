//! DB-backed tests for the zero-human-hold tripwire path: an arbiter-class
//! tripwire `Held` gate routes the source to the forensic arbiter (never a
//! `human-review-hold`), leaves NO hold label on the source, hands the
//! adjudication dossier over verbatim, and keeps the merge blocked via the
//! active-hold state. The org-policy human-adjudication escape hatch still
//! takes the legacy path, and still releases the hold — emitting
//! `tripwire.hold.released` for every held head — when its hold task closes.
//!
//! 4etb: the arbiter-adjudicated branch creates no hold CHILD, so
//! `emit_tripwire_release_on_hold_close` — which fires only when such a child
//! closes — can never run for it. An arbiter that decides `approve` /
//! `approve_conflict` / `supersede` on the SAME head would therefore leave the
//! active-hold state set forever, with nothing left that could clear it. The
//! second producer, `emit_tripwire_release_on_arbiter_clearance`, closes that
//! gap; it is gated on the task's OWN arbitration ledger recording one of those
//! clearing decisions, so an ordinary approval that never went through
//! adjudication cannot release a hold. Both halves — the release that must
//! happen and the release that must NOT — are covered at the bottom of this
//! file.

use super::*;
use crate::tripwires::{ActivityEntryRef, TripwireEvaluationInput, TripwirePolicy};
use djinn_core::models::TransitionAction;
use djinn_provider::github_api::PrFile;

const HELD_HEAD: &str = "head-tripwire-escalation-0001";
const PR_NUMBER: u64 = 4242;

/// Build a `Held` tripwire gate result from a migration change (an
/// enforcement-on rule) for the given task/project/head.
fn held_migration_result(
    task_id: &str,
    project_id: &str,
    head_sha: &str,
) -> crate::pr_poller::tripwire_gate::TripwireGateResult {
    let pr_files = vec![PrFile {
        sha: "deadbeef".to_owned(),
        filename: "migrations/20260709_add_column.sql".to_owned(),
        status: "added".to_owned(),
        additions: 12,
        deletions: 0,
        changes: 12,
        patch: None,
    }];
    let input = TripwireEvaluationInput {
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number: Some(PR_NUMBER),
        head_sha: head_sha.to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files: crate::pr_poller::tripwire_gate::convert_pr_files(&pr_files),
    };
    let result = crate::pr_poller::tripwire_gate::run_gate(&input);
    assert_eq!(
        result.decision.outcome,
        crate::tripwires::GateOutcome::Held,
        "fixture must produce a Held gate"
    );
    result
}

/// Log the `tripwire.gate.held` activity event on the source, exactly as the
/// PR poller does before creating the hold — this establishes the active-hold
/// state that gates the merge independently of any label.
async fn log_gate_held(
    repo: &TaskRepository,
    task_id: &str,
    result: &crate::pr_poller::tripwire_gate::TripwireGateResult,
) {
    let payload_json = serde_json::to_string(&result.payload).unwrap();
    repo.log_activity(
        Some(task_id),
        "coordinator",
        "system",
        result.event_type,
        &payload_json,
    )
    .await
    .unwrap();
}

async fn active_hold_held(repo: &TaskRepository, task_id: &str, head_sha: &str) -> bool {
    let entries = repo.list_activity(task_id).await.unwrap();
    let refs: Vec<ActivityEntryRef> = entries.iter().map(ActivityEntryRef::from_entry).collect();
    crate::tripwires::compute_active_hold_state(&refs, head_sha).held
}

/// Arbiter-class (default policy) tripwire `Held` → the source is routed to the
/// forensic arbiter with the adjudication dossier VERBATIM; NO human-review-hold
/// task, NO hold label on the source, and the merge stays blocked by the
/// active-hold state throughout.
///
/// 4etb: this used to mint a terminal `PlannerEscalation` review task directly
/// and park the source `open` behind it — a tripwire hold reached the TERMINAL
/// rung on its first offence with no arbiter ever reading the dossier. The
/// route now goes through `route_arbiter_adjudication` like every other in-scope
/// trigger, so the durable outcome is `needs_lead_intervention` plus an
/// unconsumed hold-cycle-0 arbitration row, and there is no hold CHILD at all.
/// The invariants that survive verbatim — no human hold, no `human-review-hold`
/// label on the source, and the tripwire dossier reaching the adjudicator
/// unmodified — are what this test asserts. (The close → `tripwire.hold.released`
/// half moved to `human_adjudicated_tripwire_hold_releases_the_hold_on_close`,
/// which still owns a hold task to close.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_tripwire_hold_creates_planner_escalation_no_human_hold() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    let result = held_migration_result(&task.id, &task.project_id, HELD_HEAD);
    log_gate_held(&repo, &task.id, &result).await;

    // Merge is blocked by the active-hold state before any label exists.
    assert!(
        active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "gate.held event must make the active-hold state block the merge"
    );

    actor.create_tripwire_hold(&task, &result, HELD_HEAD).await;

    // The source is handed to the arbiter and NOT labelled human-review-hold.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "needs_lead_intervention",
        "an arbiter-class tripwire hold routes the source to the forensic arbiter"
    );
    assert!(
        !parked.labels.contains("human-review-hold"),
        "arbiter tripwire hold must NOT stamp human-review-hold on the source; labels={}",
        parked.labels
    );

    // No hold CHILD of any kind — least of all a human one.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert!(
        blockers.is_empty(),
        "the arbiter route must not mint a remediation child; got {} blocker(s)",
        blockers.len()
    );

    // The adjudicator receives the tripwire dossier VERBATIM: finding rule,
    // evidence path, and the close-releases / reopen-supersedes instructions.
    let arbitration =
        djinn_db::repositories::task_arbitration::TaskArbitrationRepository::new(db.clone())
            .get_by_task_and_cycle(&task.id, 0)
            .await
            .unwrap()
            .expect("the tripwire hold must open hold cycle 0");
    assert!(
        arbitration.consumed_at.is_none(),
        "the arbiter must still be in flight for this hold"
    );
    let dossier = arbitration
        .dossier
        .expect("the arbiter must be handed a dossier");
    let trigger_reason = dossier["trigger_reason"]
        .as_str()
        .expect("the dossier must carry the trigger's own reason");
    for needle in [
        "Tripwire adjudication",
        "migration_change",
        "migrations/20260709_add_column.sql",
        "CLOSE this task",
        "Reopen the source",
    ] {
        assert!(
            trigger_reason.contains(needle),
            "the arbiter's dossier must carry the tripwire finding {needle:?} verbatim; got: \
             {trigger_reason}"
        );
    }

    // Merge still blocked (hold active) while the adjudication is in flight.
    assert!(
        active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "hold must stay active until the adjudication resolves"
    );
}

/// Org-policy human-adjudication escape hatch: a rule an operator opted to
/// `Human` still takes the legacy human-review remediation path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_adjudicated_tripwire_hold_keeps_legacy_human_review_path() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    let result = held_migration_result(&task.id, &task.project_id, HELD_HEAD);
    log_gate_held(&repo, &task.id, &result).await;

    // Operator opts the migration rule into human adjudication.
    let mut policy = TripwirePolicy::default();
    policy.migration.adjudication = crate::tripwires::Adjudication::Human;

    actor
        .create_tripwire_hold_with_policy(&task, &result, HELD_HEAD, policy)
        .await;

    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(blockers.len(), 1, "source must be held by one blocker");
    let hold = repo.get(&blockers[0].task_id).await.unwrap().unwrap();
    assert!(
        hold.labels.contains("human-review-hold"),
        "human-adjudicated rule must take the legacy human-review path; labels={}",
        hold.labels
    );
    assert!(
        !hold.labels.contains("planner-park-escalation"),
        "human escape hatch must NOT create a planner-park escalation; labels={}",
        hold.labels
    );
}

/// The hold-release contract: while the hold task is open the merge stays
/// blocked, and closing it emits `tripwire.hold.released` for the held head so
/// the merge may proceed.
///
/// 4etb: this coverage used to live on the arbiter path, whose hold child is
/// gone. The mechanism itself (`releases_source_on_close` →
/// `emit_tripwire_release_on_hold_close`) is unchanged and still reachable
/// through the human escape hatch, so the assertions move here rather than
/// being dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_adjudicated_tripwire_hold_releases_the_hold_on_close() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    let result = held_migration_result(&task.id, &task.project_id, HELD_HEAD);
    log_gate_held(&repo, &task.id, &result).await;
    assert!(
        active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "gate.held event must make the active-hold state block the merge"
    );

    let mut policy = TripwirePolicy::default();
    policy.migration.adjudication = crate::tripwires::Adjudication::Human;
    actor
        .create_tripwire_hold_with_policy(&task, &result, HELD_HEAD, policy)
        .await;

    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(blockers.len(), 1, "source must be held by one blocker");
    let hold = repo.get(&blockers[0].task_id).await.unwrap().unwrap();

    // Merge still blocked while the hold is open.
    assert!(
        active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "hold must stay active until it is resolved"
    );

    repo.transition(
        &hold.id,
        TransitionAction::Close,
        "planner",
        "system",
        Some("Cleared migration_change: additive column, no backfill risk."),
        None,
    )
    .await
    .unwrap();
    let closed_hold = repo.get(&hold.id).await.unwrap().unwrap();
    actor
        .emit_tripwire_release_on_hold_close(&closed_hold)
        .await;

    let entries = repo.list_activity(&task.id).await.unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.event_type == crate::tripwires::TRIPWIRE_EVENT_HOLD_RELEASED),
        "closing the hold must emit tripwire.hold.released on the source"
    );
    assert!(
        !active_hold_held(&repo, &task.id, HELD_HEAD).await,
        "the hold must be released after it closes — merge may proceed"
    );
}

// ── 4etb: the arbiter-clearance release producer ────────────────────────────

/// Every `tripwire.hold.released` activity payload on `task_id`.
async fn hold_released_payloads(repo: &TaskRepository, task_id: &str) -> Vec<serde_json::Value> {
    repo.list_activity(task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == crate::tripwires::TRIPWIRE_EVENT_HOLD_RELEASED)
        .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap())
        .collect()
}

/// Record an arbiter decision on the task's own arbitration ledger — the ONLY
/// input `emit_tripwire_release_on_arbiter_clearance` consults.
async fn record_arbiter_decision(db: &Database, task_id: &str, decision: &str) {
    let applied =
        djinn_db::repositories::task_arbitration::TaskArbitrationRepository::new(db.clone())
            .update_dispatch_ledger(
                djinn_db::repositories::task_arbitration::UpdateDispatchLedgerParams {
                    task_id,
                    hold_cycle: 0,
                    mirror_head_sha: None,
                    github_head_sha: None,
                    pr_url: None,
                    failing_ci_job_ids: None,
                    dossier: None,
                    directive: Some(&serde_json::json!({ "decision": decision })),
                    verification_command: None,
                    excluded_models: None,
                },
            )
            .await
            .unwrap();
    assert!(
        applied,
        "fixture: the tripwire route must have opened hold cycle 0 for the decision to land on"
    );
}

/// Drive a tripwire-held source all the way to an arbiter-adjudicated state:
/// `gate.held` logged, the arbiter route taken, hold cycle 0 open. Returns the
/// source as it stands while the adjudication is in flight.
async fn arbiter_adjudicated_tripwire_source(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    actor: &mut CoordinatorActor,
    repo: &TaskRepository,
) -> djinn_core::models::Task {
    let task = make_task_with_reopen_count(db, tx, 0).await;
    repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    let result = held_migration_result(&task.id, &task.project_id, HELD_HEAD);
    log_gate_held(repo, &task.id, &result).await;
    actor.create_tripwire_hold(&task, &result, HELD_HEAD).await;

    assert!(
        active_hold_held(repo, &task.id, HELD_HEAD).await,
        "fixture: the merge must be blocked before the arbiter decides"
    );
    repo.get(&task.id).await.unwrap().unwrap()
}

/// 4etb: an arbiter `approve` on the SAME head must release the tripwire hold.
///
/// The arbiter route creates no hold CHILD, so the child-close producer can
/// never fire for it. Without this second producer the `gate.held` event stays
/// unreleased for the current head and the merge is blocked forever — the hold
/// only ever clears when a new commit moves the head, which an approved task
/// will never produce.
///
/// The durable evidence is the `tripwire.hold.released` activity row and the
/// recomputed active-hold state over the source's own activity entries: no log
/// line participates, and the released payload must pin the HELD head, not the
/// task row's head.
///
/// **If `emit_tripwire_release_on_arbiter_clearance`'s body were deleted**, the
/// hold-created and dossier assertions in the tests above would all still pass;
/// only these two assertions fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_arbiter_approval_releases_the_tripwire_hold_it_cleared() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let source = arbiter_adjudicated_tripwire_source(&db, &tx, &mut actor, &repo).await;
    record_arbiter_decision(&db, &source.id, "approve").await;

    actor
        .emit_tripwire_release_on_arbiter_clearance(&source)
        .await;

    let released = hold_released_payloads(&repo, &source.id).await;
    assert_eq!(
        released.len(),
        1,
        "the arbiter's clearing decision must emit exactly one tripwire.hold.released"
    );
    assert_eq!(
        released[0]["head_sha"], HELD_HEAD,
        "the release must cover the head the gate actually holds, not a later row head"
    );
    assert!(
        !released[0]["released_findings"]
            .as_array()
            .expect("released_findings must be an array")
            .is_empty(),
        "the release must name the findings it clears; an empty release clears nothing"
    );
    assert!(
        released[0]["rationale"]
            .as_str()
            .unwrap_or_default()
            .contains("approve"),
        "the rationale must record WHICH arbiter decision cleared the hold; got {}",
        released[0]["rationale"]
    );
    assert!(
        !active_hold_held(&repo, &source.id, HELD_HEAD).await,
        "the merge-boundary gate must clear for the held head once the arbiter cleared it"
    );
}

/// 4etb, the load-bearing negative: a source with NO clearing arbiter decision
/// must NOT get a release.
///
/// A producer that fired unconditionally would defeat the tripwire gate
/// entirely — every held source would be released the moment the coordinator
/// looked at it, which is strictly worse than the stuck-hold bug this producer
/// exists to fix. Two shapes are covered: an adjudication still in flight (row
/// open, no directive at all) and a NON-clearing decision (`reopen`, which is
/// safe precisely because the worker's next push moves the head and the gate
/// re-evaluates).
///
/// **If the decision filter were deleted** — the `matches!(approve |
/// approve_conflict | supersede)` guard, or the `Some(decision) else return` —
/// both halves of this test fail while the positive test above still passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_clearing_arbiter_decision_means_no_tripwire_release() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // (a) adjudication in flight: the ledger records no decision yet.
    let in_flight = arbiter_adjudicated_tripwire_source(&db, &tx, &mut actor, &repo).await;
    actor
        .emit_tripwire_release_on_arbiter_clearance(&in_flight)
        .await;
    assert!(
        hold_released_payloads(&repo, &in_flight.id)
            .await
            .is_empty(),
        "an undecided adjudication must not release the hold it is still deciding"
    );
    assert!(
        active_hold_held(&repo, &in_flight.id, HELD_HEAD).await,
        "the merge must stay blocked while the arbiter is still in flight"
    );

    // (b) a NON-clearing decision: `reopen` sends the source back to a worker,
    // whose next push moves the head. Releasing here would clear a hold nobody
    // adjudicated away.
    let reopened = arbiter_adjudicated_tripwire_source(&db, &tx, &mut actor, &repo).await;
    record_arbiter_decision(&db, &reopened.id, "reopen").await;
    actor
        .emit_tripwire_release_on_arbiter_clearance(&reopened)
        .await;
    assert!(
        hold_released_payloads(&repo, &reopened.id).await.is_empty(),
        "a `reopen` is not a clearance: it must not emit a release"
    );
    assert!(
        active_hold_held(&repo, &reopened.id, HELD_HEAD).await,
        "the merge must stay blocked after a reopen decision"
    );
}

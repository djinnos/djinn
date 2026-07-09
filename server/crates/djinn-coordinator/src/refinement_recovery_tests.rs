// Startup-recovery regression tests: a tribunal that converged and parked in
// `AwaitingHumanReview` must survive a coordinator restart instead of being
// wiped and stamped `Interrupted`.
//
// Split out of `refinement_cap_tests.rs` to keep that file under the
// size-guard line threshold; shares its fixture helpers.

use super::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use crate::refinement::StopReason;
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::ProposalRepository;

/// Record a `refinement_start` followed by a `refinement_awaiting_review`
/// lifecycle row, simulating a tribunal that converged and parked for human
/// review before the process died — the state startup recovery must restore
/// rather than wipe.
async fn seed_awaiting_review_park(
    db: &djinn_db::Database,
    proposal_id: &str,
    refined_seq: i32,
    snapshot_seq: i32,
    stop_reason: Option<&str>,
) {
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    repo.record_refinement_lifecycle(proposal_id, "refinement_start", None)
        .await
        .expect("record refinement_start");
    // Small delay so created_at strictly advances (lifecycle rows use now()).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let meta = serde_json::json!({
        "source": "refinement_loop",
        "event": "refinement_awaiting_review",
        "judge_summary": "Converged: spec is ready.",
        "snapshot_revision_seq": snapshot_seq,
        "refined_revision_seq": refined_seq,
        "stop_reason": stop_reason,
    });
    repo.record_refinement_lifecycle(proposal_id, "refinement_awaiting_review", Some(&meta))
        .await
        .expect("record refinement_awaiting_review");
}

/// Count `refinement_stop` lifecycle rows whose reason tag is `interrupted`.
async fn interrupted_stop_count(db: &djinn_db::Database, proposal_id: &str) -> usize {
    let revs = ProposalRepository::new(db.clone(), EventBus::noop())
        .revisions(proposal_id)
        .await
        .expect("read revisions");
    revs.iter()
        .filter(|r| r.event_kind == "refinement_stop")
        .filter(|r| {
            r.event_metadata
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| v.get("reason_tag").and_then(|t| t.as_str().map(String::from)))
                .as_deref()
                == Some("interrupted")
        })
        .count()
}

/// A dangling refinement that had legitimately converged and parked awaiting
/// human review (latest lifecycle event is `refinement_awaiting_review`, head
/// revision unchanged) is RESTORED to its parked state on startup — NOT stamped
/// interrupted. This is the incident fix: a server restart used to wipe the
/// judge's converged result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_restores_awaiting_review_park_and_does_not_stamp_interrupted() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let head = repo
        .get(&fixture.proposal_id)
        .await
        .unwrap()
        .unwrap()
        .latest_revision_seq;
    // Parked on the current head — nobody edited the spec since convergence.
    seed_awaiting_review_park(&db, &fixture.proposal_id, head, head, Some("round_cap")).await;

    actor.recover_interrupted_refinements().await;

    let restored = actor
        .active_refinements
        .get(&fixture.proposal_id)
        .expect("converged park must be restored into active_refinements");
    assert!(
        restored.is_awaiting_human_review(),
        "restored state must report awaiting human review"
    );
    assert!(!restored.is_complete());
    assert_eq!(restored.current_revision_seq, head);
    assert_eq!(restored.snapshot_revision_seq, head);
    assert_eq!(restored.stop_reason, Some(StopReason::RoundCap));

    assert_eq!(
        interrupted_stop_count(&db, &fixture.proposal_id).await,
        0,
        "a restored awaiting-review park must NOT get an interrupted refinement_stop"
    );
}

/// A dangling refinement genuinely interrupted mid-tribunal (a
/// `refinement_start` with no subsequent `refinement_awaiting_review`) still
/// gets the interrupted `refinement_stop` stamp and is not restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_stamps_interrupted_for_mid_tribunal_refinement() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // Started but never parked awaiting review — a real interruption.
    ProposalRepository::new(db.clone(), EventBus::noop())
        .record_refinement_lifecycle(&fixture.proposal_id, "refinement_start", None)
        .await
        .expect("record refinement_start");

    actor.recover_interrupted_refinements().await;

    assert!(
        !actor.active_refinements.contains_key(&fixture.proposal_id),
        "a mid-tribunal refinement must not be restored"
    );
    assert_eq!(
        interrupted_stop_count(&db, &fixture.proposal_id).await,
        1,
        "a mid-tribunal refinement must be stamped interrupted"
    );
}

/// A dangling refinement that parked awaiting review but whose spec has since
/// moved on (head revision no longer equals the parked refined revision) falls
/// back to the interrupted stamp — the converged result is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_stamps_interrupted_when_parked_spec_moved_on() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let original = repo.get(&fixture.proposal_id).await.unwrap().unwrap();
    let parked_seq = original.latest_revision_seq;

    // Park on the original head, then edit the spec so the head advances past
    // the parked revision — simulating a human/agent change after convergence.
    seed_awaiting_review_park(&db, &fixture.proposal_id, parked_seq, parked_seq, None).await;
    repo.update(
        &fixture.proposal_id,
        djinn_db::ProposalUpdateInput {
            title: &original.title,
            body: "Edited body after the tribunal parked — invalidates the park.",
            acceptance_criteria: &original.acceptance_criteria,
            status: &original.status,
            superseded_by: original.superseded_by.as_deref(),
            body_format: Some(&original.body_format),
            event_metadata: None,
        },
    )
    .await
    .expect("bump proposal head revision");
    let new_head = repo
        .get(&fixture.proposal_id)
        .await
        .unwrap()
        .unwrap()
        .latest_revision_seq;
    assert!(new_head > parked_seq, "head must advance past the parked seq");

    actor.recover_interrupted_refinements().await;

    assert!(
        !actor.active_refinements.contains_key(&fixture.proposal_id),
        "a stale park (spec moved on) must not be restored"
    );
    assert_eq!(
        interrupted_stop_count(&db, &fixture.proposal_id).await,
        1,
        "a stale park must fall back to the interrupted stamp"
    );
}

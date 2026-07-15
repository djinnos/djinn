// Startup-recovery regression tests: a tribunal that converged and parked in
// `AwaitingHumanReview` must survive a coordinator restart instead of being
// wiped and stamped `Interrupted`.
//
// Split out of `refinement_cap_tests.rs` to keep that file under the
// size-guard line threshold; shares its fixture helpers.

use super::refinement_cap_tests::{
    TEST_MODEL, build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use crate::refinement::{RefinementPhase, StopReason};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{
    ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository, UserRepository,
};

/// Record a `refinement_start` boundary, then sleep so subsequent debate/task
/// `created_at` timestamps strictly advance past it (current-run scoping uses a
/// strict `>` comparison).
async fn seed_refinement_start(db: &djinn_db::Database, proposal_id: &str, owner_user_id: &str) {
    ProposalRepository::new(db.clone(), EventBus::noop())
        .start_refinement_with_owner(proposal_id, owner_user_id, None)
        .await
        .expect("record refinement_start");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

/// Append an adversary objection debate-trail entry at `round`.
async fn add_objection(
    db: &djinn_db::Database,
    proposal_id: &str,
    round: i32,
    blocking: bool,
    against_revision_seq: i32,
) {
    ProposalRepository::new(db.clone(), EventBus::noop())
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id,
            kind: "objection",
            body: "Adversary objection seeded for restart-resume test",
            blocking,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq,
            round,
            body_metadata: None,
        })
        .await
        .expect("append objection");
}

/// Append a judge verdict debate-trail entry at `round`.
async fn add_verdict(
    db: &djinn_db::Database,
    proposal_id: &str,
    round: i32,
    blocking: bool,
    against_revision_seq: i32,
) {
    ProposalRepository::new(db.clone(), EventBus::noop())
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id,
            kind: "verdict",
            body: if blocking { "needs work" } else { "ready" },
            blocking,
            agent_role: "judge",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq,
            round,
            body_metadata: None,
        })
        .await
        .expect("append verdict");
}

/// Read the proposal's current head revision seq.
async fn head_seq(db: &djinn_db::Database, proposal_id: &str) -> i32 {
    ProposalRepository::new(db.clone(), EventBus::noop())
        .get(proposal_id)
        .await
        .unwrap()
        .unwrap()
        .latest_revision_seq
}

/// Record a `refinement_start` followed by a `refinement_awaiting_review`
/// lifecycle row, simulating a tribunal that converged and parked for human
/// review before the process died — the state startup recovery must restore
/// rather than wipe.
async fn seed_awaiting_review_park(
    db: &djinn_db::Database,
    proposal_id: &str,
    owner_user_id: &str,
    refined_seq: i32,
    snapshot_seq: i32,
    stop_reason: Option<&str>,
) {
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    repo.start_refinement_with_owner(proposal_id, owner_user_id, None)
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
                .and_then(|v| {
                    v.get("reason_tag")
                        .and_then(|t| t.as_str().map(String::from))
                })
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
    seed_awaiting_review_park(
        &db,
        &fixture.proposal_id,
        &fixture.user_id,
        head,
        head,
        Some("round_cap"),
    )
    .await;

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

/// A dangling refinement started but with no durable debate artifacts yet (the
/// opener never filed) RESUMES at the round-1 Adversary opener across the
/// restart instead of being stamped interrupted. This is the core behavior
/// change: a mid-flight run keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_resumes_bare_started_refinement_at_round_one_adversary() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    seed_refinement_start(&db, &fixture.proposal_id, &fixture.user_id).await;

    actor.recover_interrupted_refinements().await;

    let resumed = actor
        .active_refinements
        .get(&fixture.proposal_id)
        .expect("bare-started refinement must resume, not be stamped interrupted");
    assert_eq!(resumed.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(resumed.current_round, 1);
    assert_eq!(
        resumed.attributed_user_id.as_deref(),
        Some(fixture.user_id.as_str()),
        "durable owner resumes even when this run has no tribunal task rows"
    );
    assert_eq!(
        interrupted_stop_count(&db, &fixture.proposal_id).await,
        0,
        "a resumed refinement must NOT be stamped interrupted"
    );
}

/// (a) A tribunal interrupted mid-round — the round-1 Adversary filed blocking
/// objections but the Judge never ruled — resumes at the Advocate for round 1
/// (blocking objections route to the Advocate), NOT stamped interrupted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_resumes_mid_round_at_advocate_after_blocking_objections() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let seq = head_seq(&db, &fixture.proposal_id).await;
    seed_refinement_start(&db, &fixture.proposal_id, &fixture.user_id).await;
    add_objection(&db, &fixture.proposal_id, 1, true, seq).await;
    add_objection(&db, &fixture.proposal_id, 1, true, seq).await;

    actor.recover_interrupted_refinements().await;

    let resumed = actor
        .active_refinements
        .get(&fixture.proposal_id)
        .expect("mid-round tribunal must resume");
    assert_eq!(
        resumed.phase,
        RefinementPhase::AdvocateRevision,
        "blocking round-1 objections resume at the Advocate"
    );
    assert_eq!(resumed.current_round, 1);
    assert_eq!(interrupted_stop_count(&db, &fixture.proposal_id).await, 0);
}

/// (a′) A round advanced by a blocking Judge verdict resumes at the NEXT round's
/// Adversary — the reconstruction mirrors the state machine's round advance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_resumes_after_blocking_verdict_at_next_round_adversary() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let seq = head_seq(&db, &fixture.proposal_id).await;
    seed_refinement_start(&db, &fixture.proposal_id, &fixture.user_id).await;
    add_objection(&db, &fixture.proposal_id, 1, true, seq).await;
    // Judge ruled round 1 not-ready (blocking) → the loop had advanced to round 2.
    add_verdict(&db, &fixture.proposal_id, 1, true, seq).await;

    actor.recover_interrupted_refinements().await;

    let resumed = actor
        .active_refinements
        .get(&fixture.proposal_id)
        .expect("post-verdict tribunal must resume");
    assert_eq!(resumed.phase, RefinementPhase::AdversaryAttack);
    assert_eq!(
        resumed.current_round, 2,
        "a blocking verdict below the cap advances to the next round"
    );
}

/// (b) An orphaned OPEN refinement task (a role session that was running at kill
/// time) is closed by recovery, and the reconstructed phase is re-dispatched by
/// the driver on the next tick WITHOUT burning a round.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_closes_orphaned_open_task_and_redispatches_same_round() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let seq = head_seq(&db, &fixture.proposal_id).await;
    seed_refinement_start(&db, &fixture.proposal_id, &fixture.user_id).await;
    // Round-1 Adversary filed blocking objections → resume phase is Advocate.
    add_objection(&db, &fixture.proposal_id, 1, true, seq).await;

    // Orphaned in-flight Advocate task, left OPEN by the kill. Created through
    // the real task builder so it carries the durable `for proposal {id},`
    // description marker and the attribution the reconstruction reads back.
    let orphan_task_id = actor
        .create_refinement_task_with_context(
            &fixture.proposal_id,
            "advocate",
            1,
            seq,
            "Proposal currently meets all DoR checks.",
            None,
            Some(&fixture.user_id),
        )
        .await
        .expect("create orphaned open refinement task");

    actor.recover_interrupted_refinements().await;

    // The reconstructed state is present at round 1 / Advocate — round NOT burned.
    let resumed = actor
        .active_refinements
        .get(&fixture.proposal_id)
        .expect("must resume");
    assert_eq!(resumed.phase, RefinementPhase::AdvocateRevision);
    assert_eq!(resumed.current_round, 1);
    assert_eq!(
        resumed.total_spawns, 1,
        "the one pre-restart refinement task counts against the spawn budget"
    );

    // The orphaned task was force-closed by recovery.
    let orphan = TaskRepository::new(db.clone(), EventBus::noop())
        .get(&orphan_task_id)
        .await
        .expect("read orphan task")
        .expect("orphan task exists");
    assert_eq!(
        orphan.status, "closed",
        "orphaned open refinement task must be closed by recovery"
    );

    // Next tick: the driver re-dispatches the SAME phase at the SAME round.
    actor.drive_active_refinements().await;
    let session = actor
        .refinement_sessions
        .get(&fixture.proposal_id)
        .expect("reconstructed phase must be re-dispatched on the next tick");
    assert_eq!(session.phase, RefinementPhase::AdvocateRevision);
    assert_eq!(session.model_id, TEST_MODEL);
    let state = &actor.active_refinements[&fixture.proposal_id];
    assert_eq!(state.current_round, 1, "re-dispatch must not burn a round");
    assert_eq!(
        state.total_spawns, 2,
        "re-dispatch consumes one more spawn on top of the reconstructed count"
    );
}

/// (c) A ready (non-blocking) Judge verdict with NO durable awaiting-review park
/// is contradictory (a ready verdict must persist a park). Recovery cannot
/// fabricate the snapshot revision, so it falls back to the interrupted stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_stamps_interrupted_for_ready_verdict_without_park() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let seq = head_seq(&db, &fixture.proposal_id).await;
    seed_refinement_start(&db, &fixture.proposal_id, &fixture.user_id).await;
    add_objection(&db, &fixture.proposal_id, 1, false, seq).await;
    // Ready verdict, but no `refinement_awaiting_review` park was written.
    add_verdict(&db, &fixture.proposal_id, 1, false, seq).await;

    actor.recover_interrupted_refinements().await;

    assert!(
        !actor.active_refinements.contains_key(&fixture.proposal_id),
        "an ambiguous ready-verdict-without-park run must not be resumed"
    );
    assert_eq!(
        interrupted_stop_count(&db, &fixture.proposal_id).await,
        1,
        "ambiguous reconstruction falls back to the interrupted stamp"
    );
}

/// (e) The resumed run reconstructs the spawn budget from this run's refinement
/// task rows, so the spawn cap still binds across the restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_reconstructs_spawn_budget_from_run_tasks() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);
    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let seq = head_seq(&db, &fixture.proposal_id).await;
    seed_refinement_start(&db, &fixture.proposal_id, &fixture.user_id).await;
    add_objection(&db, &fixture.proposal_id, 1, true, seq).await;

    // Three pre-restart refinement tasks for this run — two already closed, one
    // still open (orphaned). All three count toward the spawn budget. One row
    // deliberately has another valid creator: recovery may inspect task rows
    // for spawn count, but must never derive refinement attribution from them.
    let misleading_owner = UserRepository::new(db.clone())
        .upsert_from_github(777_101, "misleading-tribunal-owner", None, None)
        .await
        .expect("create distinct misleading task owner");
    for agent_type in ["adversary", "advocate", "judge"] {
        let task_id = actor
            .create_refinement_task_with_context(
                &fixture.proposal_id,
                agent_type,
                1,
                seq,
                "Proposal currently meets all DoR checks.",
                None,
                if agent_type == "adversary" {
                    Some(&misleading_owner.id)
                } else {
                    Some(&fixture.user_id)
                },
            )
            .await
            .expect("create run refinement task");
        if agent_type != "judge" {
            actor
                .close_refinement_task(&task_id, "pre-restart phase complete")
                .await;
        }
    }

    actor.recover_interrupted_refinements().await;

    let resumed = actor
        .active_refinements
        .get(&fixture.proposal_id)
        .expect("must resume");
    assert_eq!(
        resumed.total_spawns, 3,
        "all three this-run refinement tasks count against the reconstructed spawn budget"
    );
    // Attribution is reconstructed from the durable proposal owner, not task rows.
    assert_eq!(
        resumed.attributed_user_id.as_deref(),
        Some(fixture.user_id.as_str()),
        "attribution is reconstructed from the persisted refinement owner"
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
    seed_awaiting_review_park(
        &db,
        &fixture.proposal_id,
        &fixture.user_id,
        parked_seq,
        parked_seq,
        None,
    )
    .await;
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
    assert!(
        new_head > parked_seq,
        "head must advance past the parked seq"
    );

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

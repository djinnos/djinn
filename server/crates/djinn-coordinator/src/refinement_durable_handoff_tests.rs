//! Durable phase-handoff regressions.
//!
//! The durable intent ledger is the only dispatch authority for a run that has
//! a run identity. These tests drive a real adversary turn to completion
//! through `drive_active_refinements` — with NO hand-injected in-memory
//! projection — and assert the side effect that matters: the successor Advocate
//! intent is durably written, an Advocate role task is materialized, and no
//! ledger-dispatched role can leave its run running with a NULL stop tag.

use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    models::Task,
    refinement_liveness::{
        RefinementIntentState, RefinementPhase as DurablePhase, RefinementRole, RefinementRunState,
    },
};
use djinn_db::{
    AdmitRefinementRunRequest, CreateSessionParams, LoadRefinementRunSnapshotRequest,
    ProposalDebateTrailCreateInput, ProposalRepository, RefinementAdmissionOutcome,
    RefinementAdmissionSource, SessionRepository, TaskRepository,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::refinement_cap_tests;

/// A pool whose runner holds its slot until the session is killed, so a
/// dispatched role task stays observably running for as long as the test needs.
/// The role's durable output is seeded directly by the test, exactly as a real
/// agent would have written it.
fn spawn_holding_pool(db: &djinn_db::Database) -> djinn_slot::SlotPoolHandle {
    let cancel = CancellationToken::new();
    djinn_slot::SlotPoolHandle::spawn_with_factory(
        crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
        cancel,
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig {
                model_id: refinement_cap_tests::TEST_MODEL.to_owned(),
                max_slots: 2,
                roles: ["advocate", "adversary", "judge", "worker"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner =
                Arc::new(move |_task_id, _, _, _, kill, _, _| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                });
            djinn_slot::SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    )
}

async fn wait_until_pool_holds(pool: &djinn_slot::SlotPoolHandle, task_id: &str) {
    for _ in 0..200 {
        if pool
            .has_session(task_id)
            .await
            .expect("query holding pool session")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("holding pool never reported {task_id} running");
}

async fn release_pool_session(pool: &djinn_slot::SlotPoolHandle, task_id: &str) {
    let _ = pool.kill_session(task_id).await;
    for _ in 0..200 {
        if !pool
            .has_session(task_id)
            .await
            .expect("query holding pool session")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("holding pool did not free {task_id}");
}

struct HandoffFixture {
    db: djinn_db::Database,
    actor: crate::actor::CoordinatorActor,
    proposal_id: String,
    project_id: String,
    run_id: String,
    generation: i32,
}

async fn handoff_fixture() -> HandoffFixture {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_holding_pool(&db);
    let actor = refinement_cap_tests::build_refinement_actor(&db, &events_tx, pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) = match repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: uuid::Uuid::now_v7().to_string(),
            source: RefinementAdmissionSource::Demand {
                demand_id: uuid::Uuid::now_v7().to_string(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit durable handoff run")
    {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    };
    HandoffFixture {
        db,
        actor,
        proposal_id: fixture.proposal_id,
        project_id: fixture.project_id,
        run_id,
        generation,
    }
}

async fn snapshot(f: &HandoffFixture) -> djinn_db::RefinementRunSnapshotResult {
    ProposalRepository::new(f.db.clone(), EventBus::noop())
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: f.run_id.clone(),
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("load exact run snapshot")
        .expect("run exists")
}

/// The role task this run has materialized for `role`, if any.
async fn role_task(f: &HandoffFixture, role: &str) -> Option<Task> {
    TaskRepository::new(f.db.clone(), EventBus::noop())
        .list_by_project(&f.project_id)
        .await
        .expect("list project tasks")
        .into_iter()
        .find(|task| {
            task.issue_type == "refinement"
                && task.refinement_role.as_deref() == Some(role)
                && task.refinement_run_id.as_deref() == Some(f.run_id.as_str())
        })
}

/// Write the durable artifacts a real role agent leaves behind: a session row
/// for its task and, for the adversary, a blocking objection on this round.
async fn record_agent_turn(
    f: &HandoffFixture,
    task_id: &str,
    agent_type: &str,
    objection: Option<(&str, i32)>,
) {
    SessionRepository::new(f.db.clone(), EventBus::noop())
        .create(CreateSessionParams {
            project_id: &f.project_id,
            task_id: Some(task_id),
            model: refinement_cap_tests::TEST_MODEL,
            agent_type,
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("record role agent session");
    if let Some((body, round)) = objection {
        ProposalRepository::new(f.db.clone(), EventBus::noop())
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &f.proposal_id,
                kind: "objection",
                body,
                blocking: true,
                agent_role: agent_type,
                author_kind: "agent",
                author_model: Some(refinement_cap_tests::TEST_MODEL),
                source_task_id: Some(task_id),
                against_revision_seq: 0,
                round,
                body_metadata: None,
            })
            .await
            .expect("record adversary objection");
    }
}

/// Drive the opening Adversary intent to a real, finished agent turn: the
/// ledger materializes and enqueues the role task, the agent runs, files a
/// blocking objection, and exits cleanly with its slot freed.
async fn run_adversary_turn(f: &mut HandoffFixture) -> Task {
    f.actor.drive_active_refinements().await;
    let adversary = role_task(f, "adversary")
        .await
        .expect("the ledger must materialize the opening adversary task");
    wait_until_pool_holds(&f.actor.pool, &adversary.id).await;
    record_agent_turn(
        f,
        &adversary.id,
        "adversary",
        Some(("missing rollback plan", 1)),
    )
    .await;
    release_pool_session(&f.actor.pool, &adversary.id).await;
    adversary
}

/// The load-bearing regression.
///
/// Before this fix the coordinator never registered an in-flight projection for
/// a ledger-dispatched intent, so `process_refinement_outcome` was unreachable
/// on the durable path, `complete_refinement_intent` was never called, and the
/// run sat `running` with no successor intent and a NULL stop tag forever —
/// exactly the state proposals `bzpt`, `op33` and `8ixk` were found in, with
/// adversary debate entries and no advocate or judge turn.
///
/// The assertion is the side effect that matters: an **Advocate turn actually
/// happens**. Not "the classifier returned X", not "a projection exists" — a
/// durable Advocate intent, a completed Adversary intent, a closed Adversary
/// task, and a materialized Advocate role task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_adversary_exit_hands_off_to_a_real_advocate_turn() {
    let mut f = handoff_fixture().await;
    let adversary = run_adversary_turn(&mut f).await;

    // The coordinator must observe that clean exit and hand off.
    f.actor.drive_active_refinements().await;

    let after = snapshot(&f).await;
    let advocate_intent = after
        .snapshot
        .intents
        .iter()
        .find(|intent| {
            intent.run_id == f.run_id
                && intent.phase == DurablePhase::AdvocateRevision
                && intent.role == RefinementRole::Advocate
        })
        .unwrap_or_else(|| {
            panic!(
                "the adversary exit must durably enqueue the Advocate successor; \
                 intents were {:?}",
                after.snapshot.intents
            )
        });
    assert_ne!(
        advocate_intent.state,
        RefinementIntentState::Completed,
        "the successor advocate intent must still be dispatchable"
    );
    assert!(
        after.snapshot.intents.iter().any(|intent| {
            intent.phase == DurablePhase::AdversaryAttack
                && intent.state == RefinementIntentState::Completed
        }),
        "the adversary intent must be durably completed, not left materialized"
    );
    assert_eq!(
        TaskRepository::new(f.db.clone(), EventBus::noop())
            .get(&adversary.id)
            .await
            .expect("reload adversary task")
            .expect("adversary task exists")
            .status,
        "closed",
        "a finished role task must not linger open and keep the run artificially live"
    );

    // The successor intent must produce a real Advocate role task — the
    // advocate TURN, not merely a ledger row.
    f.actor.drive_active_refinements().await;
    let advocate = role_task(&f, "advocate")
        .await
        .expect("the advocate successor intent must materialize an advocate role task");
    assert_eq!(
        advocate.refinement_run_id.as_deref(),
        Some(f.run_id.as_str())
    );
    assert_eq!(
        advocate.refinement_generation,
        Some(i64::from(f.generation))
    );
    wait_until_pool_holds(&f.actor.pool, &advocate.id).await;
}

/// No ledger-dispatched role may leave its run `running` with a NULL stop tag.
///
/// A role that is dispatched and then never settles must still hit the
/// coordinator's execution watchdog and terminalize the run with a durable stop
/// reason. That watchdog only ever fired for legacy map-driven dispatches,
/// because a ledger-dispatched intent had no in-flight projection to watch, so
/// a durable run that lost its role simply stayed active forever.
///
/// Time is aged rather than slept: the projection the coordinator registered on
/// its own is back-dated past the execution budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ledger_dispatched_role_that_never_settles_terminalizes_the_run() {
    let mut f = handoff_fixture().await;

    f.actor.drive_active_refinements().await;
    let adversary = role_task(&f, "adversary")
        .await
        .expect("the ledger must materialize the opening adversary task");
    wait_until_pool_holds(&f.actor.pool, &adversary.id).await;
    // The agent session started but never produced an outcome.
    record_agent_turn(&f, &adversary.id, "adversary", None).await;

    let aged = std::time::Instant::now()
        .checked_sub(Duration::from_secs(4_000))
        .expect("monotonic clock supports back-dating");
    let watched = match f.actor.refinement_sessions.get_mut(&f.run_id) {
        Some(session) => {
            session.dispatched_at = aged;
            session.session_started_at = Some(aged);
            true
        }
        None => false,
    };
    assert!(
        watched,
        "a ledger-dispatched role must be registered as in-flight so the \
         execution watchdog can see it"
    );

    f.actor.drive_active_refinements().await;

    let after = snapshot(&f).await;
    assert_eq!(
        after.snapshot.run.state,
        RefinementRunState::Terminal,
        "a ledger-dispatched role that never settles must terminalize its run, \
         never leave it abandoned and active"
    );
    assert!(
        after.snapshot.run.terminal_reason.is_some(),
        "a terminal run must carry a durable stop reason, not a NULL stop tag"
    );
}

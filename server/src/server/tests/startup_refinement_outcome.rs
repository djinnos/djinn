//! A server restart must not make a live refinement round look like a dead one
//! (task `8u9x`, epic `43ww`, proposal `ih1w`).
//!
//! A refinement role runs in an ordinary task-run Job with an ordinary agent
//! session. Startup Stage A is what decides whether that session survives the
//! boot, and every downstream refinement judgement — the shared liveness
//! evaluator, the rehydration path, and the stalled-outcome retry ledger —
//! reads durable rows that Stage A/B/C can have already rewritten. So the
//! question this file answers is not "does the refinement loop behave", it is
//! "does the startup sequence hand the refinement loop the truth".
//!
//! Both cases below are the *same* durable fixture. The only difference is what
//! the immutable cluster census says about the role's Job: present and
//! non-terminal, or authoritatively absent. Everything asserted afterwards is a
//! durable row.

use std::collections::HashMap;
use std::sync::Arc;

use djinn_coordinator::startup_census::{GoneProvenance, StartupCensus, TaskRunWitness};
use djinn_coordinator::test_helpers::StartupRefinementFixture;
use djinn_core::refinement_liveness::{
    RefinementLivenessResult, RefinementSessionState, evaluate_refinement_liveness,
};
use djinn_db::{
    Database, LoadRefinementRunSnapshotRequest, ProposalRepository, SessionRepository,
    TaskRunRepository,
};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};

use crate::events::EventBus;
use crate::server::AppState;

/// The heartbeat grace the coordinator's own recovery path uses.
const HEARTBEAT_GRACE_MILLIS: i64 = 60_000;

/// A namespace inventory that answers from a fixed table, so one census can
/// hold opposite evidence about two otherwise-identical role Jobs.
struct RefinementInventory {
    listed: Vec<WorkloadRecord>,
    presence: HashMap<String, ObjectPresence>,
}

#[async_trait::async_trait]
impl WorkloadInventory for RefinementInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(self.listed.clone())
    }

    async fn get_uid(&self, _: WorkloadObjectKind, _: &str, _: &str) -> UidGetResult {
        UidGetResult::Uncertain
    }

    async fn presence(&self, _: WorkloadObjectKind, name: &str) -> ObjectPresence {
        self.presence
            .get(name)
            .cloned()
            .unwrap_or(ObjectPresence::Uncertain)
    }
}

fn live_job(run_id: &str) -> WorkloadRecord {
    WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: djinn_k8s::taskrun_job_name(run_id),
        uid: None,
        labels: std::collections::BTreeMap::new(),
        terminal: false,
        ready: true,
        deployment_revision: None,
        images: Vec::new(),
        commands: Vec::new(),
    }
}

/// `(session status, task-run status)` for a fixture's linked lifecycle rows.
async fn durable_pair(
    db: &Database,
    events: &EventBus,
    fixture: &StartupRefinementFixture,
) -> (String, String) {
    let session = SessionRepository::new(db.clone(), events.clone())
        .get(&fixture.session_id)
        .await
        .expect("read refinement session")
        .expect("refinement session exists")
        .status;
    let run = TaskRunRepository::new(db.clone())
        .get(&fixture.task_run_id)
        .await
        .expect("read refinement task run")
        .expect("refinement task run exists")
        .status;
    (session, run)
}

/// The shared liveness authority's verdict for a run, read through the same
/// durable snapshot loader the coordinator's recovery path uses.
async fn liveness_of(db: &Database, run_id: &str) -> RefinementLivenessResult {
    let exact = ProposalRepository::new(db.clone(), EventBus::noop())
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: run_id.to_owned(),
            heartbeat_grace_millis: HEARTBEAT_GRACE_MILLIS,
        })
        .await
        .expect("load exact refinement snapshot")
        .expect("refinement run exists");
    evaluate_refinement_liveness(&exact.snapshot, exact.observed_at)
}

/// Whether the durable snapshot still carries a `Live` session for the run.
async fn snapshot_session_states(db: &Database, run_id: &str) -> Vec<RefinementSessionState> {
    ProposalRepository::new(db.clone(), EventBus::noop())
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: run_id.to_owned(),
            heartbeat_grace_millis: HEARTBEAT_GRACE_MILLIS,
        })
        .await
        .expect("load exact refinement snapshot")
        .expect("refinement run exists")
        .snapshot
        .sessions
        .into_iter()
        .map(|session| session.state)
        .collect()
}

/// Drive census -> Stage A -> Stage B -> Stage C for a live refinement Job and
/// prove nothing downstream treats the round as dead.
///
/// The paired control is the same fixture with the same evidence *class* —
/// positive `Gone(AuthoritativelyAbsent)` — and it is destructively reconciled,
/// which is what makes the preserved case's assertions non-vacuous.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_preserved_session_does_not_trigger_refinement_outcome_processing() {
    let db = djinn_coordinator::test_helpers::create_test_db();
    let events = EventBus::noop();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
    let (mut actor, cancel) =
        djinn_coordinator::test_helpers::make_coordinator_actor_cancellable(&db, &events_tx);

    let live =
        djinn_coordinator::test_helpers::seed_startup_refinement_fixture(&actor, &db, "preserved")
            .await;
    let interrupted = djinn_coordinator::test_helpers::seed_startup_refinement_fixture(
        &actor,
        &db,
        "interrupted",
    )
    .await;

    // Both rounds are dispatched and running before the boot.
    assert_eq!(
        durable_pair(&db, &events, &live).await,
        ("running".to_owned(), "running".to_owned())
    );
    assert_eq!(
        durable_pair(&db, &events, &interrupted).await,
        ("running".to_owned(), "running".to_owned())
    );

    // One immutable census: the live role's Job is present and non-terminal,
    // the control's is omitted from LIST and independently confirmed absent.
    let mut presence = HashMap::new();
    presence.insert(
        djinn_k8s::taskrun_job_name(&interrupted.task_run_id),
        ObjectPresence::Absent,
    );
    let census = StartupCensus::acquire(
        db.clone(),
        Some(Arc::new(RefinementInventory {
            listed: vec![live_job(&live.task_run_id)],
            presence,
        })),
    )
    .await
    .expect("acquire the startup census before any refinement mutation");
    assert!(
        census
            .runs()
            .iter()
            .any(|run| run.task_run_id == live.task_run_id && run.witness == TaskRunWitness::Live),
        "the live refinement Job must be a Live census witness"
    );
    assert!(
        census
            .runs()
            .iter()
            .any(|run| run.task_run_id == interrupted.task_run_id
                && run.witness == TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)),
        "the control must carry positive absence evidence, not merely missing evidence"
    );

    // ── Stage A -> Stage B -> Stage C ────────────────────────────────────
    let state = AppState::new(db.clone(), tokio_util::sync::CancellationToken::new());
    state
        .interrupt_stale_sessions_on_startup_with_census(&census)
        .await;
    djinn_coordinator::complete_startup_reaper_phase(
        &db,
        "startup-refinement-incarnation",
        Some(&census),
    )
    .await;

    assert_eq!(
        durable_pair(&db, &events, &live).await,
        ("running".to_owned(), "running".to_owned()),
        "a live refinement Job keeps its linked session and task run running"
    );
    assert_eq!(
        durable_pair(&db, &events, &interrupted).await,
        ("interrupted".to_owned(), "interrupted".to_owned()),
        "the absent control is destructively reconciled, so the preserved case is not \
         merely a startup that touched nothing"
    );

    // ── The shared liveness authority ────────────────────────────────────
    assert_eq!(
        snapshot_session_states(&db, &live.run_id).await,
        vec![RefinementSessionState::Live],
        "startup must leave the preserved round's session Live to the evaluator"
    );
    assert_eq!(
        snapshot_session_states(&db, &interrupted.run_id).await,
        vec![RefinementSessionState::Ended],
        "the control's session evidence ends, which is what makes the contrast real"
    );
    assert!(
        matches!(
            liveness_of(&db, &live.run_id).await,
            RefinementLivenessResult::Live { .. }
        ),
        "the preserved run must remain Live to evaluate_refinement_liveness"
    );

    // ── The production rehydration path ──────────────────────────────────
    djinn_coordinator::test_helpers::run_refinement_recovery(&mut actor).await;
    assert_eq!(
        djinn_coordinator::test_helpers::rehydrated_refinement_round(&actor, &live.run_id),
        Some(1),
        "the preserved run must rehydrate through the production recovery path"
    );

    // ── The durable outcome ledger ───────────────────────────────────────
    assert_eq!(
        djinn_db::test_support::refinement_outcome_attempts_for_test(&db, &live.run_id).await,
        0,
        "a preserved live round must write zero durable outcome attempts"
    );
    let audit = djinn_db::test_support::refinement_run_audit_for_test(&db, &live.run_id).await;
    assert_ne!(
        audit.stop_tag.as_deref(),
        Some("agent_failure"),
        "a preserved live round must never be stamped agent_failure by a restart"
    );
    assert_eq!(audit.state, "running");

    // ── The paired control reaches the stalled-outcome path ──────────────
    // The retry ledger is keyed on the durable handoff shape (materialized
    // intent, closed role task, no successor), not on the session or task-run
    // rows the startup sequence rewrote. Close the control's role task — what
    // the loop does when a round's task finishes — and drive the production
    // stalled-outcome entry point.
    djinn_coordinator::test_helpers::close_refinement_role_task(&mut actor, &interrupted).await;
    assert!(
        ProposalRepository::new(db.clone(), EventBus::noop())
            .load_refinement_stalled_handoffs()
            .await
            .expect("enumerate stalled handoffs")
            .iter()
            .any(|handoff| handoff.run_id == interrupted.run_id && handoff.outcome_attempts == 0),
        "the control is a stalled handoff with an untouched retry ledger"
    );

    djinn_coordinator::test_helpers::apply_stalled_refinement_outcome(&mut actor, &interrupted)
        .await;
    assert_eq!(
        djinn_db::test_support::refinement_outcome_attempts_for_test(&db, &interrupted.run_id)
            .await,
        1,
        "the stalled-outcome path consumes exactly one durable retry"
    );
    assert_eq!(
        djinn_db::test_support::refinement_outcome_attempts_for_test(&db, &live.run_id).await,
        0,
        "the preserved run's ledger is untouched by the control's retry"
    );

    // Exhaust the retry budget and prove the terminal disposition is the one a
    // dead round earns — the exact state the preserved run must never reach.
    for _ in 0..2 {
        djinn_coordinator::test_helpers::apply_stalled_refinement_outcome(&mut actor, &interrupted)
            .await;
    }
    let control_audit =
        djinn_db::test_support::refinement_run_audit_for_test(&db, &interrupted.run_id).await;
    assert_eq!(control_audit.state, "terminal");
    assert_eq!(control_audit.stop_tag.as_deref(), Some("agent_failure"));

    // ...and the preserved run is still exactly where startup left it.
    let live_audit = djinn_db::test_support::refinement_run_audit_for_test(&db, &live.run_id).await;
    assert_eq!(live_audit.state, "running");
    assert_eq!(live_audit.stop_tag, None);
    assert_eq!(
        djinn_db::test_support::refinement_outcome_attempts_for_test(&db, &live.run_id).await,
        0
    );

    cancel.cancel();
}

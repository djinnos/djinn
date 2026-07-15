use super::*;
use crate::SharedCoordinatorState;
use crate::consolidation::DbConsolidationRunner;
use crate::roles::RoleRegistry;
use crate::types::{
    AutoMergeTracker, BackgroundWorkTracker, DEFAULT_MODEL_ID, InflightDispatch, PrCleanupConfig,
    STUCK_INTERVAL,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{
    ProposalCreateInput, ProposalRepository, SessionRepository, TaskRepository, UserRepository,
    UserSettingsRepository,
};
use djinn_provider::catalog::CatalogService;
use djinn_provider::catalog::health::HealthTracker;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};
use tokio_util::sync::CancellationToken;

/// The model used for refinement dispatch in `#[cfg(test)]` builds
/// (hardcoded by `resolve_dispatch_models_for_role`).
pub(crate) const TEST_MODEL: &str = DEFAULT_MODEL_ID; // "test/mock"

fn inflight_entry(creator: &str, model: &str) -> InflightDispatch {
    InflightDispatch {
        creator: Some(creator.to_owned()),
        model: model.to_owned(),
        lane: djinn_core::models::ModelLane::Plan,
    }
}

// ── Fixture ──────────────────────────────────────────────────────────

pub(crate) struct RefinementFixture {
    #[allow(dead_code)]
    db: djinn_db::Database,
    pub(crate) project_id: String,
    pub(crate) user_id: String,
    pub(crate) proposal_id: String,
}

/// Create a project, user, and proposal (with a project target) ready
/// for refinement dispatch.
pub(crate) async fn seed_refinement_fixture(db: &djinn_db::Database) -> RefinementFixture {
    let event_bus = EventBus::noop();
    let project = crate::test_helpers::create_test_project(db).await;
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            777_100,
            "refinement-cap-user",
            Some("Refinement cap test user"),
            None,
        )
        .await
        .expect("create refinement cap test user");
    let user_id = user.id.clone();

    // Create the proposal attributed to the user via the auth-context scope.
    let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                ProposalRepository::new(db.clone(), EventBus::noop())
                    .create(ProposalCreateInput {
                        title: "Refinement cap test proposal",
                        body: "A proposal for testing per-user model cap enforcement in refinement dispatch.",
                        acceptance_criteria: Some("[]"),
                        status: Some("building"),
                        body_format: None,
                    })
                    .await
                    .expect("create refinement cap test proposal")
            })
            .await;

    // Link the proposal to the project so `create_refinement_task_with_context`
    // and `resolve_refinement_project_path` can resolve a project.
    ProposalRepository::new(db.clone(), event_bus)
        .add_target(&proposal.id, &project.id, "primary")
        .await
        .expect("add proposal target");

    RefinementFixture {
        db: db.clone(),
        project_id: project.id,
        user_id,
        proposal_id: proposal.id,
    }
}

// ── Actor construction ───────────────────────────────────────────────

/// Build a `CoordinatorActor` wired to the given pool and DB.
pub(crate) fn build_refinement_actor(
    db: &djinn_db::Database,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    pool: djinn_slot::SlotPoolHandle,
) -> CoordinatorActor {
    let cancel = CancellationToken::new();
    CoordinatorActor {
        receiver: tokio::sync::mpsc::channel(1).1,
        events: events_tx.subscribe(),
        cancel: cancel.clone(),
        tick: tokio::time::interval(STUCK_INTERVAL),
        db: db.clone(),
        events_tx: events_tx.clone(),
        pool,
        catalog: CatalogService::new(),
        health: HealthTracker::default(),
        role_registry: Arc::new(RoleRegistry::new()),
        lsp: djinn_lsp::LspManager::new(),
        self_sender: tokio::sync::mpsc::channel(1).0,
        status_tx: tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        })
        .0,
        dispatch_limit: 50,
        model_priorities: HashMap::new(),
        pr_errors: HashMap::new(),
        last_dispatched: HashMap::new(),
        inflight_dispatches: HashMap::new(),
        provisional_admissions: HashMap::new(),
        dispatch_cooldowns: HashMap::new(),
        dispatch_failure_streak: HashMap::new(),
        background_work_tracker: BackgroundWorkTracker::default(),
        auto_merge_tracker: AutoMergeTracker::default(),
        consolidation_runner: Arc::new(DbConsolidationRunner::new(db.clone())),
        last_stale_sweep: StdInstant::now(),
        last_auto_dispatch_sweep: StdInstant::now(),
        last_proposal_review_sweep: StdInstant::now(),
        last_graph_refresh: StdInstant::now(),
        graph_warmer: None,
        mirror: None,
        runtime_ops: None,
        rpc_registry: None,
        prune_tick_counter: 0,
        throughput_events: HashMap::new(),
        pr_status_cache: HashMap::new(),
        pr_draft_first_seen: HashMap::new(),
        review_stuck_sha_first_seen: HashMap::new(),
        merge_fail_count: HashMap::new(),
        auto_approve_attempted: HashMap::new(),
        delegated_to_github: HashMap::new(),
        conversations_resolved: HashMap::new(),
        handled_dequeues: HashMap::new(),
        stall_killed: HashSet::new(),
        stall_progress_watermark: HashMap::new(),
        stall_cancel_streak: HashMap::new(),
        stall_extension_count: HashMap::new(),
        provider_failure_streak: HashMap::new(),
        last_idle_consolidation: None,
        idle_consolidation_cancel: None,
        idle_consolidation_handle: None,
        pr_cleanup_config: PrCleanupConfig::default(),
        worker_lifecycle_config: crate::WorkerLifecycleConfig::default(),
        active_refinements: HashMap::new(),
        refinement_sessions: HashMap::new(),
        stranded_ready_source: None,
        closed_parent_open_children_source: None,
        dispatched: 0,
        recovered: 0,
    }
}

/// Create a slot pool with the test model configured.
pub(crate) fn spawn_test_pool(
    db: &djinn_db::Database,
    max_slots: u32,
) -> djinn_slot::SlotPoolHandle {
    let cancel = CancellationToken::new();
    djinn_slot::SlotPoolHandle::spawn(
        crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
        cancel,
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig {
                model_id: TEST_MODEL.to_owned(),
                max_slots,
                roles: ["advocate", "adversary", "judge", "worker"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
    )
}

/// Seed a `RefinementLoopState` in the actor's `active_refinements` map.
pub(crate) fn seed_refinement_state(
    actor: &mut CoordinatorActor,
    proposal_id: &str,
    attributed_user_id: Option<String>,
) {
    let state = super::super::refinement::RefinementLoopState::new(proposal_id, 1)
        .with_attributed_user(attributed_user_id);
    actor
        .active_refinements
        .insert(proposal_id.to_string(), state);
}

/// Materialize a running session row for `(user, model)` so the DB
/// active-session count reflects it.
async fn materialize_running_session(
    db: &djinn_db::Database,
    project_id: &str,
    task_id: &str,
    model_id: &str,
) -> String {
    SessionRepository::new(db.clone(), EventBus::noop())
        .create(djinn_db::CreateSessionParams {
            project_id,
            task_id: Some(task_id),
            model: model_id,
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("materialize running session")
        .id
}

/// Seed a task in the DB to host a session row.
async fn seed_task(
    db: &djinn_db::Database,
    project_id: &str,
    user_id: &str,
    label: &str,
) -> String {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id.to_owned()), async {
            TaskRepository::new(db.clone(), EventBus::noop())
                .create_in_project(
                    project_id,
                    None,
                    label,
                    "test task for cap fixture",
                    "",
                    "task",
                    0,
                    "worker",
                    Some("open"),
                    Some("[]"),
                )
                .await
                .expect("seed fixture task")
                .id
        })
        .await
}

/// Configure `max_sessions` for `(user, model)`.
async fn set_user_cap(db: &djinn_db::Database, user_id: &str, model_id: &str, cap: u32) {
    UserSettingsRepository::new(db.clone())
        .upsert_max_sessions(user_id, &HashMap::from([(model_id.to_owned(), cap)]))
        .await
        .expect("set user max_sessions cap");
}

// ── AC#1 & AC#2: At-cap deferral + repeated ticks ────────────────────

/// Regression: at-cap refinement phase defers before task creation, and
/// repeated at-cap ticks create no extra refinement tasks, leave no
/// orphan sessions, consume no spawn budget, and call no pool.dispatch().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_cap_refinement_defers_and_repeated_ticks_are_noops() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    // Seed one running session so the user is at cap (cap = 1).
    let blocking_task_id = seed_task(&db, &fixture.project_id, &fixture.user_id, "blocking").await;
    let _session_id =
        materialize_running_session(&db, &fixture.project_id, &blocking_task_id, TEST_MODEL).await;
    set_user_cap(&db, &fixture.user_id, TEST_MODEL, 1).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    // ── Tick 1: should defer (at cap) ──
    actor.drive_active_refinements().await;

    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "refinement must still be active after at-cap deferral"
    );
    let state = &actor.active_refinements[&fixture.proposal_id];
    assert_eq!(
        state.phase,
        super::super::refinement::RefinementPhase::AdversaryAttack,
        "phase must not advance on deferral"
    );
    assert_eq!(state.total_spawns, 0, "no spawn budget consumed");
    assert!(
        actor.refinement_sessions.is_empty(),
        "no refinement session should be in-flight after deferral"
    );
    assert!(
        actor.inflight_dispatches.is_empty(),
        "no inflight dispatch ledger entry should exist after deferral"
    );
    assert!(
        actor.provisional_admissions.is_empty(),
        "no provisional admission should remain after deferral"
    );

    // ── Ticks 2–4: repeated at-cap ticks must be pure no-ops ──
    for tick in 2..=4 {
        actor.drive_active_refinements().await;

        assert!(
            actor.active_refinements.contains_key(&fixture.proposal_id),
            "tick {tick}: refinement still active"
        );
        let state = &actor.active_refinements[&fixture.proposal_id];
        assert_eq!(
            state.total_spawns, 0,
            "tick {tick}: no spawn budget consumed"
        );
        assert!(
            actor.refinement_sessions.is_empty(),
            "tick {tick}: no refinement session in-flight"
        );
        assert!(
            actor.inflight_dispatches.is_empty(),
            "tick {tick}: no inflight dispatch ledger entry"
        );
        assert!(
            actor.provisional_admissions.is_empty(),
            "tick {tick}: no provisional admission"
        );
    }

    // Verify no refinement tasks were created.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert!(
        refinement_tasks.is_empty(),
        "no refinement tasks should have been created during at-cap deferrals"
    );

    // Clean up: release the blocking session.
    SessionRepository::new(db.clone(), EventBus::noop())
        .update(
            &_session_id,
            djinn_core::models::SessionStatus::Completed,
            0,
            0,
            0,
            0,
            None,
        )
        .await
        .expect("complete blocking session");
}

// ── AC#3: Capacity-free retry ────────────────────────────────────────

/// Regression: once capacity becomes available after one or more
/// deferrals, the phase creates and dispatches exactly one refinement
/// task and continues through the existing state machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_free_retry_dispatches_exactly_one_refinement() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    // Seed one running session + cap = 1 so the first tick defers.
    let blocking_task_id = seed_task(&db, &fixture.project_id, &fixture.user_id, "blocking").await;
    let session_id =
        materialize_running_session(&db, &fixture.project_id, &blocking_task_id, TEST_MODEL).await;
    set_user_cap(&db, &fixture.user_id, TEST_MODEL, 1).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    // Tick 1: defer (at cap).
    actor.drive_active_refinements().await;
    assert!(actor.refinement_sessions.is_empty(), "deferred on tick 1");

    // Free the capacity by completing the blocking session.
    SessionRepository::new(db.clone(), EventBus::noop())
        .update(
            &session_id,
            djinn_core::models::SessionStatus::Completed,
            0,
            0,
            0,
            0,
            None,
        )
        .await
        .expect("complete blocking session");

    // Tick 2: should now dispatch exactly one refinement task.
    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.contains_key(&fixture.proposal_id),
        "refinement session should be in-flight after capacity-freed dispatch"
    );
    let session = &actor.refinement_sessions[&fixture.proposal_id];
    assert_eq!(
        session.phase,
        super::super::refinement::RefinementPhase::AdversaryAttack,
        "first phase is AdversaryAttack"
    );
    assert_eq!(
        session.model_id, TEST_MODEL,
        "dispatched with the test model"
    );

    // Verify exactly one refinement task was created.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert_eq!(
        refinement_tasks.len(),
        1,
        "exactly one refinement task should exist"
    );

    // The state machine should have recorded one spawn.
    let state = &actor.active_refinements[&fixture.proposal_id];
    assert_eq!(state.total_spawns, 1, "one spawn recorded");

    // The in-flight ledger should contain the dispatched task.
    assert!(
        actor.inflight_dispatches.contains_key(&session.task_id),
        "in-flight ledger must track the dispatched refinement task"
    );

    // Provisional admission must have been cleared (re-keyed to inflight).
    assert!(
        actor.provisional_admissions.is_empty(),
        "provisional admission re-keyed to inflight"
    );
}

// ── AC#4: Unresolved attribution fails closed ────────────────────────

/// Regression: missing/dangling/unresolvable attribution fails closed
/// with no refinement task row, no spawn-budget consumption, and no
/// pool.dispatch().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolved_attribution_fails_closed() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    let mut actor = build_refinement_actor(&db, &events_tx, pool);

    // Case A: no attributed_user_id and proposal has no author.
    // Create a proposal with no author_user_id (outside any user scope).
    let orphan_proposal = ProposalRepository::new(db.clone(), EventBus::noop())
        .create(ProposalCreateInput {
            title: "Orphan proposal",
            body: "No author.",
            acceptance_criteria: Some("[]"),
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create orphan proposal");
    ProposalRepository::new(db.clone(), EventBus::noop())
        .add_target(&orphan_proposal.id, &fixture.project_id, "primary")
        .await
        .expect("add target to orphan proposal");

    // Seed a refinement loop with no explicit attributed user.
    let orphan_state = super::super::refinement::RefinementLoopState::new(&orphan_proposal.id, 1);
    actor
        .active_refinements
        .insert(orphan_proposal.id.clone(), orphan_state);

    actor.drive_active_refinements().await;

    // `drive_active_refinements` retains only non-complete entries, so a
    // terminated refinement is removed from the map entirely.
    assert!(
        !actor.active_refinements.contains_key(&orphan_proposal.id),
        "orphan proposal refinement must be removed after fail-closed termination"
    );
    assert!(
        actor.refinement_sessions.is_empty(),
        "no refinement session created for unresolved attribution"
    );
    assert!(
        actor.inflight_dispatches.is_empty(),
        "no inflight dispatch for unresolved attribution"
    );
    assert!(
        actor.provisional_admissions.is_empty(),
        "no provisional admission for unresolved attribution"
    );

    // Verify no refinement task was created for the orphan proposal.
    let all_tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let orphan_refinement_tasks: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.issue_type == "refinement" && t.title.contains("Orphan"))
        .collect();
    assert!(
        orphan_refinement_tasks.is_empty(),
        "no refinement task for unresolved attribution"
    );

    // Case B: dangling user id that doesn't exist in the users table.
    let dangling_proposal = ProposalRepository::new(db.clone(), EventBus::noop())
        .create(ProposalCreateInput {
            title: "Dangling attribution proposal",
            body: "Attributed to non-existent user.",
            acceptance_criteria: Some("[]"),
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create dangling proposal");
    ProposalRepository::new(db.clone(), EventBus::noop())
        .add_target(&dangling_proposal.id, &fixture.project_id, "primary")
        .await
        .expect("add target to dangling proposal");

    let dangling_state =
        super::super::refinement::RefinementLoopState::new(&dangling_proposal.id, 1)
            .with_attributed_user(Some("non-existent-user-id-000".to_owned()));
    actor
        .active_refinements
        .insert(dangling_proposal.id.clone(), dangling_state);

    actor.drive_active_refinements().await;

    assert!(
        !actor.active_refinements.contains_key(&dangling_proposal.id),
        "dangling attribution refinement must be removed after fail-closed termination"
    );
    assert!(
        actor.refinement_sessions.is_empty(),
        "no refinement session for dangling attribution"
    );
    assert!(
        actor.inflight_dispatches.is_empty(),
        "no inflight dispatch for dangling attribution"
    );
}

// ── AC#5: Same-tick session-row lag ──────────────────────────────────

/// Regression: with DB active-session count cold but one admitted
/// in-flight refinement for `(user, model)`, the shared ledger overlay
/// makes the effective count reflect that admission. Both a second
/// refinement and a normal task for the same `(user, model)` defer
/// when admitting them would exceed `max_sessions`.
///
/// This test exercises the pure admission primitives
/// (`overlay_inflight_ledger` + `model_under_user_cap`) directly so
/// the assertion is not affected by the pool-reconciliation step that
/// `check_user_model_admission` performs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_tick_session_row_lag_defers_second_admission() {
    use crate::dispatch::{model_under_user_cap, overlay_inflight_ledger};

    // Scenario: DB active-session count is 0 (cold — no session row yet),
    // but one in-flight refinement has been admitted for (user, model).
    // Cap is 1.  The overlay must reflect the in-flight entry so that a
    // second candidate (refinement OR normal task) sees effective count
    // = 1 and defers.
    let user = "test-user";
    let model = TEST_MODEL;
    let cap: u32 = 1;

    let mut running: HashMap<(String, String), u32> = HashMap::new();
    // DB seed: no active sessions (cold).
    assert!(running.is_empty(), "precondition: cold DB");

    // In-flight ledger: one admitted refinement for (user, model).
    let inflight: HashMap<String, InflightDispatch> =
        HashMap::from([("refinement-task-1".to_string(), inflight_entry(user, model))]);

    overlay_inflight_ledger(&mut running, &inflight);

    assert_eq!(
        running.get(&(user.to_string(), model.to_string())).copied(),
        Some(1),
        "overlay must reflect the in-flight refinement"
    );
    assert!(
        !model_under_user_cap(&running, user, model, cap),
        "second admission must be blocked: effective count 1 >= cap 1"
    );

    // With cap = 2, one slot remains.
    assert!(
        model_under_user_cap(&running, user, model, 2),
        "cap=2 allows one more admission with effective count 1"
    );

    // Adding a second inflight entry fills the cap=2 slot too.
    let mut inflight2 = inflight.clone();
    inflight2.insert("normal-task-1".to_string(), inflight_entry(user, model));
    let mut running2: HashMap<(String, String), u32> = HashMap::new();
    overlay_inflight_ledger(&mut running2, &inflight2);

    assert_eq!(
        running2
            .get(&(user.to_string(), model.to_string()))
            .copied(),
        Some(2),
        "overlay with two in-flight entries shows effective count 2"
    );
    assert!(
        !model_under_user_cap(&running2, user, model, 2),
        "cap=2 blocks admission when two in-flight entries exist"
    );
}

// ── AC#6: Dispatch/setup failure clears in-flight reservation ────────

/// Regression: when pool.dispatch() fails after a refinement task has
/// been created and an in-flight reservation recorded, the reservation
/// is cleared and the refinement is terminated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_failure_clears_refinement_inflight_entry() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    // Create a pool, then cancel it and wait for the actor to exit so
    // dispatch() returns PoolError::ActorDead.
    let cancel_token = CancellationToken::new();
    let pool = djinn_slot::SlotPoolHandle::spawn(
        crate::test_helpers::agent_context_from_db(db.clone(), cancel_token.clone()),
        cancel_token.clone(),
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig {
                model_id: TEST_MODEL.to_owned(),
                max_slots: 1,
                roles: ["advocate", "adversary", "judge", "worker"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
    );
    // Cancel and wait for the pool actor to fully exit.
    cancel_token.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    // Dispatch should attempt task creation + pool.dispatch(), fail on
    // pool dispatch, and clear the in-flight reservation.
    actor.drive_active_refinements().await;

    // `drive_active_refinements` removes completed entries from the map.
    assert!(
        !actor.active_refinements.contains_key(&fixture.proposal_id),
        "refinement must be removed after dispatch-failure termination"
    );

    // The in-flight dispatch ledger must be clean.
    assert!(
        actor.inflight_dispatches.is_empty(),
        "inflight dispatch ledger must be cleared after pool dispatch failure: {:?}",
        actor.inflight_dispatches,
    );

    // Provisional admissions must be clean.
    assert!(
        actor.provisional_admissions.is_empty(),
        "provisional admissions must be empty after dispatch failure"
    );

    // The refinement session must have been removed.
    assert!(
        actor.refinement_sessions.is_empty(),
        "refinement session must be removed after dispatch failure"
    );

    // A refinement task WAS created (pool dispatch is after task creation),
    // but no inflight ledger entry should remain for it.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert_eq!(
        refinement_tasks.len(),
        1,
        "one refinement task should have been created before the pool dispatch failure"
    );
    let created_task_id = &refinement_tasks[0].id;
    assert!(
        !actor.inflight_dispatches.contains_key(created_task_id),
        "inflight ledger must NOT contain the failed refinement task"
    );
}

// ── Shared ledger overlay: normal dispatch and refinement share ledger ─

/// Regression: the in-flight ledger is shared between normal task
/// dispatch and refinement dispatch. A just-admitted refinement
/// reservation reduces capacity visible to the shared admission
/// primitives for any dispatch path (refinement or normal task).
///
/// This test exercises `overlay_inflight_ledger` and
/// `model_under_user_cap` directly with mixed entry kinds to prove
/// the shared ledger contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refinement_and_normal_dispatch_share_inflight_ledger() {
    use crate::dispatch::{model_under_user_cap, overlay_inflight_ledger};

    let user = "shared-ledger-user";
    let model = TEST_MODEL;

    // Cap = 2. Two different dispatch sources: one normal task, one
    // refinement. They share the same (user, model) ledger.
    let inflight: HashMap<String, InflightDispatch> = HashMap::from([
        ("normal-task-1".to_string(), inflight_entry(user, model)),
        ("refinement-task-1".to_string(), inflight_entry(user, model)),
    ]);

    let mut running: HashMap<(String, String), u32> = HashMap::new();
    overlay_inflight_ledger(&mut running, &inflight);

    assert_eq!(
        running.get(&(user.to_string(), model.to_string())).copied(),
        Some(2),
        "both entries count toward effective running"
    );
    assert!(
        !model_under_user_cap(&running, user, model, 2),
        "cap=2 with two in-flight entries blocks a third admission"
    );

    // Removing one entry (e.g. normal task completes) frees a slot.
    let inflight_one: HashMap<String, InflightDispatch> =
        HashMap::from([("refinement-task-1".to_string(), inflight_entry(user, model))]);
    let mut running_one: HashMap<(String, String), u32> = HashMap::new();
    overlay_inflight_ledger(&mut running_one, &inflight_one);

    assert_eq!(
        running_one
            .get(&(user.to_string(), model.to_string()))
            .copied(),
        Some(1),
        "one entry shows effective count 1"
    );
    assert!(
        model_under_user_cap(&running_one, user, model, 2),
        "cap=2 with one in-flight entry allows one more admission"
    );

    // The caller reconciles away ledger entries whose session row has landed,
    // so a remaining ledger entry is disjoint booting work and adds to DB.
    let mut running_with_db = HashMap::from([((user.to_string(), model.to_string()), 2u32)]);
    overlay_inflight_ledger(&mut running_with_db, &inflight_one);
    assert_eq!(
        running_with_db
            .get(&(user.to_string(), model.to_string()))
            .copied(),
        Some(3),
        "two running plus one booting reservation = 3"
    );
}

// ── AC#1–5: Reviewer feedback injection into tribunal task descriptions ──

/// The exact label used to inject human reviewer feedback into refinement
/// task descriptions. Must match the label emitted by
/// `create_refinement_task_with_context`.
const REVIEWER_FEEDBACK_LABEL: &str = "Human reviewer feedback for this round:";

/// Regression: when the latest current-revision demand-round carries reviewer
/// feedback, the next Advocate, Adversary, and Judge refinement task
/// descriptions each contain the exact label followed by the exact feedback
/// string, unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_feedback_injected_into_all_three_tribunal_task_descriptions() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // Record a demand round with a unique feedback string on the current
    // revision (seq == latest_revision_seq at creation time == 1).
    let unique_feedback = "UNIQUE-FEEDBACK-019f0fed-advocate-adversary-judge";
    let demand_metadata = serde_json::json!({
        "source": "human_demand_round",
        "reviewer_feedback": unique_feedback,
        "reason": unique_feedback,
    });
    ProposalRepository::new(db.clone(), EventBus::noop())
        .record_refinement_lifecycle(
            &fixture.proposal_id,
            "refinement_start",
            Some(&demand_metadata),
        )
        .await
        .expect("record demand-round lifecycle");

    let readiness_context = "Proposal currently meets all DoR checks.";
    let revision_seq = 1;

    // Create refinement tasks for all three tribunal roles and assert each
    // description contains the exact label and feedback string.
    for agent_type in ["advocate", "adversary", "judge"] {
        let task_id = actor
            .create_refinement_task_with_context(
                &fixture.proposal_id,
                agent_type,
                1,
                revision_seq,
                readiness_context,
                Some(unique_feedback),
                Some(&fixture.user_id),
            )
            .await
            .unwrap_or_else(|| panic!("create refinement task for {agent_type}"));

        let task = TaskRepository::new(db.clone(), EventBus::noop())
            .get(&task_id)
            .await
            .expect("read task")
            .expect("task exists");

        assert_eq!(
            task.created_by_user_id.as_deref(),
            Some(fixture.user_id.as_str()),
            "{agent_type} task must be atomically attributed to the durable refinement owner"
        );
        assert!(
            task.description.contains(REVIEWER_FEEDBACK_LABEL),
            "{agent_type} task description must contain the exact feedback label:\n{}",
            task.description
        );
        assert!(
            task.description.contains(unique_feedback),
            "{agent_type} task description must contain the exact unique feedback string unchanged:\n{}",
            task.description
        );
    }
}

/// Regression: when no current-revision reviewer feedback exists, the task
/// description preserves the existing DoR/readiness behavior and does NOT
/// contain the feedback label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_current_feedback_preserves_existing_task_description() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    // No demand-round feedback recorded — pass None explicitly.
    let readiness_context = "Proposal currently meets all DoR checks.";

    let task_id = actor
        .create_refinement_task_with_context(
            &fixture.proposal_id,
            "adversary",
            1,
            1,
            readiness_context,
            None,
            Some(&fixture.user_id),
        )
        .await
        .expect("create refinement task");

    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .get(&task_id)
        .await
        .expect("read task")
        .expect("task exists");

    assert!(
        !task.description.contains(REVIEWER_FEEDBACK_LABEL),
        "no feedback label should appear when no current feedback exists:\n{}",
        task.description
    );
    assert!(
        task.description.contains("Current DoR status:"),
        "DoR readiness context must still be present:\n{}",
        task.description
    );
}

/// Regression: stale reviewer feedback from a prior revision is excluded —
/// when the proposal's revision has advanced, the helper returns `None` and
/// the task description does not contain the feedback label or stale string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_revision_feedback_is_excluded_from_task_description() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    let actor = build_refinement_actor(&db, &events_tx, pool.clone());

    let stale_feedback = "STALE-FEEDBACK-from-older-revision-019f0fed";
    let demand_metadata = serde_json::json!({
        "source": "human_demand_round",
        "reviewer_feedback": stale_feedback,
        "reason": stale_feedback,
    });
    let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
    proposal_repo
        .record_refinement_lifecycle(
            &fixture.proposal_id,
            "refinement_start",
            Some(&demand_metadata),
        )
        .await
        .expect("record stale demand-round lifecycle");

    // The helper should still see the feedback on the current revision (seq=1).
    let still_current = proposal_repo
        .latest_current_revision_reviewer_feedback(&fixture.proposal_id)
        .await
        .expect("read feedback");
    assert_eq!(still_current.as_deref(), Some(stale_feedback));

    // Advance the proposal revision (body change bumps latest_revision_seq).
    let proposal = proposal_repo
        .get(&fixture.proposal_id)
        .await
        .expect("read proposal")
        .expect("proposal exists");
    proposal_repo
        .update(
            &fixture.proposal_id,
            djinn_db::ProposalUpdateInput {
                title: &proposal.title,
                body: "updated body that advances the revision seq",
                acceptance_criteria: &proposal.acceptance_criteria,
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .expect("advance proposal revision");

    // Now the helper must return None — feedback is from an older seq.
    let now_stale = proposal_repo
        .latest_current_revision_reviewer_feedback(&fixture.proposal_id)
        .await
        .expect("read feedback after revision advance");
    assert!(
        now_stale.is_none(),
        "stale feedback from an older revision must not be considered current"
    );

    // Simulating the dispatch path: with None feedback, the task description
    // must not contain the stale feedback or the label.
    let task_id = actor
        .create_refinement_task_with_context(
            &fixture.proposal_id,
            "judge",
            2,
            2,
            "Proposal currently meets all DoR checks.",
            now_stale.as_deref(),
            Some(&fixture.user_id),
        )
        .await
        .expect("create refinement task");

    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .get(&task_id)
        .await
        .expect("read task")
        .expect("task exists");

    assert!(
        !task.description.contains(REVIEWER_FEEDBACK_LABEL),
        "stale feedback must not inject the label:\n{}",
        task.description
    );
    assert!(
        !task.description.contains(stale_feedback),
        "stale feedback string must not appear in the task description:\n{}",
        task.description
    );
}

// ── Evidence lifecycle state gate tests ───────────────────────────────

/// The evidence lifecycle state gate must NOT block dispatch when the
/// proposal is in Active lifecycle state (no linked spike, no lifecycle
/// events, non-terminal status, not frozen/paused). This is the positive
/// case: normal refinement dispatch proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_lifecycle_active_allows_dispatch() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    // The fixture proposal is "building" with no linked spike — Active.
    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.contains_key(&fixture.proposal_id),
        "Active lifecycle state must allow refinement dispatch"
    );
    let session = &actor.refinement_sessions[&fixture.proposal_id];
    assert_eq!(
        session.phase,
        super::super::refinement::RefinementPhase::AdversaryAttack,
        "first dispatched phase is AdversaryAttack"
    );
}

/// When a linked evidence spike is open (task status != "closed"), the
/// evidence lifecycle state is AwaitingEvidence and dispatch must be
/// skipped. No refinement task is created and no pool dispatch occurs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_lifecycle_awaiting_evidence_skips_dispatch() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    // Create an open spike task in the DB.
    let spike_task_id = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(fixture.user_id.clone()), async {
            TaskRepository::new(db.clone(), EventBus::noop())
                .create_in_project(
                    &fixture.project_id,
                    None,
                    "Evidence spike",
                    "Investigation for evidence demand",
                    "",
                    "spike",
                    0,
                    "worker",
                    Some("open"),
                    Some("[]"),
                )
                .await
                .expect("create spike task")
                .id
        })
        .await;

    // Link the spike to the proposal (sets status to "draft").
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_needs_evidence_spike(
            &fixture.proposal_id,
            &spike_task_id,
            r#"{"question":"Is X feasible?","target_subsystem":"core","spec_unknown_anchor":"unknown","insufficient_in_session_research":"too broad","expected_findings":"proof","round":1,"against_revision_seq":1,"created_by_task_id":"judge-task"}"#,
        )
        .await
        .expect("set needs-evidence spike");

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.is_empty(),
        "AwaitingEvidence lifecycle state must skip dispatch — no session created"
    );
    // The refinement must remain active (not terminated) — it's parked.
    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "refinement must remain active when parked for evidence"
    );

    // Verify no refinement task was created.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert!(
        refinement_tasks.is_empty(),
        "no refinement task should be created when AwaitingEvidence"
    );
}

/// When an `refinement_evidence_failed` lifecycle event exists, the
/// evidence lifecycle state is EvidenceFailed and dispatch must be
/// skipped. This is distinguishable from Active and Terminal in the
/// dispatch path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_lifecycle_evidence_failed_skips_dispatch() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    // Create a spike task that will be closed (failed).
    let spike_task_id = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(fixture.user_id.clone()), async {
            TaskRepository::new(db.clone(), EventBus::noop())
                .create_in_project(
                    &fixture.project_id,
                    None,
                    "Evidence spike (failed)",
                    "Investigation that failed",
                    "",
                    "spike",
                    0,
                    "worker",
                    Some("open"),
                    Some("[]"),
                )
                .await
                .expect("create spike task")
                .id
        })
        .await;

    // Link the spike to the proposal.
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_needs_evidence_spike(
            &fixture.proposal_id,
            &spike_task_id,
            r#"{"question":"Is Y feasible?","target_subsystem":"core","spec_unknown_anchor":"unknown","insufficient_in_session_research":"too broad","expected_findings":"proof","round":1,"against_revision_seq":1,"created_by_task_id":"judge-task"}"#,
        )
        .await
        .expect("set needs-evidence spike");

    // Close the spike task (simulate failure).
    TaskRepository::new(db.clone(), EventBus::noop())
        .set_status_with_reason(&spike_task_id, "closed", Some("force_closed"))
        .await
        .expect("close spike task");

    // Record an evidence_failed lifecycle event.
    ProposalRepository::new(db.clone(), EventBus::noop())
        .record_evidence_failed(
            &fixture.proposal_id,
            &spike_task_id,
            "judge-task-1",
            1,
            1,
            "spike force-closed without findings",
        )
        .await
        .expect("record evidence failed");

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.is_empty(),
        "EvidenceFailed lifecycle state must skip dispatch — no session created"
    );
    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "refinement must remain active (not terminated) when evidence failed"
    );

    // Verify no refinement task was created.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert!(
        refinement_tasks.is_empty(),
        "no refinement task should be created when EvidenceFailed"
    );
}

/// When the proposal is build-frozen, the evidence lifecycle state is
/// PausedOrFrozen and dispatch must be skipped even though no evidence
/// demand is active. The `build_frozen` flag is not checked by the
/// existing `refinement_dispatch_paused` fast-path, so this verifies
/// the lifecycle gate catches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_lifecycle_paused_or_frozen_skips_dispatch() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    // Freeze the proposal build.
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_frozen(&fixture.proposal_id, true)
        .await
        .expect("freeze proposal");

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.is_empty(),
        "PausedOrFrozen lifecycle state must skip dispatch — no session created"
    );
    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "refinement must remain active when paused/frozen"
    );

    // Verify no refinement task was created.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert!(
        refinement_tasks.is_empty(),
        "no refinement task should be created when PausedOrFrozen"
    );
}

/// When the proposal is in a terminal status ("done"), the evidence
/// lifecycle state is Terminal and dispatch must be skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_lifecycle_terminal_skips_dispatch() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let pool = spawn_test_pool(&db, 4);

    // Move the proposal to terminal status.
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_status(&fixture.proposal_id, "done")
        .await
        .expect("set proposal status to done");

    let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
    seed_refinement_state(
        &mut actor,
        &fixture.proposal_id,
        Some(fixture.user_id.clone()),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.is_empty(),
        "Terminal lifecycle state must skip dispatch — no session created"
    );
    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "refinement must remain active in map (terminal skip does not terminate the loop)"
    );

    // Verify no refinement task was created.
    let tasks = TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(&fixture.project_id)
        .await
        .expect("list tasks");
    let refinement_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.issue_type == "refinement")
        .collect();
    assert!(
        refinement_tasks.is_empty(),
        "no refinement task should be created when Terminal"
    );
}

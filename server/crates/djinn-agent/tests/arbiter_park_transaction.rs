//! Integration tests for the arbiter park transaction via
//! `DirectServices::transition_task("arbiter_park")`.
// djinn:allow-oversize
//!
//! These tests exercise the actual code path that the supervisor calls when
//! `StageOutcome::LeadParked { park_dossier_json }` is handled, rather than
//! manually simulating the individual repository calls.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use djinn_agent::context::AgentContext;
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::supervisor::{SupervisorServices, services_for_agent_context};
use djinn_core::events::EventBus;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::repositories::task::ActivityQuery;
use djinn_db::repositories::task_arbitration::{
    ArbitrationState, CreateArbitrationParams, TaskArbitrationRepository,
};
use djinn_db::{
    Database, EffectiveCreatorProvenance, EpicCreateInput, EpicRepository, ProjectRepository,
    TaskRepository, UserRepository,
};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── Helpers (mirrors phase1_supervisor.rs inline helpers) ────────────────

static NEXT_FIXTURE_GITHUB_ID: AtomicI64 = AtomicI64::new(9_200_000_000);

fn test_agent_context(db: Database) -> AgentContext {
    AgentContext {
        db,
        event_bus: EventBus::noop(),
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        background_work_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        role_registry: Arc::new(RoleRegistry::new()),
        health_tracker: HealthTracker::new(),
        file_time: Arc::new(FileTime::new()),
        lsp: LspManager::new(),
        catalog: CatalogService::new(),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: Default::default(),
        task_ops_project_path_override: None,
        working_root: None,
        graph_warmer: None,
        repo_graph_ops: None,
        runtime_ops: None,
        cargo_target_runs_root: Some({
            let path = std::env::current_dir()
                .unwrap()
                .join("target")
                .join("test-tmp")
                .join(format!("cargo-target-runs-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&path).unwrap();
            path
        }),
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        read_source_authorization: djinn_agent::context::ReadSourceAuthorization::default(),
        reconciliation_sweep: djinn_agent::context::ReconciliationSweepConfig::default(),
        memory_intent_planner: djinn_agent::context::MemoryIntentPlannerConfig::default(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        shell_launch: None,
        compaction_cs: djinn_slot::reply_loop::CompactionCriticalSection::default(),
    }
}

async fn create_project_and_epic(db: &Database) -> (String, String) {
    let events = EventBus::noop();
    let project = ProjectRepository::new(db.clone(), events.clone())
        .create("test-project", "test", "test-owner/test-repo")
        .await
        .expect("create project");
    let epic = EpicRepository::new(db.clone(), events)
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "test-epic",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("create epic");
    (project.id, epic.id)
}

async fn create_task(db: &Database, project_id: &str, epic_id: &str) -> Task {
    let events = EventBus::noop();
    let github_id = NEXT_FIXTURE_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    let creator = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("arbiter-park-fixture-{github_id}"),
            Some("Arbiter Park Fixture"),
            None,
        )
        .await
        .expect("create task creator");
    TaskRepository::new(db.clone(), events)
        .create_in_project_with_provenance(
            project_id,
            Some(epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
            "Test task",
            "task description",
            "task design",
            "task",
            0,
            "test-owner",
            Some("open"),
            None,
        )
        .await
        .expect("create task")
}

/// Transition a task through escalate → lead_intervention_start to land at
/// `in_lead_intervention`.
async fn transition_to_in_lead_intervention(db: &Database, task_id: &str) {
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    repo.transition(
        task_id,
        TransitionAction::Escalate,
        "system",
        "coordinator",
        Some("test escalate"),
        None,
    )
    .await
    .expect("escalate");
    repo.transition(
        task_id,
        TransitionAction::LeadInterventionStart,
        "system",
        "coordinator",
        None,
        None,
    )
    .await
    .expect("lead_intervention_start");
    let task = repo
        .get(task_id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(task.status, "in_lead_intervention");
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Verify a valid arbiter park decision exercises the full
/// `DirectServices::transition_task("arbiter_park")` path: persists the
/// decision payload and structured dossier on the arbitration row, marks it
/// consumed, creates an autonomous planner escalation task whose description
/// includes the structured dossier content, and parks the task to open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_park_full_transaction() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Move to in_lead_intervention.
    transition_to_in_lead_intervention(&db, &task.id).await;

    // Create an unconsumed arbitration row.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let ci = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &ci,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row");
    let (cycle, unconsumed) = arb_repo
        .resolve_current_hold_cycle(&task.id)
        .await
        .expect("resolve");
    assert_eq!(cycle, 0);
    assert!(unconsumed.is_some(), "must have unconsumed row");

    // Build dossier and call the actual DirectServices path.
    let dossier = serde_json::json!({
        "hold_description": "Requires senior engineer review of auth flow",
        "failure_analysis": "Three attempts failed; auth logic needs domain expertise",
        "attempted_decisions": ["reopen", "reopen"],
        "recommended_action": "Assign to auth-team lead"
    });
    let dossier_json = serde_json::to_string(&dossier).unwrap();

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // This is the actual production path: transition_task("arbiter_park") calls
    // execute_arbiter_park_transaction internally, then applies the state
    // transition.
    services
        .transition_task(task.id.clone(), "arbiter_park".into(), Some(dossier_json))
        .await
        .expect("transition_task arbiter_park must succeed");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo
        .get(&task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(
        task.status, "open",
        "ArbiterPark must transition in_lead_intervention → open"
    );

    // Verify: arbitration row consumed with dossier persisted.
    let record = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("get arbitration")
        .expect("arbitration row exists");
    assert_eq!(
        record.arbitration_state(),
        Some(ArbitrationState::Consumed),
        "arbitration row must be consumed"
    );
    assert!(record.consumed_at.is_some(), "consumed_at must be set");
    let stored = record.dossier.expect("dossier must be persisted");
    assert_eq!(
        stored["hold_description"], "Requires senior engineer review of auth flow",
        "dossier hold_description must match"
    );
    assert_eq!(
        stored["failure_analysis"], "Three attempts failed; auth logic needs domain expertise",
        "dossier failure_analysis must match"
    );

    // Verify: an autonomous planner escalation (NOT a human-review hold) blocks
    // the source, carrying the dossier content in its description.
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(
        !blockers.is_empty(),
        "source task must be blocked by planner escalation task"
    );
    let hold_task = repo
        .get(&blockers[0].task_id)
        .await
        .expect("get escalation task")
        .expect("escalation task exists");
    // Dispatchable planner shape: an open `review` task carrying the
    // `planner-park-escalation` label and NOT the human-review-hold label.
    assert_eq!(
        hold_task.issue_type, "review",
        "escalation must be a review task (planner-dispatchable)"
    );
    assert_eq!(hold_task.status, "open", "escalation must be open");
    assert!(
        hold_task.labels.contains("planner-park-escalation"),
        "escalation must carry the planner-park-escalation label, got: {}",
        hold_task.labels
    );
    assert!(
        !hold_task.labels.contains("human-review-hold"),
        "park must NOT create a human-review hold, got labels: {}",
        hold_task.labels
    );
    assert!(
        hold_task
            .description
            .contains("Requires senior engineer review of auth flow"),
        "escalation description must contain the dossier hold_description, got: {}",
        hold_task.description
    );
    assert!(
        hold_task.description.contains("Three attempts failed"),
        "escalation description must contain the dossier failure_analysis, got: {}",
        hold_task.description
    );
    assert!(
        hold_task.description.contains("Arbiter park decision"),
        "escalation description must indicate arbiter park, got: {}",
        hold_task.description
    );

    // Verify: the arbiter_decision activity carries the autonomous_escalation
    // audit flag (park no longer parks on a human).
    let decision_activity = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("arbiter_decision".to_string()),
            ..ActivityQuery::default()
        })
        .await
        .expect("query arbiter_decision activity");
    assert!(
        !decision_activity.is_empty(),
        "arbiter_decision activity must be emitted"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&decision_activity[0].payload).expect("decision payload parses");
    assert_eq!(
        payload["decision"], "park",
        "decision must be park, got: {payload}"
    );
    assert_eq!(
        payload["autonomous_escalation"], true,
        "decision payload must set autonomous_escalation=true, got: {payload}"
    );
}

/// Verify the fail-closed path: when no unconsumed arbitration row exists,
/// `DirectServices::transition_task("arbiter_park")` creates a recovery row
/// and still produces a HumanReview hold with the dossier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_park_fail_closed_no_arbitration_row() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Move to in_lead_intervention — but do NOT create an arbitration row.
    transition_to_in_lead_intervention(&db, &task.id).await;

    let dossier = serde_json::json!({
        "hold_description": "No arbitration row — fail-closed recovery",
        "failure_analysis": "Arbitration row missing or malformed",
        "attempted_decisions": [],
        "recommended_action": "Escalate to human"
    });
    let dossier_json = serde_json::to_string(&dossier).unwrap();

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // Must succeed even without an unconsumed arbitration row (fail-closed).
    services
        .transition_task(task.id.clone(), "arbiter_park".into(), Some(dossier_json))
        .await
        .expect("transition_task arbiter_park must succeed (fail-closed)");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo
        .get(&task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(
        task.status, "open",
        "fail-closed park must still transition to open"
    );

    // Verify: a consumed recovery arbitration row was created.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let (cycle, record) = arb_repo
        .resolve_current_hold_cycle(&task.id)
        .await
        .expect("resolve hold cycle");
    // After fail-closed recovery, the row at hold_cycle=0 should be consumed,
    // so the next hold_cycle is 1 with no unconsumed record.
    assert_eq!(cycle, 1, "next hold cycle should be 1 after recovery row");
    assert!(
        record.is_none(),
        "no unconsumed row should remain after recovery consumption"
    );
    let recovery = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("get recovery row")
        .expect("recovery row must exist");
    assert_eq!(
        recovery.arbitration_state(),
        Some(ArbitrationState::Consumed)
    );
    let stored = recovery.dossier.expect("recovery dossier must be set");
    assert_eq!(
        stored["hold_description"], "No arbitration row — fail-closed recovery",
        "recovery dossier must contain the provided dossier"
    );

    // Verify: HumanReview hold is created even in the fail-closed path.
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(
        !blockers.is_empty(),
        "fail-closed path must still create HumanReview hold"
    );
    let hold_task = repo
        .get(&blockers[0].task_id)
        .await
        .expect("get hold task")
        .expect("hold task exists");
    assert_eq!(hold_task.issue_type, "review");
    assert_eq!(hold_task.status, "open");
    assert!(
        hold_task
            .description
            .contains("No arbitration row — fail-closed recovery"),
        "hold description must contain the fail-closed dossier, got: {}",
        hold_task.description
    );
    assert!(
        hold_task.description.contains("Arbiter park decision"),
        "hold description must indicate arbiter park, got: {}",
        hold_task.description
    );
}

/// Verify the fail-closed path when the latest arbitration row is already
/// consumed (i.e. a stale consumed row exists but no unconsumed one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_park_fail_closed_stale_consumed_row() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Move to in_lead_intervention.
    transition_to_in_lead_intervention(&db, &task.id).await;

    // Create a consumed arbitration row (simulating a prior cycle that was
    // already consumed).
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let ci = serde_json::json!([]);
    let ex = serde_json::json!([]);
    let prior_dossier = serde_json::json!({"hold_description": "prior cycle"});
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &ci,
            dossier: Some(&prior_dossier),
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row");
    // Mark it consumed immediately.
    let consumed = arb_repo
        .mark_consumed(&task.id, 0)
        .await
        .expect("mark consumed");
    assert!(consumed, "must successfully mark consumed");

    // Verify no unconsumed row.
    let (_, unconsumed) = arb_repo
        .resolve_current_hold_cycle(&task.id)
        .await
        .expect("resolve");
    assert!(unconsumed.is_none(), "must have no unconsumed row");

    let dossier = serde_json::json!({
        "hold_description": "Stale consumed row — recovery dossier",
        "failure_analysis": "Previous cycle already consumed",
        "attempted_decisions": ["reopen"],
        "recommended_action": "Human review"
    });
    let dossier_json = serde_json::to_string(&dossier).unwrap();

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // Must succeed with fail-closed recovery at hold_cycle=1.
    services
        .transition_task(task.id.clone(), "arbiter_park".into(), Some(dossier_json))
        .await
        .expect("transition_task arbiter_park must succeed (stale consumed row)");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo
        .get(&task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(task.status, "open");

    // Verify: recovery row at hold_cycle=1 was created and consumed.
    let recovery = arb_repo
        .get_by_task_and_cycle(&task.id, 1)
        .await
        .expect("get recovery row")
        .expect("recovery row at cycle 1 must exist");
    assert_eq!(
        recovery.arbitration_state(),
        Some(ArbitrationState::Consumed)
    );
    let stored = recovery.dossier.expect("recovery dossier must be set");
    assert_eq!(
        stored["hold_description"], "Stale consumed row — recovery dossier",
        "recovery dossier must contain the new dossier"
    );

    // Verify: HumanReview hold exists with the new dossier.
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(!blockers.is_empty(), "must create HumanReview hold");
    let hold_task = repo
        .get(&blockers[0].task_id)
        .await
        .expect("get hold task")
        .expect("hold task exists");
    assert!(
        hold_task
            .description
            .contains("Stale consumed row — recovery dossier"),
        "hold description must contain the recovery dossier, got: {}",
        hold_task.description
    );
}

/// Verify that a malformed (non-JSON) dossier still succeeds through the
/// fail-closed path: the transaction creates a HumanReview hold with a
/// fallback empty-dossier representation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_park_malformed_dossier_still_creates_hold() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    transition_to_in_lead_intervention(&db, &task.id).await;

    // No arbitration row + malformed dossier JSON.
    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    services
        .transition_task(
            task.id.clone(),
            "arbiter_park".into(),
            Some("not valid json {".into()),
        )
        .await
        .expect("malformed dossier must not fail the transaction");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo
        .get(&task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(task.status, "open");

    // Verify: HumanReview hold is created (with fallback empty dossier).
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(
        !blockers.is_empty(),
        "malformed dossier must still create HumanReview hold"
    );
    let hold_task = repo
        .get(&blockers[0].task_id)
        .await
        .expect("get hold task")
        .expect("hold task exists");
    assert_eq!(hold_task.issue_type, "review");
    assert!(
        hold_task.description.contains("Arbiter park decision"),
        "hold description must indicate arbiter park even with malformed dossier, got: {}",
        hold_task.description
    );
}

/// Verify the fail-closed path when the arbitration row exists but has an
/// invalid/unrecognised `state` column value (one that
/// `ArbitrationState::parse_state` returns `None` for).  This simulates a
/// corrupt or otherwise unusable durable arbitration record — distinct from
/// the "no row" or "stale consumed" cases.  The park transaction must still
/// succeed by treating the row as non-unconsumed and falling into the
/// recovery path, creating a HumanReview hold with the dossier content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_park_fail_closed_corrupt_arbitration_state() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Move to in_lead_intervention.
    transition_to_in_lead_intervention(&db, &task.id).await;

    // Create a valid unconsumed arbitration row, then corrupt its state
    // column to an unrecognised value via the db repository boundary.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let ci = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &ci,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row");

    // Corrupt the state to an unrecognised value.  ArbitrationState only
    // recognises "unconsumed", "consumed", and "failed"; anything else
    // makes arbitration_state() return None and the row unusable.
    assert!(
        arb_repo
            .force_state_for_testing(&task.id, 0, "corrupt_invalid_state")
            .await
            .expect("corrupt arbitration state"),
        "corrupting arbitration state must update the fixture row"
    );

    // Verify the row is present but its state parses as None.
    let corrupted = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("get corrupted row")
        .expect("corrupted row must exist");
    assert_eq!(
        corrupted.arbitration_state(),
        None,
        "corrupted state must parse as None"
    );

    // resolve_current_hold_cycle should treat this as non-unconsumed and
    // return (1, None) — same as consumed/failed.
    let (cycle, unconsumed) = arb_repo
        .resolve_current_hold_cycle(&task.id)
        .await
        .expect("resolve");
    assert_eq!(cycle, 1, "corrupt state must advance hold_cycle");
    assert!(
        unconsumed.is_none(),
        "corrupt state row must not be returned as unconsumed"
    );

    // Build dossier and call the actual DirectServices path.
    let dossier = serde_json::json!({
        "hold_description": "Corrupt arbitration state — fail-closed recovery",
        "failure_analysis": "Arbitration row has unrecognised state column value",
        "attempted_decisions": [],
        "recommended_action": "Escalate to human — arbitration data integrity issue"
    });
    let dossier_json = serde_json::to_string(&dossier).unwrap();

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // Must succeed via fail-closed recovery even though a (corrupt) row exists.
    services
        .transition_task(task.id.clone(), "arbiter_park".into(), Some(dossier_json))
        .await
        .expect("transition_task arbiter_park must succeed (corrupt arbitration state)");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo
        .get(&task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(
        task.status, "open",
        "corrupt-state park must still transition to open"
    );

    // Verify: a recovery arbitration row was created at hold_cycle=1 and
    // consumed.
    let recovery = arb_repo
        .get_by_task_and_cycle(&task.id, 1)
        .await
        .expect("get recovery row")
        .expect("recovery row at cycle 1 must exist");
    assert_eq!(
        recovery.arbitration_state(),
        Some(ArbitrationState::Consumed),
        "recovery row must be consumed"
    );
    let stored = recovery.dossier.expect("recovery dossier must be set");
    assert_eq!(
        stored["hold_description"], "Corrupt arbitration state — fail-closed recovery",
        "recovery dossier must contain the provided dossier"
    );

    // Verify: HumanReview hold is created with the structured dossier.
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(
        !blockers.is_empty(),
        "corrupt-state fail-closed path must create HumanReview hold"
    );
    let hold_task = repo
        .get(&blockers[0].task_id)
        .await
        .expect("get hold task")
        .expect("hold task exists");
    assert_eq!(hold_task.issue_type, "review", "hold must be a review task");
    assert_eq!(hold_task.status, "open", "hold must be open");
    assert!(
        hold_task
            .description
            .contains("Corrupt arbitration state — fail-closed recovery"),
        "hold description must contain the dossier hold_description, got: {}",
        hold_task.description
    );
    assert!(
        hold_task
            .description
            .contains("unrecognised state column value"),
        "hold description must contain the dossier failure_analysis, got: {}",
        hold_task.description
    );
    assert!(
        hold_task.description.contains("Arbiter park decision"),
        "hold description must indicate arbiter park, got: {}",
        hold_task.description
    );
}

// ── Git-evidence payload regression tests ────────────────────────────────

/// Verify that `record_arbiter_decision` emits an `arbiter_decision` activity
/// whose payload includes the git-evidence fields from the arbitration row
/// when those fields are populated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_decision_payload_includes_git_evidence_when_populated() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Create an unconsumed arbitration row WITH git-evidence fields.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let failing_jobs = serde_json::json!([12345, 67890]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: Some("mirror-sha-abc123"),
            github_head_sha: Some("github-sha-def456"),
            pr_url: Some("https://github.com/test/repo/pull/42"),
            failing_ci_job_ids: &failing_jobs,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row with evidence");

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // Call record_arbiter_decision with an approve decision.
    services
        .record_arbiter_decision(
            task.id.clone(),
            "approve".into(),
            r#"{"summary": "looks good"}"#.into(),
        )
        .await
        .expect("record_arbiter_decision must succeed");

    // Read back the arbiter_decision activity event.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("arbiter_decision".to_string()),
            ..Default::default()
        })
        .await
        .expect("query activity");
    assert_eq!(
        entries.len(),
        1,
        "must have exactly one arbiter_decision event"
    );

    let payload: serde_json::Value =
        serde_json::from_str(&entries[0].payload).expect("parse payload JSON");

    // Assert git-evidence fields are present and match.
    assert_eq!(
        payload["mirror_head_sha"].as_str(),
        Some("mirror-sha-abc123"),
        "mirror_head_sha must match arbitration row"
    );
    assert_eq!(
        payload["github_head_sha"].as_str(),
        Some("github-sha-def456"),
        "github_head_sha must match arbitration row"
    );
    assert_eq!(
        payload["pr_url"].as_str(),
        Some("https://github.com/test/repo/pull/42"),
        "pr_url must match arbitration row"
    );
    assert_eq!(
        payload["failing_ci_job_ids"],
        serde_json::json!([12345, 67890]),
        "failing_ci_job_ids must match arbitration row"
    );
    assert_eq!(
        payload["decision"].as_str(),
        Some("approve"),
        "decision must be approve"
    );
}

/// Verify that `record_arbiter_decision` emits an `arbiter_decision` activity
/// whose git-evidence fields are null / empty when the arbitration row has
/// no evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_decision_payload_has_empty_evidence_when_absent() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Create an unconsumed arbitration row WITHOUT git-evidence fields.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let empty_jobs = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty_jobs,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row without evidence");

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    services
        .record_arbiter_decision(
            task.id.clone(),
            "approve_conflict".into(),
            r#"{"summary": "conflict resolved"}"#.into(),
        )
        .await
        .expect("record_arbiter_decision must succeed");

    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("arbiter_decision".to_string()),
            ..Default::default()
        })
        .await
        .expect("query activity");
    assert_eq!(entries.len(), 1);

    let payload: serde_json::Value =
        serde_json::from_str(&entries[0].payload).expect("parse payload JSON");

    // Git-evidence fields must be null when absent.
    assert!(
        payload["mirror_head_sha"].is_null(),
        "mirror_head_sha must be null when absent, got: {}",
        payload["mirror_head_sha"]
    );
    assert!(
        payload["github_head_sha"].is_null(),
        "github_head_sha must be null when absent, got: {}",
        payload["github_head_sha"]
    );
    assert!(
        payload["pr_url"].is_null(),
        "pr_url must be null when absent, got: {}",
        payload["pr_url"]
    );
    // failing_ci_job_ids should be an empty array when absent.
    assert_eq!(
        payload["failing_ci_job_ids"],
        serde_json::json!([]),
        "failing_ci_job_ids must be empty array when absent"
    );
    assert_eq!(payload["decision"].as_str(), Some("approve_conflict"));
}

/// Verify that the decision-failure cap park dossier and its associated
/// `arbiter_decision` activity event include explicit git-evidence fields
/// when the arbitration row was created with evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_failure_cap_dossier_includes_git_evidence_when_populated() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Create an unconsumed arbitration row WITH git-evidence fields.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let failing_jobs = serde_json::json!([99887, 11223]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: Some("mirror-evidence-sha"),
            github_head_sha: Some("github-evidence-sha"),
            pr_url: Some("https://github.com/org/repo/pull/99"),
            failing_ci_job_ids: &failing_jobs,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row with evidence");

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // First termination: increments decision_failure_count to 1 (below cap).
    let capped = services
        .record_arbiter_session_termination(task.id.clone(), false)
        .await
        .expect("first termination");
    assert!(!capped, "first termination must not reach cap");

    // Second termination: hits cap (decision_failure_count = 2 >= CAP).
    let capped = services
        .record_arbiter_session_termination(task.id.clone(), false)
        .await
        .expect("second termination");
    assert!(capped, "second termination must reach decision-failure cap");

    // Read back the dossier from the arbitration row (now in failed state).
    let record = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("get arbitration")
        .expect("arbitration row must exist");
    let dossier = record.dossier.expect("dossier must be set after cap");

    // Assert dossier contains git-evidence fields.
    assert_eq!(
        dossier["mirror_head_sha"].as_str(),
        Some("mirror-evidence-sha"),
        "dossier mirror_head_sha must match"
    );
    assert_eq!(
        dossier["github_head_sha"].as_str(),
        Some("github-evidence-sha"),
        "dossier github_head_sha must match"
    );
    assert_eq!(
        dossier["pr_url"].as_str(),
        Some("https://github.com/org/repo/pull/99"),
        "dossier pr_url must match"
    );
    assert_eq!(
        dossier["failing_ci_job_ids"],
        serde_json::json!([99887, 11223]),
        "dossier failing_ci_job_ids must match"
    );
    assert_eq!(
        dossier["kind"].as_str(),
        Some("arbiter_decision_failure_cap"),
    );

    // Read the arbiter_decision activity event emitted for the cap.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("arbiter_decision".to_string()),
            ..Default::default()
        })
        .await
        .expect("query activity");
    assert!(
        !entries.is_empty(),
        "must have at least one arbiter_decision event for the cap"
    );
    let cap_event = entries
        .iter()
        .find_map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.payload).ok()?;
            if v["reason"].as_str() == Some("decision_failure_cap") {
                Some(v)
            } else {
                None
            }
        })
        .expect("must find decision_failure_cap arbiter_decision event");

    assert_eq!(
        cap_event["mirror_head_sha"].as_str(),
        Some("mirror-evidence-sha"),
        "activity mirror_head_sha must match"
    );
    assert_eq!(
        cap_event["github_head_sha"].as_str(),
        Some("github-evidence-sha"),
        "activity github_head_sha must match"
    );
    assert_eq!(
        cap_event["pr_url"].as_str(),
        Some("https://github.com/org/repo/pull/99"),
        "activity pr_url must match"
    );
    assert_eq!(
        cap_event["failing_ci_job_ids"],
        serde_json::json!([99887, 11223]),
        "activity failing_ci_job_ids must match"
    );
}

/// Verify that the decision-failure cap park dossier and activity event
/// have null / empty git-evidence fields when the arbitration row had
/// no evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_failure_cap_dossier_has_empty_evidence_when_absent() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    // Create an unconsumed arbitration row WITHOUT git-evidence fields.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let empty_jobs = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty_jobs,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row without evidence");

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // Two non-infra terminations to reach the cap.
    let _ = services
        .record_arbiter_session_termination(task.id.clone(), false)
        .await
        .expect("first termination");
    let capped = services
        .record_arbiter_session_termination(task.id.clone(), false)
        .await
        .expect("second termination");
    assert!(capped, "must reach cap");

    // Read back the dossier.
    let record = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("get arbitration")
        .expect("arbitration row must exist");
    let dossier = record.dossier.expect("dossier must be set");

    // Git-evidence fields in the dossier must be null.
    assert!(
        dossier["mirror_head_sha"].is_null(),
        "dossier mirror_head_sha must be null when absent, got: {}",
        dossier["mirror_head_sha"]
    );
    assert!(
        dossier["github_head_sha"].is_null(),
        "dossier github_head_sha must be null when absent, got: {}",
        dossier["github_head_sha"]
    );
    assert!(
        dossier["pr_url"].is_null(),
        "dossier pr_url must be null when absent, got: {}",
        dossier["pr_url"]
    );
    assert_eq!(
        dossier["failing_ci_job_ids"],
        serde_json::json!([]),
        "dossier failing_ci_job_ids must be empty array when absent"
    );

    // Read the activity event.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("arbiter_decision".to_string()),
            ..Default::default()
        })
        .await
        .expect("query activity");
    let cap_event = entries
        .iter()
        .find_map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.payload).ok()?;
            if v["reason"].as_str() == Some("decision_failure_cap") {
                Some(v)
            } else {
                None
            }
        })
        .expect("must find decision_failure_cap event");

    assert!(
        cap_event["mirror_head_sha"].is_null(),
        "activity mirror_head_sha must be null when absent"
    );
    assert!(
        cap_event["github_head_sha"].is_null(),
        "activity github_head_sha must be null when absent"
    );
    assert!(
        cap_event["pr_url"].is_null(),
        "activity pr_url must be null when absent"
    );
    assert_eq!(
        cap_event["failing_ci_job_ids"],
        serde_json::json!([]),
        "activity failing_ci_job_ids must be empty array when absent"
    );
}

/// Verify that the arbiter park transaction persists git-evidence from the
/// arbitration row into the dispatch ledger on the update path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_park_transaction_persists_git_evidence_on_ledger() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    transition_to_in_lead_intervention(&db, &task.id).await;

    // Create an unconsumed arbitration row WITH git-evidence fields.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let failing_jobs = serde_json::json!([55555]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: Some("park-mirror-sha"),
            github_head_sha: Some("park-github-sha"),
            pr_url: Some("https://github.com/test/repo/pull/7"),
            failing_ci_job_ids: &failing_jobs,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row with evidence");

    let dossier = serde_json::json!({
        "hold_description": "Park with evidence test",
        "failure_analysis": "test",
    });
    let dossier_json = serde_json::to_string(&dossier).unwrap();

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    services
        .transition_task(task.id.clone(), "arbiter_park".into(), Some(dossier_json))
        .await
        .expect("transition_task arbiter_park must succeed");

    // Read back the arbitration row — it should be consumed with evidence.
    let record = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("get arbitration")
        .expect("arbitration row must exist");
    assert_eq!(
        record.arbitration_state(),
        Some(ArbitrationState::Consumed),
        "must be consumed"
    );
    assert_eq!(
        record.mirror_head_sha.as_deref(),
        Some("park-mirror-sha"),
        "ledger must retain mirror_head_sha"
    );
    assert_eq!(
        record.github_head_sha.as_deref(),
        Some("park-github-sha"),
        "ledger must retain github_head_sha"
    );
    assert_eq!(
        record.pr_url.as_deref(),
        Some("https://github.com/test/repo/pull/7"),
        "ledger must retain pr_url"
    );
    assert_eq!(
        record.failing_ci_job_ids,
        serde_json::json!([55555]),
        "ledger must retain failing_ci_job_ids"
    );

    // Also verify the arbiter_decision activity has the evidence.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("arbiter_decision".to_string()),
            ..Default::default()
        })
        .await
        .expect("query activity");
    assert!(
        !entries.is_empty(),
        "must have arbiter_decision event from park transaction"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&entries[0].payload).expect("parse payload");
    assert_eq!(payload["mirror_head_sha"].as_str(), Some("park-mirror-sha"),);
    assert_eq!(payload["github_head_sha"].as_str(), Some("park-github-sha"),);
    assert_eq!(
        payload["pr_url"].as_str(),
        Some("https://github.com/test/repo/pull/7"),
    );
    assert_eq!(payload["failing_ci_job_ids"], serde_json::json!([55555]),);
}

/// Closing the planner escalation (as the Planner would after resolving)
/// releases the parked source: its blocker resolves and
/// `human_review_resolved_at` is stamped — exactly like the old human-hold
/// close, but driven by an autonomous planner rather than a human.
///
/// The coordinator-side tripwire release on close (`tripwire.hold.released`)
/// is covered separately in the coordinator crate; here we assert the
/// DB-level source-release semantics the `planner-park-escalation` label now
/// triggers via the broadened close path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn planner_escalation_close_releases_source() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let task = create_task(&db, &project_id, &epic_id).await;

    transition_to_in_lead_intervention(&db, &task.id).await;

    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let ci = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &ci,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row");

    let dossier = serde_json::json!({
        "hold_description": "Escalate to planner for restructuring",
        "failure_analysis": "Repeated review rejections on one criterion",
        "attempted_decisions": ["reopen"],
        "recommended_action": "Decompose the failing criterion"
    });
    let dossier_json = serde_json::to_string(&dossier).unwrap();

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());
    services
        .transition_task(task.id.clone(), "arbiter_park".into(), Some(dossier_json))
        .await
        .expect("transition_task arbiter_park must succeed");

    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);

    // Source is blocked by the escalation; resolved marker not yet set.
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert_eq!(blockers.len(), 1, "source must have exactly one blocker");
    let escalation_id = blockers[0].task_id.clone();
    assert!(
        repo.human_review_resolved_at(&task.id)
            .await
            .expect("resolved marker")
            .is_none(),
        "human_review_resolved_at must be None before the escalation closes"
    );

    // The planner resolves and closes the escalation. `transition(Close)` on a
    // task carrying `planner-park-escalation` runs the broadened source-release
    // path (stamp + unblock) — `set_status` would bypass it.
    repo.transition(
        &escalation_id,
        TransitionAction::Close,
        "planner",
        "planner",
        Some("decomposed source into replacement subtasks"),
        None,
    )
    .await
    .expect("planner closes the escalation");

    // Source is unblocked and the resolved marker is stamped.
    assert!(
        repo.human_review_resolved_at(&task.id)
            .await
            .expect("resolved marker")
            .is_some(),
        "human_review_resolved_at must be stamped after the planner closes the escalation"
    );
    let blockers_after = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(
        blockers_after.iter().all(|b| b.status == "closed"),
        "source must be unblocked after the escalation closes, got: {blockers_after:?}"
    );
}

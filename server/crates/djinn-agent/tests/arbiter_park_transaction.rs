//! Integration tests for the arbiter park transaction via
//! `DirectServices::transition_task("arbiter_park")`.
//!
//! These tests exercise the actual code path that the supervisor calls when
//! `StageOutcome::LeadParked { park_dossier_json }` is handled, rather than
//! manually simulating the individual repository calls.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use djinn_agent::context::AgentContext;
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::supervisor::{SupervisorServices, services_for_agent_context};
use djinn_core::events::EventBus;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::repositories::task_arbitration::{
    ArbitrationState, CreateArbitrationParams, TaskArbitrationRepository,
};
use djinn_db::{Database, EpicCreateInput, EpicRepository, ProjectRepository, TaskRepository};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── Helpers (mirrors phase1_supervisor.rs inline helpers) ────────────────

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
        reconciliation_sweep: djinn_agent::context::ReconciliationSweepConfig::default(),
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
    TaskRepository::new(db.clone(), events)
        .create_in_project(
            project_id,
            Some(epic_id),
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
    let task = repo.get(task_id).await.expect("get task").expect("task exists");
    assert_eq!(task.status, "in_lead_intervention");
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Verify a valid arbiter park decision exercises the full
/// `DirectServices::transition_task("arbiter_park")` path: persists the
/// decision payload and structured dossier on the arbitration row, marks it
/// consumed, creates a HumanReview remediation hold whose description
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
    let services: Arc<dyn SupervisorServices> = services_for_agent_context(ctx, CancellationToken::new());

    // This is the actual production path: transition_task("arbiter_park") calls
    // execute_arbiter_park_transaction internally, then applies the state
    // transition.
    services
        .transition_task(
            task.id.clone(),
            "arbiter_park".into(),
            Some(dossier_json),
        )
        .await
        .expect("transition_task arbiter_park must succeed");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo.get(&task.id).await.expect("get task").expect("task exists");
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
        stored["failure_analysis"],
        "Three attempts failed; auth logic needs domain expertise",
        "dossier failure_analysis must match"
    );

    // Verify: HumanReview hold blocks source with dossier content in description.
    let blockers = repo.list_blockers(&task.id).await.expect("list blockers");
    assert!(
        !blockers.is_empty(),
        "source task must be blocked by HumanReview hold"
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
            .contains("Requires senior engineer review of auth flow"),
        "hold description must contain the dossier hold_description, got: {}",
        hold_task.description
    );
    assert!(
        hold_task.description.contains("Three attempts failed"),
        "hold description must contain the dossier failure_analysis, got: {}",
        hold_task.description
    );
    assert!(
        hold_task.description.contains("Arbiter park decision"),
        "hold description must indicate arbiter park, got: {}",
        hold_task.description
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
    let services: Arc<dyn SupervisorServices> = services_for_agent_context(ctx, CancellationToken::new());

    // Must succeed even without an unconsumed arbitration row (fail-closed).
    services
        .transition_task(
            task.id.clone(),
            "arbiter_park".into(),
            Some(dossier_json),
        )
        .await
        .expect("transition_task arbiter_park must succeed (fail-closed)");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo.get(&task.id).await.expect("get task").expect("task exists");
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
    assert_eq!(recovery.arbitration_state(), Some(ArbitrationState::Consumed));
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
    let services: Arc<dyn SupervisorServices> = services_for_agent_context(ctx, CancellationToken::new());

    // Must succeed with fail-closed recovery at hold_cycle=1.
    services
        .transition_task(
            task.id.clone(),
            "arbiter_park".into(),
            Some(dossier_json),
        )
        .await
        .expect("transition_task arbiter_park must succeed (stale consumed row)");

    // Verify: task landed at open.
    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);
    let task = repo.get(&task.id).await.expect("get task").expect("task exists");
    assert_eq!(task.status, "open");

    // Verify: recovery row at hold_cycle=1 was created and consumed.
    let recovery = arb_repo
        .get_by_task_and_cycle(&task.id, 1)
        .await
        .expect("get recovery row")
        .expect("recovery row at cycle 1 must exist");
    assert_eq!(recovery.arbitration_state(), Some(ArbitrationState::Consumed));
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
    let services: Arc<dyn SupervisorServices> = services_for_agent_context(ctx, CancellationToken::new());

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
    let task = repo.get(&task.id).await.expect("get task").expect("task exists");
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

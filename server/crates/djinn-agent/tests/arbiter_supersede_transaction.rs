//! Integration tests for the arbiter supersede transaction via
//! `DirectServices::transition_task("arbiter_supersede")`.
//!
//! These tests exercise the actual code path the supervisor drives when
//! `StageOutcome::LeadSuperseded { reason, replacement_task_ids }` is handled:
//! the source task is force-closed as superseded, the arbitration row is
//! consumed with the supersede decision, downstream blockers are transferred to
//! the last replacement subtask, and — unlike the park path — NO human-review
//! hold is created.

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

// ── Helpers (mirror arbiter_park_transaction.rs) ─────────────────────────

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
        memory_intent_planner: djinn_agent::context::MemoryIntentPlannerConfig::default(),
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

async fn create_task(db: &Database, project_id: &str, epic_id: &str, title: &str) -> Task {
    let events = EventBus::noop();
    TaskRepository::new(db.clone(), events)
        .create_in_project(
            project_id,
            Some(epic_id),
            title,
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

fn supersede_payload(reason: &str, replacement_ids: &[&str]) -> String {
    serde_json::json!({
        "reason": reason,
        "replacement_task_ids": replacement_ids,
    })
    .to_string()
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Verify a valid arbiter supersede decision exercises the full
/// `DirectServices::transition_task("arbiter_supersede")` path: force-closes
/// the source, consumes the arbitration row with the supersede decision,
/// transfers a downstream blocker onto the last replacement, and creates NO
/// human-review hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_supersede_full_transaction() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let source = create_task(&db, &project_id, &epic_id, "Superseded source").await;

    // Two replacement subtasks + one downstream task blocked by the source.
    let repl_a = create_task(&db, &project_id, &epic_id, "Replacement A").await;
    let repl_b = create_task(&db, &project_id, &epic_id, "Replacement B").await;
    let downstream = create_task(&db, &project_id, &epic_id, "Downstream consumer").await;

    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events.clone());
    repo.add_blocker(&downstream.id, &source.id)
        .await
        .expect("block downstream on source");

    // Move the source to in_lead_intervention with an unconsumed arbitration row.
    transition_to_in_lead_intervention(&db, &source.id).await;
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let ci = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &source.id,
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

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    // Production path: transition_task("arbiter_supersede") runs the supersede
    // transaction then applies the force-close.
    let payload = supersede_payload(
        "decomposed into replacements",
        &[repl_a.short_id.as_str(), repl_b.short_id.as_str()],
    );
    services
        .transition_task(source.id.clone(), "arbiter_supersede".into(), Some(payload))
        .await
        .expect("transition_task arbiter_supersede must succeed");

    // Source is force-closed.
    let closed = repo
        .get(&source.id)
        .await
        .expect("get source")
        .expect("source exists");
    assert_eq!(
        closed.status, "closed",
        "arbiter_supersede must force-close the source, got: {}",
        closed.status
    );
    assert_eq!(
        closed.close_reason.as_deref(),
        Some("force_closed"),
        "supersede must close with force_closed reason"
    );

    // Arbitration row consumed with the supersede decision persisted.
    let record = arb_repo
        .get_by_task_and_cycle(&source.id, 0)
        .await
        .expect("get arbitration")
        .expect("arbitration row exists");
    assert_eq!(
        record.arbitration_state(),
        Some(ArbitrationState::Consumed),
        "arbitration row must be consumed"
    );
    let directive = record.directive.expect("decision must be persisted");
    assert_eq!(
        directive["decision"], "supersede",
        "arbitration decision must be supersede"
    );

    // Downstream blocker transferred onto the LAST replacement (repl_b).
    let downstream_blockers = repo
        .list_blockers(&downstream.id)
        .await
        .expect("list downstream blockers");
    assert!(
        downstream_blockers.iter().any(|b| b.task_id == repl_b.id),
        "downstream must be blocked by the last replacement (repl_b), got: {downstream_blockers:?}"
    );

    // NO human-review hold was created: no review-type / human-review-hold task
    // exists in the project.
    let all = repo
        .list_by_project(&project_id)
        .await
        .expect("list project tasks");
    assert!(
        !all.iter()
            .any(|t| t.issue_type == "review" || t.labels.contains("human-review-hold")),
        "supersede must NOT create a human-review hold task, got: {:?}",
        all.iter()
            .map(|t| (t.short_id.clone(), t.issue_type.clone(), t.labels.clone()))
            .collect::<Vec<_>>()
    );
}

/// Verify the fail-closed path: when no unconsumed arbitration row exists, the
/// supersede transaction still force-closes the source and creates a consumed
/// recovery row — and still no human-review hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_services_arbiter_supersede_fail_closed_no_arbitration_row() {
    let db = Database::open_in_memory().expect("open in-memory db");
    let (project_id, epic_id) = create_project_and_epic(&db).await;
    let source = create_task(&db, &project_id, &epic_id, "Superseded source").await;
    let repl = create_task(&db, &project_id, &epic_id, "Replacement").await;

    // Move to in_lead_intervention — but do NOT create an arbitration row.
    transition_to_in_lead_intervention(&db, &source.id).await;

    let ctx = test_agent_context(db.clone());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    let payload = supersede_payload("decomposed", &[repl.short_id.as_str()]);
    services
        .transition_task(source.id.clone(), "arbiter_supersede".into(), Some(payload))
        .await
        .expect("transition_task arbiter_supersede must succeed (fail-closed)");

    let events = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), events);

    // Source force-closed.
    let closed = repo
        .get(&source.id)
        .await
        .expect("get source")
        .expect("source exists");
    assert_eq!(
        closed.status, "closed",
        "fail-closed supersede must still force-close the source"
    );

    // A consumed recovery arbitration row was created at hold_cycle=0.
    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let recovery = arb_repo
        .get_by_task_and_cycle(&source.id, 0)
        .await
        .expect("get recovery row")
        .expect("recovery row must exist");
    assert_eq!(
        recovery.arbitration_state(),
        Some(ArbitrationState::Consumed),
        "recovery row must be consumed"
    );
    let directive = recovery.directive.expect("decision must be persisted");
    assert_eq!(directive["decision"], "supersede");

    // Still no human-review hold.
    let all = repo
        .list_by_project(&project_id)
        .await
        .expect("list project tasks");
    assert!(
        !all.iter()
            .any(|t| t.issue_type == "review" || t.labels.contains("human-review-hold")),
        "fail-closed supersede must NOT create a human-review hold"
    );
}

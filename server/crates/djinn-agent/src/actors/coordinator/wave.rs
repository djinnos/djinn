// Wave-based Planner planning (task `watx`).
//
// When an epic is created, the coordinator creates a single `planning`
// task so the Planner can plan the first wave of work.  When all
// non-planning worker tasks under the epic are closed (and the epic
// itself is still open), a new planning task is created for the next
// wave.
//
// Rules
// ─────
// • Only one open/in-progress planning task per epic at a time.
// • Planning tasks are never counted as "worker tasks" for batch-
//   completion purposes (`issue_type == "planning"` is excluded).
// • Non-worker issue types: planning, spike, review.  All other tasks
//   (task, research, …) are worker tasks.

use super::reentrance::{DispatchEvent, should_auto_dispatch_planner};
use super::*;
use djinn_core::models::task::PRIORITY_CRITICAL;

impl CoordinatorActor {
    /// Called when an epic is created.  Creates the first planning task
    /// unless one already exists for the epic (idempotent).
    pub(super) async fn maybe_create_planning_task(&mut self, epic: &djinn_core::models::Epic) {
        // Only create planning tasks for epics that are fully open.
        // Drafting epics are still being refined by the user; closed epics
        // are done.  ADR-051 Epic C adds `proposed` for architect-drafted
        // epic shells that must NEVER trigger auto-dispatch until they are
        // explicitly accepted via `propose_adr_accept` (which promotes the
        // epic to `open`).  The promotion from drafting→open / proposed→open
        // is handled separately via the epic_updated event path.
        if epic.status != "open" {
            tracing::debug!(
                epic_id = %epic.short_id,
                status = %epic.status,
                "CoordinatorActor: skipping planning task — epic not open"
            );
            return;
        }
        let task_repo = self.task_repo();
        match task_repo.list_by_epic(&epic.id).await {
            Ok(tasks) => {
                let has_open_planning = tasks.iter().any(|t| {
                    matches!(t.issue_type.as_str(), "planning" | "decomposition")
                        && matches!(t.status.as_str(), "open" | "in_progress")
                });
                if has_open_planning {
                    tracing::debug!(
                        epic_id = %epic.short_id,
                        "CoordinatorActor: planning task already exists, skipping"
                    );
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(
                    epic_id = %epic.id,
                    error = %e,
                    "CoordinatorActor: failed to list tasks for planning task check"
                );
                return;
            }
        }

        // ADR-051 §7 — reentrance guard.  Epic C threads the real
        // `auto_breakdown` value from the epic row; when `false`, this
        // creation came from a Planner mid-decomposition (wave 2+) or from
        // `propose_adr_accept` which wants to create epic shells without
        // dispatching.
        if !should_auto_dispatch_planner(
            &self.db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: epic.auto_breakdown,
            },
        )
        .await
        {
            tracing::debug!(
                epic_id = %epic.short_id,
                "CoordinatorActor: epic-created auto-dispatch suppressed by reentrance guard"
            );
            return;
        }

        self.create_planning_task(epic).await;
    }

    /// Internal: create a planning task for an epic and trigger dispatch.
    async fn create_planning_task(&mut self, epic: &djinn_core::models::Epic) {
        let task_repo = self.task_repo();

        let title = format!("Plan next wave: {}", epic.title);
        let description = format!(
            "Planning task for epic '{}' ({}). \
             The Planner should:\n\
             1. Read the epic's memory_refs for context and prior roadmap notes.\n\
             2. Review any previous wave results (closed tasks, session reflections).\n\
             3. Decide: is the epic's goal fully met? If YES → call `epic_close({})`, \
             then `submit_grooming`. Do NOT create new tasks.\n\
             4. If NO → write or update the epic roadmap design note, \
             create 3–5 worker tasks (or a spike if uncertainty is high).\n\
             5. Call `submit_grooming` when done.",
            epic.title, epic.short_id, epic.short_id
        );
        let originating_adr_section = match epic.originating_adr_id.as_deref() {
            Some(adr) if !adr.is_empty() => format!(
                "\nOriginating ADR: `{adr}` — this epic was spawned from an \
                 accepted proposal. Read the ADR for architectural rationale, \
                 acceptance criteria, and the work shape it sketches before \
                 creating tasks. Use `memory_read(\"{adr}\")` or look under \
                 `.djinn/decisions/` for the full document."
            ),
            _ => String::new(),
        };
        let design = format!(
            "Epic: {} ({})\nEpic memory_refs: {}{}\n\n\
             Use `epic_show({})` to read full epic context and memory_refs.\n\
             Use `build_context` enriched with session reflections from \
             previously completed tasks under this epic.",
            epic.title, epic.short_id, epic.memory_refs, originating_adr_section, epic.short_id
        );

        let ac = serde_json::json!([
            {"criterion": "Epic state assessed: either closed via epic_close (if goal fully met) or roadmap updated with next-wave plan", "met": false},
            {"criterion": "If epic remains open: 3–5 worker tasks (or a spike) created with acceptance criteria", "met": false},
            {"criterion": "submit_grooming called to finalize the wave", "met": false},
        ]).to_string();

        match task_repo
            .create_with_ac(
                &epic.id,
                &title,
                &description,
                &design,
                "planning",
                PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                Some(&ac),
            )
            .await
        {
            Ok(task) => {
                tracing::info!(
                    epic_id = %epic.short_id,
                    task_id = %task.short_id,
                    "CoordinatorActor: created planning task for epic"
                );
                self.dispatch_ready_tasks(Some(&epic.project_id)).await;
            }
            Err(e) => {
                tracing::warn!(
                    epic_id = %epic.id,
                    error = %e,
                    "CoordinatorActor: failed to create planning task"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::actors::coordinator::{
        CoordinatorDeps, CoordinatorHandle, DEFAULT_MODEL_ID, VerificationTracker,
    };
    use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{Database, EpicRepository, TaskRepository};
    use djinn_provider::catalog::CatalogService;
    use djinn_provider::catalog::health::HealthTracker;

    fn spawn_coordinator_with_planner(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 4,
                    roles: ["worker", "reviewer", "planner", "architect"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let catalog = CatalogService::new();
        let health = HealthTracker::new();
        let verification_tracker = VerificationTracker::default();
        let role_registry = Arc::new(RoleRegistry::new());
        CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            catalog,
            health,
            role_registry,
            verification_tracker,
            crate::lsp::LspManager::new(),
        ))
    }

    async fn wait_for_decomp_tasks(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        epic_id: &str,
        min_count: usize,
    ) -> Vec<djinn_core::models::Task> {
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let tasks = task_repo.list_by_epic(epic_id).await.unwrap_or_default();
            let open_decomp: Vec<_> = tasks
                .into_iter()
                .filter(|t| {
                    matches!(t.issue_type.as_str(), "planning" | "decomposition")
                        && matches!(t.status.as_str(), "open" | "in_progress")
                })
                .collect();
            if open_decomp.len() >= min_count {
                return open_decomp;
            }
            if tokio::time::Instant::now() >= deadline {
                return open_decomp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_skips_when_no_open_epics() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let handle = spawn_coordinator_with_planner(&db, &tx);
        handle.trigger_planner_patrol().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let review_tasks = task_repo
            .list_ready(djinn_db::ReadyQuery {
                issue_type: Some("review".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(review_tasks.is_empty());
        assert_eq!(handle.get_status().unwrap().tasks_dispatched, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_creation_triggers_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Wave Test Epic",
                    description: "test",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_skips_when_planner_already_running() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Test Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        let session_repo =
            djinn_db::SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test/mock",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
            })
            .await
            .unwrap();

        let handle = spawn_coordinator_with_planner(&db, &tx);
        handle.trigger_planner_patrol().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(handle.get_status().unwrap().tasks_dispatched, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_creation_does_not_create_duplicate_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Dedup Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        let _ = tx.send(DjinnEventEnvelope::epic_created(&epic));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning_count = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(open_planning_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_epic_creation_does_not_trigger_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Drafting Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(planning_count, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposed_epic_creation_does_not_trigger_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Proposed Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("proposed"),
                    auto_breakdown: None,
                    originating_adr_id: Some("adr-999-test"),
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(planning_count, 0);
        assert_eq!(epic.status, "proposed");
        assert_eq!(epic.originating_adr_id.as_deref(), Some("adr-999-test"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_epic_with_auto_breakdown_false_skips_dispatch() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "No Auto Breakdown Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(planning_count, 0);
        assert!(!epic.auto_breakdown);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_to_open_promotion_triggers_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Promote Me Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let promoted = epic_repo.set_status_raw(&epic.id, "open").await.unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_to_open_promotion_does_not_duplicate_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "No Dup Promote Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let promoted = epic_repo.set_status_raw(&epic.id, "open").await.unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning_count = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(open_planning_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_creates_review_task_when_open_epic_exists() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Active Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_in_project(
                &project.id,
                Some(&epic.id),
                "Test task",
                "",
                "",
                "task",
                1,
                "",
                None,
                None,
            )
            .await
            .unwrap();

        let handle = spawn_coordinator_with_planner(&db, &tx);
        handle.trigger_planner_patrol().await.unwrap();

        // Poll (up to 5s) rather than sleep-then-check: the planner patrol
        // task is async and under concurrent test load the 200ms sleep
        // occasionally ran out before the review task landed in the DB.
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let tasks_by_project = task_repo.list_by_project(&project.id).await.unwrap();
            if tasks_by_project
                .iter()
                .any(|t| t.issue_type == "review" && t.title.contains("patrol"))
            {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "patrol review task did not land within 5s; current tasks: {:?}",
                    tasks_by_project
                        .iter()
                        .map(|t| (&t.issue_type, &t.title))
                        .collect::<Vec<_>>()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_triggers_next_wave_decomposition() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Batch Completion Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let initial_decomp = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(initial_decomp.len(), 1);
        let decomp_task = &initial_decomp[0];

        task_repo
            .set_status_with_reason(&decomp_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        let w1 = task_repo
            .create(
                &epic.id,
                "Worker Task 1",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
            )
            .await
            .unwrap();
        let w2 = task_repo
            .create(
                &epic.id,
                "Worker Task 2",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        task_repo
            .set_status_with_reason(&w1.id, "closed", Some("completed"))
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&w2.id, "closed", Some("completed"))
            .await
            .unwrap();

        let next_wave = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(next_wave.len(), 1);
    }
}

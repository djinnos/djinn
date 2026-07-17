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
        // Only create planning tasks for `open` epics (closed epics are done).
        // Epics are open → closed now; staging without auto-dispatch is done
        // via `auto_breakdown = false` (checked separately), and pre-execution
        // refinement lives in proposals.
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

                // Wave-1 guard.  This entry point fires on the generic
                // `epic.updated` event, which is re-emitted on EVERY epic
                // mutation — including the Planner re-linking the roadmap note
                // via `update_memory_refs` mid-grooming.  Without this guard,
                // each such update regenerates a "Plan next wave" planning task
                // as soon as the previous one has closed: a self-sustaining
                // respawn loop that starves the epic's own worker tasks (epic
                // `lywz`, 2026-06-02 — 7 planner generations in ~40min while
                // zero worker tasks ran).
                //
                // `maybe_create_planning_task` is for an epic's FIRST wave only
                // (epic created open). Once any worker task exists the epic has
                // already been
                // decomposed; subsequent waves are owned exclusively by the
                // batch-completion rule (`on_task_closed`, all workers closed).
                // So bail if any non-planning worker task already exists,
                // regardless of status.
                let has_worker_task = tasks.iter().any(|t| {
                    !matches!(
                        t.issue_type.as_str(),
                        "planning" | "decomposition" | "review"
                    )
                });
                if has_worker_task {
                    tracing::debug!(
                        epic_id = %epic.short_id,
                        "CoordinatorActor: epic already decomposed (worker tasks exist), \
                         skipping wave-1 planning task creation"
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
             4. If the epic is BLOCKED on another epic (a foundation/ownership \
             gate that is still open) and you can create no useful work yet, \
             create NO tasks and call \
             `submit_grooming(decision=\"escalate\", blocked_on=[<blocking epic short_id(s)>])`. \
             This parks the epic until the blocker closes — the coordinator \
             wakes it automatically. Do NOT restate a known block on every run.\n\
             5. Otherwise → write or update the epic roadmap design note, \
             create 3–5 worker tasks (or a spike if uncertainty is high).\n\
             6. Call `submit_grooming` when done.",
            epic.title, epic.short_id, epic.short_id
        );
        let originating_adr_section = match epic.originating_adr_id.as_deref() {
            Some(adr) if !adr.is_empty() => format!(
                "\nOriginating ADR: `{adr}` — this epic was spawned from an \
                 accepted proposal. Call `memory_read(identifier=\"{adr}\")` \
                 for the architectural rationale, acceptance criteria, and the \
                 work shape it sketches before creating tasks."
            ),
            _ => String::new(),
        };
        let design = format!(
            "Epic: {} ({}){}\n\n\
             Call `epic_show({})` to load the epic's memory_refs, then \
             `memory_read(identifier=<each-ref>)` for each one to pull context. \
             Call `build_context` for session reflections from previously \
             completed tasks under this epic. Notes are stored in the project \
             database and accessed through the `memory_*` MCP tools.",
            epic.title, epic.short_id, originating_adr_section, epic.short_id
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

    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use crate::{BackgroundWorkTracker, CoordinatorDeps, CoordinatorHandle, DEFAULT_MODEL_ID};
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{Database, EpicRepository, TaskRepository};
    use djinn_provider::catalog::CatalogService;
    use djinn_provider::catalog::health::HealthTracker;
    use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};

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
        let background_work_tracker = BackgroundWorkTracker::default();
        let role_registry = Arc::new(RoleRegistry::new());
        CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            catalog,
            health,
            role_registry,
            background_work_tracker,
            djinn_lsp::LspManager::new(),
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
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);
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
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        let _ = tx.send(DjinnEventEnvelope::epic_created(
            &djinn_core::models::EpicEventPayload::bare(&epic),
        ));
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
                    blocked_by: None,
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
    async fn closed_to_open_promotion_triggers_planning_task() {
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
                    status: Some("closed"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let promoted = epic_repo.set_status_raw(&epic.id, "open").await.unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(
            &djinn_core::models::EpicEventPayload::bare(&promoted),
        ));

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_to_open_promotion_does_not_duplicate_planning_task() {
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
                    status: Some("closed"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let promoted = epic_repo.set_status_raw(&epic.id, "open").await.unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(
            &djinn_core::models::EpicEventPayload::bare(&promoted),
        ));

        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        let _ = tx.send(DjinnEventEnvelope::epic_updated(
            &djinn_core::models::EpicEventPayload::bare(&promoted),
        ));
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

    /// Regression (epic `lywz`, 2026-06-02): the Planner re-links the epic
    /// roadmap note while grooming, which re-emits `epic.updated`.  Once the
    /// epic already has worker tasks, that event must NOT regenerate a wave-1
    /// planning task — otherwise the planner respawns every cycle and starves
    /// its own worker tasks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_updated_does_not_recreate_planning_when_worker_tasks_exist() {
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
                    title: "Already Decomposed Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        // Wave 1: epic creation produces the first planning task.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Simulate the planner having decomposed the epic: close the planning
        // task and add a worker task (mirrors the real board after wave 1).
        task_repo
            .set_status_with_reason(&decomp_tasks[0].id, "closed", Some("completed"))
            .await
            .unwrap();
        task_repo
            .create(
                &epic.id,
                "Worker Task 1",
                "",
                "",
                "task",
                1,
                "",
                Some("open"),
            )
            .await
            .unwrap();

        // The planner re-touches the epic (e.g. update_memory_refs) → epic.updated.
        let touched = epic_repo
            .update_memory_refs(&epic.id, r#"["design/roadmap"]"#)
            .await
            .unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(
            &djinn_core::models::EpicEventPayload::bare(&touched),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // No NEW planning task must have been created — the worker task is still
        // open, so the epic is already decomposed.
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(
            open_planning, 0,
            "epic.updated must not recreate a planning task while worker tasks exist"
        );
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
                    blocked_by: None,
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

    // ── Regression tests: race-condition + propagation (i528-1 §4) ──────────

    /// Regression test: blocked epic does NOT get a planning task while its
    /// blocker is still open.
    ///
    /// Simulates the proposal decomposition flow: epic A (foundation) and
    /// epic B (dependent) are created, then B.blocked_by=A is wired.
    /// B must NOT receive a planning task until A closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_epic_does_not_get_planning_task_while_blocker_open() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create epic A (foundation, no blockers).
        let epic_a = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Foundation Epic A",
                    description: "owns migration",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        // Epic A gets its planning task immediately (no blockers).
        let a_tasks = wait_for_decomp_tasks(&db, &tx, &epic_a.id, 1).await;
        assert_eq!(
            a_tasks.len(),
            1,
            "foundation epic A should get a planning task"
        );

        // Create epic B (dependent).
        let epic_b = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Dependent Epic B",
                    description: "depends on A",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        // B initially has no blockers — its planning task may or may not
        // have fired by now. Wire the blocker immediately (simulates the
        // proposal planner wiring edges after creation).
        epic_repo.add_blocker(&epic_b.id, &epic_a.id).await.unwrap();

        // Wait a bit for any pending dispatches to settle.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // B must NOT have an open planning task while A is still open.
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let b_tasks = task_repo.list_by_epic(&epic_b.id).await.unwrap();
        let b_open_planning: Vec<_> = b_tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .collect();
        assert!(
            b_open_planning.is_empty(),
            "epic B must NOT have an open planning task while blocker A is open, \
             but found {} open planning tasks",
            b_open_planning.len()
        );
    }

    /// Regression test (two-phased): closing blocker triggers decomposition
    /// of the dependent via emit_unblocked_epics.
    ///
    /// P1 owns a migration. P2 depends on P1.
    /// P2 does NOT decompose while P1 is open.
    /// Once P1 closes, P2 decomposes automatically.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_phased_p2_decomposes_after_p1_closes() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // P1: foundation epic (owns a migration).
        let p1 = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "P1 Migration Foundation",
                    description: "adds migration that P2 depends on",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        // P1 gets its planning task.
        let p1_tasks = wait_for_decomp_tasks(&db, &tx, &p1.id, 1).await;
        assert_eq!(p1_tasks.len(), 1, "P1 should get a planning task");

        // P2: dependent epic.
        let p2 = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "P2 Dependent Feature",
                    description: "builds on P1 migration",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        // Wire P2.blocked_by = P1 (simulates proposal planner wiring).
        epic_repo.add_blocker(&p2.id, &p1.id).await.unwrap();

        // Wait for dispatches to settle.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // P2 must NOT have an open planning task while P1 is open.
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let p2_tasks = task_repo.list_by_epic(&p2.id).await.unwrap();
        let p2_open_planning: Vec<_> = p2_tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .collect();
        assert!(
            p2_open_planning.is_empty(),
            "P2 must NOT decompose while P1 is open"
        );

        // Close P1 — triggers emit_unblocked_epics, which emits epic.updated
        // for P2, which re-drives the coordinator's wave-1 path.
        epic_repo.close(&p1.id).await.unwrap();

        // Now P2 should get its planning task via the re-drive path.
        let p2_decomp = wait_for_decomp_tasks(&db, &tx, &p2.id, 1).await;
        assert_eq!(
            p2_decomp.len(),
            1,
            "P2 must decompose automatically after P1 closes"
        );
    }

    /// Regression test: no-blocker epics decompose immediately at
    /// coordinator level.
    ///
    /// An epic with no blockers and auto_breakdown=true (the default) must
    /// get a planning task as soon as it's created. This is the normal
    /// single-epic proposal path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_blocker_epic_gets_planning_task_immediately() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a single epic with no blockers and default auto_breakdown.
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Simple Epic",
                    description: "no blockers, should decompose immediately",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        // It should get a planning task immediately.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            decomp_tasks.len(),
            1,
            "single epic with no blockers must decompose immediately"
        );
    }
}

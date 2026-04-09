// Coordinator tick rules (ADR-034):
//   (1) Spike/research task closure → create planning task for Planner.
//   (2) Batch completion (all worker tasks closed under open epic) → new planning task.
//   (3) Architect patrol: scheduled every 5 min, skipped when no open epics.
//   (4) Epic throughput: tasks merged per hour per epic (rolling window, in-memory).
//
// All rules are deterministic — zero LLM calls.

use super::reentrance::{DispatchEvent, should_auto_dispatch_planner};
use super::*;
use djinn_core::models::IssueType;
use djinn_core::models::task::PRIORITY_CRITICAL;
use djinn_db::EpicRepository;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default patrol interval (used when the planner has not self-scheduled).
/// Per ADR-051 §1 the Planner owns the board patrol (previously Architect).
pub(super) const DEFAULT_PLANNER_PATROL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Minimum patrol interval the planner may request.
pub(crate) const MIN_PLANNER_PATROL_MINUTES: u32 = 5;

/// Maximum patrol interval the planner may request.
pub(crate) const MAX_PLANNER_PATROL_MINUTES: u32 = 60;

/// Rolling window for throughput tracking.
pub(super) const THROUGHPUT_WINDOW: Duration = Duration::from_secs(60 * 60);

// ── Epic completion rules ─────────────────────────────────────────────────────

impl CoordinatorActor {
    /// Called when any task transitions to `closed`.
    ///
    /// Checks the two epic-level completion rules:
    /// 1. Spike/research closure → planning task for Planner.
    /// 2. All worker tasks closed under an open epic → planning task for Planner.
    ///
    /// Deduplicates by checking whether an open planning task already exists.
    pub(super) async fn on_task_closed(&mut self, task: &djinn_core::models::Task) {
        let Some(epic_id) = task.epic_id.as_deref() else {
            return;
        };

        let epic_repo = EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let epic = match epic_repo.get(epic_id).await {
            Ok(Some(e)) => e,
            _ => return,
        };

        // Only fire rules when the epic itself is still open.
        if epic.status != "open" {
            return;
        }

        let task_repo = self.task_repo();

        // Rule 1: Spike or Research closure.
        // Only fire when all other worker tasks are also closed (epic is drained),
        // so the planner doesn't create new waves while work is still in progress.
        let is_spike_or_research = matches!(task.issue_type.as_str(), "spike" | "research");

        if is_spike_or_research {
            let all_tasks = match task_repo.list_by_epic(epic_id).await {
                Ok(t) => t,
                Err(_) => return,
            };
            let has_open_work = all_tasks.iter().any(|t| {
                t.id != task.id
                    && !matches!(
                        t.issue_type.as_str(),
                        "planning" | "decomposition" | "review"
                    )
                    && t.status != "closed"
            });
            if !has_open_work && !self.open_planning_task_exists(&task_repo, epic_id).await {
                if should_auto_dispatch_planner(
                    &self.db,
                    DispatchEvent::TaskClosed {
                        epic_id,
                        close_reason: task.close_reason.as_deref(),
                    },
                )
                .await
                {
                    self.create_planning_task_by_ids(
                        &task_repo,
                        epic_id,
                        &task.project_id,
                        "spike_research_complete",
                    )
                    .await;
                } else {
                    tracing::debug!(
                        epic_id,
                        trigger = "spike_research_complete",
                        "CoordinatorActor: auto-dispatch suppressed by reentrance guard"
                    );
                }
            }
            return; // Rule 1 fires; skip rule 2 for this event.
        }

        // Rule 2: Batch completion — all non-planning/review tasks closed.
        // (Planning tasks themselves don't trigger further planning.)
        let is_planning_or_review = matches!(
            task.issue_type.as_str(),
            "planning" | "decomposition" | "review"
        );
        if is_planning_or_review {
            return;
        }

        // Query all tasks under the epic.
        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    error = %e,
                    "CoordinatorActor: failed to list epic tasks for batch completion check"
                );
                return;
            }
        };

        // Worker tasks = not planning / review.
        let worker_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| {
                !matches!(
                    t.issue_type.as_str(),
                    "planning" | "decomposition" | "review"
                )
            })
            .collect();

        if worker_tasks.is_empty() {
            return;
        }

        // Batch completion: all worker tasks are closed AND no tasks are in_progress.
        let all_closed = worker_tasks.iter().all(|t| t.status == "closed");
        let any_in_progress = all_tasks.iter().any(|t| {
            matches!(
                t.status.as_str(),
                "in_progress"
                    | "in_task_review"
                    | "in_lead_intervention"
                    | "needs_task_review"
                    | "needs_lead_intervention"
                    | "verifying"
            )
        });

        if all_closed
            && !any_in_progress
            && !self.open_planning_task_exists(&task_repo, epic_id).await
        {
            if should_auto_dispatch_planner(
                &self.db,
                DispatchEvent::TaskClosed {
                    epic_id,
                    close_reason: task.close_reason.as_deref(),
                },
            )
            .await
            {
                self.create_planning_task_by_ids(
                    &task_repo,
                    epic_id,
                    &task.project_id,
                    "batch_complete",
                )
                .await;
            } else {
                tracing::debug!(
                    epic_id,
                    trigger = "batch_complete",
                    "CoordinatorActor: auto-dispatch suppressed by reentrance guard"
                );
            }
        }
    }

    /// Returns `true` if there is already an open `planning` task under the epic.
    pub(super) async fn open_planning_task_exists(
        &self,
        task_repo: &djinn_db::TaskRepository,
        epic_id: &str,
    ) -> bool {
        match task_repo.list_by_epic(epic_id).await {
            Ok(tasks) => tasks.iter().any(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && t.status != "closed"
            }),
            Err(_) => false,
        }
    }

    /// ADR-051 §7 — shared epic-eligibility check used by exit-recheck and
    /// the 15-min stale sweep.
    ///
    /// An epic is eligible for a new planning wave when:
    ///   - it still exists and is `open`,
    ///   - it has at least one non-planning worker task,
    ///   - all worker tasks are closed,
    ///   - no tasks are in a mid-flight status,
    ///   - no open planning/decomposition task exists.
    ///
    /// The active-planner guard and close_reason filter are applied by
    /// `should_auto_dispatch_planner` at the actual dispatch site; this
    /// helper only checks the board-shape preconditions so callers can
    /// avoid pointless queries.
    pub(super) async fn epic_is_eligible_for_next_wave(
        &self,
        task_repo: &djinn_db::TaskRepository,
        epic_id: &str,
    ) -> bool {
        let epic_repo = EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let epic = match epic_repo.get(epic_id).await {
            Ok(Some(e)) => e,
            _ => return false,
        };
        if epic.status != "open" {
            return false;
        }

        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(t) => t,
            Err(_) => return false,
        };

        let worker_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| {
                !matches!(
                    t.issue_type.as_str(),
                    "planning" | "decomposition" | "review"
                )
            })
            .collect();

        if worker_tasks.is_empty() {
            return false;
        }
        if !worker_tasks.iter().all(|t| t.status == "closed") {
            return false;
        }
        let any_in_progress = all_tasks.iter().any(|t| {
            matches!(
                t.status.as_str(),
                "in_progress"
                    | "in_task_review"
                    | "in_lead_intervention"
                    | "needs_task_review"
                    | "needs_lead_intervention"
                    | "verifying"
            )
        });
        if any_in_progress {
            return false;
        }
        if self.open_planning_task_exists(task_repo, epic_id).await {
            return false;
        }
        true
    }

    /// Create a planning task for the Planner under the given epic (by ID).
    /// Used by spike/research and batch-completion rules that already hold the IDs.
    pub(super) async fn create_planning_task_by_ids(
        &self,
        task_repo: &djinn_db::TaskRepository,
        epic_id: &str,
        project_id: &str,
        trigger: &str,
    ) {
        let title = format!("Plan next wave ({trigger})");
        match task_repo
            .create_in_project(
                project_id,
                Some(epic_id),
                &title,
                "Plan the next wave of work for this epic. Review completed work, update the roadmap, and create 3–5 tasks.",
                "",
                IssueType::Planning.as_str(),
                PRIORITY_CRITICAL,
                "system",
                Some("open"),
                None,
            )
            .await
        {
            Ok(t) => {
                tracing::info!(
                    epic_id,
                    task_short_id = %t.short_id,
                    trigger,
                    "CoordinatorActor: created planning task"
                );
            }
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    trigger,
                    error = %e,
                    "CoordinatorActor: failed to create planning task"
                );
            }
        }
    }

    // ── Throughput tracking ───────────────────────────────────────────────────

    /// Record a task merge event for the given epic (updates in-memory rolling window).
    pub(super) fn record_merge_event(&mut self, epic_id: &str) {
        let events = self
            .throughput_events
            .entry(epic_id.to_owned())
            .or_default();
        events.push(StdInstant::now());
        // Eagerly evict events outside the rolling window to bound memory.
        events.retain(|t| t.elapsed() < THROUGHPUT_WINDOW);
    }

    /// Evict expired throughput events to bound memory usage.
    pub(super) fn evict_throughput_events(&mut self) {
        for events in self.throughput_events.values_mut() {
            events.retain(|t| t.elapsed() < THROUGHPUT_WINDOW);
        }
        self.throughput_events.retain(|_, v| !v.is_empty());
    }

    /// Return a snapshot of tasks-merged-per-hour per epic (within the rolling window).
    pub fn throughput_snapshot(&self) -> HashMap<String, usize> {
        self.throughput_events
            .iter()
            .map(|(epic_id, events)| {
                let count = events
                    .iter()
                    .filter(|t| t.elapsed() < THROUGHPUT_WINDOW)
                    .count();
                (epic_id.clone(), count)
            })
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_db::{EpicRepository, TaskRepository};
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn spawn_coordinator(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 2,
                    roles: ["worker", "reviewer"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        CoordinatorHandle::spawn(CoordinatorDeps::new(
            tx.clone(),
            cancel,
            db.clone(),
            pool,
            djinn_provider::catalog::CatalogService::new(),
            HealthTracker::new(),
            Arc::new(RoleRegistry::new()),
            VerificationTracker::default(),
            crate::lsp::LspManager::new(),
        ))
    }

    async fn make_epic(
        db: &Database,
        project_id: &str,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> djinn_core::models::Epic {
        EpicRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .create_for_project(
                project_id,
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
            .unwrap()
    }

    async fn create_task(
        db: &Database,
        epic_id: &str,
        project_id: &str,
        title: &str,
        issue_type: &str,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> djinn_core::models::Task {
        TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .create_in_project(
                project_id,
                Some(epic_id),
                title,
                "",
                "",
                issue_type,
                0,
                "",
                Some("open"),
                None,
            )
            .await
            .unwrap()
    }

    async fn close_task(db: &Database, task_id: &str, tx: &broadcast::Sender<DjinnEventEnvelope>) {
        TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
            .transition(
                task_id,
                djinn_core::models::TransitionAction::Close,
                "test",
                "system",
                None,
                None,
            )
            .await
            .unwrap();
    }

    fn planning_count(tasks: &[djinn_core::models::Task]) -> usize {
        tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && t.status != "closed"
            })
            .count()
    }

    async fn assert_planning_task_count(
        task_repo: &TaskRepository,
        epic_id: &str,
        expected: usize,
        message: &str,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let tasks = task_repo.list_by_epic(epic_id).await.unwrap();
            let count = planning_count(&tasks);
            if count == expected {
                return;
            }

            if tokio::time::Instant::now() >= deadline {
                assert_eq!(count, expected, "{message}");
            }

            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    // ── AC1: Spike/research closure → decomposition task ──────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spike_closure_creates_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let spike = create_task(&db, &epic.id, &project.id, "Spike task", "spike", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);

        // Close the spike task — should trigger decomposition task creation.
        close_task(&db, &spike.id, &tx).await;
        assert_planning_task_count(
            &task_repo,
            &epic.id,
            1,
            "spike closure should create exactly one planning task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn research_closure_creates_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let research =
            create_task(&db, &epic.id, &project.id, "Research task", "research", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);

        close_task(&db, &research.id, &tx).await;
        assert_planning_task_count(
            &task_repo,
            &epic.id,
            1,
            "research closure should create one planning task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spike_closure_does_not_duplicate_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Pre-create an open planning task.
        create_task(&db, &epic.id, &project.id, "Existing plan", "planning", &tx).await;

        let spike = create_task(&db, &epic.id, &project.id, "Spike", "spike", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);
        close_task(&db, &spike.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            1,
            "should not create a duplicate planning task when one already exists"
        );
    }

    // ── AC2: Batch completion → decomposition task ─────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_creates_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;
        let t2 = create_task(&db, &epic.id, &project.id, "Task 2", "feature", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);

        // Close t1 first — epic not yet complete.
        close_task(&db, &t1.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            0,
            "partial completion should not create planning task"
        );

        // Close t2 — batch complete now.
        close_task(&db, &t2.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            1,
            "batch completion should create exactly one planning task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_skipped_when_decomposition_exists() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;
        // Pre-existing open planning task.
        create_task(&db, &epic.id, &project.id, "Existing plan", "planning", &tx).await;

        let _handle = spawn_coordinator(&db, &tx);
        close_task(&db, &t1.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            1,
            "should not create duplicate planning task on batch completion"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_does_not_fire_for_closed_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;

        // Close the epic first.
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .close(&epic.id)
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);
        close_task(&db, &t1.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            0,
            "closed epic should not trigger planning task"
        );
    }

    // ── AC4: Throughput tracking ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn throughput_snapshot_counts_recent_events() {
        let db = test_helpers::create_test_db();
        let (events_tx, _rx) = broadcast::channel::<DjinnEventEnvelope>(16);

        use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (status_tx, _) = tokio::sync::watch::channel(SharedCoordinatorState {
            paused_projects: HashSet::new(),
            unhealthy_project_ids: HashSet::new(),
            unhealthy_project_errors: HashMap::new(),
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let mut actor = CoordinatorActor::new(
            CoordinatorDeps::new(
                events_tx.clone(),
                cancel,
                db,
                pool,
                djinn_provider::catalog::CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                VerificationTracker::default(),
                crate::lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        );

        // Record 3 events for epic "epic-1".
        actor.record_merge_event("epic-1");
        actor.record_merge_event("epic-1");
        actor.record_merge_event("epic-1");

        // Record 1 event for epic "epic-2".
        actor.record_merge_event("epic-2");

        let snap = actor.throughput_snapshot();
        assert_eq!(snap.get("epic-1"), Some(&3));
        assert_eq!(snap.get("epic-2"), Some(&1));
        assert_eq!(snap.get("epic-3"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn throughput_evict_removes_old_events() {
        let db = test_helpers::create_test_db();
        let (events_tx, _rx) = broadcast::channel::<DjinnEventEnvelope>(16);

        use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
        use crate::roles::RoleRegistry;
        use djinn_provider::catalog::health::HealthTracker;

        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        let (status_tx, _) = tokio::sync::watch::channel(SharedCoordinatorState {
            paused_projects: HashSet::new(),
            unhealthy_project_ids: HashSet::new(),
            unhealthy_project_errors: HashMap::new(),
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let mut actor = CoordinatorActor::new(
            CoordinatorDeps::new(
                events_tx.clone(),
                cancel,
                db,
                pool,
                djinn_provider::catalog::CatalogService::new(),
                HealthTracker::new(),
                Arc::new(RoleRegistry::new()),
                VerificationTracker::default(),
                crate::lsp::LspManager::new(),
            ),
            receiver,
            sender,
            status_tx,
        );

        // Manually insert an expired event into the throughput map.
        actor
            .throughput_events
            .entry("epic-1".to_owned())
            .or_default()
            .push(StdInstant::now() - THROUGHPUT_WINDOW - Duration::from_secs(1));

        // Add a fresh event.
        actor.record_merge_event("epic-1");

        actor.evict_throughput_events();
        let snap = actor.throughput_snapshot();
        assert_eq!(
            snap.get("epic-1"),
            Some(&1),
            "expired events should be evicted"
        );
    }

    // ── ADR-051 §7: reentrance guard suppresses auto-dispatch ─────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_suppressed_when_active_planner_on_epic() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id, &tx).await;
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // One worker task plus a separate "host" task that the planner
        // session is attached to.  The host is a `review` task (non-worker,
        // non-planning) so it is excluded from both the worker-task count
        // and the open-planning-exists check — leaving the reentrance
        // guard's active-planner check as the ONLY thing that can suppress
        // dispatch.
        let t1 = create_task(&db, &epic.id, &project.id, "Task 1", "task", &tx).await;
        let planner_host =
            create_task(&db, &epic.id, &project.id, "Planner host", "review", &tx).await;

        // Insert a running planner session on `planner_host`.
        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&planner_host.id),
                model: "openai/gpt-5",
                agent_type: "planner",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        let _handle = spawn_coordinator(&db, &tx);

        // Closing the worker task should normally trigger batch-completion
        // auto-dispatch — but the active planner guard must suppress it.
        close_task(&db, &t1.id, &tx).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(
            planning_count(&tasks),
            0,
            "reentrance guard must suppress new planning task while planner is active"
        );
    }
}

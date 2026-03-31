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

use super::*;
use djinn_core::models::task::PRIORITY_CRITICAL;
use djinn_db::EpicRepository;

/// Worker issue_types that count toward batch-completion.  Any `issue_type`
/// not in this list is treated as a planner/meta task and excluded.
const WORKER_ISSUE_TYPES: &[&str] = &["task", "research", "feature"];

impl CoordinatorActor {
    /// Called when an epic is created.  Creates the first planning task
    /// unless one already exists for the epic (idempotent).
    pub(super) async fn maybe_create_planning_task(&mut self, epic: &djinn_core::models::Epic) {
        // Only create planning tasks for epics that are fully open.
        // Drafting epics are still being refined by the user; closed epics
        // are done.  The promotion from drafting→open is handled separately
        // via the epic_updated event path.
        if epic.status != "open" {
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

        self.create_planning_task(epic).await;
    }

    /// Called when a task is closed.  Checks whether all worker tasks under
    /// the epic are now closed (batch complete) and if so creates a new
    /// planning task for the next wave.
    pub(super) async fn maybe_create_next_wave_planning(
        &mut self,
        epic_id: &str,
        project_id: &str,
    ) {
        let epic_repo = EpicRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        // Fetch epic — bail if not found or already closed.
        let epic = match epic_repo.get(epic_id).await {
            Ok(Some(e)) => e,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    error = %e,
                    "CoordinatorActor: failed to fetch epic for batch-completion check"
                );
                return;
            }
        };

        if epic.status != "open" {
            return;
        }

        let task_repo = self.task_repo();
        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    error = %e,
                    "CoordinatorActor: failed to list tasks for batch-completion check"
                );
                return;
            }
        };

        // Check for an existing open/in-progress planning task — if one
        // exists we don't create another.
        let has_open_planning = all_tasks.iter().any(|t| {
            matches!(t.issue_type.as_str(), "planning" | "decomposition")
                && matches!(t.status.as_str(), "open" | "in_progress")
        });
        if has_open_planning {
            return;
        }

        // Worker tasks: include only WORKER_ISSUE_TYPES.  A batch is complete
        // when every worker task is closed.
        let worker_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| WORKER_ISSUE_TYPES.contains(&t.issue_type.as_str()))
            .collect();

        // No worker tasks yet — the first decomposition wave hasn't created
        // anything to close.  Skip.
        if worker_tasks.is_empty() {
            return;
        }

        let all_closed = worker_tasks.iter().all(|t| t.status == "closed");

        if !all_closed {
            return;
        }

        tracing::info!(
            epic_id,
            project_id,
            worker_count = worker_tasks.len(),
            "CoordinatorActor: batch complete — creating next-wave planning task"
        );

        self.create_planning_task(&epic).await;
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
        let design = format!(
            "Epic: {} ({})\nEpic memory_refs: {}\n\n\
             Use `epic_show({})` to read full epic context and memory_refs.\n\
             Use `build_context` enriched with session reflections from \
             previously completed tasks under this epic.",
            epic.title, epic.short_id, epic.memory_refs, epic.short_id
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

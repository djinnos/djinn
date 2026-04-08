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
use super::reentrance::{DispatchEvent, should_auto_dispatch_planner};
use djinn_core::models::task::PRIORITY_CRITICAL;

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

        // ADR-051 §7 — reentrance guard.  Epic C will plumb the real
        // `auto_breakdown` value; Epic B hard-codes `true` so the check
        // still runs the active-planner guard.
        if !should_auto_dispatch_planner(
            &self.db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: true,
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

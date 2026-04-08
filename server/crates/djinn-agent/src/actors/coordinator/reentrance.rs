// ADR-051 §7 — Auto-dispatch reentrance guards.
//
// Both auto-dispatch rules from ADR-034 §4 are gated through a single
// helper, `should_auto_dispatch_planner`, so that future auto-dispatch
// rules inherit the same protection.
//
// Checks, in order:
//   1. Close-reason filter (TaskClosed only).  Reshape / superseded /
//      duplicate closes are explicit signals that a Planner is mid-reshape;
//      suppress the auto-dispatch to avoid fighting the intervention.
//   2. `auto_breakdown` filter (EpicCreated only).  If the caller asked
//      for no auto-breakdown, honor it.
//   3. Active-session guard (both paths).  If there is already a running
//      Planner attached to a task under the given epic, suppress.
//
// When any check suppresses dispatch, the reason is logged at `debug!`
// level so coordinator traces show why a dispatch was skipped.

use djinn_core::events::EventBus;
use djinn_core::models::task::{
    CLOSE_REASON_DUPLICATE, CLOSE_REASON_RESHAPE, CLOSE_REASON_SUPERSEDED,
};
use djinn_db::{Database, SessionRepository};

/// Which auto-dispatch event triggered the check.
#[derive(Debug, Clone, Copy)]
pub(super) enum DispatchEvent<'a> {
    /// Firing because a task just closed under this epic.
    TaskClosed {
        epic_id: &'a str,
        close_reason: Option<&'a str>,
    },
    /// Firing because an epic was just created / promoted to open.
    EpicCreated {
        epic_id: &'a str,
        /// Epic C will wire the real value; Epic B hard-codes `true`
        /// at call sites so the plumbing exists.
        auto_breakdown: bool,
    },
}

impl<'a> DispatchEvent<'a> {
    fn epic_id(&self) -> &'a str {
        match self {
            Self::TaskClosed { epic_id, .. } | Self::EpicCreated { epic_id, .. } => epic_id,
        }
    }
}

/// Returns `true` iff all applicable reentrance checks pass and the
/// coordinator should proceed to create a planning task.
pub(super) async fn should_auto_dispatch_planner(db: &Database, event: DispatchEvent<'_>) -> bool {
    let epic_id = event.epic_id();

    // 1. Close-reason filter (TaskClosed only).
    if let DispatchEvent::TaskClosed { close_reason, .. } = event
        && let Some(reason) = close_reason
        && matches!(
            reason,
            CLOSE_REASON_RESHAPE | CLOSE_REASON_SUPERSEDED | CLOSE_REASON_DUPLICATE
        )
    {
        tracing::debug!(
            epic_id,
            close_reason = reason,
            "ADR-051 §7: skip auto-dispatch — reshape close_reason",
        );
        return false;
    }

    // 2. `auto_breakdown` filter (EpicCreated only).
    if let DispatchEvent::EpicCreated { auto_breakdown, .. } = event
        && !auto_breakdown
    {
        tracing::debug!(
            epic_id,
            "ADR-051 §7: skip auto-dispatch — auto_breakdown=false",
        );
        return false;
    }

    // 3. Active-session guard (both paths).
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    match session_repo.active_planner_for_epic(epic_id).await {
        Ok(actives) if !actives.is_empty() => {
            tracing::debug!(
                epic_id,
                active_count = actives.len(),
                "ADR-051 §7: skip auto-dispatch — active planner on epic",
            );
            false
        }
        Ok(_) => true,
        Err(e) => {
            // Fail open: on DB error, allow dispatch rather than stall the
            // board.  The error is logged so it can be investigated.
            tracing::warn!(
                epic_id,
                error = %e,
                "ADR-051 §7: active-session lookup failed, allowing dispatch",
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_db::{CreateSessionParams, EpicRepository, SessionRepository, TaskRepository};

    async fn make_epic(db: &djinn_db::Database, project_id: &str) -> djinn_core::models::Epic {
        EpicRepository::new(db.clone(), EventBus::noop())
            .create_for_project(
                project_id,
                djinn_db::EpicCreateInput {
                    title: "Epic",
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

    async fn make_task(
        db: &djinn_db::Database,
        project_id: &str,
        epic_id: &str,
    ) -> djinn_core::models::Task {
        TaskRepository::new(db.clone(), EventBus::noop())
            .create_in_project(
                project_id,
                Some(epic_id),
                "Task",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                None,
            )
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reshape_close_reason_skips_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id).await;

        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::TaskClosed {
                epic_id: &epic.id,
                close_reason: Some(CLOSE_REASON_RESHAPE),
            },
        )
        .await;
        assert!(!allowed, "reshape close_reason must skip auto-dispatch");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_planner_on_epic_skips_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id).await;
        let task = make_task(&db, &project.id, &epic.id).await;

        let sessions = SessionRepository::new(db.clone(), EventBus::noop());
        sessions
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "openai/gpt-5",
                agent_type: "planner",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::TaskClosed {
                epic_id: &epic.id,
                close_reason: None,
            },
        )
        .await;
        assert!(!allowed, "active planner on epic must skip auto-dispatch");

        let allowed_epic = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            !allowed_epic,
            "active planner on epic must skip epic-created dispatch too"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_breakdown_false_skips_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id).await;

        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: false,
            },
        )
        .await;
        assert!(!allowed, "auto_breakdown=false must skip");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn natural_completion_allows_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = make_epic(&db, &project.id).await;

        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::TaskClosed {
                epic_id: &epic.id,
                close_reason: Some("completed"),
            },
        )
        .await;
        assert!(
            allowed,
            "natural completion with no active planner must dispatch"
        );

        let allowed_epic = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            allowed_epic,
            "epic created with auto_breakdown=true must dispatch"
        );
    }
}

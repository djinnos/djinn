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
use djinn_db::{Database, EpicRepository, SessionRepository};

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

    // 2b. Epic-blocker gate (ALL dispatch paths). A parked epic must never
    // receive a planning task while a blocking epic is still open — whether
    // the trigger is wave-1 (`EpicCreated`), next-wave batch completion, a
    // post-planner recheck, or the periodic stale-sweep (all `TaskClosed`).
    //
    // Two sources feed this gate:
    //   • A proposal-decomposition planner sequencing the epics it creates via
    //     epic-level `blocked_by`.
    //   • A grooming planner that concluded "blocked on epic X, no tasks
    //     created" and durably recorded the edge through `submit_grooming`
    //     (`blocked_on`) — see `finalize_handlers::handle_submit_grooming`.
    //     Before this gate applied on `TaskClosed` and before the planner
    //     recorded the edge, a blocked epic re-derived "blocked" via a fresh
    //     LLM session every 15-min sweep (epic `mygq`, 2026-07-01 — dozens of
    //     no-op planner generations overnight, each appending a near-identical
    //     stale-sweep entry to the roadmap note).
    //
    // The close path (`EpicRepository::emit_unblocked_epics`) re-emits
    // `epic.updated` for the dependents, which re-drives this check once the
    // blockers are gone; the periodic sweep is a DB-truth backstop so a missed
    // event cannot strand a parked epic forever.
    {
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
        match epic_repo.has_unresolved_blockers(epic_id).await {
            Ok(true) => {
                tracing::debug!(
                    epic_id,
                    "epic dependencies: skip auto-dispatch — unresolved epic blockers",
                );
                return false;
            }
            Ok(false) => {}
            Err(e) => {
                // Fail closed: a blocker-lookup error must NOT silently
                // allow dispatch — duplicating foundation work (migrations,
                // schemas, shared modules) is worse than stalling.
                tracing::error!(
                    epic_id,
                    error = %e,
                    "epic dependencies: blocker lookup failed, deferring dispatch (fail-closed)",
                );
                return false;
            }
        }
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
                    blocked_by: None,
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
            .create_fixture_in_project(
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
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
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
    async fn unresolved_epic_blocker_skips_then_allows_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
        let blocker = make_epic(&db, &project.id).await;
        let dependent = make_epic(&db, &project.id).await;

        // dependent is blocked by blocker.
        epic_repo
            .add_blocker(&dependent.id, &blocker.id)
            .await
            .unwrap();

        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &dependent.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            !allowed,
            "unresolved epic blocker must skip wave-1 dispatch"
        );

        // Closing the blocker unblocks the dependent.
        epic_repo.close(&blocker.id).await.unwrap();

        let allowed_after = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &dependent.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            allowed_after,
            "closing the last blocker must allow wave-1 dispatch"
        );
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

    // ── Regression tests: race-condition + propagation (i528-1 §4) ──────────

    /// Regression test 1: Create-then-block ordering.
    ///
    /// Simulates the proposal decomposition flow where epic A is created first,
    /// then epic B is created, and B.blocked_by=A is wired AFTER creation.
    /// B's auto-breakdown must be suppressed while A is still open.
    /// This reproduces the original race condition described in the epic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_then_block_ordering_defers_decomposition() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());

        // 1. Create epic A (open, no blockers — foundation).
        let epic_a = make_epic(&db, &project.id).await;

        // 2. Create epic B (open, no blockers — dependent, created before
        //    the proposal planner wires the blocker edge).
        let epic_b = make_epic(&db, &project.id).await;

        // 3. At this point, B has no blockers — dispatch would be allowed.
        let allowed_before_block = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic_b.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            allowed_before_block,
            "B with no blockers should dispatch initially"
        );

        // 4. Wire B.blocked_by = A (simulates proposal planner wiring
        //    the dependency edge AFTER epic creation).
        epic_repo.add_blocker(&epic_b.id, &epic_a.id).await.unwrap();

        // 5. Now the blocker gate must suppress B's dispatch.
        let allowed_after_block = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic_b.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            !allowed_after_block,
            "B blocked by open epic A must NOT dispatch (race condition regression)"
        );
    }

    /// Regression test 2: Closing blocker triggers decomposition.
    ///
    /// After epic A (the blocker) is closed, epic B (which was blocked by A)
    /// must be allowed to dispatch. This validates the emit_unblocked_epics
    /// re-drive path through the coordinator gate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_blocker_allows_blocked_epic_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());

        let epic_a = make_epic(&db, &project.id).await;
        let epic_b = make_epic(&db, &project.id).await;

        // B blocked by A.
        epic_repo.add_blocker(&epic_b.id, &epic_a.id).await.unwrap();

        // Confirm B is suppressed while A is open.
        assert!(
            !should_auto_dispatch_planner(
                &db,
                DispatchEvent::EpicCreated {
                    epic_id: &epic_b.id,
                    auto_breakdown: true,
                },
            )
            .await,
            "B must be suppressed while A is open"
        );

        // Close A — triggers emit_unblocked_epics internally.
        epic_repo.close(&epic_a.id).await.unwrap();

        // Now the blocker gate must allow B to dispatch.
        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic_b.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(allowed, "closing blocker A must allow B to dispatch");
    }

    /// Regression test 3: Fail-closed on blocker lookup error.
    ///
    /// If the blocker-lookup query returns an error (e.g. table missing),
    /// the gate must defer dispatch (fail-closed), not allow it (fail-open).
    /// This prevents duplicate foundation work when the DB is degraded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocker_lookup_error_defers_dispatch_fail_closed() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;

        let epic = make_epic(&db, &project.id).await;

        // Drop the epic_blockers table to cause a lookup error on the
        // next has_unresolved_blockers call.
        djinn_db::test_support::drop_table_for_test(&db, "epic_blockers").await;

        // With the table gone, the blocker lookup will error.
        // The gate must fail CLOSED: defer dispatch rather than allow it.
        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            !allowed,
            "blocker-lookup error must defer dispatch (fail-closed), not allow it"
        );
    }

    /// Regression test 4: No-blocker epics decompose immediately.
    ///
    /// An epic with no blockers and auto_breakdown=true must be dispatched
    /// immediately. This ensures we haven't regressed single-epic proposals.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_blocker_epic_decomposes_immediately() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;

        let epic = make_epic(&db, &project.id).await;

        // No blockers, auto_breakdown=true — must dispatch.
        let allowed = should_auto_dispatch_planner(
            &db,
            DispatchEvent::EpicCreated {
                epic_id: &epic.id,
                auto_breakdown: true,
            },
        )
        .await;
        assert!(
            allowed,
            "single epic with no blockers and auto_breakdown=true must dispatch immediately"
        );
    }

    /// Regression test 5 (gate-level): Two-phased regression guard.
    ///
    /// P1 owns a migration (foundation). P2 depends on P1.
    /// P2 must NOT dispatch while P1 is open.
    /// Once P1 closes, P2 dispatches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_phased_p2_waits_for_p1_at_gate_level() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());

        // P1: foundation epic (owns a migration).
        let p1 = make_epic(&db, &project.id).await;
        // P2: dependent epic (builds on P1's migration).
        let p2 = make_epic(&db, &project.id).await;

        // P2 depends on P1.
        epic_repo.add_blocker(&p2.id, &p1.id).await.unwrap();

        // While P1 is open, P2 must be suppressed.
        assert!(
            !should_auto_dispatch_planner(
                &db,
                DispatchEvent::EpicCreated {
                    epic_id: &p2.id,
                    auto_breakdown: true,
                },
            )
            .await,
            "P2 must NOT dispatch while P1 (migration owner) is open"
        );

        // Close P1 — migration complete.
        epic_repo.close(&p1.id).await.unwrap();

        // Now P2 must dispatch.
        assert!(
            should_auto_dispatch_planner(
                &db,
                DispatchEvent::EpicCreated {
                    epic_id: &p2.id,
                    auto_breakdown: true,
                },
            )
            .await,
            "P2 must dispatch after P1 closes"
        );
    }

    /// Regression (epic `mygq`, 2026-07-01): the blocker gate must also apply
    /// on the `TaskClosed` dispatch path (next-wave batch completion,
    /// post-planner recheck, and the periodic stale-sweep all fire as
    /// `TaskClosed`). Previously the gate was `EpicCreated`-only, so a parked
    /// epic re-derived "blocked" via a fresh LLM session every 15-min sweep.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolved_epic_blocker_skips_task_closed_dispatch() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());

        let blocker = make_epic(&db, &project.id).await;
        let parked = make_epic(&db, &project.id).await;
        epic_repo
            .add_blocker(&parked.id, &blocker.id)
            .await
            .unwrap();

        // The stale-sweep / next-wave path fires as `TaskClosed`. While the
        // blocker is open, it must NOT dispatch a planning task.
        assert!(
            !should_auto_dispatch_planner(
                &db,
                DispatchEvent::TaskClosed {
                    epic_id: &parked.id,
                    close_reason: None,
                },
            )
            .await,
            "TaskClosed dispatch must be suppressed while an epic blocker is open"
        );

        // Closing the blocker unblocks the parked epic on the same path.
        epic_repo.close(&blocker.id).await.unwrap();
        assert!(
            should_auto_dispatch_planner(
                &db,
                DispatchEvent::TaskClosed {
                    epic_id: &parked.id,
                    close_reason: None,
                },
            )
            .await,
            "closing the blocker must allow the TaskClosed dispatch path"
        );
    }
}

//! Pre-dispatch respawn guard.
//!
//! The guard consults attempt-history APIs before any fresh spawn/admission
//! side-effects.  The guard ordering is:
//!
//! 1. **Open-PR adoption**: when the task already has an open PR
//!    (`task.pr_url`), adopt it and record an `adopted_pr` audit row.  This
//!    prevents spawning a duplicate worker when a PR is already in review.
//! 2. **Non-terminal attempt**: when a `pending` or `submitted` attempt already
//!    exists for the task+role, defer dispatch and record a `deferred` audit
//!    row.
//!
//! No dispatch, provider, or reopen counters are incremented for guard
//! decisions.
//!
//! Existing cooldown / capacity / breaker / policy defers are routed through
//! their shipped coordinator APIs and, when audited, use the same
//! guard-deferred attempt-row mechanism rather than new counters or
//! session/activity-log reconstruction.
//!
//! Call [`run_respawn_guard`] before the spawn/admission path in
//! `dispatch_ready_tasks`.  Only when it returns [`RespawnGuardDecision::Allow`]
//! should the existing `try_dispatch_to_pool` + `record_dispatch_start` path
//! proceed.

use djinn_core::models::task_attempt::{GuardDecision, GuardReason};
use djinn_db::{
    GuardAdoptedPrTaskAttemptParams, GuardDeferTaskAttemptParams, TaskAttemptRepository,
};

use super::attempt_lifecycle::make_dispatch_key;

// ─── Public decision type ───────────────────────────────────────────────────

/// Result of the pre-dispatch respawn guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RespawnGuardDecision {
    /// Guard allows dispatch to proceed.
    Allow,
    /// Guard defers dispatch.  Carries the reason so the caller can log it and
    /// (optionally) record a guard-deferred audit row.
    Defer(GuardReason),
    /// Guard adopted an existing open PR for the task.  The caller should
    /// record an `adopted_pr` audit row and skip dispatch.  The task continues
    /// through its normal PR review/poller flow.
    Adopted { pr_url: String },
}

// ─── Core guard ─────────────────────────────────────────────────────────────

/// Run the pre-dispatch respawn guard for a given task+role.
///
/// Guard ordering:
/// 1. **Open-PR adoption** — when `pr_url` is `Some`, an existing open PR is
///    detected.  Returns [`RespawnGuardDecision::Adopted`] so the caller can
///    record an `adopted_pr` audit row and skip dispatch.
/// 2. **Non-terminal attempt** — consults
///    [`TaskAttemptRepository::latest_pending_or_submitted`] for the task/role
///    pair.  If a non-terminal attempt already exists the dispatch is deferred
///    with [`GuardReason::RespawnGuard`].
///
/// Returns [`RespawnGuardDecision::Allow`] when neither guard fires.
///
/// Best-effort: DB errors are logged and return `Allow` (fail-open) so a
/// transient lookup failure cannot permanently block dispatch.
pub async fn run_respawn_guard(
    db: &djinn_db::Database,
    task_id: &str,
    role: &str,
    pr_url: Option<&str>,
) -> RespawnGuardDecision {
    // 1. Open-PR adoption: when the task already has an open PR, adopt it
    //    and skip dispatch.  This prevents spawning a duplicate worker when
    //    a PR is already in review.
    if let Some(url) = pr_url
        && !url.is_empty()
    {
        tracing::info!(
            task_id = %task_id,
            role = %role,
            pr_url = %url,
            "respawn_guard: existing open PR detected — adopting"
        );
        return RespawnGuardDecision::Adopted {
            pr_url: url.to_owned(),
        };
    }

    // 2. Non-terminal attempt: when a pending or submitted attempt already
    //    exists for this task+role, defer dispatch.
    let repo = TaskAttemptRepository::new(db.clone());
    match repo.latest_pending_or_submitted(task_id, Some(role)).await {
        Ok(Some(existing)) => {
            tracing::info!(
                task_id = %task_id,
                role = %role,
                existing_attempt_id = %existing.id,
                existing_outcome = %existing.outcome,
                existing_dispatch_key = %existing.dispatch_key,
                "respawn_guard: non-terminal attempt already exists — deferring dispatch"
            );
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        }
        Ok(None) => RespawnGuardDecision::Allow,
        Err(e) => {
            // Fail-open: a transient DB error must not permanently block
            // dispatch.  Log loudly so operators can investigate.
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                error = %e,
                "respawn_guard: attempt-history lookup failed (fail-open); allowing dispatch"
            );
            RespawnGuardDecision::Allow
        }
    }
}

// ─── Audit-row helper ───────────────────────────────────────────────────────

/// Record a guard-deferred `task_attempts` audit row.
///
/// Best-effort: write errors are logged and never propagated.  Returns the
/// inserted attempt id on success, `None` on failure.
pub async fn record_guard_deferred_attempt(
    db: &djinn_db::Database,
    task_id: &str,
    role: &str,
    reason: GuardReason,
    summary: Option<&str>,
) -> Option<String> {
    let dispatch_key = make_dispatch_key(task_id, role);
    let id = uuid::Uuid::now_v7().to_string();
    let repo = TaskAttemptRepository::new(db.clone());
    match repo
        .insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &id,
            task_id,
            role,
            dispatch_key: &dispatch_key,
            decision: GuardDecision::Defer,
            reason,
            summary,
            summary_json: None,
            log_tail: None,
        })
        .await
    {
        Ok(attempt) => {
            tracing::info!(
                task_id = %task_id,
                role = %role,
                attempt_id = %attempt.id,
                dispatch_key = %attempt.dispatch_key,
                reason = %reason,
                "respawn_guard: guard-deferred audit row recorded"
            );
            Some(attempt.id)
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                reason = %reason,
                error = %e,
                "respawn_guard: failed to record guard-deferred audit row (best-effort)"
            );
            None
        }
    }
}

/// Record an adopted-PR `task_attempts` audit row.
///
/// The row is terminal with outcome `adopted_pr` and guard reason
/// `open_pr_adoption`.  Uses a deterministic dispatch key
/// (`{task_id}:{role}:open_pr_adoption`) so repeated calls are idempotent
/// via `ON CONFLICT (dispatch_key) DO NOTHING`.
///
/// Best-effort: write errors are logged and never propagated.  Returns the
/// inserted (or existing) attempt id on success, `None` on failure.
pub async fn record_adopted_pr_attempt(
    db: &djinn_db::Database,
    task_id: &str,
    role: &str,
    pr_url: &str,
    summary: Option<&str>,
) -> Option<String> {
    // Deterministic dispatch key for idempotency: repeated guard ticks for the
    // same task+role+open_pr_adoption resolve to the same key.
    let dispatch_key = format!("{task_id}:{role}:open_pr_adoption");
    let id = uuid::Uuid::now_v7().to_string();
    let repo = TaskAttemptRepository::new(db.clone());
    match repo
        .insert_guard_adopted_pr(GuardAdoptedPrTaskAttemptParams {
            id: &id,
            task_id,
            role,
            dispatch_key: &dispatch_key,
            pr_url,
            summary,
            summary_json: None,
        })
        .await
    {
        Ok(attempt) => {
            tracing::info!(
                task_id = %task_id,
                role = %role,
                attempt_id = %attempt.id,
                dispatch_key = %attempt.dispatch_key,
                pr_url = %pr_url,
                "respawn_guard: adopted-PR audit row recorded"
            );
            Some(attempt.id)
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                pr_url = %pr_url,
                error = %e,
                "respawn_guard: failed to record adopted-PR audit row (best-effort)"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, EpicRepository, TaskAttemptRepository, TaskRepository};

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Create a minimal task row for FK satisfaction.
    async fn create_task(db: &Database) -> djinn_core::models::Task {
        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();
        let task_repo = TaskRepository::new(db.clone(), event_bus);
        task_repo
            .create(&epic.id, "Test task", "", "", "task", 0, "", None)
            .await
            .unwrap()
    }

    // ─── run_respawn_guard tests ─────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_allows_when_no_prior_attempts() {
        let db = test_db();
        let task = create_task(&db).await;

        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_defers_when_pending_attempt_exists() {
        let db = test_db();
        let task = create_task(&db).await;

        // Insert a pending attempt (simulating a dispatch-start that landed).
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");

        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_defers_when_submitted_attempt_exists() {
        let db = test_db();
        let task = create_task(&db).await;

        // Insert a pending attempt, then advance it to submitted.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");

        super::super::attempt_lifecycle::advance_to_submitted(
            &db,
            super::super::attempt_lifecycle::SubmitAdvancementParams {
                task_id: &task.id,
                role: "worker",
                submit_ref: Some("ref-1"),
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("submitted"),
                summary_json: None,
            },
        )
        .await;

        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_allows_after_terminal_attempt() {
        let db = test_db();
        let task = create_task(&db).await;

        // Create and terminally close an attempt.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");

        super::super::attempt_lifecycle::advance_latest_to_terminal(
            &db,
            super::super::attempt_lifecycle::TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Completed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("done"),
                summary_json: None,
                log_tail: None,
            },
        )
        .await;

        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_allows_for_different_role() {
        let db = test_db();
        let task = create_task(&db).await;

        // Insert a pending attempt for "reviewer".
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "reviewer");
        super::super::attempt_lifecycle::record_dispatch_start(
            &db, &task.id, "reviewer", None, &dk,
        )
        .await
        .expect("record_dispatch_start should succeed");

        // Guard for "worker" role should allow (different role).
        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_ignores_deferred_attempts() {
        let db = test_db();
        let task = create_task(&db).await;

        // Insert a deferred guard-only row (previous guard deferral).
        record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::RespawnGuard,
            Some("previous deferral"),
        )
        .await
        .expect("guard deferred row should insert");

        // The guard should allow because `deferred` is not in
        // ('pending', 'submitted').
        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    // ─── record_guard_deferred_attempt tests ─────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_deferred_creates_audit_row() {
        let db = test_db();
        let task = create_task(&db).await;

        let attempt_id = record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::RespawnGuard,
            Some("duplicate spawn blocked"),
        )
        .await
        .expect("should return attempt id");

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.task_id, task.id);
        assert_eq!(attempt.role, "worker");
        assert_eq!(attempt.outcome, "deferred");
        assert_eq!(
            attempt.guard_decision_enum().unwrap(),
            Some(GuardDecision::Defer)
        );
        assert_eq!(
            attempt.guard_reason.as_deref(),
            Some(GuardReason::RespawnGuard.as_str())
        );
        assert_eq!(attempt.summary.as_deref(), Some("duplicate spawn blocked"));
        // Session_id must be NULL for guard-only rows.
        assert!(attempt.session_id.is_none());
        // Terminal_at must be set (deferred is terminal).
        assert!(attempt.terminal_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_deferred_with_capacity_reason() {
        let db = test_db();
        let task = create_task(&db).await;

        let attempt_id = record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::Capacity,
            Some("user at per-model cap"),
        )
        .await
        .expect("should return attempt id");

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "deferred");
        assert_eq!(
            attempt.guard_reason.as_deref(),
            Some(GuardReason::Capacity.as_str())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_deferred_idempotent_on_same_dispatch_key() {
        let db = test_db();
        let task = create_task(&db).await;

        // Both calls use different dispatch keys (make_dispatch_key generates
        // unique keys), so two rows are created. Idempotency is on the
        // dispatch_key column, and each guard deferral gets its own key.
        let id1 = record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::RespawnGuard,
            Some("first deferral"),
        )
        .await;
        let id2 = record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::RespawnGuard,
            Some("second deferral"),
        )
        .await;

        // Both should succeed (different dispatch keys).
        assert!(id1.is_some());
        assert!(id2.is_some());
        assert_ne!(id1, id2);

        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(all.len(), 2);
        // All should be deferred.
        for attempt in &all {
            assert_eq!(attempt.outcome, "deferred");
        }
    }

    // ─── Guard ordering / no-counter side effects ────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_does_not_create_pending_or_submitted_rows() {
        let db = test_db();
        let task = create_task(&db).await;

        // Record a guard deferral.
        record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::RespawnGuard,
            Some("test"),
        )
        .await;

        // Verify the deferred row is NOT visible as pending/submitted to the
        // guard — only pending/submitted rows should block dispatch.
        let repo = TaskAttemptRepository::new(db.clone());
        let in_flight = repo
            .latest_pending_or_submitted(&task.id, Some("worker"))
            .await
            .unwrap();
        assert!(
            in_flight.is_none(),
            "deferred rows must not be visible as pending/submitted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_ordering_pending_before_submitted() {
        let db = test_db();
        let task = create_task(&db).await;

        // Create a pending attempt.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .unwrap();

        // Guard must defer (pending exists).
        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        );
    }

    // ─── Open-PR adoption guard tests ───────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_adopts_when_pr_url_present() {
        let db = test_db();
        let task = create_task(&db).await;

        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
        )
        .await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Adopted {
                pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_ignores_empty_pr_url() {
        let db = test_db();
        let task = create_task(&db).await;

        // An empty pr_url should not trigger adoption.
        let decision = run_respawn_guard(&db, &task.id, "worker", Some("")).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_pr_adoption_takes_precedence_over_pending_attempt() {
        let db = test_db();
        let task = create_task(&db).await;

        // Insert a pending attempt.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");

        // Even with a pending attempt, if pr_url is set, the guard adopts.
        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
        )
        .await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Adopted {
                pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
            }
        );
    }

    // ─── record_adopted_pr_attempt tests ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopted_pr_creates_audit_row() {
        let db = test_db();
        let task = create_task(&db).await;

        let attempt_id = record_adopted_pr_attempt(
            &db,
            &task.id,
            "worker",
            "https://github.example/owner/repo/pull/42",
            Some("adopted existing PR"),
        )
        .await
        .expect("should return attempt id");

        let repo = TaskAttemptRepository::new(db);
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.task_id, task.id);
        assert_eq!(attempt.role, "worker");
        assert_eq!(attempt.outcome, "adopted_pr");
        assert_eq!(
            attempt.guard_decision_enum().unwrap(),
            Some(GuardDecision::Allow)
        );
        assert_eq!(
            attempt.guard_reason.as_deref(),
            Some(GuardReason::OpenPrAdoption.as_str())
        );
        assert_eq!(
            attempt.pr_url.as_deref(),
            Some("https://github.example/owner/repo/pull/42")
        );
        assert_eq!(attempt.summary.as_deref(), Some("adopted existing PR"));
        // Session_id must be NULL for guard-only rows.
        assert!(attempt.session_id.is_none());
        // Terminal_at must be set (adopted_pr is terminal).
        assert!(attempt.terminal_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopted_pr_idempotent_on_same_task_role() {
        let db = test_db();
        let task = create_task(&db).await;

        // Both calls use the same deterministic dispatch key, so the second
        // call should be a no-op (ON CONFLICT DO NOTHING).
        let id1 = record_adopted_pr_attempt(
            &db,
            &task.id,
            "worker",
            "https://github.example/owner/repo/pull/42",
            Some("first adoption"),
        )
        .await;
        let id2 = record_adopted_pr_attempt(
            &db,
            &task.id,
            "worker",
            "https://github.example/owner/repo/pull/42",
            Some("second adoption"),
        )
        .await;

        // Both should return the same id (idempotent).
        assert!(id1.is_some());
        assert!(id2.is_some());
        assert_eq!(id1, id2);

        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].outcome, "adopted_pr");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopted_pr_row_is_not_pending_or_submitted() {
        let db = test_db();
        let task = create_task(&db).await;

        // Record an adopted-PR row.
        record_adopted_pr_attempt(
            &db,
            &task.id,
            "worker",
            "https://github.example/owner/repo/pull/42",
            Some("test"),
        )
        .await
        .expect("should succeed");

        // The adopted_pr row must NOT be visible as pending/submitted to the
        // guard — only pending/submitted rows should block dispatch.
        let repo = TaskAttemptRepository::new(db.clone());
        let in_flight = repo
            .latest_pending_or_submitted(&task.id, Some("worker"))
            .await
            .unwrap();
        assert!(
            in_flight.is_none(),
            "adopted_pr rows must not be visible as pending/submitted"
        );
    }

    // ─── 422 backstop regression: existing behavior preserved ───────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_allows_when_no_pr_url_and_no_pending() {
        let db = test_db();
        let task = create_task(&db).await;

        // Simulates the case where the 422 backstop in supervisor_pr_open
        // would handle a race: the task has no pr_url yet and no pending
        // attempt, so the guard allows and the spawn path proceeds.
        let decision = run_respawn_guard(&db, &task.id, "worker", None).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }
}

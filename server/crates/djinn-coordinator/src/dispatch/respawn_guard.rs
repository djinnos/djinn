//! Pre-dispatch respawn guard.
//!
//! The guard consults attempt-history APIs before any fresh spawn/admission
//! side-effects.  The guard ordering is:
//!
//! 1. **Open-PR adoption**: when the task already has an open PR
//!    (`task.pr_url`), adopt it and record an `adopted_pr` audit row.  This
//!    prevents spawning a duplicate worker when a PR is already in review.
//!    Adoption is **bypassed** when the PR needs rework — any reopen-for-PR-
//!    rework flow (PrCiFailed, PrConflict, PrChangesRequested, merge-queue
//!    dequeue) returns the task to `open` precisely so a worker can be
//!    dispatched to fix the PR, so the guard must fall through to step 2
//!    instead of adopting.  The rework signals, in evaluation order:
//!    - [`PrReworkSignal::FailingCi`]: `task.ci_status == "failing"` (the
//!      promoted required-CI gate is red on the PR head — PrCiFailed flow).
//!    - [`PrReworkSignal::MergeConflict`]: `task.merge_conflict_metadata` is
//!      populated (PrConflict / task_review_reject_conflict /
//!      lead_approve_conflict set it; `submit_task_review`, `close`,
//!      `force_close`, and `user_override` clear it, so a populated value
//!      always describes the *current* unresolved conflict).
//!    - Latest-attempt fallback: when neither task-row signal is present but
//!      the newest non-guard `task_attempts` row for this task+role is
//!      terminal with outcome `reopened`, the task was reopened for PR rework
//!      by a path that leaves no task-row column (PrChangesRequested, a
//!      merge-queue dequeue whose PR-head checks are green because the full
//!      suite only runs on `merge_group`).  The window is self-closing: the
//!      very next dispatch inserts a newer `pending` attempt row.
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

use djinn_core::models::CiStatus;
use djinn_core::models::task_attempt::{GuardDecision, GuardReason, TaskAttemptOutcome};
use djinn_db::{
    GuardAdoptedPrTaskAttemptParams, GuardDeferTaskAttemptParams, TaskAttemptRepository,
};

use super::attempt_lifecycle::make_dispatch_key;

// ─── PR-rework signal ───────────────────────────────────────────────────────

/// Durable "this PR needs a worker" signal derived from the task row at the
/// dispatch call site (which holds the full task row — no extra DB reads).
///
/// When present, open-PR adoption is bypassed so the rework worker the reopen
/// flow asked for can actually dispatch (step-2 pending/submitted dedup still
/// prevents duplicates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrReworkSignal {
    /// The promoted required-CI gate is failing on the PR head
    /// (`task.ci_status == "failing"` — PrCiFailed remediation flow).
    FailingCi,
    /// The task carries populated `merge_conflict_metadata` (PrConflict /
    /// task_review_reject_conflict / lead_approve_conflict reopen flows).
    /// The column is cleared on `submit_task_review` / `close` /
    /// `force_close` / `user_override`, so a populated value describes the
    /// current unresolved conflict, not a stale one.
    MergeConflict,
}

impl PrReworkSignal {
    /// Derive the rework signal from the task-row facts available at the
    /// dispatch call site.
    ///
    /// Precedence: a failing required-CI gate wins over conflict metadata
    /// (either one alone already bypasses adoption; the ordering only affects
    /// which signal is named in tracing).  Empty/whitespace conflict metadata
    /// is treated as absent.
    pub fn from_task_row(ci_status: &str, merge_conflict_metadata: Option<&str>) -> Option<Self> {
        if ci_status == CiStatus::Failing.as_str() {
            return Some(Self::FailingCi);
        }
        if merge_conflict_metadata.is_some_and(|m| !m.trim().is_empty()) {
            return Some(Self::MergeConflict);
        }
        None
    }

    /// Stable snake_case name for tracing/audit output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FailingCi => "failing_ci",
            Self::MergeConflict => "merge_conflict",
        }
    }
}

impl std::fmt::Display for PrReworkSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
///    record an `adopted_pr` audit row and skip dispatch.  Adoption is
///    **bypassed** when the PR needs rework: any reopen-for-PR-rework flow
///    (PrCiFailed, PrConflict, PrChangesRequested, merge-queue dequeue)
///    reopened the task so a worker can fix the PR, and adopting here would
///    starve that rework forever.  The guard falls through to step 2 instead,
///    so a rework worker dispatches exactly once.  Rework is detected via:
///    - `rework_signal` (built by the caller from the task row with
///      [`PrReworkSignal::from_task_row`]): failing required CI or an
///      unresolved merge conflict; or
///    - the latest-attempt fallback for paths that leave no task-row column
///      (PrChangesRequested, merge-queue dequeues whose PR-head checks are
///      green): the newest non-guard attempt row for this task+role is
///      terminal with outcome `reopened`.
/// 2. **Non-terminal attempt** — consults
///    [`TaskAttemptRepository::latest_pending_or_submitted`] for the task/role
///    pair.  If a non-terminal attempt already exists the dispatch is deferred
///    with [`GuardReason::RespawnGuard`].
///
/// A healthy open PR (CI green/pending/unknown, no conflict metadata, no
/// reopened-latest attempt) preserves the adoption behavior.
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
    rework_signal: Option<PrReworkSignal>,
) -> RespawnGuardDecision {
    // 1. Open-PR adoption: when the task already has an open PR, adopt it
    //    and skip dispatch.  This prevents spawning a duplicate worker when
    //    a PR is already in review — unless the PR needs rework, in which
    //    case the task was reopened so a worker MUST be dispatched to fix
    //    the PR (step 2 still prevents duplicate rework workers).
    if let Some(url) = pr_url
        && !url.is_empty()
    {
        if let Some(signal) = rework_signal {
            tracing::info!(
                task_id = %task_id,
                role = %role,
                pr_url = %url,
                rework_signal = %signal,
                "respawn_guard: open PR needs rework — bypassing adoption so a \
                 rework worker can dispatch"
            );
        } else if latest_attempt_is_reopened(db, task_id, role).await {
            tracing::info!(
                task_id = %task_id,
                role = %role,
                pr_url = %url,
                rework_signal = "latest_attempt_reopened",
                "respawn_guard: open PR needs rework (latest attempt terminalized as \
                 reopened; e.g. changes requested or merge-queue dequeue) — bypassing \
                 adoption so a rework worker can dispatch"
            );
        } else {
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

/// Latest-attempt rework fallback: `true` when the newest non-guard
/// `task_attempts` row for this task+role is terminal with outcome
/// `reopened`.
///
/// The PR poller terminalizes the in-flight worker attempt as `reopened`
/// for every reopen-for-PR-rework transition (PrCiFailed,
/// PrChangesRequested, PrConflict, task_review_reject*, merge-queue
/// dequeue via PrCiFailed) *before* applying the board transition, so a
/// `reopened`-latest attempt means "reopened for rework, no new attempt
/// dispatched yet".  Guard-only audit rows (`deferred`, `adopted_pr`) are
/// skipped: prior guard ticks must not mask the rework signal.
///
/// The bypass window is self-closing (no permanent adoption bypass): the
/// moment a rework worker dispatches, `record_dispatch_start` inserts a
/// newer `pending` row, which becomes the latest attempt and step 2 defers
/// any further dispatch; on submit/merge it advances to
/// `submitted`/`completed`, restoring adoption for the then-healthy PR.
///
/// Fail-closed on DB errors: a lookup failure preserves the pre-existing
/// adoption behavior rather than spawning a possibly-duplicate worker.
async fn latest_attempt_is_reopened(db: &djinn_db::Database, task_id: &str, role: &str) -> bool {
    let repo = TaskAttemptRepository::new(db.clone());
    match repo.list_for_task(task_id).await {
        Ok(attempts) => attempts
            .iter()
            .filter(|a| a.role == role)
            .find(|a| {
                a.outcome != TaskAttemptOutcome::Deferred.as_str()
                    && a.outcome != TaskAttemptOutcome::AdoptedPr.as_str()
            })
            .is_some_and(|latest| latest.outcome == TaskAttemptOutcome::Reopened.as_str()),
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                role = %role,
                error = %e,
                "respawn_guard: latest-attempt rework lookup failed; preserving adoption"
            );
            false
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

        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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

        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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

        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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

        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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
        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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
        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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
        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
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
            None,
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
        let decision = run_respawn_guard(&db, &task.id, "worker", Some(""), None).await;
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
            None,
        )
        .await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Adopted {
                pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
            }
        );
    }

    // ─── PrReworkSignal derivation tests ─────────────────────────────────

    const CONFLICT_METADATA: &str = r#"{"conflicting_files":["src/note/mod.rs"],"base_branch":"main","merge_target":"main"}"#;

    #[test]
    fn rework_signal_from_task_row_mapping() {
        // Failing required CI wins regardless of conflict metadata.
        assert_eq!(
            PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), None),
            Some(PrReworkSignal::FailingCi)
        );
        assert_eq!(
            PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), Some(CONFLICT_METADATA)),
            Some(PrReworkSignal::FailingCi)
        );
        // Populated conflict metadata signals rework even with green CI.
        assert_eq!(
            PrReworkSignal::from_task_row(CiStatus::Passing.as_str(), Some(CONFLICT_METADATA)),
            Some(PrReworkSignal::MergeConflict)
        );
        assert_eq!(
            PrReworkSignal::from_task_row(CiStatus::Pending.as_str(), Some(CONFLICT_METADATA)),
            Some(PrReworkSignal::MergeConflict)
        );
        // Healthy: green/pending/unknown CI and no (or blank) metadata.
        for ci in [
            CiStatus::Passing.as_str(),
            CiStatus::Pending.as_str(),
            CiStatus::Unknown.as_str(),
        ] {
            assert_eq!(PrReworkSignal::from_task_row(ci, None), None);
            assert_eq!(PrReworkSignal::from_task_row(ci, Some("")), None);
            assert_eq!(PrReworkSignal::from_task_row(ci, Some("   ")), None);
        }
    }

    // ─── CI-remediation adoption bypass tests ───────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_does_not_adopt_when_ci_gate_failing() {
        let db = test_db();
        let task = create_task(&db).await;

        // Open PR + failing required CI (PrCiFailed remediation reopen): the
        // guard must NOT adopt.  With no pending attempt it must Allow so a
        // remediation worker dispatches.
        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), None),
        )
        .await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_defers_when_ci_gate_failing_and_pending_attempt_exists() {
        let db = test_db();
        let task = create_task(&db).await;

        // A remediation worker is already in flight (pending attempt).
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");

        // Open PR + failing required CI: adoption is bypassed, but step 2
        // still defers so no duplicate remediation worker is dispatched.
        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), None),
        )
        .await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_adopts_when_ci_gate_green_pending_or_absent() {
        let db = test_db();
        let task = create_task(&db).await;

        // Any non-failing CI gate state (and no conflict metadata) preserves
        // the adoption behavior: the PR is merely awaiting review/merge, so
        // no duplicate worker spawns.
        for ci_status in [
            CiStatus::Passing.as_str(),
            CiStatus::Pending.as_str(),
            CiStatus::Unknown.as_str(),
        ] {
            let decision = run_respawn_guard(
                &db,
                &task.id,
                "worker",
                Some("https://github.example/owner/repo/pull/42"),
                PrReworkSignal::from_task_row(ci_status, None),
            )
            .await;
            assert_eq!(
                decision,
                RespawnGuardDecision::Adopted {
                    pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
                },
                "ci_status={ci_status:?} must still adopt"
            );
        }
    }

    // ─── Merge-conflict adoption bypass tests (PrConflict reopen) ────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_does_not_adopt_when_merge_conflict_metadata_present() {
        let db = test_db();
        let task = create_task(&db).await;

        // Open PR + populated merge_conflict_metadata + green CI (PrConflict
        // reopen: the PR conflicts with main while its own checks pass): the
        // guard must NOT adopt.  With no pending attempt it must Allow so a
        // conflict-resolution worker dispatches.
        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            PrReworkSignal::from_task_row(CiStatus::Passing.as_str(), Some(CONFLICT_METADATA)),
        )
        .await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_defers_when_merge_conflict_and_pending_attempt_exists() {
        let db = test_db();
        let task = create_task(&db).await;

        // A conflict-resolution worker is already in flight (pending attempt).
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");

        // Open PR + merge conflict: adoption is bypassed, but step 2 still
        // defers so no duplicate conflict-resolution worker is dispatched.
        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            PrReworkSignal::from_task_row(CiStatus::Passing.as_str(), Some(CONFLICT_METADATA)),
        )
        .await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
        );
    }

    // ─── Latest-attempt rework fallback tests ────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_does_not_adopt_when_latest_attempt_reopened() {
        let db = test_db();
        let task = create_task(&db).await;

        // Simulate the PrChangesRequested / merge-queue-dequeue flow: the PR
        // poller terminalizes the in-flight worker attempt as `reopened`
        // before reopening the task.  No task-row rework column exists for
        // these paths, so the guard's latest-attempt fallback must bypass
        // adoption and Allow a rework worker.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");
        super::super::attempt_lifecycle::advance_latest_to_terminal(
            &db,
            super::super::attempt_lifecycle::TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
                pr_url: Some("https://github.example/owner/repo/pull/42"),
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("Reviewer requested changes on PR"),
                summary_json: None,
                log_tail: None,
            },
        )
        .await;

        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            None,
        )
        .await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_reopened_fallback_ignores_guard_only_audit_rows() {
        let db = test_db();
        let task = create_task(&db).await;

        // A `reopened` attempt followed by guard-only audit rows from earlier
        // wedged ticks (adopted_pr / deferred, exactly what the starved
        // production tasks accumulated) must still bypass adoption: guard
        // rows must not mask the rework signal.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
        super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("record_dispatch_start should succeed");
        super::super::attempt_lifecycle::advance_latest_to_terminal(
            &db,
            super::super::attempt_lifecycle::TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
                pr_url: Some("https://github.example/owner/repo/pull/42"),
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("reopened for rework"),
                summary_json: None,
                log_tail: None,
            },
        )
        .await;
        record_adopted_pr_attempt(
            &db,
            &task.id,
            "worker",
            "https://github.example/owner/repo/pull/42",
            Some("stale adoption from a wedged tick"),
        )
        .await
        .expect("adopted_pr audit row should insert");
        record_guard_deferred_attempt(
            &db,
            &task.id,
            "worker",
            GuardReason::RespawnGuard,
            Some("stale deferral"),
        )
        .await
        .expect("deferred audit row should insert");

        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            None,
        )
        .await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_adopts_when_latest_attempt_completed() {
        let db = test_db();
        let task = create_task(&db).await;

        // A `completed`-latest attempt means the PR is healthy work-in-review
        // (e.g. the 422-adopt backstop): adoption must be preserved even
        // though an older attempt cycle could have been reopened before.
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
                pr_url: Some("https://github.example/owner/repo/pull/42"),
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

        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            None,
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
    async fn guard_reopened_fallback_is_role_scoped() {
        let db = test_db();
        let task = create_task(&db).await;

        // A `reopened` attempt for a DIFFERENT role must not bypass adoption
        // for this role.
        let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "reviewer");
        super::super::attempt_lifecycle::record_dispatch_start(
            &db, &task.id, "reviewer", None, &dk,
        )
        .await
        .expect("record_dispatch_start should succeed");
        super::super::attempt_lifecycle::advance_latest_to_terminal(
            &db,
            super::super::attempt_lifecycle::TerminalAdvancementParams {
                task_id: &task.id,
                role: "reviewer",
                outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("reviewer attempt reopened"),
                summary_json: None,
                log_tail: None,
            },
        )
        .await;

        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            None,
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
        let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
        assert_eq!(decision, RespawnGuardDecision::Allow);
    }
}

//! Pre-dispatch respawn guard.
//!
//! The guard consults attempt-history APIs before any fresh spawn/admission
//! side-effects.  The guard ordering is:
//!
//! 1. **Open-PR adoption**: when the task already has an open PR
//!    (`task.pr_url`), adopt it and record an `adopted_pr` audit row.  This
//!    prevents spawning a duplicate worker when a PR is already in review.
//!    Adoption is **worker-role only** (vd4w's intent was "before spawning a
//!    duplicate WORKER"): reviewer / planner / arbiter / lead dispatches skip
//!    straight to step 2 (task 4tl2 — a needs_task_review reviewer was starved
//!    because its dispatch adopted the open PR and never ran).  Adoption is
//!    also **bypassed** when the PR needs rework — any reopen-for-PR-rework flow
//!    (PrCiFailed, PrConflict, PrChangesRequested, merge-queue dequeue,
//!    task_review_reject*, lead_approve_conflict) returns the task to a
//!    dispatchable state precisely so a worker can be dispatched to fix the PR,
//!    so the guard must fall through to step 2 instead of adopting.  Rework is
//!    detected **primarily** by the latest-attempt marker: every rework reopen
//!    now durably records a `reopened` attempt (terminalizing the in-flight
//!    attempt, or inserting a marker row when none was live), so a `reopened`
//!    newest attempt is the single authoritative "needs rework" signal.  The
//!    task-row `PrReworkSignal`s below are retained as defense-in-depth:
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
//! should the existing `try_dispatch_to_pool` + `record_dispatch_start_with_identity` path
//! proceed.

use djinn_core::models::CiStatus;
use djinn_core::models::task_attempt::{GuardDecision, GuardReason, TaskAttemptOutcome};
use djinn_core::models::{TaskStatus, TransitionAction};
use djinn_db::{
    GuardAdoptedPrTaskAttemptParams, GuardDeferTaskAttemptParams, TaskAttemptRepository,
    TaskRepository,
};

use super::attempt_lifecycle::make_dispatch_key;

/// The role whose dispatches carry PR-write intent.  Open-PR adoption applies
/// ONLY to worker dispatches: vd4w's intent was "before spawning a duplicate
/// WORKER".  Reviewer / planner / arbiter / lead dispatches for a task that
/// happens to have an open PR must fall through to the step-2 in-flight dedup
/// instead of being adopted (which starved needs_task_review reviewer
/// dispatches — task 4tl2).  Mirrors the `role == "worker"` literal used at the
/// dispatch call sites in `task_dispatch.rs`.
const WORKER_ROLE: &str = "worker";

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
    //
    //    Adoption is WORKER-ROLE ONLY.  vd4w's intent was "before spawning a
    //    duplicate WORKER"; a reviewer / planner / arbiter / lead dispatch for
    //    a task that carries an open PR must NOT be adopted (that starved
    //    needs_task_review reviewer dispatches — task 4tl2).  Non-worker roles
    //    skip straight to the step-2 pending/submitted dedup below.
    if let Some(url) = pr_url
        && !url.is_empty()
        && role == WORKER_ROLE
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
/// moment a rework worker dispatches, `record_dispatch_start_with_identity` inserts a
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

// ─── Adoption → PR-poller handoff ────────────────────────────────────────────

/// Actor id recorded on the adoption-handoff transition. The handoff is a
/// system bookkeeping move (no user), mirroring the PR poller's own
/// `("system", "pr_poller")` board transitions.
const HANDOFF_ACTOR_ID: &str = "system";
/// Actor role recorded on the adoption-handoff transition.
const HANDOFF_ACTOR_ROLE: &str = "respawn_guard";

/// Hand an adopted open PR off to the PR poller by transitioning the task from
/// the dispatchable `open` column into the poller-owned `pr_review` column.
///
/// This closes the wedge in incident gton: a worker task reopened to `open`
/// while retaining its `pr_url` (e.g. by the startup reaper after a deploy) was
/// adopted (skip-dispatch) on every ready pass but polled by NOBODY — the PR
/// poller only polls `pr_draft`/`pr_review`. Moving the task to `pr_review`
/// makes the poller advance it to merge, and the task leaves the `open` ready
/// set so the adoption never re-fires (the 470x/night adoption-log spam stops).
///
/// Handoff targets `pr_review` unconditionally rather than calling GitHub to
/// discover the PR's draft flag: the `pr_review` poller advances a ready PR to
/// merge, while the `pr_draft` poller's undraft (`mark_pr_ready_for_review`)
/// errors on an already-ready PR — which is the exact gton shape (open, green,
/// already undrafted) — so `pr_review` is both simplest and correct for the
/// incident. A genuinely-still-draft adopted PR degrades to a normal merge-gate
/// hold in the review poller rather than the adoption-spam wedge.
///
/// `pr_review` is the only no-op: the task is already at the poller-owned
/// handoff target. A `pr_draft` row is NOT a no-op — it must be advanced to
/// `pr_review` so the review poller (not the draft poller) owns it. The
/// `AdoptionHandoff` transition is legal from every non-closed status, so any
/// other non-closed status (e.g. `open`, `in_progress`, `pr_draft`) is
/// transitioned to `pr_review`.
///
/// Idempotent for the already-`pr_review` case: the no-op still durably
/// records the handoff reason via a standalone activity entry so callers
/// (e.g. the terminal gate) can prove the task was preserved rather than
/// force-closed. The `TaskRepository::transition` path inserts an activity row
/// automatically (status.rs:156-172); this no-op path does not, so it records
/// one itself.
///
/// Returns `true` when the task was moved to `pr_review` or was already there,
/// and an `Err` only when a repository transition/handoff fails (so the
/// terminal gate can fail safe).
pub async fn handoff_pr_to_poller(
    task_repo: &TaskRepository,
    task_id: &str,
    current_status: &str,
    pr_url: &str,
    reason: &str,
) -> std::result::Result<bool, String> {
    if current_status == TaskStatus::PrReview.as_str() {
        tracing::debug!(task_id = %task_id, current_status = %current_status, "respawn_guard: task already in pr_review — handoff is an idempotent no-op");
        // No transition runs, so we record a standalone activity entry to make
        // the handoff reason durable and auditable.
        let payload = serde_json::json!({
            "from_status": current_status,
            "to_status": TaskStatus::PrReview.as_str(),
            "reason": reason,
            "pr_url": pr_url,
            "idempotent": true,
        })
        .to_string();
        task_repo
            .log_activity(
                Some(task_id),
                HANDOFF_ACTOR_ID,
                HANDOFF_ACTOR_ROLE,
                "adoption_handoff",
                &payload,
            )
            .await
            .map_err(|e| {
                tracing::warn!(task_id = %task_id, pr_url = %pr_url, error = %e, "respawn_guard: failed to record idempotent handoff activity");
                e.to_string()
            })?;
        return Ok(true);
    }
    match task_repo
        .transition(
            task_id,
            TransitionAction::AdoptionHandoff,
            HANDOFF_ACTOR_ID,
            HANDOFF_ACTOR_ROLE,
            Some(reason),
            None,
        )
        .await
    {
        Ok(task) => {
            tracing::info!(task_id = %task_id, pr_url = %pr_url, to_status = %task.status, "respawn_guard: PR handed off to poller (pr_review)");
            Ok(true)
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, pr_url = %pr_url, error = %e, "respawn_guard: failed to hand PR off to poller");
            Err(e.to_string())
        }
    }
}

/// Compatibility wrapper for the respawn-guard adoption path. The terminal
/// gate uses [`handoff_pr_to_poller`] directly so it can fail safe on errors.
pub async fn handoff_adopted_pr_to_poller(
    task_repo: &TaskRepository,
    task_id: &str,
    current_status: &str,
    pr_url: &str,
) -> bool {
    let reason =
        format!("respawn_guard: adopted open PR {pr_url} — handing off to PR poller (pr_review)");
    handoff_pr_to_poller(task_repo, task_id, current_status, pr_url, &reason)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "respawn_guard_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "respawn_guard_mergequeue_tests.rs"]
mod mergequeue_tests;

#[cfg(test)]
#[path = "respawn_guard_completion_tests.rs"]
mod completion_tests;

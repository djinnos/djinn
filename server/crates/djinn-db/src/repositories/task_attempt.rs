// djinn:allow-oversize — lifecycle repository: dispatch-start through infra-death log-tail persistence; split when touched substantively.
use djinn_core::models::task_attempt::{
    GuardDecision, GuardReason, TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN, TASK_ATTEMPT_LOG_TAIL_MAX_LEN,
    TASK_ATTEMPT_SUMMARY_MAX_LEN, TaskAttempt, TaskAttemptHistoryRow, TaskAttemptLedgerRow,
    TaskAttemptOutcome, TaskAttemptPromptSummary,
};

use uuid::Uuid;

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

/// Summary of a completed task blocker parent for prompt context.
#[derive(Clone, Debug)]
pub struct CompletedParentSummary {
    pub task_id: String,
    pub short_id: String,
    pub title: String,
    pub terminal_at: String,
    pub latest_completed_attempt: Option<TaskAttemptPromptSummary>,
}

pub struct TaskAttemptRepository {
    db: Database,
}

/// Bound on re-allocations when an auto-allocated `attempt_seq` loses the
/// `(task_id, attempt_seq)` unique race to a concurrent inserter.  Every
/// conflict round has exactly one winner, so a writer needs at most
/// (concurrent writers) tries; the bound only backstops a pathological storm,
/// and hitting it surfaces the underlying unique-violation error unchanged.
const ATTEMPT_SEQ_ALLOC_RETRIES: usize = 16;

/// A `pending` attempt row identified as orphaned by
/// [`TaskAttemptRepository::list_orphaned_pending`]: older than the caller's
/// threshold with no live `task_run` and no `running` session for its task.
#[derive(Clone, Debug)]
pub struct OrphanedPendingAttempt {
    pub id: String,
    pub task_id: String,
    pub role: String,
    pub dispatch_key: String,
    pub created_at: String,
}

/// Parameters for creating or idempotently returning a pending attempt row.
#[derive(Clone, Debug)]
pub struct CreateTaskAttemptParams<'a> {
    pub id: &'a str,
    pub task_id: &'a str,
    pub role: &'a str,
    pub dispatch_key: &'a str,
    pub session_id: Option<&'a str>,
    /// If `None`, the next per-task `attempt_seq` is allocated automatically.
    pub attempt_seq: Option<i32>,
}

/// Parameters for advancing an attempt to `submitted`.
#[derive(Clone, Debug)]
pub struct SubmitTaskAttemptParams<'a> {
    pub id: &'a str,
    pub submit_ref: Option<&'a str>,
    pub checkpoint_ref: Option<&'a str>,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
    pub log_tail: Option<&'a str>,
}

/// Parameters for advancing an attempt to a terminal outcome.
#[derive(Clone, Debug)]
pub struct TerminalTaskAttemptParams<'a> {
    pub id: &'a str,
    pub outcome: TaskAttemptOutcome,
    pub pr_url: Option<&'a str>,
    pub submit_ref: Option<&'a str>,
    pub checkpoint_ref: Option<&'a str>,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
    pub log_tail: Option<&'a str>,
}

/// Evidence recorded consistently on every pending member terminalized as part
/// of an exact dispatch group.
#[derive(Clone, Debug, Default)]
pub struct DispatchGroupTerminalEvidence<'a> {
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
}

/// Stable result of exact dispatch-group terminalization. An empty list is an
/// idempotent repeat/no-op; returned IDs are sorted for deterministic callers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispatchGroupTerminalization {
    pub updated_attempt_ids: Vec<String>,
}

/// Parameters for inserting a guard-only deferred attempt row.
#[derive(Clone, Debug)]
pub struct GuardDeferTaskAttemptParams<'a> {
    pub id: &'a str,
    pub task_id: &'a str,
    pub role: &'a str,
    pub dispatch_key: &'a str,
    pub decision: GuardDecision,
    pub reason: GuardReason,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
    pub log_tail: Option<&'a str>,
}

/// Parameters for inserting a guard-only adopted-PR attempt row.
///
/// Similar to [`GuardDeferTaskAttemptParams`] but uses `outcome = 'adopted_pr'`
/// instead of `outcome = 'deferred'`. The adopted-PR row is terminal and records
/// that an existing open PR was adopted rather than spawning a duplicate worker.
#[derive(Clone, Debug)]
pub struct GuardAdoptedPrTaskAttemptParams<'a> {
    pub id: &'a str,
    pub task_id: &'a str,
    pub role: &'a str,
    pub dispatch_key: &'a str,
    pub pr_url: &'a str,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
}

/// Parameters for inserting a durable `reopened` rework-marker attempt row.
///
/// A rework-marker row is a synthetic terminal `reopened` attempt inserted when
/// a rework reopen (PrCiFailed / PrChangesRequested / PrConflict /
/// task_review_reject* / lead_approve_conflict / merge-queue dequeue) fires but
/// no in-flight `pending`/`submitted` attempt existed to terminalize. It gives
/// the respawn guard's latest-attempt gate a durable "this PR needs a worker"
/// signal even for reopens that leave no live attempt behind (the kv6i
/// invisible-reopen gap). Idempotent on `dispatch_key`
/// (`{task_id}:{role}:rework_marker`): on conflict the marker is re-asserted
/// (its `created_at` refreshed to now) so it becomes the newest attempt again
/// after a later non-reopened attempt landed — the incident-gton merge-queue
/// requeue-loop fix.
#[derive(Clone, Debug)]
pub struct ReworkMarkerTaskAttemptParams<'a> {
    pub id: &'a str,
    pub task_id: &'a str,
    pub role: &'a str,
    pub dispatch_key: &'a str,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
}

/// Parameters for filling previously-null refs/SHAs/summary/log_tail without
/// changing the outcome/lifecycle.
#[derive(Clone, Debug, Default)]
pub struct FillTaskAttemptParams<'a> {
    pub id: &'a str,
    pub checkpoint_ref: Option<&'a str>,
    pub submit_ref: Option<&'a str>,
    pub pr_url: Option<&'a str>,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub github_publication_error: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub summary_json: Option<&'a str>,
    pub log_tail: Option<&'a str>,
}

impl TaskAttemptRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    fn validate_dispatch_key(s: &str) -> Result<()> {
        if s.is_empty() {
            return Err(DbError::InvalidData(
                "dispatch_key must not be empty".to_owned(),
            ));
        }
        if s.len() > TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN {
            return Err(DbError::InvalidData(format!(
                "dispatch_key exceeds max length of {TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN}"
            )));
        }
        Ok(())
    }

    fn validate_bounded_field(name: &str, value: Option<&str>, max: usize) -> Result<()> {
        if let Some(v) = value
            && v.len() > max
        {
            return Err(DbError::InvalidData(format!(
                "{name} exceeds max length of {max}"
            )));
        }
        Ok(())
    }

    fn validate_summary_json(value: Option<&str>) -> Result<()> {
        if let Some(v) = value {
            if v.is_empty() {
                return Err(DbError::InvalidData(
                    "summary_json must be a non-empty JSON object".to_owned(),
                ));
            }
            let parsed: serde_json::Value = serde_json::from_str(v)?;
            if !parsed.is_object() {
                return Err(DbError::InvalidData(
                    "summary_json must be a JSON object".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn validate_summary(value: Option<&str>) -> Result<()> {
        Self::validate_bounded_field("summary", value, TASK_ATTEMPT_SUMMARY_MAX_LEN)
    }

    fn validate_log_tail(value: Option<&str>) -> Result<()> {
        Self::validate_bounded_field("log_tail", value, TASK_ATTEMPT_LOG_TAIL_MAX_LEN)
    }

    /// Create or return an existing pending attempt row keyed by `dispatch_key`.
    ///
    /// Idempotency: on conflict with `dispatch_key`, the existing row is returned
    /// unchanged.  On conflict with `(task_id, attempt_seq)` when the caller
    /// supplied a duplicate sequence, the existing row is also returned.
    pub async fn create_or_get_pending(
        &self,
        params: CreateTaskAttemptParams<'_>,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        Self::validate_dispatch_key(params.dispatch_key)?;
        if let Some(seq) = params.attempt_seq
            && seq <= 0
        {
            return Err(DbError::InvalidData(
                "attempt_seq must be positive".to_owned(),
            ));
        }

        let mut tries = 0;
        loop {
            let attempt_seq = match params.attempt_seq {
                Some(seq) => seq,
                None => self.next_attempt_seq(params.task_id).await?,
            };

            let insert = sqlx::query!(
                r#"INSERT INTO task_attempts
                    (id, task_id, role, attempt_seq, dispatch_key, session_id, outcome)
                 VALUES ($1, $2, $3, $4, $5, $6, 'pending')
                 ON CONFLICT (dispatch_key) DO NOTHING"#,
                params.id,
                params.task_id,
                params.role,
                attempt_seq,
                params.dispatch_key,
                params.session_id,
            )
            .execute(self.db.pool())
            .await;

            match insert {
                Ok(_) => break,
                Err(e)
                    if params.attempt_seq.is_none()
                        && Self::is_attempt_seq_conflict(&e)
                        && tries + 1 < ATTEMPT_SEQ_ALLOC_RETRIES =>
                {
                    tries += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }

        let row = self.get_by_dispatch_key(params.dispatch_key).await?;
        row.ok_or_else(|| DbError::Internal("task_attempt row disappeared after insert".to_owned()))
    }

    /// Allocate the next monotonic `attempt_seq` for a task.
    ///
    /// `MAX + 1` is read outside the INSERT's snapshot, so two concurrent
    /// allocators for the same task (the coordinator's `record_dispatch_start`
    /// and the slot supervisor's exact-attempt allocation race on every
    /// dispatch) can compute the same value and one loses on
    /// `task_attempts_task_id_attempt_seq_unique`.  Every auto-allocating
    /// insert therefore retries via [`Self::is_attempt_seq_conflict`], and the
    /// loser recomputes against the winner's committed row.
    async fn next_attempt_seq(&self, task_id: &str) -> Result<i32> {
        let max: Option<i32> = sqlx::query_scalar!(
            "SELECT MAX(attempt_seq) FROM task_attempts WHERE task_id = $1",
            task_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(max.unwrap_or(0) + 1)
    }

    /// `true` when the error is the losing side of a concurrent per-task
    /// `attempt_seq` allocation: a unique violation on
    /// `task_attempts_task_id_attempt_seq_unique`.
    fn is_attempt_seq_conflict(err: &sqlx::Error) -> bool {
        match err {
            sqlx::Error::Database(db) => {
                db.constraint() == Some("task_attempts_task_id_attempt_seq_unique")
            }
            _ => false,
        }
    }

    pub async fn get(&self, id: &str) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskAttempt,
            r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
             FROM task_attempts WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_dispatch_key(&self, dispatch_key: &str) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskAttempt,
            r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
             FROM task_attempts WHERE dispatch_key = $1"#,
            dispatch_key
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Advance an attempt to `submitted`.  Idempotent and forward-only: if the
    /// row is already `submitted` or terminal, it is returned unchanged.
    pub async fn advance_to_submitted(
        &self,
        params: SubmitTaskAttemptParams<'_>,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        Self::validate_summary(params.summary)?;
        Self::validate_log_tail(params.log_tail)?;
        Self::validate_summary_json(params.summary_json)?;

        sqlx::query!(
            r#"UPDATE task_attempts
               SET outcome = 'submitted',
                   submitted_at = COALESCE(submitted_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
                   updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                   submit_ref = COALESCE(submit_ref, $2),
                   checkpoint_ref = COALESCE(checkpoint_ref, $3),
                   mirror_head_sha = COALESCE(mirror_head_sha, $4),
                   github_head_sha = COALESCE(github_head_sha, $5),
                   summary = COALESCE(summary, $6),
                   summary_json = COALESCE(summary_json, $7::text::jsonb),
                   log_tail = COALESCE(log_tail, $8)
               WHERE id = $1
                 AND outcome IN ('pending', 'submitted')"#,
            params.id,
            params.submit_ref,
            params.checkpoint_ref,
            params.mirror_head_sha,
            params.github_head_sha,
            params.summary,
            params.summary_json,
            params.log_tail,
        )
        .execute(self.db.pool())
        .await?;

        let row = self.get(params.id).await?;
        row.ok_or_else(|| DbError::Internal("task_attempt row disappeared after submit".to_owned()))
    }

    /// Advance an attempt to a terminal outcome.  Forward-only and idempotent:
    /// if the row is already terminal with the same or a higher rank, it is
    /// returned unchanged; moves from non-terminal to terminal are allowed; moves
    /// from terminal to non-terminal are rejected by the SQL predicate.
    pub async fn advance_to_terminal(
        &self,
        params: TerminalTaskAttemptParams<'_>,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        if params.outcome.is_non_terminal() {
            return Err(DbError::InvalidTransition(format!(
                "advance_to_terminal requires a terminal outcome, got {}",
                params.outcome.as_str()
            )));
        }
        Self::validate_summary(params.summary)?;
        Self::validate_log_tail(params.log_tail)?;
        Self::validate_summary_json(params.summary_json)?;

        let outcome_str = params.outcome.as_str();

        sqlx::query!(
            r#"UPDATE task_attempts
               SET outcome = CASE
                     WHEN outcome IN ('pending', 'submitted')
                       OR (outcome IN ('completed', 'reopened', 'crashed', 'timed_out', 'cancelled',
                                        'loop_guard_tripped', 'spawn_failed', 'deferred', 'adopted_pr',
                                        'force_closed', 'handoff')
                           AND $2 = outcome)
                     THEN $2
                     ELSE outcome
                   END,
                   terminal_at = COALESCE(terminal_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
                   updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                   pr_url = COALESCE(pr_url, $3),
                   submit_ref = COALESCE(submit_ref, $4),
                   checkpoint_ref = COALESCE(checkpoint_ref, $5),
                   mirror_head_sha = COALESCE(mirror_head_sha, $6),
                   github_head_sha = COALESCE(github_head_sha, $7),
                   summary = COALESCE(summary, $8),
                   summary_json = COALESCE(summary_json, $9::text::jsonb),
                   log_tail = COALESCE(log_tail, $10)
               WHERE id = $1
                 AND (outcome IN ('pending', 'submitted')
                      OR (outcome = $2 AND outcome IN ('completed', 'reopened', 'crashed', 'timed_out',
                                                        'cancelled', 'loop_guard_tripped', 'spawn_failed',
                                                        'deferred', 'adopted_pr', 'force_closed', 'handoff')))"#,
            params.id,
            outcome_str,
            params.pr_url,

            params.submit_ref,
            params.checkpoint_ref,
            params.mirror_head_sha,
            params.github_head_sha,
            params.summary,
            params.summary_json,
            params.log_tail,
        )
        .execute(self.db.pool())
        .await?;

        let row = self.get(params.id).await?;
        row.ok_or_else(|| {
            DbError::Internal("task_attempt row disappeared after terminal".to_owned())
        })
    }

    /// Terminalize every and only `pending` attempt in the supplied exact,
    /// non-NULL dispatch group in one transaction. Legacy NULL-group rows are
    /// deliberately never batch-correlated.
    pub async fn terminalize_dispatch_group(
        &self,
        group_id: &str,
        outcome: TaskAttemptOutcome,
        evidence: DispatchGroupTerminalEvidence<'_>,
    ) -> Result<DispatchGroupTerminalization> {
        self.db.ensure_initialized().await?;
        Uuid::parse_str(group_id)
            .map_err(|_| DbError::InvalidData("dispatch_group_id must be a UUID".to_owned()))?;
        if outcome.is_non_terminal() {
            return Err(DbError::InvalidTransition(format!(
                "terminalize_dispatch_group requires a terminal outcome, got {}",
                outcome.as_str()
            )));
        }
        Self::validate_summary(evidence.summary)?;
        Self::validate_summary_json(evidence.summary_json)?;
        let mut tx = self.db.pool().begin().await?;
        let mut updated_attempt_ids: Vec<String> = sqlx::query_scalar(
            r#"UPDATE task_attempts
               SET outcome = $2,
                   terminal_at = COALESCE(terminal_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
                   updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                   summary = $3,
                   summary_json = $4::text::jsonb
             WHERE dispatch_group_id IS NOT NULL
               AND dispatch_group_id = $1
               AND outcome = 'pending'
             RETURNING id"#,
        ).bind(group_id).bind(outcome.as_str()).bind(evidence.summary).bind(evidence.summary_json)
        .fetch_all(&mut *tx).await?;
        tx.commit().await?;
        updated_attempt_ids.sort_unstable();
        Ok(DispatchGroupTerminalization {
            updated_attempt_ids,
        })
    }
    /// Insert a guard-only deferred attempt row. Idempotent on `dispatch_key`.
    pub async fn insert_guard_deferred(
        &self,
        params: GuardDeferTaskAttemptParams<'_>,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        Self::validate_dispatch_key(params.dispatch_key)?;
        Self::validate_summary(params.summary)?;
        Self::validate_log_tail(params.log_tail)?;
        Self::validate_summary_json(params.summary_json)?;

        let decision_str = params.decision.as_str();
        let reason_str = params.reason.as_str();
        let outcome_str = TaskAttemptOutcome::Deferred.as_str();

        let mut tries = 0;
        loop {
            let attempt_seq = self.next_attempt_seq(params.task_id).await?;

            let insert = sqlx::query!(
                r#"INSERT INTO task_attempts
                    (id, task_id, role, attempt_seq, dispatch_key, session_id, outcome,
                     guard_decision, guard_reason, summary, summary_json, log_tail, terminal_at)
                 VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8, $9, $10::text::jsonb, $11,
                         to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                 ON CONFLICT (dispatch_key) DO NOTHING"#,
                params.id,
                params.task_id,
                params.role,
                attempt_seq,
                params.dispatch_key,
                outcome_str,
                decision_str,
                reason_str,
                params.summary,
                params.summary_json,
                params.log_tail,
            )
            .execute(self.db.pool())
            .await;

            match insert {
                Ok(_) => break,
                Err(e)
                    if Self::is_attempt_seq_conflict(&e)
                        && tries + 1 < ATTEMPT_SEQ_ALLOC_RETRIES =>
                {
                    tries += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }

        let row = self.get_by_dispatch_key(params.dispatch_key).await?;
        row.ok_or_else(|| {
            DbError::Internal("guard_deferred task_attempt row disappeared after insert".to_owned())
        })
    }

    /// Insert a guard-only adopted-PR attempt row.  Idempotent on `dispatch_key`.
    ///
    /// The row is terminal (`adopted_pr` outcome) and records the existing open PR
    /// URL that was adopted.  The guard decision is `allow` (the adoption itself is
    /// the guard's resolution) and the reason is `open_pr_adoption`.
    pub async fn insert_guard_adopted_pr(
        &self,
        params: GuardAdoptedPrTaskAttemptParams<'_>,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        Self::validate_dispatch_key(params.dispatch_key)?;
        Self::validate_summary(params.summary)?;
        Self::validate_summary_json(params.summary_json)?;

        let outcome_str = TaskAttemptOutcome::AdoptedPr.as_str();
        let decision_str = GuardDecision::Allow.as_str();
        let reason_str = GuardReason::OpenPrAdoption.as_str();

        let mut tries = 0;
        loop {
            let attempt_seq = self.next_attempt_seq(params.task_id).await?;

            // Runtime-checked query: avoids sqlx compile-time cache dependency for
            // this new query.  The column/parameter types mirror `insert_guard_deferred`.
            let insert = sqlx::query(
                r#"INSERT INTO task_attempts
                    (id, task_id, role, attempt_seq, dispatch_key, session_id, outcome,
                     guard_decision, guard_reason, pr_url, summary, summary_json, terminal_at)
                 VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8, $9, $10, $11::text::jsonb,
                         to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                 ON CONFLICT (dispatch_key) DO NOTHING"#,
            )
            .bind(params.id)
            .bind(params.task_id)
            .bind(params.role)
            .bind(attempt_seq)
            .bind(params.dispatch_key)
            .bind(outcome_str)
            .bind(decision_str)
            .bind(reason_str)
            .bind(params.pr_url)
            .bind(params.summary)
            .bind(params.summary_json)
            .execute(self.db.pool())
            .await;

            match insert {
                Ok(_) => break,
                Err(e)
                    if Self::is_attempt_seq_conflict(&e)
                        && tries + 1 < ATTEMPT_SEQ_ALLOC_RETRIES =>
                {
                    tries += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }

        let row = self.get_by_dispatch_key(params.dispatch_key).await?;
        row.ok_or_else(|| {
            DbError::Internal(
                "guard_adopted_pr task_attempt row disappeared after insert".to_owned(),
            )
        })
    }

    /// Insert (or re-assert) a durable `reopened` rework-marker attempt row.
    /// Idempotent on `dispatch_key` — one marker row per (task, role).
    ///
    /// The row is terminal (`reopened` outcome) with a NULL `session_id` and no
    /// guard decision — it is a synthetic marker, not a real dispatch.  It is
    /// inserted by the rework-reopen path when no in-flight `pending`/`submitted`
    /// attempt existed to terminalize, so the respawn guard's latest-attempt
    /// gate still sees a `reopened` newest attempt and dispatches a rework
    /// worker instead of adopting the open PR (the kv6i invisible-reopen gap).
    ///
    /// **Re-assertable (incident gton):** on conflict the marker is REFRESHED
    /// (`created_at`/`terminal_at`/`updated_at` bumped to now, summary replaced)
    /// rather than left untouched.  The fixed dispatch key with the old
    /// `ON CONFLICT DO NOTHING` could not re-assert the signal after a NEWER
    /// non-reopened attempt landed (e.g. a rework worker ran and `completed`,
    /// then the merge queue rejected the PR again with no live attempt to
    /// terminalize): the stale marker stayed pinned behind the `completed` row
    /// in `created_at` order, so the respawn guard saw a non-reopened latest
    /// attempt and re-adopted the open PR — the 13-cycle merge-queue requeue
    /// loop.  Refreshing `created_at` makes the marker the newest attempt again
    /// so the guard bypasses adoption and dispatches a rework worker (which then
    /// reaches the reopen-count intervention gate).  The caller
    /// (`ensure_rework_marker`) only reaches this insert when the latest
    /// non-guard attempt is NOT already `reopened`, so a still-pending rework
    /// never triggers a needless refresh.
    pub async fn insert_rework_marker(
        &self,
        params: ReworkMarkerTaskAttemptParams<'_>,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        Self::validate_dispatch_key(params.dispatch_key)?;
        Self::validate_summary(params.summary)?;
        Self::validate_summary_json(params.summary_json)?;

        let outcome_str = TaskAttemptOutcome::Reopened.as_str();

        let mut tries = 0;
        loop {
            let attempt_seq = self.next_attempt_seq(params.task_id).await?;

            // Runtime-checked query: avoids sqlx compile-time cache dependency for
            // this new query.  The column/parameter types mirror `insert_guard_deferred`.
            let insert = sqlx::query(
                r#"INSERT INTO task_attempts
                    (id, task_id, role, attempt_seq, dispatch_key, session_id, outcome,
                     summary, summary_json, terminal_at)
                 VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8::text::jsonb,
                         to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                 ON CONFLICT (dispatch_key) DO UPDATE SET
                     outcome = EXCLUDED.outcome,
                     summary = EXCLUDED.summary,
                     summary_json = EXCLUDED.summary_json,
                     created_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                     terminal_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                     updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
            )
            .bind(params.id)
            .bind(params.task_id)
            .bind(params.role)
            .bind(attempt_seq)
            .bind(params.dispatch_key)
            .bind(outcome_str)
            .bind(params.summary)
            .bind(params.summary_json)
            .execute(self.db.pool())
            .await;

            match insert {
                Ok(_) => break,
                Err(e)
                    if Self::is_attempt_seq_conflict(&e)
                        && tries + 1 < ATTEMPT_SEQ_ALLOC_RETRIES =>
                {
                    tries += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }

        let row = self.get_by_dispatch_key(params.dispatch_key).await?;
        row.ok_or_else(|| {
            DbError::Internal("rework_marker task_attempt row disappeared after insert".to_owned())
        })
    }

    /// Fill previously-null refs/SHAs/summary/log_tail without changing outcome.
    /// Only non-null provided values are applied, and only when the current
    /// column value is NULL.  This is safe to call on terminal rows.
    pub async fn fill_nullable_fields(&self, params: FillTaskAttemptParams<'_>) -> Result<()> {
        self.db.ensure_initialized().await?;
        Self::validate_summary(params.summary)?;
        Self::validate_log_tail(params.log_tail)?;
        Self::validate_summary_json(params.summary_json)?;

        sqlx::query!(
            r#"UPDATE task_attempts
               SET checkpoint_ref = COALESCE(checkpoint_ref, $2),
                   submit_ref = COALESCE(submit_ref, $3),
                   pr_url = COALESCE(pr_url, $4),
                   mirror_head_sha = COALESCE(mirror_head_sha, $5),
                   github_head_sha = COALESCE(github_head_sha, $6),
                   github_publication_error = COALESCE(github_publication_error, $7),
                   summary = COALESCE(summary, $8),
                   summary_json = COALESCE(summary_json, $9::text::jsonb),
                   log_tail = COALESCE(log_tail, $10),
                   updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1"#,
            params.id,
            params.checkpoint_ref,
            params.submit_ref,
            params.pr_url,
            params.mirror_head_sha,
            params.github_head_sha,
            params.github_publication_error,
            params.summary,
            params.summary_json,
            params.log_tail,
        )
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// Persist infra-death log-tail capture metadata on the matching attempt.
    ///
    /// Forward-only and idempotent:
    /// - `log_tail` is COALESCE'd: first non-null value wins (never overwritten).
    /// - `summary_json` merges infra-death metadata: if the existing
    ///   `summary_json` already contains an `infra_death_log_tail` key, it is
    ///   left unchanged; otherwise the infra-death metadata object is merged
    ///   in via the JSON `||` operator.
    /// - Does not change outcome or lifecycle timestamps.
    /// - Safe to call on terminal rows.
    pub async fn persist_infra_death_log_tail(
        &self,
        attempt_id: &str,
        log_tail: Option<&str>,
        infra_death_meta_json: &str,
    ) -> Result<TaskAttempt> {
        self.db.ensure_initialized().await?;
        Self::validate_log_tail(log_tail)?;
        Self::validate_summary_json(Some(infra_death_meta_json))?;

        sqlx::query!(
            r#"UPDATE task_attempts
               SET log_tail = COALESCE(log_tail, $2),
                   summary_json = CASE
                     WHEN summary_json IS NULL THEN $3::text::jsonb
                     WHEN NOT (summary_json ? 'infra_death_log_tail')
                       THEN summary_json || $3::text::jsonb
                     ELSE summary_json
                   END,
                   updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1"#,
            attempt_id,
            log_tail,
            infra_death_meta_json,
        )
        .execute(self.db.pool())
        .await?;

        let row = self.get(attempt_id).await?;
        row.ok_or_else(|| {
            DbError::Internal(
                "task_attempt row disappeared after infra-death log-tail persist".to_owned(),
            )
        })
    }

    /// List all attempts for a task, newest-first by creation time.
    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskAttempt,
            r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
             FROM task_attempts
             WHERE task_id = $1
             ORDER BY created_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Latest non-terminal attempt for a task, optionally filtered by role.
    /// Returns `pending` or `submitted` rows only, newest-first.
    pub async fn latest_pending_or_submitted(
        &self,
        task_id: &str,
        role: Option<&str>,
    ) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        let row = if let Some(r) = role {
            sqlx::query_as!(
                TaskAttempt,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND role = $2 AND outcome IN ('pending', 'submitted')
                 ORDER BY created_at DESC
                 LIMIT 1"#,
                task_id,
                r
            )
            .fetch_optional(self.db.pool())
            .await?
        } else {
            sqlx::query_as!(
                TaskAttempt,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND outcome IN ('pending', 'submitted')
                 ORDER BY created_at DESC
                 LIMIT 1"#,
                task_id
            )
            .fetch_optional(self.db.pool())
            .await?
        };
        Ok(row)
    }

    /// Return the sole submitted worker attempt durably associated with a PR.
    ///
    /// PR lifecycle consumers must use this association rather than selecting a
    /// task's newest attempt. Multiple submitted attempts claiming one PR are a
    /// data contradiction, so deliberately fail rather than arbitrarily choose.
    pub async fn submitted_worker_for_pr_url(&self, pr_url: &str) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM task_attempts
             WHERE pr_url = $1 AND role = 'worker' AND outcome = 'submitted'",
        )
        .bind(pr_url)
        .fetch_all(self.db.pool())
        .await?;
        match ids.as_slice() {
            [] => Ok(None),
            [id] => self.get(id).await,
            _ => Err(DbError::InvalidData(
                "multiple submitted worker attempts are associated with one PR".into(),
            )),
        }
    }

    /// List `pending` attempt rows that look orphaned: created before
    /// `created_before_iso` (lexicographic compare over the ISO-8601 UTC text
    /// timestamps, same convention as `TaskRunRepository::reap_stale_running`)
    /// with no live (`starting`/`running`, NULL `ended_at`) `task_run` and no
    /// `running` session for their task.
    ///
    /// `task_attempts` rows do not carry a `task_run` FK (the session link is
    /// NULL for fresh dispatches), so "linked task_run is terminal or absent"
    /// is evaluated at task granularity: any live run or running session for
    /// the task keeps every one of its pending attempts out of the result
    /// (conservative — a later sweep re-evaluates).
    ///
    /// Used by the coordinator's orphaned-attempt reaper: a `pending` row with
    /// nothing executing behind it can never be advanced by the normal
    /// lifecycle paths, yet it hard-blocks the respawn guard for its
    /// (task, role) pair forever.
    pub async fn list_orphaned_pending(
        &self,
        created_before_iso: &str,
    ) -> Result<Vec<OrphanedPendingAttempt>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            OrphanedPendingAttempt,
            r#"SELECT ta.id AS "id!", ta.task_id AS "task_id!", ta.role AS "role!",
                ta.dispatch_key AS "dispatch_key!", ta.created_at AS "created_at!"
             FROM task_attempts ta
             WHERE ta.outcome = 'pending'
               AND ta.created_at < $1
               AND NOT EXISTS (
                   SELECT 1 FROM task_runs tr
                   WHERE tr.task_id = ta.task_id
                     AND tr.status IN ('starting', 'running')
                     AND tr.ended_at IS NULL
               )
               AND NOT EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = ta.task_id
                     AND s.status = 'running'
               )
             ORDER BY ta.created_at ASC"#,
            created_before_iso
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List `submitted` attempt rows that look orphaned AND that the PR poller
    /// can never own: created before `created_before_iso`, with no live
    /// (`starting`/`running`) `task_run` and no `running` session, and whose
    /// task falls in the poller's blind spot.
    ///
    /// Companion to [`list_orphaned_pending`] for the reaper. The PR poller
    /// drives adoption/terminalization ONLY for the statuses it polls
    /// (`pr_draft`/`pr_review`), keyed off `tasks.pr_url`. A `submitted` attempt
    /// is therefore "unowned" — and reapable — in exactly two poller-blind
    /// cases, both of which otherwise hard-block the respawn guard's step-2
    /// dedup forever:
    ///
    ///   1. The task carries NO PR (and the attempt itself no `pr_url`) — e.g.
    ///      an internal task-review rejection that reopened the task without
    ///      terminalizing the worker's `submitted` row (the ylme orphan). The
    ///      poller never sees a PR-less task.
    ///   2. The task sits in `open` — the sole dispatchable status
    ///      (`list_ready` / `build_ready_where` require `status = 'open'`), which
    ///      the poller never polls even when a stale `pr_url` was RETAINED across
    ///      a `PrConflict` reopen (`task` transition logic does not clear it).
    ///      Here the pr_url gate is bypassed entirely: no `open`-status task is
    ///      ever poller-owned, so the retained PR link is irrelevant.
    ///
    /// The old assumption "has pr_url ⟹ poller owns it" is FALSE for `open`
    /// tasks; the true rule is "the poller owns `submitted` attempts only for
    /// tasks in poller-polled statuses (`pr_draft`/`pr_review`)". The reaper
    /// finalizes every match to `reopened` (submitted work existed, and the
    /// reopened marker lets a rework worker dispatch).
    pub async fn list_orphaned_submitted_unowned(
        &self,
        created_before_iso: &str,
    ) -> Result<Vec<OrphanedPendingAttempt>> {
        self.db.ensure_initialized().await?;
        use sqlx::Row;
        // Runtime-checked query: avoids sqlx compile-time cache dependency for
        // this new query (mirrors `insert_rework_marker`/`ledger_for_task_since`).
        let rows = sqlx::query(
            r#"SELECT ta.id, ta.task_id, ta.role, ta.dispatch_key, ta.created_at
             FROM task_attempts ta
             JOIN tasks t ON t.id = ta.task_id
             WHERE ta.outcome = 'submitted'
               AND ta.created_at < $1
               AND (
                   -- Case 2: `open` is the sole dispatchable status and is never
                   -- polled — reap regardless of a retained (task or attempt) PR.
                   t.status = 'open'
                   -- Case 1: PR-less task the poller never sees (the ylme orphan).
                   OR (
                       (t.pr_url IS NULL OR t.pr_url = '')
                       AND (ta.pr_url IS NULL OR ta.pr_url = '')
                   )
               )
               AND NOT EXISTS (
                   SELECT 1 FROM task_runs tr
                   WHERE tr.task_id = ta.task_id
                     AND tr.status IN ('starting', 'running')
                     AND tr.ended_at IS NULL
               )
               AND NOT EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = ta.task_id
                     AND s.status = 'running'
               )
             ORDER BY ta.created_at ASC"#,
        )
        .bind(created_before_iso)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| OrphanedPendingAttempt {
                id: r.get("id"),
                task_id: r.get("task_id"),
                role: r.get("role"),
                dispatch_key: r.get("dispatch_key"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    /// Latest `submitted` attempt for a task, optionally filtered by role.
    pub async fn latest_submitted(
        &self,
        task_id: &str,
        role: Option<&str>,
    ) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        let row = if let Some(r) = role {
            sqlx::query_as!(
                TaskAttempt,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND role = $2 AND outcome = 'submitted'
                 ORDER BY created_at DESC
                 LIMIT 1"#,
                task_id,
                r
            )
            .fetch_optional(self.db.pool())
            .await?
        } else {
            sqlx::query_as!(
                TaskAttempt,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND outcome = 'submitted'
                 ORDER BY created_at DESC
                 LIMIT 1"#,
                task_id
            )
            .fetch_optional(self.db.pool())
            .await?
        };
        Ok(row)
    }

    /// Latest `pending` attempt for a task, optionally filtered by role.
    pub async fn latest_pending(
        &self,
        task_id: &str,
        role: Option<&str>,
    ) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        let row = if let Some(r) = role {
            sqlx::query_as!(
                TaskAttempt,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND role = $2 AND outcome = 'pending'
                 ORDER BY created_at DESC
                 LIMIT 1"#,
                task_id,
                r
            )
            .fetch_optional(self.db.pool())
            .await?
        } else {
            sqlx::query_as!(
                TaskAttempt,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", updated_at AS "updated_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND outcome = 'pending'
                 ORDER BY created_at DESC
                 LIMIT 1"#,
                task_id
            )
            .fetch_optional(self.db.pool())
            .await?
        };
        Ok(row)
    }

    /// Latest completed attempt summary for a task, used for dependency-parent
    /// prompt context. Returns None if the task has no `completed` attempt.
    pub async fn latest_completed_prompt_summary(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskAttemptPromptSummary>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskAttemptPromptSummary,
            r#"SELECT attempt_seq, role, outcome AS "outcome!", summary, created_at AS "created_at!",
                terminal_at, submit_ref, pr_url,
                guard_decision, guard_reason, checkpoint_ref, summary_json::text
             FROM task_attempts
             WHERE task_id = $1 AND outcome = 'completed'
             ORDER BY COALESCE(terminal_at, created_at) DESC
             LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Prompt-summaries for tasks that block a given task and are completed,
    /// ordered by completed-at descending then stable task id, bounded.
    pub async fn completed_blocker_parent_summaries(
        &self,
        task_id: &str,
        limit: i64,
    ) -> Result<Vec<CompletedParentSummary>> {
        self.db.ensure_initialized().await?;
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let parents = sqlx::query!(
            r#"SELECT t.id as task_id, t.short_id, t.title, t.closed_at as "terminal_at!"
             FROM blockers b
             JOIN tasks t ON t.id = b.blocking_task_id
             WHERE b.task_id = $1
               AND b.blocking_task_id IS NOT NULL
               AND t.status = 'closed'
             ORDER BY t.closed_at DESC, t.id ASC
             LIMIT $2"#,
            task_id,
            limit
        )
        .fetch_all(self.db.pool())
        .await?;
        let mut out = Vec::with_capacity(parents.len());
        for p in parents {
            let latest = self.latest_completed_prompt_summary(&p.task_id).await?;
            out.push(CompletedParentSummary {
                task_id: p.task_id,
                short_id: p.short_id,
                title: p.title,
                terminal_at: p.terminal_at,
                latest_completed_attempt: latest,
            });
        }
        Ok(out)
    }

    /// Prompt-context summaries for a task: newest-first, bounded, optionally
    /// filtered by role.
    pub async fn prompt_summaries_for_task(
        &self,
        task_id: &str,
        role: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TaskAttemptPromptSummary>> {
        self.db.ensure_initialized().await?;
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = if let Some(r) = role {
            sqlx::query_as!(
                TaskAttemptPromptSummary,
                r#"SELECT attempt_seq, role, outcome AS "outcome!", summary, created_at AS "created_at!",
                    terminal_at, submit_ref, pr_url,
                    guard_decision, guard_reason, checkpoint_ref, summary_json::text
                 FROM task_attempts
                 WHERE task_id = $1 AND role = $2
                 ORDER BY created_at DESC
                 LIMIT $3"#,
                task_id,
                r,
                limit
            )
            .fetch_all(self.db.pool())
            .await?
        } else {
            sqlx::query_as!(
                TaskAttemptPromptSummary,
                r#"SELECT attempt_seq, role, outcome AS "outcome!", summary, created_at AS "created_at!",
                    terminal_at, submit_ref, pr_url,
                    guard_decision, guard_reason, checkpoint_ref, summary_json::text
                 FROM task_attempts
                 WHERE task_id = $1
                 ORDER BY created_at DESC
                 LIMIT $2"#,
                task_id,
                limit
            )
            .fetch_all(self.db.pool())
            .await?
        };
        Ok(rows)
    }

    /// Arbiter / ledger-facing history rows for a task, newest-first by terminal
    /// time (or creation time for non-terminal rows), optionally filtered by role.
    pub async fn history_for_task(
        &self,
        task_id: &str,
        role: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TaskAttemptHistoryRow>> {
        self.db.ensure_initialized().await?;
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = if let Some(r) = role {
            sqlx::query_as!(
                TaskAttemptHistoryRow,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary,
                    checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1 AND role = $2
                 ORDER BY COALESCE(terminal_at, created_at) DESC
                 LIMIT $3"#,
                task_id,
                r,
                limit
            )
            .fetch_all(self.db.pool())
            .await?
        } else {
            sqlx::query_as!(
                TaskAttemptHistoryRow,
                r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                    outcome AS "outcome!", guard_decision, guard_reason, summary,
                    checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha, github_publication_error,
                    created_at AS "created_at!", submitted_at, terminal_at
                 FROM task_attempts
                 WHERE task_id = $1
                 ORDER BY COALESCE(terminal_at, created_at) DESC
                 LIMIT $2"#,
                task_id,
                limit
            )
            .fetch_all(self.db.pool())
            .await?
        };
        Ok(rows)
    }

    /// Arbiter/operator audit ledger for a task, optionally filtered by role
    /// and post-intervention timestamp.
    ///
    /// Returns [`TaskAttemptLedgerRow`] rows (newest-first) that include
    /// `summary_json`, explicit log-tail presence/error-class metadata, and
    /// model identity.  Raw `log_tail` text is never included in the returned
    /// rows.
    ///
    /// When `last_intervention_at` is supplied, only attempts created strictly
    /// after that timestamp are returned (post-intervention ledger).  This is
    /// the primary contract for `dispatch_ledger_json` consumers and operator
    /// audit surfaces.
    pub async fn ledger_for_task_since(
        &self,
        task_id: &str,
        role: Option<&str>,
        last_intervention_at: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TaskAttemptLedgerRow>> {
        self.db.ensure_initialized().await?;
        if limit <= 0 {
            return Ok(Vec::new());
        }

        // Runtime query: avoids sqlx compile-time cache dependency.
        // Uses the same SELECT columns as existing history/list queries.
        let base = r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                outcome, guard_decision, guard_reason, summary, summary_json::text,
                log_tail, checkpoint_ref, submit_ref, pr_url,
                mirror_head_sha, github_head_sha, github_publication_error,
                created_at, updated_at, submitted_at, terminal_at
             FROM task_attempts"#;

        let attempts: Vec<TaskAttempt> = match (role, last_intervention_at) {
            (Some(r), Some(since)) => {
                sqlx::query_as(&format!(
                    "{base} WHERE task_id = $1 AND role = $2 AND created_at > $3 \
                     ORDER BY COALESCE(terminal_at, created_at) DESC LIMIT $4"
                ))
                .bind(task_id)
                .bind(r)
                .bind(since)
                .bind(limit)
                .fetch_all(self.db.pool())
                .await?
            }
            (Some(r), None) => {
                sqlx::query_as(&format!(
                    "{base} WHERE task_id = $1 AND role = $2 \
                     ORDER BY COALESCE(terminal_at, created_at) DESC LIMIT $3"
                ))
                .bind(task_id)
                .bind(r)
                .bind(limit)
                .fetch_all(self.db.pool())
                .await?
            }
            (None, Some(since)) => {
                sqlx::query_as(&format!(
                    "{base} WHERE task_id = $1 AND created_at > $2 \
                     ORDER BY COALESCE(terminal_at, created_at) DESC LIMIT $3"
                ))
                .bind(task_id)
                .bind(since)
                .bind(limit)
                .fetch_all(self.db.pool())
                .await?
            }
            (None, None) => {
                sqlx::query_as(&format!(
                    "{base} WHERE task_id = $1 \
                     ORDER BY COALESCE(terminal_at, created_at) DESC LIMIT $2"
                ))
                .bind(task_id)
                .bind(limit)
                .fetch_all(self.db.pool())
                .await?
            }
        };

        Ok(attempts
            .iter()
            .map(TaskAttemptLedgerRow::from_task_attempt)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;
    use djinn_core::models::task_attempt::TaskAttemptOutcome;

    use super::*;
    use crate::repositories::epic::EpicRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn create_task(db: &Database) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
            task_id,
            epic.project_id,
            short_id,
            epic.id
        )
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    fn new_attempt_id() -> String {
        uuid::Uuid::now_v7().to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_or_get_pending_creates_row_and_returns_record() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-1",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        assert_eq!(attempt.id, id);
        assert_eq!(attempt.task_id, task_id);
        assert_eq!(attempt.role, "worker");
        assert_eq!(attempt.attempt_seq, 1);
        assert_eq!(attempt.dispatch_key, "dk-1");
        assert_eq!(attempt.outcome, "pending");
        assert!(attempt.session_id.is_none());
        assert!(attempt.summary.is_none());
        assert!(attempt.terminal_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_or_get_pending_is_idempotent_on_dispatch_key() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let a1 = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-idem",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        let id2 = new_attempt_id();
        let a2 = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id2,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-idem",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        assert_eq!(a1.id, a2.id);
        assert_eq!(a1.attempt_seq, a2.attempt_seq);
        let attempts = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(attempts.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attempt_seq_is_monotonic_per_task() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        for i in 1..=3 {
            let id = new_attempt_id();
            repo.create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: &format!("dk-{i}"),
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        }

        let attempts = repo.list_for_task(&task_id).await.unwrap();
        let seqs: Vec<i32> = attempts.iter().map(|a| a.attempt_seq).collect();
        assert_eq!(seqs, vec![3, 2, 1]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advance_to_submitted_moves_pending_forward() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-submit",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        let submitted = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: Some("submit-1"),
                checkpoint_ref: Some("cp-1"),
                mirror_head_sha: Some("mirror-sha"),
                github_head_sha: Some("github-sha"),
                summary: Some("summary"),
                summary_json: Some(r#"{"key": "value"}"#),
                log_tail: Some("log"),
            })
            .await
            .unwrap();

        assert_eq!(submitted.outcome, "submitted");
        assert!(submitted.submitted_at.is_some());
        assert_eq!(submitted.submit_ref.as_deref(), Some("submit-1"));
        assert_eq!(submitted.checkpoint_ref.as_deref(), Some("cp-1"));
        assert_eq!(submitted.summary.as_deref(), Some("summary"));
        // jsonb canonicalizes to a space after the colon on read-back.
        assert_eq!(
            submitted.summary_json.as_deref(),
            Some(r#"{"key": "value"}"#)
        );
        assert_eq!(submitted.log_tail.as_deref(), Some("log"));

        // Idempotent: same call again returns same row.
        let submitted2 = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();
        assert_eq!(submitted2.outcome, "submitted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advance_to_terminal_is_forward_only_and_idempotent() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-term",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        repo.advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        let terminal = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: Some("http://pr"),
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("done"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        assert_eq!(terminal.outcome, "completed");
        assert_eq!(terminal.pr_url.as_deref(), Some("http://pr"));
        assert_eq!(terminal.summary.as_deref(), Some("done"));
        assert!(terminal.terminal_at.is_some());

        // Idempotent: repeated terminal with same outcome is no-op.
        let terminal2 = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();
        assert_eq!(terminal2.id, terminal.id);
        assert_eq!(terminal2.outcome, "completed");
        // pr_url should remain filled from first terminal call.
        assert_eq!(terminal2.pr_url.as_deref(), Some("http://pr"));

        // Forward-only: a backward move to pending is not allowed by helper.
        let backward = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Reopened,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();
        // Forward-only: once a row is terminal (`completed`), the SQL predicate
        // only permits an idempotent same-outcome move, so a switch to a
        // different terminal outcome (`reopened`) is a no-op and the row keeps
        // its existing `completed` outcome. The test is mainly about
        // non-terminal -> terminal and idempotent duplicate terminal calls.
        assert_eq!(backward.outcome, "completed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fill_nullable_fields_fills_without_rolling_back_outcome() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-fill",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        repo.fill_nullable_fields(FillTaskAttemptParams {
            id: &attempt.id,
            checkpoint_ref: Some("cp-fill"),
            submit_ref: Some("submit-fill"),
            pr_url: Some("pr-fill"),
            mirror_head_sha: Some("mirror-fill"),
            github_head_sha: Some("github-fill"),
            github_publication_error: None,
            summary: Some("summary-fill"),
            summary_json: Some(r#"{"filled": true}"#),
            log_tail: Some("tail-fill"),
        })
        .await
        .unwrap();

        let filled = repo.get(&attempt.id).await.unwrap().unwrap();
        assert_eq!(filled.outcome, "completed");
        assert_eq!(filled.checkpoint_ref.as_deref(), Some("cp-fill"));
        assert_eq!(filled.submit_ref.as_deref(), Some("submit-fill"));
        assert_eq!(filled.pr_url.as_deref(), Some("pr-fill"));
        assert_eq!(filled.mirror_head_sha.as_deref(), Some("mirror-fill"));
        assert_eq!(filled.github_head_sha.as_deref(), Some("github-fill"));
        assert_eq!(filled.summary.as_deref(), Some("summary-fill"));
        assert_eq!(filled.summary_json.as_deref(), Some(r#"{"filled": true}"#));
        assert_eq!(filled.log_tail.as_deref(), Some("tail-fill"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_deferred_row_has_no_session_and_is_terminal() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .insert_guard_deferred(GuardDeferTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "guard",
                dispatch_key: "dk-guard",
                decision: GuardDecision::Defer,
                reason: GuardReason::ParkRung,
                summary: Some("parked"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        assert_eq!(attempt.outcome, "deferred");
        assert_eq!(attempt.guard_decision.as_deref(), Some("defer"));
        assert_eq!(attempt.guard_reason.as_deref(), Some("park_rung"));
        assert!(attempt.session_id.is_none());
        assert!(attempt.terminal_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_pending_or_submitted_and_lookups_work() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id1 = new_attempt_id();
        let a1 = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id1,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-latest-1",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        let id2 = new_attempt_id();
        let a2 = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id2,
                task_id: &task_id,
                role: "planner",
                dispatch_key: "dk-latest-2",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        repo.advance_to_submitted(SubmitTaskAttemptParams {
            id: &a2.id,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: None,
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        let latest = repo
            .latest_pending_or_submitted(&task_id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, a2.id);

        let latest_worker = repo
            .latest_pending(&task_id, Some("worker"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest_worker.id, a1.id);

        let latest_planner = repo
            .latest_submitted(&task_id, Some("planner"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest_planner.id, a2.id);

        let by_key = repo
            .get_by_dispatch_key("dk-latest-2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_key.id, a2.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_summaries_and_history_ordered_newest_first() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        for i in 1..=3 {
            let id = new_attempt_id();
            let attempt = repo
                .create_or_get_pending(CreateTaskAttemptParams {
                    id: &id,
                    task_id: &task_id,
                    role: "worker",
                    dispatch_key: &format!("dk-order-{i}"),
                    session_id: None,
                    attempt_seq: None,
                })
                .await
                .unwrap();
            repo.advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some(&format!("summary {i}")),
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let summaries = repo
            .prompt_summaries_for_task(&task_id, None, 2)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].attempt_seq, 3);
        assert_eq!(summaries[1].attempt_seq, 2);

        let history = repo.history_for_task(&task_id, None, 10).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].attempt_seq, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_fields_rejected_when_too_large() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = Uuid::new_v4().to_string();
        let big_summary = "x".repeat(TASK_ATTEMPT_SUMMARY_MAX_LEN + 1);
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-1",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        assert!(attempt.summary.is_none());

        let err = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some(&big_summary),
                summary_json: None,
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        let big_tail = "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN + 1);
        let err = repo
            .fill_nullable_fields(FillTaskAttemptParams {
                id: &attempt.id,
                checkpoint_ref: None,
                submit_ref: None,
                pr_url: None,
                mirror_head_sha: None,
                github_head_sha: None,
                github_publication_error: None,
                summary: None,
                summary_json: None,
                log_tail: Some(&big_tail),
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_infra_death_log_tail_sets_fields_idempotently() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-infra",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // First persist: sets log_tail and merges summary_json.
        let meta = r#"{"infra_death_log_tail":{"fetched":true,"line_count":42}}"#;
        let updated = repo
            .persist_infra_death_log_tail(&attempt.id, Some("first tail lines"), meta)
            .await
            .unwrap();
        assert_eq!(updated.log_tail.as_deref(), Some("first tail lines"));
        let sj = updated.summary_json.as_deref().unwrap();
        assert!(sj.contains("infra_death_log_tail"));

        // Second persist: log_tail is preserved (COALESCE), summary_json with
        // existing infra_death_log_tail key is also preserved.
        let meta2 = r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"timeout"}}"#;
        let updated2 = repo
            .persist_infra_death_log_tail(&attempt.id, Some("overwritten tail"), meta2)
            .await
            .unwrap();
        // log_tail: first non-null wins.
        assert_eq!(updated2.log_tail.as_deref(), Some("first tail lines"));
        // summary_json: infra_death_log_tail key already present, so no overwrite.
        assert_eq!(updated2.summary_json, updated.summary_json);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_infra_death_log_tail_merges_into_existing_summary_json() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-infra-merge",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Pre-populate summary_json with some existing key.
        repo.fill_nullable_fields(FillTaskAttemptParams {
            id: &attempt.id,
            summary_json: Some(r#"{"existing_key":"existing_value"}"#),
            ..Default::default()
        })
        .await
        .unwrap();

        // Now persist infra-death log tail: should merge.
        let meta = r#"{"infra_death_log_tail":{"fetched":true,"line_count":100}}"#;
        let updated = repo
            .persist_infra_death_log_tail(&attempt.id, Some("tail content"), meta)
            .await
            .unwrap();
        let sj = updated.summary_json.as_deref().unwrap();
        assert!(sj.contains("existing_key"));
        assert!(sj.contains("infra_death_log_tail"));
        assert_eq!(updated.log_tail.as_deref(), Some("tail content"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persist_infra_death_log_tail_works_on_terminal_rows() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-infra-term",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Terminalize the attempt.
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("crashed"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        // Persist infra-death log tail on the terminal row.
        let meta = r#"{"infra_death_log_tail":{"fetched":true,"line_count":50}}"#;
        let updated = repo
            .persist_infra_death_log_tail(&attempt.id, Some("terminal tail"), meta)
            .await
            .unwrap();
        assert_eq!(updated.outcome, "crashed"); // unchanged
        assert_eq!(updated.log_tail.as_deref(), Some("terminal tail"));
        assert!(
            updated
                .summary_json
                .as_deref()
                .unwrap()
                .contains("infra_death_log_tail")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ledger_for_task_since_returns_post_intervention_rows() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // Create 3 attempts with tiny sleeps so timestamps differ.
        let mut ids = Vec::new();
        for i in 1..=3u32 {
            let id = new_attempt_id();
            repo.create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: &format!("dk-ledger-{i}"),
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
            repo.advance_to_terminal(TerminalTaskAttemptParams {
                id: &id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: Some(&format!("https://example.com/pr/{i}")),
                submit_ref: Some(&format!("submit-{i}")),
                checkpoint_ref: Some(&format!("cp-{i}")),
                mirror_head_sha: Some(&format!("mirror-sha-{i}")),
                github_head_sha: Some(&format!("github-sha-{i}")),
                summary: Some(&format!("summary {i}")),
                summary_json: Some(&format!(r#"{{"model":"test-model-{i}"}}"#)),
                log_tail: if i == 2 {
                    Some("log tail content")
                } else {
                    None
                },
            })
            .await
            .unwrap();
            ids.push(id);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Full ledger (no filters) — should return all 3, newest first.
        let all = repo
            .ledger_for_task_since(&task_id, None, None, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].attempt_seq, 3);
        assert_eq!(all[1].attempt_seq, 2);
        assert_eq!(all[2].attempt_seq, 1);

        // Attempt 2 has log_tail; others don't.
        assert!(!all[0].log_tail_present);
        assert!(all[1].log_tail_present);
        assert!(!all[2].log_tail_present);
        // Model extracted from summary_json.
        assert_eq!(all[0].model.as_deref(), Some("test-model-3"));
        assert_eq!(all[1].model.as_deref(), Some("test-model-2"));
        // summary_json is present.
        assert!(all[0].summary_json.is_some());

        // Post-intervention filter: only rows after attempt 1's creation.
        let since = &all[2].created_at; // attempt 1's created_at (oldest)
        let post = repo
            .ledger_for_task_since(&task_id, None, Some(since), 100)
            .await
            .unwrap();
        // Should include attempts 2 and 3 (created after attempt 1).
        assert!(post.len() >= 2);
        assert_eq!(post[0].attempt_seq, 3);
        assert_eq!(post[1].attempt_seq, 2);

        // Limit works.
        let limited = repo
            .ledger_for_task_since(&task_id, None, None, 1)
            .await
            .unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].attempt_seq, 3);

        // Zero limit returns empty.
        let zero = repo
            .ledger_for_task_since(&task_id, None, None, 0)
            .await
            .unwrap();
        assert!(zero.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ledger_for_task_since_filters_by_role() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // One worker attempt.
        let w_id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &w_id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-ledger-role-w",
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &w_id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("worker done"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        // One guard attempt.
        let g_id = new_attempt_id();
        repo.insert_guard_deferred(GuardDeferTaskAttemptParams {
            id: &g_id,
            task_id: &task_id,
            role: "guard",
            dispatch_key: "dk-ledger-role-g",
            decision: GuardDecision::Defer,
            reason: GuardReason::ParkRung,
            summary: Some("parked"),
            summary_json: Some(r#"{"reason":"park_rung"}"#),
            log_tail: None,
        })
        .await
        .unwrap();

        // Filter by role=worker.
        let workers = repo
            .ledger_for_task_since(&task_id, Some("worker"), None, 100)
            .await
            .unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].role, "worker");

        // Filter by role=guard.
        let guards = repo
            .ledger_for_task_since(&task_id, Some("guard"), None, 100)
            .await
            .unwrap();
        assert_eq!(guards.len(), 1);
        assert_eq!(guards[0].role, "guard");
        assert_eq!(guards[0].outcome, "deferred");
        assert_eq!(guards[0].guard_decision.as_deref(), Some("defer"));
        assert_eq!(guards[0].guard_reason.as_deref(), Some("park_rung"));
        assert!(!guards[0].log_tail_present);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ledger_for_task_since_extracts_log_tail_error_class() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-ledger-meta",
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &id,
            outcome: TaskAttemptOutcome::Crashed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("crashed"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        // Persist infra-death log-tail metadata.
        let meta = r#"{"infra_death_log_tail":{"fetched":false,"fetch_error_class":"timeout"}}"#;
        repo.persist_infra_death_log_tail(&id, Some("tail content"), meta)
            .await
            .unwrap();

        let rows = repo
            .ledger_for_task_since(&task_id, None, None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].log_tail_present);
        assert_eq!(rows[0].log_tail_error_class.as_deref(), Some("timeout"));
    }

    /// Incident gton: a re-asserted rework marker must refresh its `created_at`
    /// (so it sorts as the newest attempt again) without stacking a duplicate
    /// row. Pre-fix the fixed-key `ON CONFLICT DO NOTHING` left the marker
    /// pinned behind a newer non-reopened attempt, wedging the respawn guard
    /// into a merge-queue requeue loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_rework_marker_re_asserts_created_at_on_conflict() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);
        let dispatch_key = format!("{task_id}:worker:rework_marker");

        // First insert.
        let first = repo
            .insert_rework_marker(ReworkMarkerTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: &dispatch_key,
                summary: Some("cycle 1"),
                summary_json: None,
            })
            .await
            .unwrap();
        assert_eq!(first.outcome, "reopened");

        // A newer non-reopened terminal attempt lands (a rework worker that ran
        // and completed).
        let worker_id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &worker_id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: &format!("{task_id}:worker:dispatch-1"),
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &worker_id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("completed"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();
        let completed = repo.get(&worker_id).await.unwrap().unwrap();

        // Re-assert the marker (second merge-queue reopen, same dispatch key).
        let second = repo
            .insert_rework_marker(ReworkMarkerTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: &dispatch_key,
                summary: Some("cycle 2"),
                summary_json: None,
            })
            .await
            .unwrap();

        // Same row (idempotent id), refreshed summary, and `created_at` advanced
        // to be strictly newer than the completed attempt so it sorts first.
        assert_eq!(second.id, first.id, "re-assert must reuse the marker row");
        assert_eq!(second.outcome, "reopened");
        assert_eq!(second.summary.as_deref(), Some("cycle 2"));
        assert!(
            second.created_at >= completed.created_at,
            "re-asserted marker created_at ({}) must be >= the newer completed \
             attempt ({}) so the guard sees it as the latest attempt",
            second.created_at,
            completed.created_at,
        );

        // No duplicate marker rows.
        let markers = repo
            .list_for_task(&task_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|a| a.dispatch_key == dispatch_key)
            .count();
        assert_eq!(markers, 1, "re-assert must not stack marker rows");
    }
}

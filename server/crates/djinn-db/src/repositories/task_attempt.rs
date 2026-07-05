use djinn_core::models::task_attempt::{
    GuardDecision, GuardReason, TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN, TASK_ATTEMPT_LOG_TAIL_MAX_LEN,
    TASK_ATTEMPT_SUMMARY_MAX_LEN, TaskAttempt, TaskAttemptHistoryRow, TaskAttemptOutcome,
    TaskAttemptPromptSummary,
};
#[cfg(test)]
use uuid::Uuid;

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

pub struct TaskAttemptRepository {
    db: Database,
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

    /// Terminal outcomes whose lifecycle rank is less than or equal to `rank`.
    /// Used to build the SQL guard for forward-only terminal transitions.
    fn terminal_outcomes_at_or_before(rank: u8) -> Vec<String> {
        [
            TaskAttemptOutcome::Completed,
            TaskAttemptOutcome::Reopened,
            TaskAttemptOutcome::Crashed,
            TaskAttemptOutcome::TimedOut,
            TaskAttemptOutcome::Cancelled,
            TaskAttemptOutcome::LoopGuardTripped,
            TaskAttemptOutcome::SpawnFailed,
            TaskAttemptOutcome::Deferred,
            TaskAttemptOutcome::AdoptedPr,
            TaskAttemptOutcome::ForceClosed,
            TaskAttemptOutcome::Handoff,
        ]
        .into_iter()
        .filter(|o| o.rank() <= rank)
        .map(|o| o.as_str().to_owned())
        .collect()
    }

    /// Create or return an existing pending attempt row keyed by `dispatch_key`.
    ///
    /// Idempotency: on conflict with `dispatch_key`, the existing row is returned
    /// unchanged.  The caller-supplied `attempt_seq` must be unique per task;
    /// a duplicate sequence surfaces as a database unique-constraint error.
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

        let attempt_seq = match params.attempt_seq {
            Some(seq) => seq,
            None => self.next_attempt_seq(params.task_id).await?,
        };

        sqlx::query!(
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
        .await?;

        let row = self.get_by_dispatch_key(params.dispatch_key).await?;
        row.ok_or_else(|| DbError::Internal("task_attempt row disappeared after insert".to_owned()))
    }

    /// Allocate the next monotonic `attempt_seq` for a task.
    async fn next_attempt_seq(&self, task_id: &str) -> Result<i32> {
        let max: Option<i32> = sqlx::query_scalar!(
            "SELECT MAX(attempt_seq) FROM task_attempts WHERE task_id = $1",
            task_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(max.unwrap_or(0) + 1)
    }

    pub async fn get(&self, id: &str) -> Result<Option<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskAttempt,
            r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
    /// moves from non-terminal to terminal are allowed; moves from a terminal
    /// outcome to a higher-or-equal rank terminal outcome are allowed; moves
    /// to a weaker (lower-rank) terminal outcome or to a non-terminal outcome
    /// are rejected and the existing row is returned unchanged.
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

        // Fetch the current row to enforce a rank-forward transition in Rust.
        // Weaker terminal calls (or any backward move) return the existing row
        // unchanged without touching the database.
        let current = match self.get(params.id).await? {
            Some(row) => row,
            None => {
                return Err(DbError::Internal(
                    "task_attempt row disappeared before terminal".to_owned(),
                ));
            }
        };
        let current_outcome: TaskAttemptOutcome = current
            .outcome
            .parse()
            .map_err(|e| DbError::Internal(format!("invalid stored outcome: {e}")))?;
        if !params.outcome.is_forward_from(current_outcome) {
            return Ok(current);
        }

        let outcome_str = params.outcome.as_str();
        let allowed_terminals = Self::terminal_outcomes_at_or_before(params.outcome.rank());

        sqlx::query!(
            r#"UPDATE task_attempts
               SET outcome = $2,
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
                      OR outcome = ANY($11))"#,
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
            &allowed_terminals[..],
        )
        .execute(self.db.pool())
        .await?;

        let row = self.get(params.id).await?;
        row.ok_or_else(|| {
            DbError::Internal("task_attempt row disappeared after terminal".to_owned())
        })
    }

    /// Insert a guard-only deferred attempt row.  Idempotent on `dispatch_key`.
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
        let attempt_seq = self.next_attempt_seq(params.task_id).await?;

        sqlx::query!(
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
        .await?;

        let row = self.get_by_dispatch_key(params.dispatch_key).await?;
        row.ok_or_else(|| {
            DbError::Internal("guard_deferred task_attempt row disappeared after insert".to_owned())
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
                   summary = COALESCE(summary, $7),
                   summary_json = COALESCE(summary_json, $8::text::jsonb),
                   log_tail = COALESCE(log_tail, $9),
                   updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1"#,
            params.id,
            params.checkpoint_ref,
            params.submit_ref,
            params.pr_url,
            params.mirror_head_sha,
            params.github_head_sha,
            params.summary,
            params.summary_json,
            params.log_tail,
        )
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// List all attempts for a task, newest-first by creation time.
    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<TaskAttempt>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            TaskAttempt,
            r#"SELECT id, task_id, role, attempt_seq, dispatch_key, session_id,
                outcome AS "outcome!", guard_decision, guard_reason, summary, summary_json::text,
                log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    log_tail, checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    terminal_at, submit_ref, pr_url
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
                    terminal_at, submit_ref, pr_url
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
                    checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
                    checkpoint_ref, submit_ref, pr_url, mirror_head_sha, github_head_sha,
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
}

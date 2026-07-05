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
        let first_terminal_at = terminal.terminal_at.clone();

        // Idempotent: repeated terminal with same outcome is no-op and preserves
        // the original terminal_at and filled fields.
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
        assert_eq!(terminal2.terminal_at, first_terminal_at);
        // pr_url should remain filled from first terminal call.
        assert_eq!(terminal2.pr_url.as_deref(), Some("http://pr"));

        // Forward-only: a higher-rank terminal outcome overwrites a lower one.
        let advanced = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Handoff,
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
        assert_eq!(advanced.outcome, "handoff");
        // Previously filled fields remain (fill-forward, no rollback).
        assert_eq!(advanced.pr_url.as_deref(), Some("http://pr"));
        assert_eq!(advanced.summary.as_deref(), Some("done"));

        // Weaker terminal calls cannot overwrite a higher-rank terminal outcome.
        let weaker = repo
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
        assert_eq!(weaker.outcome, "handoff");
        assert_eq!(weaker.pr_url.as_deref(), Some("http://pr"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advance_to_submitted_does_not_roll_back_terminal() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-submit-on-terminal",
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

        let after_submit = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: Some("submit-after-terminal"),
                checkpoint_ref: Some("cp-after-terminal"),
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        assert_eq!(after_submit.outcome, "completed");
        assert!(after_submit.terminal_at.is_some());
        // Refs should not be filled on a terminal row by the submit helper.
        assert!(after_submit.submit_ref.is_none());
        assert!(after_submit.checkpoint_ref.is_none());
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
                summary: None,
                summary_json: None,
                log_tail: Some(&big_tail),
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_summary_json_rejected() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-json",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Malformed JSON is rejected.
        let err = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: Some("not json"),
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        // Non-object JSON (array) is rejected.
        let err = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: Some("[1, 2, 3]"),
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        // Non-object JSON (scalar string) is rejected.
        let err = repo
            .fill_nullable_fields(FillTaskAttemptParams {
                id: &attempt.id,
                checkpoint_ref: None,
                submit_ref: None,
                pr_url: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: Some("\"string\""),
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        // Valid JSON object is accepted.
        let submitted = repo
            .advance_to_submitted(SubmitTaskAttemptParams {
                id: &attempt.id,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: Some(r#"{"ok": true}"#),
                log_tail: None,
            })
            .await
            .unwrap();
        assert_eq!(submitted.summary_json.as_deref(), Some(r#"{"ok": true}"#));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_key_length_bound_enforced() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let long_key = "k".repeat(TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN + 1);
        let err = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: &long_key,
                session_id: None,
                attempt_seq: None,
            })
            .await;
        assert!(err.is_err());

        // Exactly the max length is allowed.
        let max_key = "k".repeat(TASK_ATTEMPT_DISPATCH_KEY_MAX_LEN);
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: &max_key,
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        assert_eq!(attempt.dispatch_key, max_key);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_historical_backfill_after_task_run_and_session_creation() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // task_attempts starts empty for a newly-created task.
        let initial_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
                .bind(&task_id)
                .fetch_one(repo.db.pool())
                .await
                .unwrap();
        assert_eq!(initial_count, 0);

        // Create a session and a task_run for the same task, mimicking the
        // pre-existing substrate. No task_attempts row should be created.
        let session_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status)
             VALUES ($1, $2, $3, 'model-1', 'worker', 'running')",
        )
        .bind(&session_id)
        .bind(&_pid)
        .bind(&task_id)
        .execute(repo.db.pool())
        .await
        .unwrap();

        let run_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status)
             VALUES ($1, $2, $3, 'new_task', 'running')",
        )
        .bind(&run_id)
        .bind(&_pid)
        .bind(&task_id)
        .execute(repo.db.pool())
        .await
        .unwrap();

        let after_preexisting_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
                .bind(&task_id)
                .fetch_one(repo.db.pool())
                .await
                .unwrap();
        assert_eq!(after_preexisting_count, 0);

        // Only an explicit repository write populates the table.
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-backfill",
                session_id: Some(&session_id),
                attempt_seq: None,
            })
            .await
            .unwrap();

        let final_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
                .bind(&task_id)
                .fetch_one(repo.db.pool())
                .await
                .unwrap();
        assert_eq!(final_count, 1);
        assert_eq!(attempt.session_id.as_deref(), Some(session_id.as_str()));
    }

    // ── AC1: duplicate dispatch-key idempotency and per-task seq uniqueness ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_deferred_idempotent_on_dispatch_key() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id1 = new_attempt_id();
        let a1 = repo
            .insert_guard_deferred(GuardDeferTaskAttemptParams {
                id: &id1,
                task_id: &task_id,
                role: "guard",
                dispatch_key: "dk-guard-idem",
                decision: GuardDecision::Defer,
                reason: GuardReason::ParkRung,
                summary: Some("parked"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        let id2 = new_attempt_id();
        let a2 = repo
            .insert_guard_deferred(GuardDeferTaskAttemptParams {
                id: &id2,
                task_id: &task_id,
                role: "guard",
                dispatch_key: "dk-guard-idem",
                decision: GuardDecision::Defer,
                reason: GuardReason::LoopThreshold,
                summary: Some("loop"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        // Same dispatch_key → same row returned, no duplicate.
        assert_eq!(a1.id, a2.id);
        assert_eq!(a1.attempt_seq, a2.attempt_seq);
        // Original decision/reason preserved (ON CONFLICT DO NOTHING).
        assert_eq!(a2.guard_decision.as_deref(), Some("defer"));
        assert_eq!(a2.guard_reason.as_deref(), Some("park_rung"));

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM task_attempts WHERE task_id = $1")
                .bind(&task_id)
                .fetch_one(repo.db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attempt_seq_independent_across_tasks() {
        let db = test_db();
        let (_pid1, task_a) = create_task(&db).await;
        let (_pid2, task_b) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // Three attempts on task A.
        for i in 1..=3 {
            let id = new_attempt_id();
            repo.create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_a,
                role: "worker",
                dispatch_key: &format!("dk-a-{i}"),
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        }

        // First attempt on task B should be seq=1, not seq=4.
        let id_b = new_attempt_id();
        let b = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id_b,
                task_id: &task_b,
                role: "worker",
                dispatch_key: "dk-b-1",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        assert_eq!(b.attempt_seq, 1);
        assert_eq!(b.task_id, task_b);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_key_unique_constraint_prevents_cross_id_collision() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // First insert succeeds.
        let id1 = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &id1,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-unique",
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

        // Second insert with same dispatch_key but different id and attempt_seq
        // still returns the original row (ON CONFLICT DO NOTHING).
        let id2 = new_attempt_id();
        let a2 = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id2,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-unique",
                session_id: None,
                attempt_seq: Some(999),
            })
            .await
            .unwrap();
        assert_eq!(a2.id, id1);
        assert_eq!(a2.attempt_seq, 1); // original seq, not 999
    }

    // ── AC2: lifecycle forward-only, terminal→nonterminal rejected ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_to_terminal_direct_skipping_submitted() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-direct-term",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        assert_eq!(attempt.outcome, "pending");

        // Advance directly from pending to terminal (skip submitted).
        let terminal = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Crashed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("crashed early"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();

        assert_eq!(terminal.outcome, "crashed");
        assert!(terminal.terminal_at.is_some());
        assert_eq!(terminal.summary.as_deref(), Some("crashed early"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advance_to_terminal_rejects_non_terminal_outcome() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-nonterm",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        let err = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Pending,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        let err = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Submitted,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        // Row unchanged (still pending).
        let row = repo.get(&attempt.id).await.unwrap().unwrap();
        assert_eq!(row.outcome, "pending");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn weaker_terminal_cannot_overwrite_completed() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-weaker",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Move to completed.
        repo.advance_to_terminal(TerminalTaskAttemptParams {
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

        // Try to overwrite with crashed (rank 32 > 30, so this IS forward).
        // But let's test the case where we go to a HIGHER terminal and then
        // try to go back.
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::ForceClosed,
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

        let after_force = repo.get(&attempt.id).await.unwrap().unwrap();
        assert_eq!(after_force.outcome, "force_closed");

        // Now try to go back to completed (weaker terminal).
        let weaker = repo
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
        assert_eq!(
            weaker.outcome, "force_closed",
            "weaker terminal must not overwrite stronger"
        );
    }

    // ── AC3: nullable guard-only rows, fill-forward, lookups, ordering ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_rows_start_with_nullable_refs() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db.clone());

        // Create a session so the FK constraint is satisfied.
        let session_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status)
             VALUES ($1, $2, $3, 'model-1', 'worker', 'running')",
        )
        .bind(&session_id)
        .bind(&project_id)
        .bind(&task_id)
        .execute(repo.db.pool())
        .await
        .unwrap();

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-nullable",
                session_id: Some(&session_id),
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Session_id is set, but refs/summaries are null.
        assert_eq!(attempt.session_id.as_deref(), Some(session_id.as_str()));
        assert!(attempt.summary.is_none());
        assert!(attempt.summary_json.is_none());
        assert!(attempt.log_tail.is_none());
        assert!(attempt.checkpoint_ref.is_none());
        assert!(attempt.submit_ref.is_none());
        assert!(attempt.pr_url.is_none());
        assert!(attempt.mirror_head_sha.is_none());
        assert!(attempt.github_head_sha.is_none());
        assert!(attempt.submitted_at.is_none());
        assert!(attempt.terminal_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_only_row_without_session_id() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-no-session",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        assert!(attempt.session_id.is_none());
        assert_eq!(attempt.outcome, "pending");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fill_forward_preserves_existing_values() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-fill-preserve",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Set initial values via submit.
        repo.advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: Some("original-submit"),
            checkpoint_ref: Some("original-cp"),
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("original-summary"),
            summary_json: None,
            log_tail: Some("original-tail"),
        })
        .await
        .unwrap();

        // Fill-nullable should NOT overwrite existing values.
        repo.fill_nullable_fields(FillTaskAttemptParams {
            id: &attempt.id,
            checkpoint_ref: Some("new-cp"),
            submit_ref: Some("new-submit"),
            pr_url: Some("new-pr"),
            mirror_head_sha: Some("new-mirror"),
            github_head_sha: Some("new-github"),
            summary: Some("new-summary"),
            summary_json: Some(r#"{"new": true}"#),
            log_tail: Some("new-tail"),
        })
        .await
        .unwrap();

        let filled = repo.get(&attempt.id).await.unwrap().unwrap();
        // Previously-set values are preserved (COALESCE behavior).
        assert_eq!(filled.submit_ref.as_deref(), Some("original-submit"));
        assert_eq!(filled.checkpoint_ref.as_deref(), Some("original-cp"));
        assert_eq!(filled.summary.as_deref(), Some("original-summary"));
        assert_eq!(filled.log_tail.as_deref(), Some("original-tail"));
        // Previously-null values are filled.
        assert_eq!(filled.pr_url.as_deref(), Some("new-pr"));
        assert_eq!(filled.mirror_head_sha.as_deref(), Some("new-mirror"));
        assert_eq!(filled.github_head_sha.as_deref(), Some("new-github"));
        assert_eq!(filled.summary_json.as_deref(), Some(r#"{"new": true}"#));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_pending_or_submitted_returns_none_when_all_terminal() {
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
                    dispatch_key: &format!("dk-all-term-{i}"),
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
        }

        let latest = repo
            .latest_pending_or_submitted(&task_id, None)
            .await
            .unwrap();
        assert!(latest.is_none(), "all attempts are terminal");

        let latest_pending = repo.latest_pending(&task_id, None).await.unwrap();
        assert!(latest_pending.is_none());

        let latest_submitted = repo.latest_submitted(&task_id, None).await.unwrap();
        assert!(latest_submitted.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_for_task_with_role_filter() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // Worker attempt.
        let w_id = new_attempt_id();
        let w = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &w_id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-role-w",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &w.id,
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

        // Planner attempt.
        let p_id = new_attempt_id();
        let p = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &p_id,
                task_id: &task_id,
                role: "planner",
                dispatch_key: "dk-role-p",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &p.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("planner done"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .unwrap();

        // Unfiltered history returns both.
        let all = repo.history_for_task(&task_id, None, 100).await.unwrap();
        assert_eq!(all.len(), 2);

        // Role-filtered history returns only worker.
        let worker_only = repo
            .history_for_task(&task_id, Some("worker"), 100)
            .await
            .unwrap();
        assert_eq!(worker_only.len(), 1);
        assert_eq!(worker_only[0].role, "worker");
        assert_eq!(worker_only[0].summary.as_deref(), Some("worker done"));

        // Role-filtered history returns only planner.
        let planner_only = repo
            .history_for_task(&task_id, Some("planner"), 100)
            .await
            .unwrap();
        assert_eq!(planner_only.len(), 1);
        assert_eq!(planner_only[0].role, "planner");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_summaries_zero_limit_returns_empty() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        // Create an attempt so there's data.
        let id = new_attempt_id();
        repo.create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task_id,
            role: "worker",
            dispatch_key: "dk-zero",
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();

        let summaries = repo
            .prompt_summaries_for_task(&task_id, None, 0)
            .await
            .unwrap();
        assert!(summaries.is_empty());

        let history = repo.history_for_task(&task_id, None, 0).await.unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_for_task_returns_empty_for_nonexistent_task() {
        let db = test_db();
        let repo = TaskAttemptRepository::new(db);

        let attempts = repo.list_for_task("nonexistent-task-id").await.unwrap();
        assert!(attempts.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_row_shape_includes_all_expected_fields() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-shape",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        repo.advance_to_submitted(SubmitTaskAttemptParams {
            id: &attempt.id,
            submit_ref: Some("submit-ref-val"),
            checkpoint_ref: Some("cp-val"),
            mirror_head_sha: Some("mirror-sha-val"),
            github_head_sha: Some("github-sha-val"),
            summary: Some("summary-val"),
            summary_json: Some(r#"{"status": "ok"}"#),
            log_tail: Some("tail-val"),
        })
        .await
        .unwrap();

        repo.advance_to_terminal(TerminalTaskAttemptParams {
            id: &attempt.id,
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some("http://example.com/pr/42"),
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

        let history = repo.history_for_task(&task_id, None, 10).await.unwrap();
        assert_eq!(history.len(), 1);
        let h = &history[0];

        assert_eq!(h.id, id);
        assert_eq!(h.task_id, task_id);
        assert_eq!(h.role, "worker");
        assert_eq!(h.attempt_seq, 1);
        assert_eq!(h.dispatch_key, "dk-shape");
        assert_eq!(h.outcome, "completed");
        assert!(h.session_id.is_none());
        assert!(h.guard_decision.is_none());
        assert!(h.guard_reason.is_none());
        assert_eq!(h.summary.as_deref(), Some("summary-val"));
        assert_eq!(h.checkpoint_ref.as_deref(), Some("cp-val"));
        assert_eq!(h.submit_ref.as_deref(), Some("submit-ref-val"));
        assert_eq!(h.pr_url.as_deref(), Some("http://example.com/pr/42"));
        assert_eq!(h.mirror_head_sha.as_deref(), Some("mirror-sha-val"));
        assert_eq!(h.github_head_sha.as_deref(), Some("github-sha-val"));
        assert!(!h.created_at.is_empty());
        assert!(h.submitted_at.is_some());
        assert!(h.terminal_at.is_some());
    }

    // ── AC4: bounded fields and JSON validity ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_dispatch_key_rejected() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let err = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: "",
                session_id: None,
                attempt_seq: None,
            })
            .await;
        assert!(err.is_err());

        let err = repo
            .insert_guard_deferred(GuardDeferTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "guard",
                dispatch_key: "",
                decision: GuardDecision::Defer,
                reason: GuardReason::ParkRung,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn negative_attempt_seq_rejected() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let err = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-neg-seq",
                session_id: None,
                attempt_seq: Some(-1),
            })
            .await;
        assert!(err.is_err());

        let err = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-zero-seq",
                session_id: None,
                attempt_seq: Some(0),
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_deferred_rejects_oversize_summary_and_log_tail() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let big_summary = "x".repeat(TASK_ATTEMPT_SUMMARY_MAX_LEN + 1);
        let err = repo
            .insert_guard_deferred(GuardDeferTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "guard",
                dispatch_key: "dk-guard-big-sum",
                decision: GuardDecision::Defer,
                reason: GuardReason::ParkRung,
                summary: Some(&big_summary),
                summary_json: None,
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        let big_tail = "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN + 1);
        let err = repo
            .insert_guard_deferred(GuardDeferTaskAttemptParams {
                id: &new_attempt_id(),
                task_id: &task_id,
                role: "guard",
                dispatch_key: "dk-guard-big-tail",
                decision: GuardDecision::Defer,
                reason: GuardReason::ParkRung,
                summary: None,
                summary_json: None,
                log_tail: Some(&big_tail),
            })
            .await;
        assert!(err.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_rejects_invalid_summary_json() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-term-json",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        // Malformed JSON rejected.
        let err = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: Some("{not valid"),
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        // Array JSON rejected.
        let err = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: Some("[1, 2]"),
                log_tail: None,
            })
            .await;
        assert!(err.is_err());

        // Row unchanged.
        let row = repo.get(&attempt.id).await.unwrap().unwrap();
        assert_eq!(row.outcome, "pending");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_rejects_oversize_summary_and_log_tail() {
        let db = test_db();
        let (_pid, task_id) = create_task(&db).await;
        let repo = TaskAttemptRepository::new(db);

        let id = new_attempt_id();
        let attempt = repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "dk-term-oversize",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();

        let big_summary = "x".repeat(TASK_ATTEMPT_SUMMARY_MAX_LEN + 1);
        let err = repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: TaskAttemptOutcome::Completed,
                pr_url: None,
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
                log_tail: Some(&big_tail),
            })
            .await;
        assert!(err.is_err());

        // Row unchanged.
        let row = repo.get(&attempt.id).await.unwrap().unwrap();
        assert_eq!(row.outcome, "pending");
    }
}

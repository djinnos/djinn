use djinn_core::models::{
    AutoSubmitReviewRecord, TaskRejectedSubmissionIntegrityRecord, VerifyRunRecord,
};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

// ─── VerifyRunRepository ──────────────────────────────────────────────────────

pub struct VerifyRunRepository {
    db: Database,
}

pub struct CreateVerifyRunParams<'a> {
    pub id: &'a str,
    pub task_run_id: &'a str,
    pub verify_source: &'a str,
    pub verify_run_id: &'a str,
    pub command_version: Option<&'a str>,
    pub profile_version: Option<&'a str>,
    pub completed_at: &'a str,
    pub result: &'a str,
    pub diff_fingerprint: &'a str,
    pub check_coverage: Option<&'a serde_json::Value>,
}

/// One command descriptor required by a final-verification plan, in plan order.
///
/// The identifier is supplied independently from the persisted JSON result so a
/// caller cannot claim an arbitrary passing result object is a complete plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequiredFinalVerificationCommand<'a> {
    pub descriptor_id: &'a str,
}

/// Complete data required to create one reusable final-verification pass.
pub struct RecordEligibleFinalVerificationPassParams<'a> {
    pub id: &'a str,
    pub task_run_id: &'a str,
    pub verify_source: &'a str,
    pub verify_run_id: &'a str,
    pub verification_attempt_id: &'a str,
    /// Required plan descriptors, in the exact order in which they must run.
    pub required_commands: &'a [RequiredFinalVerificationCommand<'a>],
    pub ordered_commands: &'a serde_json::Value,
    pub covered_checks: &'a serde_json::Value,
    pub required_checks: &'a [String],
    pub verification_input_fingerprint: &'a str,
    pub manifest_version: &'a str,
    pub environment_identity_json: &'a serde_json::Value,
    pub environment_identity_digest: &'a str,
    pub environment_identity_version: &'a str,
    pub completed_at: &'a str,
    pub diff_fingerprint: &'a str,
}

impl VerifyRunRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a new verify run record.
    pub async fn create(&self, params: CreateVerifyRunParams<'_>) -> Result<VerifyRunRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "INSERT INTO verify_runs
                (id, task_run_id, verify_source, verify_run_id,
                 command_version, profile_version, completed_at,
                 result, diff_fingerprint, check_coverage)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            params.id,
            params.task_run_id,
            params.verify_source,
            params.verify_run_id,
            params.command_version,
            params.profile_version,
            params.completed_at,
            params.result,
            params.diff_fingerprint,
            params.check_coverage,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as::<_, VerifyRunRecord>(
            r#"SELECT id, task_run_id, verify_source, verify_run_id,
                command_version, profile_version, completed_at,
                result, diff_fingerprint, check_coverage,
                source_phase, verification_attempt_id, ordered_commands,
                covered_checks, verification_input_fingerprint, manifest_version,
                environment_identity_json, environment_identity_digest,
                environment_identity_version, created_at
             FROM verify_runs WHERE id = $1"#,
        )
        .bind(params.id)
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Return a single verify run by its id.
    pub async fn get(&self, id: &str) -> Result<Option<VerifyRunRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as::<_, VerifyRunRecord>(
            r#"SELECT id, task_run_id, verify_source, verify_run_id,
                command_version, profile_version, completed_at,
                result, diff_fingerprint, check_coverage,
                source_phase, verification_attempt_id, ordered_commands,
                covered_checks, verification_input_fingerprint, manifest_version,
                environment_identity_json, environment_identity_digest,
                environment_identity_version, created_at
             FROM verify_runs WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return the most recently created verify run for a task_run.
    ///
    /// When multiple verify runs exist (e.g. re-verification after a push) the
    /// latest `created_at` wins — this is the canonical result auto-submit
    /// should evaluate.
    pub async fn latest_for_task_run(&self, task_run_id: &str) -> Result<Option<VerifyRunRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as::<_, VerifyRunRecord>(
            r#"SELECT id, task_run_id, verify_source, verify_run_id,
                command_version, profile_version, completed_at,
                result, diff_fingerprint, check_coverage,
                source_phase, verification_attempt_id, ordered_commands,
                covered_checks, verification_input_fingerprint, manifest_version,
                environment_identity_json, environment_identity_digest,
                environment_identity_version, created_at
             FROM verify_runs
             WHERE task_run_id = $1
             ORDER BY created_at DESC
             LIMIT 1"#,
        )
        .bind(task_run_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return the most recent **passing** verify run for a task (across all of
    /// its task runs) that matches the exact submission `diff_fingerprint`.
    ///
    /// This is the `(task, fingerprint)` cache lookup for the pre-approval
    /// CI-grade verification gate (proposal `uv3p`): an unchanged resubmission
    /// with a fingerprint that already passed must NOT recompile. Because
    /// `verify_runs` is keyed by `task_run_id`, this joins through `task_runs`
    /// to resolve the durable task identity, so a fresh task run can still hit a
    /// green result recorded by an earlier run for the same diff.
    ///
    /// Returns `None` when no passing run exists for the pair — the explicit
    /// cache-miss path the gate re-runs the check set for.
    pub async fn latest_pass_for_task_and_fingerprint(
        &self,
        task_id: &str,
        diff_fingerprint: &str,
    ) -> Result<Option<VerifyRunRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as::<_, VerifyRunRecord>(
            r#"SELECT vr.id, vr.task_run_id, vr.verify_source, vr.verify_run_id,
                vr.command_version, vr.profile_version, vr.completed_at,
                vr.result, vr.diff_fingerprint, vr.check_coverage,
                vr.source_phase, vr.verification_attempt_id, vr.ordered_commands,
                vr.covered_checks, vr.verification_input_fingerprint, vr.manifest_version,
                vr.environment_identity_json, vr.environment_identity_digest,
                vr.environment_identity_version, vr.created_at
             FROM verify_runs vr
             JOIN task_runs tr ON tr.id = vr.task_run_id
             WHERE tr.task_id = $1
               AND vr.diff_fingerprint = $2
               AND vr.result = 'pass'
             ORDER BY vr.created_at DESC
             LIMIT 1"#,
        )
        .bind(task_id)
        .bind(diff_fingerprint)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return all verify runs for a task_run, newest first.
    pub async fn list_for_task_run(&self, task_run_id: &str) -> Result<Vec<VerifyRunRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as::<_, VerifyRunRecord>(
            r#"SELECT id, task_run_id, verify_source, verify_run_id,
                command_version, profile_version, completed_at,
                result, diff_fingerprint, check_coverage,
                source_phase, verification_attempt_id, ordered_commands,
                covered_checks, verification_input_fingerprint, manifest_version,
                environment_identity_json, environment_identity_digest,
                environment_identity_version, created_at
             FROM verify_runs
             WHERE task_run_id = $1
             ORDER BY created_at DESC"#,
        )
        .bind(task_run_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Validate then atomically insert exactly one complete reusable pass.
    pub async fn record_eligible_final_verification_pass(
        &self,
        p: RecordEligibleFinalVerificationPassParams<'_>,
    ) -> Result<VerifyRunRecord> {
        validate_eligible_final_pass(&p)?;
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("INSERT INTO verify_runs (id,task_run_id,verify_source,verify_run_id,completed_at,result,diff_fingerprint,check_coverage,source_phase,verification_attempt_id,ordered_commands,covered_checks,verification_input_fingerprint,manifest_version,environment_identity_json,environment_identity_digest,environment_identity_version) VALUES ($1,$2,$3,$4,$5,'pass',$6,$7,'final_verification',$8,$9,$10,$11,$12,$13,$14,$15)")
            .bind(p.id).bind(p.task_run_id).bind(p.verify_source).bind(p.verify_run_id).bind(p.completed_at).bind(p.diff_fingerprint).bind(p.covered_checks).bind(p.verification_attempt_id).bind(p.ordered_commands).bind(p.covered_checks).bind(p.verification_input_fingerprint).bind(p.manifest_version).bind(p.environment_identity_json).bind(p.environment_identity_digest).bind(p.environment_identity_version).execute(&mut *tx).await?;
        let row = sqlx::query_as::<_, VerifyRunRecord>("SELECT id,task_run_id,verify_source,verify_run_id,command_version,profile_version,completed_at,result,diff_fingerprint,check_coverage,source_phase,verification_attempt_id,ordered_commands,covered_checks,verification_input_fingerprint,manifest_version,environment_identity_json,environment_identity_digest,environment_identity_version,created_at FROM verify_runs WHERE id=$1").bind(p.id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Reuse lookup requires a task-scoped, complete final-verification contract.
    pub async fn latest_compatible_passing_final_verification(
        &self,
        task_id: &str,
        fingerprint: &str,
        manifest_version: &str,
        supported_identity_version: &str,
        required_checks: &[String],
    ) -> Result<Option<VerifyRunRecord>> {
        if supported_identity_version.trim().is_empty() {
            return Err(DbError::InvalidData(
                "supported identity version is required".to_owned(),
            ));
        }
        self.db.ensure_initialized().await?;
        let required = serde_json::Value::Array(
            required_checks
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        );
        Ok(sqlx::query_as::<_, VerifyRunRecord>("SELECT vr.id,vr.task_run_id,vr.verify_source,vr.verify_run_id,vr.command_version,vr.profile_version,vr.completed_at,vr.result,vr.diff_fingerprint,vr.check_coverage,vr.source_phase,vr.verification_attempt_id,vr.ordered_commands,vr.covered_checks,vr.verification_input_fingerprint,vr.manifest_version,vr.environment_identity_json,vr.environment_identity_digest,vr.environment_identity_version,vr.created_at FROM verify_runs vr JOIN task_runs tr ON tr.id=vr.task_run_id WHERE tr.task_id=$1 AND vr.verification_input_fingerprint=$2 AND vr.manifest_version=$3 AND vr.environment_identity_version=$4 AND vr.environment_identity_digest IS NOT NULL AND vr.source_phase='final_verification' AND vr.result='pass' AND vr.covered_checks @> $5::jsonb ORDER BY vr.created_at DESC LIMIT 1").bind(task_id).bind(fingerprint).bind(manifest_version).bind(supported_identity_version).bind(required).fetch_optional(self.db.pool()).await?)
    }
}

fn validate_eligible_final_pass(p: &RecordEligibleFinalVerificationPassParams<'_>) -> Result<()> {
    for (name, value) in [
        ("attempt", p.verification_attempt_id),
        ("fingerprint", p.verification_input_fingerprint),
        ("manifest", p.manifest_version),
        ("identity digest", p.environment_identity_digest),
        ("identity version", p.environment_identity_version),
    ] {
        if value.trim().is_empty() {
            return Err(DbError::InvalidData(format!("{name} is required")));
        }
    }
    if p.required_commands.is_empty()
        || p.required_commands
            .iter()
            .enumerate()
            .any(|(index, command)| {
                command.descriptor_id.trim().is_empty()
                    || p.required_commands[index + 1..]
                        .iter()
                        .any(|other| other.descriptor_id == command.descriptor_id)
            })
    {
        return Err(DbError::InvalidData(
            "required command descriptor IDs must be non-empty and unique".to_owned(),
        ));
    }
    let commands = p
        .ordered_commands
        .as_array()
        .ok_or_else(|| DbError::InvalidData("ordered commands must be an array".to_owned()))?;
    if commands.len() != p.required_commands.len() {
        return Err(DbError::InvalidData(
            "ordered commands do not match the required command plan".to_owned(),
        ));
    }
    if commands
        .iter()
        .zip(p.required_commands)
        .any(|(command, required)| {
            !command.is_object()
                || command
                    .get("descriptor_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(required.descriptor_id)
                || command.get("result").and_then(serde_json::Value::as_str) != Some("pass")
                || command.get("passed").and_then(serde_json::Value::as_bool) != Some(true)
        })
    {
        return Err(DbError::InvalidData(
            "ordered command descriptors must match the required plan and pass".to_owned(),
        ));
    }
    let covered = p
        .covered_checks
        .as_array()
        .ok_or_else(|| DbError::InvalidData("covered checks must be an array".to_owned()))?;
    if !p
        .required_checks
        .iter()
        .all(|r| covered.iter().any(|c| c.as_str() == Some(r)))
    {
        return Err(DbError::InvalidData(
            "covered checks do not include every required check".to_owned(),
        ));
    }
    if !p.environment_identity_json.is_object() {
        return Err(DbError::InvalidData(
            "environment identity must be a JSON object".to_owned(),
        ));
    }
    Ok(())
}

// ─── AutoSubmitReviewRepository ───────────────────────────────────────────────

pub struct AutoSubmitReviewRepository {
    db: Database,
}

pub struct CreateAutoSubmitReviewParams<'a> {
    pub id: &'a str,
    pub task_run_id: &'a str,
    pub trigger_reason: &'a str,
    pub diff_fingerprint: &'a str,
    pub verify_source: Option<&'a str>,
    pub verify_run_id: Option<&'a str>,
    pub verify_timestamp: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub no_progress_streak: i32,
    pub model_called_submit_work: bool,
}

impl AutoSubmitReviewRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist a new auto-submit review record.
    pub async fn create(
        &self,
        params: CreateAutoSubmitReviewParams<'_>,
    ) -> Result<AutoSubmitReviewRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "INSERT INTO auto_submit_reviews
                (id, task_run_id, trigger_reason, diff_fingerprint,
                 verify_source, verify_run_id, verify_timestamp,
                 session_id, model_id, no_progress_streak, model_called_submit_work)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            params.id,
            params.task_run_id,
            params.trigger_reason,
            params.diff_fingerprint,
            params.verify_source,
            params.verify_run_id,
            params.verify_timestamp,
            params.session_id,
            params.model_id,
            params.no_progress_streak,
            params.model_called_submit_work,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            AutoSubmitReviewRecord,
            r#"SELECT id, task_run_id, trigger_reason, diff_fingerprint,
                verify_source, verify_run_id, verify_timestamp,
                session_id, model_id,
                no_progress_streak, model_called_submit_work, created_at
             FROM auto_submit_reviews WHERE id = $1"#,
            params.id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Return a single auto-submit review by its id.
    pub async fn get(&self, id: &str) -> Result<Option<AutoSubmitReviewRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            AutoSubmitReviewRecord,
            r#"SELECT id, task_run_id, trigger_reason, diff_fingerprint,
                verify_source, verify_run_id, verify_timestamp,
                session_id, model_id,
                no_progress_streak, model_called_submit_work, created_at
             FROM auto_submit_reviews WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return all auto-submit reviews for a task_run, newest first.
    pub async fn list_for_task_run(
        &self,
        task_run_id: &str,
    ) -> Result<Vec<AutoSubmitReviewRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            AutoSubmitReviewRecord,
            r#"SELECT id, task_run_id, trigger_reason, diff_fingerprint,
                verify_source, verify_run_id, verify_timestamp,
                session_id, model_id,
                no_progress_streak, model_called_submit_work, created_at
             FROM auto_submit_reviews
             WHERE task_run_id = $1
             ORDER BY created_at DESC"#,
            task_run_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }
}

// ─── TaskRejectedSubmissionIntegrityRepository ───────────────────────────────

/// Repository for durable task-level rejected submission fingerprints.
///
/// The live submit-work guard reloads the latest rejected fingerprint by
/// `task_id` across redispatch / new task-run boundaries. This repository
/// owns the `task_rejected_submission_integrity` table added in migration 91.
pub struct TaskRejectedSubmissionIntegrityRepository {
    db: Database,
}

/// Parameters for recording a new rejected submission fingerprint at the
/// task level.
///
/// `task_id` is required and durable across task runs; `task_run_id`,
/// `review_id`, and `activity_id` are optional associations for callers that
/// only know the task identity. `no_progress_streak` is the task-level streak
/// value as of this rejection; the repository does not mutate it on insert.
pub struct RecordTaskRejectedSubmissionParams<'a> {
    pub id: &'a str,
    pub task_id: &'a str,
    pub task_run_id: Option<&'a str>,
    pub review_id: Option<&'a str>,
    pub verdict_kind: &'a str,
    pub activity_id: Option<&'a str>,
    pub rejected_at: &'a str,
    pub diff_fingerprint: &'a str,
    pub no_progress_streak: i32,
}

impl TaskRejectedSubmissionIntegrityRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist a new rejected-submission fingerprint row at the task level.
    ///
    /// Multiple rows per `task_id` are permitted; [`Self::latest_for_task`]
    /// picks the most recent by `rejected_at` (then `created_at` as a
    /// tie-break), so this method is append-only.
    pub async fn record(
        &self,
        params: RecordTaskRejectedSubmissionParams<'_>,
    ) -> Result<TaskRejectedSubmissionIntegrityRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "INSERT INTO task_rejected_submission_integrity
                (id, task_id, task_run_id, review_id, verdict_kind,
                 activity_id, rejected_at, diff_fingerprint, no_progress_streak)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            params.id,
            params.task_id,
            params.task_run_id,
            params.review_id,
            params.verdict_kind,
            params.activity_id,
            params.rejected_at,
            params.diff_fingerprint,
            params.no_progress_streak,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity WHERE id = $1"#,
            params.id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Return the latest rejected submission fingerprint for a task.
    ///
    /// Returns `None` when no rejected fingerprint has ever been recorded for
    /// `task_id` — the explicit no-comparison path used by the live submit-work
    /// guard so historical state is never fabricated.
    ///
    /// Ordering is `rejected_at DESC, created_at DESC`: `rejected_at` is the
    /// authoritative rejection timestamp (the activity/verdict event), while
    /// `created_at` is the tie-break for rows recorded in the same wall-clock
    /// instant.
    pub async fn latest_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskRejectedSubmissionIntegrityRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity
               WHERE task_id = $1
               ORDER BY rejected_at DESC, created_at DESC
               LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return the latest task-level no-progress streak for a task.
    ///
    /// Mirrors [`Self::latest_for_task`] but returns only the streak value,
    /// defaulting to `0` when no rejected fingerprint exists (the
    /// no-comparison path).
    pub async fn latest_no_progress_streak_for_task(&self, task_id: &str) -> Result<i32> {
        Ok(self
            .latest_for_task(task_id)
            .await?
            .map(|r| r.no_progress_streak)
            .unwrap_or(0))
    }

    /// Return all rejected-submission fingerprint rows for a task, newest
    /// first.
    pub async fn list_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskRejectedSubmissionIntegrityRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity
               WHERE task_id = $1
               ORDER BY rejected_at DESC, created_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Reset the task-level no-progress streak to zero.
    ///
    /// Semantically this records a sentinel row with the *current* (incoming)
    /// diff fingerprint and a zero streak, so subsequent [`Self::latest_for_task`]
    /// lookups observe the reset. This keeps the append-only model honest: a
    /// reset is recorded, not silently mutated, so the audit trail is
    /// preserved.
    ///
    /// `reset_diff_fingerprint` is the fingerprint that triggered the reset
    /// (typically a fresh, progressed submission). Callers that do not have a
    /// fresh fingerprint should pass the *previous* latest fingerprint — the
    /// point of a reset is that streak semantics restart, not that the
    /// fingerprint changes.
    pub async fn reset_no_progress_streak(
        &self,
        task_id: &str,
        reset_diff_fingerprint: &str,
        reset_at: &str,
        task_run_id: Option<&str>,
    ) -> Result<TaskRejectedSubmissionIntegrityRecord> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO task_rejected_submission_integrity
                (id, task_id, task_run_id, verdict_kind,
                 rejected_at, diff_fingerprint, no_progress_streak)
             VALUES ($1, $2, $3, 'no_progress', $4, $5, 0)",
            id,
            task_id,
            task_run_id,
            reset_at,
            reset_diff_fingerprint,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Return a single record by its id.
    pub async fn get(&self, id: &str) -> Result<Option<TaskRejectedSubmissionIntegrityRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }
}
// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;

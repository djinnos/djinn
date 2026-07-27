//! Durable repository boundary for frozen evidence plans and command provenance.
//!
//! This module intentionally has no executor or lifecycle policy. It only admits
//! a one-time plan, appends server-authored invocation facts, and hydrates the
//! complete identity-scoped record for a later atomic finalizer.

use djinn_core::models::{
    EvidenceCommandInvocation, EvidenceFinalizedProjection, EvidencePlan, EvidencePlanCheck,
    EvidencePlanHydration,
};
use sqlx::{Postgres, Row, Transaction};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

pub struct EvidenceRepository {
    db: Database,
}

#[derive(Clone, Debug)]
pub struct InsertEvidencePlan {
    pub id: String,
    pub spike_task_id: String,
    pub session_id: String,
    pub captured_commit_sha: String,
    pub worktree_fingerprint: String,
    pub checks: Vec<InsertEvidencePlanCheck>,
}

#[derive(Clone, Debug)]
pub struct InsertEvidencePlanCheck {
    pub check_id: String,
    pub question: String,
    pub method: String,
}

#[derive(Clone, Debug)]
pub struct AppendEvidenceInvocation {
    pub id: String,
    pub plan_id: String,
    pub spike_task_id: String,
    pub session_id: String,
    pub captured_commit_sha: String,
    pub worktree_fingerprint: String,
    pub check_id: String,
    pub argv: Vec<String>,
    pub canonical_cwd: String,
    pub launch_state: String,
    pub process_state: String,
    pub launched_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub runner_failure: Option<String>,
    pub elapsed_millis: Option<i64>,
    pub timeout_millis: Option<i64>,
    pub timed_out: bool,
    pub stdout_digest: Option<String>,
    pub stdout_excerpt: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_digest: Option<String>,
    pub stderr_excerpt: Option<String>,
    pub stderr_truncated: bool,
}

#[derive(Clone, Debug)]
pub struct InsertEvidenceFinalizedProjection {
    pub id: String,
    pub plan_id: String,
    pub version: i32,
    pub payload: serde_json::Value,
}

impl EvidenceRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Insert a plan and all ordered checks as one transaction. The database's
    /// `(spike_task_id, session_id)` constraint rejects a second frozen plan.
    pub async fn insert_plan(&self, input: InsertEvidencePlan) -> Result<EvidencePlan> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let plan = Self::insert_plan_in_transaction(&mut tx, input).await?;
        tx.commit().await?;
        Ok(plan)
    }

    pub async fn insert_plan_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: InsertEvidencePlan,
    ) -> Result<EvidencePlan> {
        validate_plan(&input)?;
        let row = sqlx::query(
            "INSERT INTO evidence_plans (id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint) \
             VALUES ($1, $2, $3, $4, $5) RETURNING created_at, updated_at",
        )
        .bind(&input.id).bind(&input.spike_task_id).bind(&input.session_id)
        .bind(&input.captured_commit_sha).bind(&input.worktree_fingerprint)
        .fetch_one(&mut **tx).await?;
        let mut checks = Vec::with_capacity(input.checks.len());
        for (index, check) in input.checks.into_iter().enumerate() {
            let ordinal = i32::try_from(index + 1)
                .map_err(|_| DbError::InvalidData("too many evidence plan checks".to_owned()))?;
            sqlx::query("INSERT INTO evidence_plan_checks (plan_id, ordinal, check_id, question, method) VALUES ($1, $2, $3, $4, $5)")
                .bind(&input.id).bind(ordinal).bind(&check.check_id).bind(&check.question).bind(&check.method)
                .execute(&mut **tx).await?;
            checks.push(EvidencePlanCheck {
                plan_id: input.id.clone(),
                ordinal,
                check_id: check.check_id,
                question: check.question,
                method: check.method,
            });
        }
        Ok(EvidencePlan {
            id: input.id,
            spike_task_id: input.spike_task_id,
            session_id: input.session_id,
            captured_commit_sha: input.captured_commit_sha,
            worktree_fingerprint: input.worktree_fingerprint,
            checks,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    /// Append one immutable server-authored command event. A retry must use a
    /// new opaque id; there is deliberately no update or upsert API.
    pub async fn append_invocation(
        &self,
        input: AppendEvidenceInvocation,
    ) -> Result<EvidenceCommandInvocation> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let event = Self::append_invocation_in_transaction(&mut tx, input).await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn append_invocation_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: AppendEvidenceInvocation,
    ) -> Result<EvidenceCommandInvocation> {
        validate_invocation(&input)?;
        let argv = serde_json::to_value(&input.argv)?;
        let row = sqlx::query(
            "INSERT INTO evidence_command_invocations (id, plan_id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint, check_id, argv, canonical_cwd, launch_state, process_state, launched_at, finished_at, exit_code, signal, runner_failure, elapsed_millis, timeout_millis, timed_out, stdout_digest, stdout_excerpt, stdout_truncated, stderr_digest, stderr_excerpt, stderr_truncated) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25) RETURNING created_at"
        ).bind(&input.id).bind(&input.plan_id).bind(&input.spike_task_id).bind(&input.session_id)
        .bind(&input.captured_commit_sha).bind(&input.worktree_fingerprint).bind(&input.check_id).bind(argv)
        .bind(&input.canonical_cwd).bind(&input.launch_state).bind(&input.process_state)
        .bind(&input.launched_at).bind(&input.finished_at).bind(input.exit_code).bind(input.signal)
        .bind(&input.runner_failure).bind(input.elapsed_millis).bind(input.timeout_millis).bind(input.timed_out)
        .bind(&input.stdout_digest).bind(&input.stdout_excerpt).bind(input.stdout_truncated)
        .bind(&input.stderr_digest).bind(&input.stderr_excerpt).bind(input.stderr_truncated)
        .fetch_one(&mut **tx).await?;
        Ok(invocation_from_input(input, row.get("created_at")))
    }

    /// Insert the versioned final hand-off inside the caller's finalization
    /// transaction. `plan_id` is unique, so an existing projection is never
    /// overwritten.
    pub async fn insert_finalized_projection_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: InsertEvidenceFinalizedProjection,
    ) -> Result<EvidenceFinalizedProjection> {
        if input.version <= 0 || !input.payload.is_object() {
            return Err(DbError::InvalidData(
                "projection requires a positive version and JSON object payload".to_owned(),
            ));
        }
        let row = sqlx::query("INSERT INTO evidence_finalized_projections (id, plan_id, version, payload) VALUES ($1, $2, $3, $4) RETURNING finalized_at")
            .bind(&input.id).bind(&input.plan_id).bind(input.version).bind(&input.payload)
            .fetch_one(&mut **tx).await?;
        Ok(EvidenceFinalizedProjection {
            id: input.id,
            plan_id: input.plan_id,
            version: input.version,
            payload: input.payload,
            finalized_at: row.get("finalized_at"),
        })
    }

    pub async fn insert_finalized_projection(
        &self,
        input: InsertEvidenceFinalizedProjection,
    ) -> Result<EvidenceFinalizedProjection> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let projection = Self::insert_finalized_projection_in_transaction(&mut tx, input).await?;
        tx.commit().await?;
        Ok(projection)
    }

    /// Hydrate only the plan matching this exact task/session identity.
    pub async fn hydrate_by_identity(
        &self,
        spike_task_id: &str,
        session_id: &str,
    ) -> Result<Option<EvidencePlanHydration>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let hydration =
            Self::hydrate_by_identity_in_transaction(&mut tx, spike_task_id, session_id).await?;
        tx.commit().await?;
        Ok(hydration)
    }

    /// Hydrate the complete identity-scoped snapshot using a caller-owned
    /// transaction. Finalizers use this with their projection insert so checks,
    /// invocations, and any existing projection share one database snapshot.
    pub async fn hydrate_by_identity_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        spike_task_id: &str,
        session_id: &str,
    ) -> Result<Option<EvidencePlanHydration>> {
        let plan_row = sqlx::query("SELECT id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint, created_at, updated_at FROM evidence_plans WHERE spike_task_id = $1 AND session_id = $2")
            .bind(spike_task_id).bind(session_id).fetch_optional(&mut **tx).await?;
        let Some(row) = plan_row else { return Ok(None) };
        let plan_id: String = row.get("id");
        let check_rows = sqlx::query("SELECT plan_id, ordinal, check_id, question, method FROM evidence_plan_checks WHERE plan_id = $1 ORDER BY ordinal")
            .bind(&plan_id).fetch_all(&mut **tx).await?;
        let checks = check_rows
            .into_iter()
            .map(|r| EvidencePlanCheck {
                plan_id: r.get("plan_id"),
                ordinal: r.get("ordinal"),
                check_id: r.get("check_id"),
                question: r.get("question"),
                method: r.get("method"),
            })
            .collect();
        let plan = EvidencePlan {
            id: plan_id.clone(),
            spike_task_id: row.get("spike_task_id"),
            session_id: row.get("session_id"),
            captured_commit_sha: row.get("captured_commit_sha"),
            worktree_fingerprint: row.get("worktree_fingerprint"),
            checks,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        };
        let invocation_rows = sqlx::query("SELECT id, plan_id, spike_task_id, session_id, captured_commit_sha, worktree_fingerprint, check_id, argv, canonical_cwd, launch_state, process_state, launched_at, finished_at, exit_code, signal, runner_failure, elapsed_millis, timeout_millis, timed_out, stdout_digest, stdout_excerpt, stdout_truncated, stderr_digest, stderr_excerpt, stderr_truncated, created_at FROM evidence_command_invocations WHERE plan_id = $1 ORDER BY created_at, id")
            .bind(&plan_id).fetch_all(&mut **tx).await?;
        let invocations = invocation_rows
            .into_iter()
            .map(invocation_from_row)
            .collect::<Result<Vec<_>>>()?;
        let projection = sqlx::query("SELECT id, plan_id, version, payload, finalized_at FROM evidence_finalized_projections WHERE plan_id = $1")
            .bind(&plan_id).fetch_optional(&mut **tx).await?.map(|r| EvidenceFinalizedProjection { id: r.get("id"), plan_id: r.get("plan_id"), version: r.get("version"), payload: r.get("payload"), finalized_at: r.get("finalized_at") });
        Ok(Some(EvidencePlanHydration {
            plan,
            invocations,
            finalized_projection: projection,
        }))
    }
}

fn validate_plan(input: &InsertEvidencePlan) -> Result<()> {
    if [
        input.id.as_str(),
        input.spike_task_id.as_str(),
        input.session_id.as_str(),
        input.captured_commit_sha.as_str(),
        input.worktree_fingerprint.as_str(),
    ]
    .iter()
    .any(|value| value.trim().is_empty())
        || input.checks.is_empty()
    {
        return Err(DbError::InvalidData(
            "evidence plan identity and checks must be nonempty".to_owned(),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for check in &input.checks {
        if check.check_id.trim().is_empty()
            || check.question.trim().is_empty()
            || !matches!(check.method.as_str(), "code" | "graph" | "command")
            || !ids.insert(&check.check_id)
        {
            return Err(DbError::InvalidData(
                "evidence checks require unique ids, questions, and supported methods".to_owned(),
            ));
        }
    }
    Ok(())
}
fn validate_invocation(input: &AppendEvidenceInvocation) -> Result<()> {
    if input.id.trim().is_empty()
        || input.plan_id.trim().is_empty()
        || input.check_id.trim().is_empty()
        || input.argv.is_empty()
        || input.argv.iter().any(|v| v.is_empty())
        || input.canonical_cwd.trim().is_empty()
    {
        return Err(DbError::InvalidData(
            "invocation identity, argv, and canonical cwd must be nonempty".to_owned(),
        ));
    }
    Ok(())
}
fn invocation_from_input(
    input: AppendEvidenceInvocation,
    created_at: String,
) -> EvidenceCommandInvocation {
    EvidenceCommandInvocation {
        id: input.id,
        plan_id: input.plan_id,
        spike_task_id: input.spike_task_id,
        session_id: input.session_id,
        captured_commit_sha: input.captured_commit_sha,
        worktree_fingerprint: input.worktree_fingerprint,
        check_id: input.check_id,
        argv: input.argv,
        canonical_cwd: input.canonical_cwd,
        launch_state: input.launch_state,
        process_state: input.process_state,
        launched_at: input.launched_at,
        finished_at: input.finished_at,
        exit_code: input.exit_code,
        signal: input.signal,
        runner_failure: input.runner_failure,
        elapsed_millis: input.elapsed_millis,
        timeout_millis: input.timeout_millis,
        timed_out: input.timed_out,
        stdout_digest: input.stdout_digest,
        stdout_excerpt: input.stdout_excerpt,
        stdout_truncated: input.stdout_truncated,
        stderr_digest: input.stderr_digest,
        stderr_excerpt: input.stderr_excerpt,
        stderr_truncated: input.stderr_truncated,
        created_at,
    }
}
fn invocation_from_row(row: sqlx::postgres::PgRow) -> Result<EvidenceCommandInvocation> {
    Ok(EvidenceCommandInvocation {
        id: row.get("id"),
        plan_id: row.get("plan_id"),
        spike_task_id: row.get("spike_task_id"),
        session_id: row.get("session_id"),
        captured_commit_sha: row.get("captured_commit_sha"),
        worktree_fingerprint: row.get("worktree_fingerprint"),
        check_id: row.get("check_id"),
        argv: serde_json::from_value(row.get("argv"))?,
        canonical_cwd: row.get("canonical_cwd"),
        launch_state: row.get("launch_state"),
        process_state: row.get("process_state"),
        launched_at: row.get("launched_at"),
        finished_at: row.get("finished_at"),
        exit_code: row.get("exit_code"),
        signal: row.get("signal"),
        runner_failure: row.get("runner_failure"),
        elapsed_millis: row.get("elapsed_millis"),
        timeout_millis: row.get("timeout_millis"),
        timed_out: row.get("timed_out"),
        stdout_digest: row.get("stdout_digest"),
        stdout_excerpt: row.get("stdout_excerpt"),
        stdout_truncated: row.get("stdout_truncated"),
        stderr_digest: row.get("stderr_digest"),
        stderr_excerpt: row.get("stderr_excerpt"),
        stderr_truncated: row.get("stderr_truncated"),
        created_at: row.get("created_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::test_support::{
        UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row_with_id,
        seed_task_row,
    };

    async fn fixture() -> (EvidenceRepository, String, String) {
        let db = Database::ephemeral()
            .await
            .expect("open isolated Postgres database");
        db.ensure_initialized().await.expect("initialize schema");
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id, &format!("evidence-{project_id}")).await;
        let task_id = seed_task_row(
            &db,
            UsageTestTaskSeed {
                project_id: &project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        let session_id = uuid::Uuid::now_v7().to_string();
        seed_session_row_with_id(
            &db,
            &session_id,
            UsageTestSessionSeed {
                project_id: &project_id,
                model_id: "test-model",
                agent_type: "worker",
                started_at: "2025-01-01T00:00:00.000Z",
                tokens_in: 0,
                tokens_out: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: None,
                cost_basis: "unpriced",
                task_id: Some(&task_id),
            },
        )
        .await;
        (EvidenceRepository::new(db), task_id, session_id)
    }

    fn plan_input(id: String, task_id: &str, session_id: &str) -> InsertEvidencePlan {
        InsertEvidencePlan {
            id,
            spike_task_id: task_id.to_owned(),
            session_id: session_id.to_owned(),
            captured_commit_sha: "a1b2c3d4".to_owned(),
            worktree_fingerprint: "worktree:fixture".to_owned(),
            checks: vec![InsertEvidencePlanCheck {
                check_id: "command-check".to_owned(),
                question: "Does the command preserve every provenance field?".to_owned(),
                method: "command".to_owned(),
            }],
        }
    }

    fn invocation_input(
        id: String,
        plan: &EvidencePlan,
        exit_code: Option<i32>,
    ) -> AppendEvidenceInvocation {
        AppendEvidenceInvocation {
            id,
            plan_id: plan.id.clone(),
            spike_task_id: plan.spike_task_id.clone(),
            session_id: plan.session_id.clone(),
            captured_commit_sha: plan.captured_commit_sha.clone(),
            worktree_fingerprint: plan.worktree_fingerprint.clone(),
            check_id: "command-check".to_owned(),
            argv: vec![
                "rg".to_owned(),
                "evidence".to_owned(),
                "--glob".to_owned(),
                "*.rs".to_owned(),
            ],
            canonical_cwd: "/workspace/repository".to_owned(),
            launch_state: "launched".to_owned(),
            process_state: "exited".to_owned(),
            launched_at: Some("2025-01-01T00:00:01.000Z".to_owned()),
            finished_at: Some("2025-01-01T00:00:02.234Z".to_owned()),
            exit_code,
            signal: None,
            runner_failure: Some("captured runner diagnostic".to_owned()),
            elapsed_millis: Some(1234),
            timeout_millis: Some(5000),
            timed_out: false,
            stdout_digest: Some("sha256:stdout".to_owned()),
            stdout_excerpt: Some("stdout excerpt".to_owned()),
            stdout_truncated: true,
            stderr_digest: Some("sha256:stderr".to_owned()),
            stderr_excerpt: Some("stderr excerpt".to_owned()),
            stderr_truncated: true,
        }
    }

    #[tokio::test]
    async fn rejects_second_plan_for_same_task_session_identity() {
        let (repo, task_id, session_id) = fixture().await;
        repo.insert_plan(plan_input(
            uuid::Uuid::now_v7().to_string(),
            &task_id,
            &session_id,
        ))
        .await
        .expect("insert first frozen plan");

        let duplicate = repo
            .insert_plan(plan_input(
                uuid::Uuid::now_v7().to_string(),
                &task_id,
                &session_id,
            ))
            .await;
        assert!(
            duplicate.is_err(),
            "task/session identity must admit only one plan"
        );
    }

    #[tokio::test]
    async fn appends_retries_round_trips_provenance_and_rejects_mutation() {
        let (repo, task_id, session_id) = fixture().await;
        let plan = repo
            .insert_plan(plan_input(
                uuid::Uuid::now_v7().to_string(),
                &task_id,
                &session_id,
            ))
            .await
            .expect("insert plan");
        let first = repo
            .append_invocation(invocation_input(
                uuid::Uuid::now_v7().to_string(),
                &plan,
                Some(17),
            ))
            .await
            .expect("append first invocation");
        let second = repo
            .append_invocation(invocation_input(
                uuid::Uuid::now_v7().to_string(),
                &plan,
                Some(0),
            ))
            .await
            .expect("append retry invocation");
        assert_ne!(first.id, second.id, "retries must have distinct opaque ids");

        let mut tx = repo
            .db()
            .pool()
            .begin()
            .await
            .expect("begin hydration transaction");
        let hydrated =
            EvidenceRepository::hydrate_by_identity_in_transaction(&mut tx, &task_id, &session_id)
                .await
                .expect("hydrate in finalizer transaction")
                .expect("plan exists");
        tx.commit().await.expect("commit hydration transaction");
        assert_eq!(hydrated.plan, plan);
        assert_eq!(hydrated.invocations.len(), 2);
        assert!(hydrated.invocations.contains(&first));
        assert!(hydrated.invocations.contains(&second));

        // The migration trigger protects the append-only ledger even against
        // direct SQL; the repository intentionally has no overwrite operation.
        assert!(
            sqlx::query(
                "UPDATE evidence_command_invocations SET canonical_cwd = '/tampered' WHERE id = $1"
            )
            .bind(&first.id)
            .execute(repo.db().pool())
            .await
            .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM evidence_command_invocations WHERE id = $1")
                .bind(&first.id)
                .execute(repo.db().pool())
                .await
                .is_err()
        );

        let after = repo
            .hydrate_by_identity(&task_id, &session_id)
            .await
            .expect("hydrate append-only ledger")
            .expect("plan remains");
        assert!(after.invocations.contains(&first));
        assert!(after.invocations.contains(&second));
    }
}

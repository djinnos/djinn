use std::path::Path;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;
use tokio::sync::broadcast;

use crate::database::Database;

mod refinement_read_only;

pub use refinement_read_only::*;

/// Override a task short id for identifier-resolution regressions.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn set_task_short_id_for_test(db: &Database, task_id: &str, short_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE tasks SET short_id = $1 WHERE id = $2")
        .bind(short_id)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("failed to override task short id");
}

/// Return task-owned liveness rows in durable insertion order.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn liveness_evidence_for_task_for_test(
    db: &Database,
    task_id: &str,
) -> Vec<(String, serde_json::Value)> {
    db.ensure_initialized().await.unwrap();
    sqlx::query_as(
        "SELECT id, evidence FROM liveness_evidence \
         WHERE task_id = $1 ORDER BY created_at, id",
    )
    .bind(task_id)
    .fetch_all(db.pool())
    .await
    .expect("failed to load task liveness evidence")
}

/// Reject task-owned liveness evidence inserts at the repository boundary.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_task_liveness_evidence_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "ALTER TABLE liveness_evidence ADD CONSTRAINT reject_execution_kill_evidence \
         CHECK (task_id IS NULL)",
    )
    .execute(db.pool())
    .await
    .expect("failed to install task liveness evidence fault");
}

/// Reject one exact interrupted-session settlement while allowing later rows.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_interrupted_session_for_test(db: &Database, session_id: &str) {
    db.ensure_initialized().await.unwrap();
    let session_id = uuid::Uuid::parse_str(session_id).expect("session id must be a UUID");
    sqlx::query(&format!(
        "ALTER TABLE sessions ADD CONSTRAINT reject_reconcile_settlement \
         CHECK (id <> '{session_id}' OR status <> 'interrupted')"
    ))
    .execute(db.pool())
    .await
    .expect("failed to install exact settlement fault");
}

/// Inject one new running row after an exact session settlement.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn inject_residual_session_after_settlement_for_test(
    db: &Database,
    captured_id: &str,
    residual_id: &str,
) {
    db.ensure_initialized().await.unwrap();
    let captured_id = uuid::Uuid::parse_str(captured_id).expect("captured id must be a UUID");
    let residual_id = uuid::Uuid::parse_str(residual_id).expect("residual id must be a UUID");
    sqlx::query(&format!(
        "CREATE FUNCTION inject_reconcile_residual() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF OLD.id = '{captured_id}' AND NEW.status = 'interrupted' THEN \
             INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status, task_run_id, cost_basis) \
             VALUES ('{residual_id}', OLD.project_id, OLD.task_id, OLD.model_id, OLD.agent_type, 'running', OLD.task_run_id, 'unpriced'); \
           END IF; \
           RETURN NEW; \
         END $$"
    ))
    .execute(db.pool())
    .await
    .expect("failed to install residual injection function");
    sqlx::query(
        "CREATE TRIGGER inject_reconcile_residual AFTER UPDATE OF status ON sessions \
         FOR EACH ROW EXECUTE FUNCTION inject_reconcile_residual()",
    )
    .execute(db.pool())
    .await
    .expect("failed to install residual injection trigger");
}

/// Durable rows written by the structured evidence hand-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredEvidenceHandoffCountsForTest {
    pub finalized_projections: i64,
    pub compatibility_debates: i64,
}

/// Count both durable outputs of a structured evidence hand-off.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn structured_evidence_handoff_counts_for_test(
    db: &Database,
    plan_id: &str,
    proposal_id: &str,
) -> StructuredEvidenceHandoffCountsForTest {
    db.ensure_initialized().await.unwrap();
    let finalized_projections = sqlx::query_scalar(
        "SELECT COUNT(*) FROM evidence_finalized_projections WHERE plan_id = $1",
    )
    .bind(plan_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to count finalized evidence projections");
    let compatibility_debates = sqlx::query_scalar(
        "SELECT COUNT(*) FROM proposal_debate_trail WHERE proposal_id = $1 AND kind = 'evidence_findings'",
    )
    .bind(proposal_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to count evidence compatibility debates");
    StructuredEvidenceHandoffCountsForTest {
        finalized_projections,
        compatibility_debates,
    }
}

/// Reject every finalized evidence projection insert to exercise transaction rollback.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_evidence_projection_inserts_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "ALTER TABLE evidence_finalized_projections ADD CONSTRAINT reject_structured_handoff_projection CHECK (false)",
    )
    .execute(db.pool())
    .await
    .expect("failed to install finalized evidence projection fault");
}

/// Reject evidence-findings debate inserts to exercise transaction rollback.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_evidence_findings_debates_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "ALTER TABLE proposal_debate_trail ADD CONSTRAINT reject_structured_handoff_debate CHECK (kind <> 'evidence_findings')",
    )
    .execute(db.pool())
    .await
    .expect("failed to install evidence-findings debate fault");
}

/// Populate a current attempt's persisted projection for a cross-crate detail
/// read test. This fixture lives at the database boundary so consumer tests do
/// not bypass repository ownership with direct readiness-table SQL.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn seed_readiness_detail_projection_for_test(
    db: &Database,
    run_id: &str,
    area_id: &str,
    attempt_id: &str,
) {
    db.ensure_initialized().await.unwrap();
    let mut transaction = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE readiness_area_attempts SET status='succeeded', terminal_at='2026-01-01T00:00:00.000Z' WHERE id=$1")
        .bind(attempt_id)
        .execute(&mut *transaction)
        .await
        .expect("terminal readiness attempt");
    sqlx::query("UPDATE readiness_composition_areas SET status='succeeded' WHERE id=$1")
        .bind(area_id)
        .execute(&mut *transaction)
        .await
        .expect("terminal readiness area");
    sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,status,severity,confidence,accepted,evidence) VALUES ('query-finding',$1,$2,$3,'auth','covered','high',0.9,true,'{\"path\":\"web/auth.ts\"}')")
        .bind(run_id)
        .bind(area_id)
        .bind(attempt_id)
        .execute(&mut *transaction)
        .await
        .expect("readiness finding");
    sqlx::query("INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ($1,$2,$3,'{\"warnings\":[\"preserved\"]}')")
        .bind(run_id)
        .bind(area_id)
        .bind(attempt_id)
        .execute(&mut *transaction)
        .await
        .expect("readiness output");
    sqlx::query("INSERT INTO readiness_area_scores (run_id,area_id,score,applicable_weight,covered_weight,status) VALUES ($1,$2,0.75,4,3,'supported')")
        .bind(run_id)
        .bind(area_id)
        .execute(&mut *transaction)
        .await
        .expect("readiness area score");
    sqlx::query(
        "INSERT INTO readiness_project_scores (run_id,score,band) VALUES ($1,0.75,'ready')",
    )
    .bind(run_id)
    .execute(&mut *transaction)
    .await
    .expect("readiness project score");
    sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('query-suggestion',$1,'auth-remediation','{\"action\":\"add auth\"}')")
        .bind(run_id)
        .execute(&mut *transaction)
        .await
        .expect("readiness suggestion");
    sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ('query-event',$1,'fixture_completed','{\"source\":\"test\"}')")
        .bind(run_id)
        .execute(&mut *transaction)
        .await
        .expect("readiness event");
    transaction
        .commit()
        .await
        .expect("commit readiness projection fixture");
}

/// Refinement test fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementRunAuditForTest {
    pub generation: i32,
    pub state: String,
    pub park_kind: Option<String>,
    pub stop_tag: Option<String>,
    pub stop_context: Option<serde_json::Value>,
    pub typed_reap_count: i64,
}

/// Read a refinement run for tests.
pub async fn refinement_run_audit_for_test(
    db: &Database,
    run_id: &str,
) -> RefinementRunAuditForTest {
    db.ensure_initialized().await.unwrap();
    let row: (
        i32,
        String,
        Option<String>,
        Option<String>,
        Option<serde_json::Value>,
    ) = sqlx::query_as(
        "SELECT generation, state, park_kind, stop_tag, stop_context \
             FROM refinement_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to read refinement run audit row");
    let typed_reap_count = sqlx::query_scalar(
        "SELECT COUNT(*) FROM refinement_runs \
         WHERE id = $1 AND stop_tag = 'reaped_phantom'",
    )
    .bind(run_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to count typed refinement reaps");
    RefinementRunAuditForTest {
        generation: row.0,
        state: row.1,
        park_kind: row.2,
        stop_tag: row.3,
        stop_context: row.4,
        typed_reap_count,
    }
}

/// Count every row in `tasks`, across all projects and statuses.
///
/// No repository read is equivalent: every `TaskRepository` list is scoped to a
/// project, an epic, or a status set, so a task row created and then stranded
/// outside the fixture's project would not appear in any of them. The tests that
/// use this assert that a *rejected* tool call left the whole table unchanged,
/// which is precisely a claim about rows no scoped read can see.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn task_row_count_for_test(db: &Database) -> i64 {
    db.ensure_initialized().await.unwrap();
    sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(db.pool())
        .await
        .expect("failed to count task rows")
}

/// Correlate a task to a refinement run by run id ALONE, leaving
/// `refinement_intent_id`, `refinement_generation`, `refinement_round`,
/// `refinement_phase`, and `refinement_role` null.
///
/// [`crate::repositories::task::TaskRepository::set_refinement_correlation`]
/// only writes a complete [`djinn_core::models::TaskRefinementCorrelation`], and
/// [`ProposalRepository::acknowledge_refinement_task_materialization`] refuses a
/// task that is not fully correlated. Neither can express the partially
/// correlated row this fixture needs: the liveness snapshot selects task
/// evidence on `refinement_run_id` alone, so a run-correlated task with no
/// intent identity is exactly the durable shape that distinguishes task
/// evidence from intent evidence.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn correlate_task_to_refinement_run_for_test(db: &Database, task_id: &str, run_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE tasks SET refinement_run_id = $1 WHERE id = $2")
        .bind(run_id)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("failed to correlate task to refinement run");
}

/// Replace the `park_kind` of a run that is ALREADY parked.
///
/// [`ProposalRepository::park_refinement_run`] carries `AND state = 'running'`
/// and [`ProposalRepository::park_refinement_run_from_intent`] consumes a live
/// source intent, so neither can re-park a run that is already parked, and
/// there is no un-park transition. Status tests that must observe both park
/// kinds project from the same exact run have no repository path to the second
/// one.
///
/// `parked_at` is left untouched so the run keeps the timestamp its original
/// park wrote.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn force_refinement_run_park_kind_for_test(
    db: &Database,
    run_id: &str,
    kind: djinn_core::refinement_liveness::RefinementParkKind,
) {
    db.ensure_initialized().await.unwrap();
    let kind = match kind {
        djinn_core::refinement_liveness::RefinementParkKind::AwaitingReview => "awaiting_review",
        djinn_core::refinement_liveness::RefinementParkKind::AwaitingEvidence => {
            "awaiting_evidence"
        }
    };
    sqlx::query("UPDATE refinement_runs SET park_kind = $2 WHERE id = $1 AND state = 'parked'")
        .bind(run_id)
        .bind(kind)
        .execute(db.pool())
        .await
        .expect("failed to force refinement run park kind");
}

/// Terminalize a run row out of band: no `proposal_revisions` audit append, no
/// heartbeat refresh, and no generation fence.
///
/// [`ProposalRepository::terminal_refinement_run`] additionally appends a
/// `refinement_stop` revision in the same transaction and stamps
/// `heartbeat_at`. Both are observable, and the tests that use this helper
/// observe them:
///
/// * one counts `refinement_stop` revisions to prove a tool handler emitted no
///   spurious stop — a fixture-written audit row would be indistinguishable
///   from the defect;
/// * one asserts that a *prior* terminal run's stop reason does not leak into
///   the current run's status, and `build_refinement_status` falls back to the
///   latest `refinement_stop` revision when the exact run has no terminal
///   reason — so a fixture-written audit row would make the regression
///   undetectable.
///
/// `terminal_at` is caller-supplied because these fixtures model historic rows.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn force_refinement_run_terminal_for_test(
    db: &Database,
    run_id: &str,
    reason: &djinn_core::refinement_liveness::RefinementStopReason,
    terminal_at: &str,
) {
    db.ensure_initialized().await.unwrap();
    let context = serde_json::to_value(reason)
        .expect("failed to serialize refinement stop reason")
        .get("context")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    sqlx::query(
        "UPDATE refinement_runs \
         SET state = 'terminal', parked_at = NULL, park_kind = NULL, \
             terminal_at = $2, stop_tag = $3, stop_context = $4 \
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(terminal_at)
    .bind(reason.tag())
    .bind(&context)
    .execute(db.pool())
    .await
    .expect("failed to force refinement run terminal");
}

/// Complete a dispatch intent WITHOUT persisting its successor.
///
/// [`ProposalRepository::complete_refinement_intent`] always inserts exactly one
/// next-phase intent in the same transaction — that atomicity is the point of
/// the production method. A fixture that needs the completed intent to be the
/// run's last intent (so that some other durable row is the sole remaining
/// liveness evidence) cannot use it: the successor lands `pending`, and a
/// pending intent outranks every task, session, and heartbeat fallback in the
/// shared evaluator.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn complete_refinement_intent_without_successor_for_test(db: &Database, intent_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE refinement_dispatch_intents \
         SET state = 'completed', claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, \
             terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
             updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
         WHERE id = $1",
    )
    .bind(intent_id)
    .execute(db.pool())
    .await
    .expect("failed to complete refinement intent without successor");
}

/// Corrupt a task role to model malformed legacy correlation evidence.
pub async fn corrupt_refinement_task_role_for_test(db: &Database, task_id: &str, role: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE tasks SET refinement_role = $1 WHERE id = $2")
        .bind(role)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("failed to corrupt refinement task role");
}

/// Remove an exact run's intent evidence and age its heartbeat so recovery can
/// exercise the otherwise-unrepresentable phantom-run boundary.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn make_refinement_run_phantom_for_test(db: &Database, run_id: &str) {
    db.ensure_initialized().await.unwrap();
    let mut transaction = db.pool().begin().await.unwrap();
    sqlx::query("DELETE FROM refinement_dispatch_intents WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *transaction)
        .await
        .expect("failed to remove phantom refinement intent evidence");
    sqlx::query(
        "UPDATE refinement_runs SET heartbeat_at = '2000-01-01T00:00:00.000Z' WHERE id = $1",
    )
    .bind(run_id)
    .execute(&mut *transaction)
    .await
    .expect("failed to age phantom refinement run heartbeat");
    transaction.commit().await.unwrap();
}

/// Rewind an exact run's durable timestamps by `seconds`, which is the exact
/// dual of `seconds` of wall-clock time passing for that run.
///
/// Both the dispatch-intent claim CAS and the liveness snapshot are evaluated
/// against Postgres `transaction_timestamp()`, so no in-process clock can move
/// them. The claim expiry and the run heartbeat are rewound together: aging one
/// while the other stays fresh would leave a liveness evidence class frozen and
/// silently mask the window under test.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn elapse_refinement_run_wall_clock_for_test(db: &Database, run_id: &str, seconds: i64) {
    db.ensure_initialized().await.unwrap();
    let mut transaction = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE refinement_dispatch_intents SET claim_expires_at = to_char((claim_expires_at::timestamptz - ($2 * interval '1 second')) AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE run_id = $1 AND claim_expires_at IS NOT NULL")
        .bind(run_id)
        .bind(seconds)
        .execute(&mut *transaction)
        .await
        .expect("failed to elapse refinement claim expiry");
    sqlx::query("UPDATE refinement_runs SET heartbeat_at = to_char((heartbeat_at::timestamptz - ($2 * interval '1 second')) AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1")
        .bind(run_id)
        .bind(seconds)
        .execute(&mut *transaction)
        .await
        .expect("failed to elapse refinement run heartbeat");
    transaction.commit().await.unwrap();
}

/// Replace a legacy proposal head and its current spec snapshot for tests.
pub async fn replace_legacy_proposal_head_for_test(
    db: &Database,
    proposal_id: &str,
    body: &str,
    body_format: &str,
) {
    db.ensure_initialized().await.unwrap();
    let mut transaction = db.pool().begin().await.unwrap();
    sqlx::query("UPDATE proposals SET body = $1, body_format = $2 WHERE id = $3")
        .bind(body)
        .bind(body_format)
        .bind(proposal_id)
        .execute(&mut *transaction)
        .await
        .expect("failed to replace legacy proposal head");
    sqlx::query(
        "UPDATE proposal_revisions SET body = $1, body_format = $2 \
         WHERE proposal_id = $3 \
           AND seq = (SELECT latest_revision_seq FROM proposals WHERE id = $3) \
           AND event_kind = 'spec_revision'",
    )
    .bind(body)
    .bind(body_format)
    .bind(proposal_id)
    .execute(&mut *transaction)
    .await
    .expect("failed to replace legacy material head snapshot");
    transaction.commit().await.unwrap();
}

/// Delete a proposal's lint cache rows to model a head written before lint
/// persistence was introduced.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn delete_proposal_lint_results_for_test(db: &Database, proposal_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("DELETE FROM proposal_revision_lint_results WHERE proposal_id = $1")
        .bind(proposal_id)
        .execute(db.pool())
        .await
        .expect("failed to delete proposal lint results");
}

/// Delete one revision's lint cache row to model a current head written before
/// lint persistence while retaining all historical revision results.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn delete_proposal_lint_result_for_revision_for_test(
    db: &Database,
    proposal_id: &str,
    revision_seq: i32,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "DELETE FROM proposal_revision_lint_results \
         WHERE proposal_id = $1 AND revision_seq = $2",
    )
    .bind(proposal_id)
    .bind(revision_seq)
    .execute(db.pool())
    .await
    .expect("failed to delete proposal revision lint result");
}

/// Return the immutable revision id to which a persisted lint result is
/// attached. Tests use this to assert that later material revisions neither
/// delete nor reattach historical lint rows.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn proposal_lint_revision_id_for_test(
    db: &Database,
    proposal_id: &str,
    revision_seq: i32,
) -> Option<String> {
    db.ensure_initialized().await.unwrap();
    sqlx::query_scalar(
        "SELECT revision_id FROM proposal_revision_lint_results \
         WHERE proposal_id = $1 AND revision_seq = $2",
    )
    .bind(proposal_id)
    .bind(revision_seq)
    .fetch_optional(db.pool())
    .await
    .expect("failed to read proposal revision lint attachment")
}

/// Create a fresh FK-valid user for a raw latest-schema fixture.
/// UUIDv7-derived values avoid a repository-wide fixed test identity.
pub async fn seed_test_user(db: &Database) -> String {
    db.ensure_initialized().await.unwrap();
    let uuid = uuid::Uuid::now_v7();
    let id = uuid.to_string();
    let github_id = (uuid.as_u128() & i64::MAX as u128) as i64;
    let github_login = format!("fixture-{}", uuid.simple());
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&id)
        .bind(github_id)
        .bind(&github_login)
        .execute(db.pool())
        .await
        .expect("failed to seed fixture user");
    id
}

/// Apply the full embedded migrator to a fresh, empty database through the
/// supported creator-contract path: provision a designated operator on the
/// pre-contract schema, then run the remaining migrations with the operator
/// bound to the migration session. The creator-contract migration refuses to
/// run without a validated operator, so a bare `sqlx::migrate!().run()` on an
/// empty database is no longer a supported entry point.
///
/// Returns the provisioned operator's user id so callers seeding
/// latest-schema task rows have an FK-valid creator to bind.
pub async fn apply_all_migrations_to_fresh_database(db_url: &str) -> String {
    const FRESH_MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000141";
    crate::migrations::bootstrap_designated_operator(
        db_url,
        &crate::migrations::DesignatedOperatorBootstrap {
            user_id: FRESH_MIGRATION_OPERATOR_ID.to_owned(),
            github_id: 9_000_000_141,
            github_login: "fresh-migration-operator".to_owned(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await
    .expect("bootstrap designated operator on fresh database");
    crate::migrations::run_postgres_migrations(
        db_url,
        &crate::migrations::MigrationContext {
            designated_operator_user_id: Some(FRESH_MIGRATION_OPERATOR_ID.to_owned()),
        },
    )
    .await
    .expect("apply all migrations to fresh database");
    FRESH_MIGRATION_OPERATOR_ID.to_owned()
}

// ── Seed helpers for usage-analytics route tests ─────────────────────────
// These insert rows directly via raw SQL so integration tests outside
// djinn-db can seed deterministic fixture data without going through the
// full service-layer code paths.

/// Parameters for [`seed_session_row`].
pub struct UsageTestSessionSeed<'a> {
    pub project_id: &'a str,
    pub model_id: &'a str,
    pub agent_type: &'a str,
    pub started_at: &'a str,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: Option<f64>,
    pub cost_basis: &'a str,
    pub task_id: Option<&'a str>,
}

/// Parameters for [`seed_task_row`].
pub struct UsageTestTaskSeed<'a> {
    pub project_id: &'a str,
    pub status: &'a str,
    pub close_reason: Option<&'a str>,
    pub total_reopen_count: i32,
}

/// Seed a task row directly into the database for integration-level
/// contract tests. Returns the generated task id.
pub async fn seed_task_row(db: &Database, seed: UsageTestTaskSeed<'_>) -> String {
    db.ensure_initialized().await.unwrap();
    let creator = seed_test_user(db).await;
    let task_uuid = uuid::Uuid::now_v7();
    let id = task_uuid.to_string();
    let compact_id = task_uuid.simple().to_string();
    let short_id = format!("task-{}-{}", &compact_id[..8], &compact_id[20..32]);
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, epic_id, title, description, design, \
          issue_type, status, priority, owner, labels, acceptance_criteria, \
          reopen_count, continuation_count, verification_failure_count, \
          total_reopen_count, \
          intervention_count, created_at, updated_at, closed_at, close_reason, \
          merge_commit_sha, memory_refs, merge_conflict_metadata, agent_type, pr_url, created_by_user_id) \
         VALUES ($1, $2, $3, NULL, 'test title', 'test desc', 'test design', \
                 'task', $4, 0, '', '[]', '[]', \
                 0, 0, 0, \
                 $5, \
                 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z', NULL, $6, \
                 NULL, '[]', NULL, NULL, NULL, $7)",
    )
    .bind(&id)
    .bind(seed.project_id)
    .bind(&short_id)
    .bind(seed.status)
    .bind(seed.total_reopen_count)
    .bind(seed.close_reason)
    .bind(&creator)
    .execute(db.pool())
    .await
    .expect("failed to seed task row");
    id
}

/// Seed one deterministic board-health mismatch candidate for coordinator
/// integration tests. Raw fixture SQL stays behind the `djinn-db` boundary.
pub async fn seed_board_health_mismatch_candidate(db: &Database, project_id: &str, task_id: &str) {
    db.ensure_initialized().await.unwrap();
    let creator = seed_test_user(db).await;
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, issue_type, status, \
          labels, acceptance_criteria, memory_refs, total_reopen_count, created_by_user_id) \
         VALUES ($1, $2, 'mismatch-storm', 'mismatch storm', 'requires task_create', \
                 '', 'task', 'open', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, 3, $3)",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(&creator)
    .execute(db.pool())
    .await
    .expect("failed to seed board-health mismatch candidate");
}

/// Seed raw session rows directly into the database for integration-level
/// contract tests that need actual query results.
pub async fn seed_session_row(db: &Database, seed: UsageTestSessionSeed<'_>) {
    let id = uuid::Uuid::now_v7().to_string();
    seed_session_row_with_id(db, &id, seed).await;
}

/// Like [`seed_session_row`] but accepts an explicit session id so callers
/// can control the id (e.g. for boundary tests that need a known session id).
pub async fn seed_session_row_with_id(
    db: &Database,
    session_id: &str,
    seed: UsageTestSessionSeed<'_>,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, $2, $3, $4, $5, 'completed', $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(session_id)
    .bind(seed.project_id)
    .bind(seed.task_id)
    .bind(seed.model_id)
    .bind(seed.agent_type)
    .bind(seed.started_at)
    .bind(seed.tokens_in)
    .bind(seed.tokens_out)
    .bind(seed.cache_read_tokens)
    .bind(seed.cache_write_tokens)
    .bind(seed.cost_usd)
    .bind(seed.cost_basis)
    .execute(db.pool())
    .await
    .expect("failed to seed session row");
}

/// Delete a session row for integration tests that must verify FK cascade
/// behavior without bypassing the database repository boundary.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn delete_session_row(db: &Database, session_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(session_id)
        .execute(db.pool())
        .await
        .expect("failed to delete session row");
}

/// Seed a projectless global-chat session row for boundary/compaction tests.
///
/// Global chat sessions (`agent_type = 'chat'`) are user-scoped and exist
/// outside any project, so migration 15's `sessions_project_scope_by_agent_type`
/// CHECK requires `project_id IS NULL`. The `UsageTestSessionSeed` helper can
/// only express a non-null project id, so chat-session tests use this dedicated
/// seed instead. `cost_basis` is `'unpriced'` to satisfy migration 83's
/// `sessions_cost_basis_check`.
pub async fn seed_chat_session_row(db: &Database, session_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, NULL, NULL, 'test-model', 'chat', 'completed', \
                 '2025-01-01T00:00:00Z', 0, 0, 0, 0, NULL, 'unpriced')",
    )
    .bind(session_id)
    .execute(db.pool())
    .await
    .expect("failed to seed chat session row");
}

/// Seed a project so that `project_id` FK constraints pass.
pub async fn seed_project(db: &Database, project_id: &str, name: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, 'test', $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(project_id)
    .bind(name)
    .execute(db.pool())
    .await
    .expect("failed to seed project");
}

/// Persist one workspace coverage fixture for cross-crate publication tests.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn seed_workspace_coverage_for_test(
    db: &Database,
    project_id: &str,
    revision: &str,
    status: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO project_workspace_coverage \
         (project_id, workspace_slug, language, status, commit_sha) \
         VALUES ($1, 'workspace', 'rust', $2, $3)",
    )
    .bind(project_id)
    .bind(status)
    .bind(revision)
    .execute(db.pool())
    .await
    .expect("failed to seed workspace coverage");
}

/// Restore the pre-cutover launcher-authority seed for rollout integration
/// tests that deliberately exercise the leaf-v1 -> resize-v2 transition.
/// Fresh-database tests must not call this helper: migration 171 owns their
/// shipped resize-v2/epoch-zero assertion.
pub async fn seed_legacy_launcher_authority_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE launcher_authority_mode \
         SET mode = 'leaf-v1', epoch = 0, updated_at = now() \
         WHERE mode_key = 'global'",
    )
    .execute(db.pool())
    .await
    .expect("seed pre-cutover launcher authority fixture");
}

/// Overwrite a debate-trail entry's `body_metadata`, bypassing the
/// evidence-findings validation enforced by `add_debate_trail_entry`.
/// Recovery tests use this to fabricate legacy/corrupt rows that can no
/// longer be written through the repository API.
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn override_debate_trail_body_metadata(
    db: &Database,
    entry_id: &str,
    body_metadata: &serde_json::Value,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE proposal_debate_trail SET body_metadata = $2 WHERE id = $1")
        .bind(entry_id)
        .bind(body_metadata)
        .execute(db.pool())
        .await
        .expect("failed to override debate trail body_metadata");
}

/// Count the rows in `table_name`. Test-fixture helper for tests that must
/// prove a code path wrote NOTHING to a specific relation.
///
/// Deliberately per-relation rather than a summed total: a ledger with several
/// relations lets a write to one hide behind an absent write to another, so the
/// census that proves "zero writes" has to be taken relation by relation.
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn count_rows_for_test(db: &Database, table_name: &str) -> i64 {
    db.ensure_initialized().await.unwrap();
    let (count,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table_name}"))
        .fetch_one(db.pool())
        .await
        .unwrap_or_else(|error| panic!("count rows in {table_name}: {error}"));
    count
}

/// Drop a database table if it exists.  Test-fixture helper for
/// failure-injection tests that need to simulate a missing-table error
/// (e.g. the coordinator reentrance `blocker_lookup_error` test).
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn drop_table_for_test(db: &Database, table_name: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(&format!("DROP TABLE IF EXISTS {table_name}"))
        .execute(db.pool())
        .await
        .unwrap();
}

/// Drop a database table if it exists, cascading to dependent constraints.
/// Test-fixture helper for failure-injection tests where the table is
/// referenced by foreign keys (e.g. `images` is referenced by
/// `projects.selected_image_id` with `ON DELETE RESTRICT`).
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn drop_table_cascade_for_test(db: &Database, table_name: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(&format!("DROP TABLE IF EXISTS {table_name} CASCADE"))
        .execute(db.pool())
        .await
        .unwrap();
}

/// Make the `notes.confidence` column nullable and set one note's confidence
/// to NULL. Test-fixture helper for fail-open retrieval tests that need the
/// production note query to exclude one row while a trace/query mapping path
/// observes malformed data.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn nullify_note_confidence_for_test(db: &Database, note_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("ALTER TABLE notes ALTER COLUMN confidence DROP NOT NULL")
        .execute(db.pool())
        .await
        .expect("failed to make notes.confidence nullable");
    sqlx::query("UPDATE notes SET confidence = NULL WHERE id = $1")
        .bind(note_id)
        .execute(db.pool())
        .await
        .expect("failed to nullify note confidence");
}

/// Rename `notes.confidence` so note retrieval queries fail without dropping
/// the dependency-heavy `notes` table. Test-fixture helper for fail-open
/// retrieval tests that need both production and trace-candidate searches to
/// hit a schema error on an ephemeral database.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn rename_note_confidence_column_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("ALTER TABLE notes RENAME COLUMN confidence TO confidence_for_test")
        .execute(db.pool())
        .await
        .expect("failed to rename notes.confidence for test");
}

/// Add a test-only constraint that leaves existing `task_arbitrations` rows
/// readable but makes subsequent INSERTs fail.  This lets coordinator
/// regressions exercise the real `TaskArbitrationRepository::try_create` error
/// branch after `resolve_current_hold_cycle` has succeeded, without dropping the
/// table and turning the scenario into a read/hold-cycle-resolution failure.
///
/// **Not for production use.**  Panics on SQL errors.
pub async fn reject_new_task_arbitrations_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "ALTER TABLE task_arbitrations \
         ADD CONSTRAINT task_arbitrations_reject_insert_for_test \
         CHECK (false) NOT VALID",
    )
    .execute(db.pool())
    .await
    .expect("failed to add task_arbitrations reject-insert constraint");
}

/// Install a test-only trigger that rejects creation of a successor refinement
/// intent. This keeps raw failure-injection SQL inside the database-owner crate
/// while coordinator tests exercise the real atomic completion transaction.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_refinement_successor_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "CREATE FUNCTION reject_refinement_successor_for_test() RETURNS trigger AS $$ \
         BEGIN \
           RAISE EXCEPTION 'injected successor persistence failure'; \
         END; \
         $$ LANGUAGE plpgsql",
    )
    .execute(db.pool())
    .await
    .expect("failed to create refinement successor rejection function");
    sqlx::query(
        "CREATE TRIGGER reject_refinement_successor_for_test \
         BEFORE INSERT ON refinement_dispatch_intents \
         FOR EACH ROW EXECUTE FUNCTION reject_refinement_successor_for_test()",
    )
    .execute(db.pool())
    .await
    .expect("failed to create refinement successor rejection trigger");
}

/// Install a test-only trigger that rejects an exact-run terminal lifecycle
/// audit append. Terminalization updates the run and appends the audit in one
/// transaction, so this injects a repository error without leaving a false
/// terminal row behind.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn reject_refinement_terminal_audit_for_test(db: &Database) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "CREATE FUNCTION reject_refinement_terminal_audit_for_test() RETURNS trigger AS $$ \
         BEGIN \
           IF NEW.event_kind = 'refinement_stop' THEN \
             RAISE EXCEPTION 'injected terminal lifecycle persistence failure'; \
           END IF; \
           RETURN NEW; \
         END; \
         $$ LANGUAGE plpgsql",
    )
    .execute(db.pool())
    .await
    .expect("failed to create terminal audit rejection function");
    sqlx::query(
        "CREATE TRIGGER reject_refinement_terminal_audit_for_test \
         BEFORE INSERT ON proposal_revisions \
         FOR EACH ROW EXECUTE FUNCTION reject_refinement_terminal_audit_for_test()",
    )
    .execute(db.pool())
    .await
    .expect("failed to create terminal audit rejection trigger");
}

/// Backdate a task's `updated_at` by a PostgreSQL `interval` string
/// (e.g. `'20 minutes'`).
///
/// Test-fixture helper: production `updated_at` is stamped automatically
/// by status transitions.  Used by coordinator orphan / zombie tests to
/// fabricate task timestamps that predate a session's `started_at`.
pub async fn backdate_task_updated_at(db: &Database, task_id: &str, interval: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE tasks SET updated_at = to_char(
             now() AT TIME ZONE 'utc' - $1::interval,
             'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $2",
    )
    .bind(interval)
    .bind(task_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Backdate a `task_attempts` row's `created_at` by a PostgreSQL `interval`
/// string (e.g. `'1 hour'`).
///
/// Test-fixture helper for the coordinator's orphaned-pending-attempt reaper,
/// whose age threshold compares against `created_at`.
pub async fn backdate_task_attempt_created_at(db: &Database, attempt_id: &str, interval: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE task_attempts SET created_at = to_char(
             now() AT TIME ZONE 'utc' - $1::interval,
             'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $2",
    )
    .bind(interval)
    .bind(attempt_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Close a task at an explicit timestamp. Test-fixture helper: production
/// `closed_at` is stamped automatically by terminal status transitions.
pub async fn close_task_at(db: &Database, task_id: &str, closed_at: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("UPDATE tasks SET status = 'closed', closed_at = $1 WHERE id = $2")
        .bind(closed_at)
        .bind(task_id)
        .execute(db.pool())
        .await
        .expect("failed to close task at timestamp");
}

/// Backdate a `coordinator_incarnations` row's `last_renewed_at` by a
/// PostgreSQL `interval` string (e.g. `'1 hour'`).
///
/// Test-fixture helper for the coordinator's orphaned-pending-attempt reaper,
/// which classifies orphans from the durable owner lease's expiry relative to
/// the orphan threshold.
pub async fn backdate_coordinator_incarnation_lease(
    db: &Database,
    incarnation_id: &str,
    interval: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "UPDATE coordinator_incarnations SET last_renewed_at = to_char(
             now() AT TIME ZONE 'utc' - $1::interval,
             'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         WHERE id = $2",
    )
    .bind(interval)
    .bind(incarnation_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Insert a pending `task_attempts` row with an arbitrary (possibly malformed)
/// `dispatch_owner_incarnation_id`, bypassing the repository's UUID validation.
/// Used by reaper evidence tests to seed a malformed-owner orphan.
pub async fn insert_pending_attempt_with_raw_owner(
    db: &Database,
    id: &str,
    task_id: &str,
    role: &str,
    dispatch_key: &str,
    owner_incarnation_id: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        r#"INSERT INTO task_attempts (id, task_id, role, attempt_seq, dispatch_key, outcome,
           dispatch_owner_incarnation_id, dispatch_group_id)
           VALUES ($1, $2, $3, 1, $4, 'pending', $5, NULL)"#,
    )
    .bind(id)
    .bind(task_id)
    .bind(role)
    .bind(dispatch_key)
    .bind(owner_incarnation_id)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Replace the `coordinator_incarnations` table with a view that returns the
/// specified row on the **first** query but returns no rows on all subsequent
/// queries. This simulates the incarnation row vanishing between the reaper's
/// `get()` call (which succeeds) and its `is_live()` call (which returns
/// `None`), exercising the ambiguous-owner classification branch.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn make_coordinator_incarnation_vanish_after_first_read(
    db: &Database,
    incarnation_id: &str,
    registered_at: &str,
    last_renewed_at: &str,
) {
    db.ensure_initialized().await.unwrap();
    // Drop the real table and replace it with a view backed by an access
    // counter sequence. The first SELECT from the view returns the row
    // (counter = 1); every subsequent SELECT returns nothing.
    sqlx::query("DROP TABLE IF EXISTS coordinator_incarnations")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP SEQUENCE IF EXISTS ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE SEQUENCE ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    // The view must project EVERY column `CoordinatorIncarnationRepository`
    // selects, not just the ones this fixture varies. A column added to the
    // real table and forgotten here turns the first read into a *lookup
    // error*, and the branch this fixture exists to reach is never entered —
    // the test still fails, but on the wrong classification, and the reason is
    // three crates away. Migration 191's `draining_at` /
    // `provider_actions_drained_at` are NULL here because a vanished lease has
    // no drain history to report.
    let sql = format!(
        r#"CREATE VIEW coordinator_incarnations AS
           SELECT '{incarnation_id}'::varchar AS id,
                  '{registered_at}'::varchar AS registered_at,
                  '{last_renewed_at}'::varchar AS last_renewed_at,
                  NULL::varchar AS draining_at,
                  NULL::varchar AS provider_actions_drained_at
           WHERE nextval('ci_access_count') = 1"#
    );
    sqlx::query(&sql).execute(db.pool()).await.unwrap();
}

/// Replace the `coordinator_incarnations` table with a view that returns the
/// specified row on the **first** query but **raises an error** on all
/// subsequent queries. This simulates the incarnation table becoming
/// unavailable between the reaper's `get()` call (which succeeds) and its
/// `is_live()` call (which returns `Err`), exercising the is_live lookup-error
/// classification branch.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn make_coordinator_incarnation_error_after_first_read(
    db: &Database,
    incarnation_id: &str,
    registered_at: &str,
    last_renewed_at: &str,
) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS coordinator_incarnations")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP SEQUENCE IF EXISTS ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE SEQUENCE ci_access_count")
        .execute(db.pool())
        .await
        .unwrap();
    // Guard function: admits the first SELECT and raises on every later one.
    sqlx::query(
        r#"CREATE OR REPLACE FUNCTION ci_error_guard() RETURNS boolean AS $$
           BEGIN
             IF nextval('ci_access_count') >= 2 THEN
               RAISE EXCEPTION 'simulated coordinator_incarnations lookup error';
             END IF;
             RETURN true;
           END;
           $$ LANGUAGE plpgsql"#,
    )
    .execute(db.pool())
    .await
    .unwrap();
    // Same completeness requirement as the vanish fixture above: project
    // every column the repository selects, or the guard fires on the wrong
    // read and this fixture silently starts testing the `get()` error branch
    // instead of the `is_live()` one.
    let sql = format!(
        r#"CREATE VIEW coordinator_incarnations AS
           SELECT '{incarnation_id}'::varchar AS id,
                  '{registered_at}'::varchar AS registered_at,
                  '{last_renewed_at}'::varchar AS last_renewed_at,
                  NULL::varchar AS draining_at,
                  NULL::varchar AS provider_actions_drained_at
           WHERE ci_error_guard()"#
    );
    sqlx::query(&sql).execute(db.pool()).await.unwrap();
}

/// Wire a blocker edge: `blocking_task_id` blocks `task_id`. Test-fixture helper.
pub async fn add_blocker_edge(db: &Database, task_id: &str, blocking_task_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
        .bind(task_id)
        .bind(blocking_task_id)
        .execute(db.pool())
        .await
        .expect("failed to add blocker edge");
}

pub fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}

pub async fn make_project(db: &Database, path: &Path) -> Project {
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let path_slug = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("root");
    let project_name = format!("test-project-{path_slug}-{id}");
    // Synthesize unique (owner, repo) coords; the actual filesystem
    // `path` argument is used only for downstream note fixtures, not
    // persisted.
    let owner = "test";
    let repo_slug = format!("{path_slug}-{id}");
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        id,
        project_name,
        owner,
        repo_slug,
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query_as!(
        Project,
        r#"SELECT id, name,
                  github_owner AS "github_owner!: String",
                  github_repo  AS "github_repo!: String",
                  created_at, target_branch,
                  auto_merge AS "auto_merge!: bool",
                  sync_enabled AS "sync_enabled!: bool",
                  sync_remote
           FROM projects WHERE id = $1"#,
        id
    )
    .fetch_one(db.pool())
    .await
    .unwrap()
}

// Larger multi-row fixtures live beside this module and are re-exported here,
// so `crate::repositories::test_support::*` remains the single import path.
pub use super::test_support_fixtures::*;

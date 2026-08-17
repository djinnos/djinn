//! Repository-owned fixtures for the coordinator's six-phase typed-evidence
//! rollout matrix.
//!
//! Two of the six deployment shapes cannot be produced by any current
//! production writer: the legacy-only writer that predates typed authority,
//! and the mixed-version drift that repoints or rewrites the compatibility
//! columns underneath a typed finding. Both are raw-column facts, so the
//! database owner crate materializes them instead of `djinn-coordinator`
//! reaching for `sqlx` inside a test.
//!
//! The snapshot readers below exist for the same reason: the matrix has to
//! prove that a fail-closed read changed *nothing*, and that requires reading
//! rows the repository API deliberately refuses to project while authority is
//! mismatched.

use serde_json::Value;
use sqlx::Row;

use crate::database::Database;

/// Every persisted authority, attempt, transition, and task fact the rollout
/// matrix compares across a phase boundary.
///
/// Equality of two snapshots is the matrix's "no mutation, no replacement
/// task" proof, so this deliberately carries whole rows rather than counts.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedEvidenceRolloutSnapshotForTest {
    /// `proposals.linked_spike_task_id` — the legacy compatibility link.
    pub legacy_link: Option<String>,
    /// `proposals.needs_evidence_claim`, parsed when it is JSON.
    pub legacy_claim: Option<Value>,
    pub findings: Vec<Value>,
    pub attempts: Vec<Value>,
    pub transitions: Vec<Value>,
    /// Every task in the proposal's project, so a replacement spike or an
    /// unexpected refinement role shows up as a snapshot difference.
    pub tasks: Vec<Value>,
}

/// Read the complete rollout-relevant state for one proposal and its project.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn typed_evidence_rollout_snapshot_for_test(
    db: &Database,
    proposal_id: &str,
    project_id: &str,
) -> TypedEvidenceRolloutSnapshotForTest {
    db.ensure_initialized().await.unwrap();
    let legacy =
        sqlx::query("SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1")
            .bind(proposal_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let legacy_claim = legacy
        .get::<Option<String>, _>("needs_evidence_claim")
        .map(|raw| serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw)));
    let findings = sqlx::query(
        "SELECT id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id \
         FROM typed_evidence_findings WHERE proposal_id=$1 ORDER BY id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        serde_json::json!({
            "id": row.get::<String, _>("id"),
            "demand_hash": row.get::<String, _>("demand_hash"),
            "lifecycle": row.get::<String, _>("lifecycle"),
            "claim": row.get::<Value, _>("claim"),
            "demanded_revision_seq": row.get::<i32, _>("demanded_revision_seq"),
            "created_by_task_id": row.get::<Option<String>, _>("created_by_task_id"),
        })
    })
    .collect();
    let attempts = sqlx::query(
        "SELECT a.id,a.finding_id,a.sequence,a.spike_task_id,a.evidence_plan_id \
         FROM typed_evidence_attempts a JOIN typed_evidence_findings f ON f.id=a.finding_id \
         WHERE f.proposal_id=$1 ORDER BY a.sequence,a.id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        serde_json::json!({
            "id": row.get::<String, _>("id"),
            "finding_id": row.get::<String, _>("finding_id"),
            "sequence": row.get::<i32, _>("sequence"),
            "spike_task_id": row.get::<String, _>("spike_task_id"),
            "evidence_plan_id": row.get::<Option<String>, _>("evidence_plan_id"),
        })
    })
    .collect();
    let transitions = sqlx::query(
        "SELECT t.id,t.ordinal,t.from_lifecycle,t.to_lifecycle,t.actor_task_id,t.metadata \
         FROM typed_evidence_transitions t JOIN typed_evidence_findings f ON f.id=t.finding_id \
         WHERE f.proposal_id=$1 ORDER BY t.ordinal,t.id",
    )
    .bind(proposal_id)
    .fetch_all(db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        serde_json::json!({
            "id": row.get::<String, _>("id"),
            "ordinal": row.get::<i32, _>("ordinal"),
            "from_lifecycle": row.get::<Option<String>, _>("from_lifecycle"),
            "to_lifecycle": row.get::<String, _>("to_lifecycle"),
            "actor_task_id": row.get::<Option<String>, _>("actor_task_id"),
            "metadata": row.get::<Value, _>("metadata"),
        })
    })
    .collect();
    let tasks = sqlx::query(
        "SELECT id,issue_type,agent_type,status,close_reason FROM tasks \
         WHERE project_id=$1 ORDER BY id",
    )
    .bind(project_id)
    .fetch_all(db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        serde_json::json!({
            "id": row.get::<String, _>("id"),
            "issue_type": row.get::<String, _>("issue_type"),
            "agent_type": row.get::<Option<String>, _>("agent_type"),
            "status": row.get::<String, _>("status"),
            "close_reason": row.get::<Option<String>, _>("close_reason"),
        })
    })
    .collect();
    TypedEvidenceRolloutSnapshotForTest {
        legacy_link: legacy.get("linked_spike_task_id"),
        legacy_claim,
        findings,
        attempts,
        transitions,
        tasks,
    }
}

/// Write the pre-typed deployment's authority: the legacy compatibility
/// columns alone, with no typed finding, attempt, or transition behind them.
///
/// The assertion is the point of the helper. Every shipped writer dual-writes
/// typed authority, so a legacy-only row can only be materialized here, and it
/// is only the historical shape if nothing typed already exists.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn write_legacy_only_evidence_authority_for_test(
    db: &Database,
    proposal_id: &str,
    spike_task_id: &str,
    claim: &Value,
) {
    db.ensure_initialized().await.unwrap();
    let typed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_findings WHERE proposal_id=$1")
            .bind(proposal_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        typed, 0,
        "the legacy-only writer shape requires a proposal with no typed authority"
    );
    overwrite_legacy_evidence_authority_for_test(db, proposal_id, Some(spike_task_id), Some(claim))
        .await;
}

/// Set the legacy compatibility columns to an arbitrary value, including the
/// mixed-version drift the dual read must fail closed on: a link repointed to
/// another task, or a claim rewritten out from under the typed finding.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn overwrite_legacy_evidence_authority_for_test(
    db: &Database,
    proposal_id: &str,
    spike_task_id: Option<&str>,
    claim: Option<&Value>,
) {
    db.ensure_initialized().await.unwrap();
    let updated = sqlx::query(
        "UPDATE proposals SET linked_spike_task_id=$1,needs_evidence_claim=$2 WHERE id=$3",
    )
    .bind(spike_task_id)
    .bind(claim.map(|value| value.to_string()))
    .bind(proposal_id)
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(
        updated.rows_affected(),
        1,
        "legacy authority write must address exactly one proposal"
    );
}

/// Project the immutable evidence-plan and command-invocation facts reachable
/// from one attempt's planned checks.
///
/// A reverse rollback must leave these resolvable: they are the anchors the
/// new reader hydrates, and they are captured once by the spike session and
/// never rewritten.
///
/// **Not for production use.** Panics on SQL errors.
pub async fn evidence_plan_invocation_availability_for_test(
    db: &Database,
    attempt_id: &str,
) -> Vec<Value> {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "SELECT pc.ordinal,pc.check_id,pc.method,pc.evidence_plan_id,pc.evidence_plan_check_id, \
                p.spike_task_id AS plan_spike_task_id,p.captured_commit_sha AS plan_commit, \
                p.worktree_fingerprint, \
                i.id AS invocation_id,i.launch_state,i.process_state,i.exit_code,i.timed_out, \
                i.captured_commit_sha AS invocation_commit \
         FROM typed_evidence_planned_checks pc \
         JOIN evidence_plans p ON p.id=pc.evidence_plan_id \
         LEFT JOIN evidence_command_invocations i \
           ON i.plan_id=pc.evidence_plan_id AND i.check_id=pc.check_id \
         WHERE pc.attempt_id=$1 ORDER BY pc.ordinal",
    )
    .bind(attempt_id)
    .fetch_all(db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        serde_json::json!({
            "ordinal": row.get::<i32, _>("ordinal"),
            "check_id": row.get::<String, _>("check_id"),
            "method": row.get::<String, _>("method"),
            "evidence_plan_id": row.get::<Option<String>, _>("evidence_plan_id"),
            "evidence_plan_check_id": row.get::<Option<String>, _>("evidence_plan_check_id"),
            "plan_spike_task_id": row.get::<String, _>("plan_spike_task_id"),
            "plan_commit": row.get::<String, _>("plan_commit"),
            "worktree_fingerprint": row.get::<String, _>("worktree_fingerprint"),
            "invocation_id": row.get::<Option<String>, _>("invocation_id"),
            "launch_state": row.get::<Option<String>, _>("launch_state"),
            "process_state": row.get::<Option<String>, _>("process_state"),
            "exit_code": row.get::<Option<i32>, _>("exit_code"),
            "timed_out": row.get::<Option<bool>, _>("timed_out"),
            "invocation_commit": row.get::<Option<String>, _>("invocation_commit"),
        })
    })
    .collect()
}

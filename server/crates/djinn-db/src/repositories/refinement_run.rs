//! Read-only, exact-run refinement liveness projections.
//!
//! This module owns the durable-to-core mapping.  It deliberately does not
//! mutate lifecycle rows, claims, or heartbeats; consumers receive the shared
//! pure evaluator's result rather than a repository-local interpretation.

use djinn_core::refinement_liveness::{
    DbTimestamp, RefinementBetweenPhaseSnapshot, RefinementHeartbeatSnapshot,
    RefinementIntentSnapshot, RefinementIntentState, RefinementLivenessResult,
    RefinementLivenessSnapshot, RefinementParkKind, RefinementParkSnapshot, RefinementPhase,
    RefinementRole, RefinementRunSnapshot, RefinementRunState, RefinementSessionEvidence,
    RefinementSessionState, RefinementStopReason, RefinementTaskEvidence, RefinementTaskState,
    evaluate_refinement_liveness,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, postgres::PgRow};

use crate::Error;
use crate::repositories::proposal::ProposalRepository;

/// Input for an exact durable refinement-run observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadRefinementRunSnapshotRequest {
    pub run_id: String,
    /// Heartbeat grace evaluated against the transaction's database time.
    pub heartbeat_grace_millis: i64,
}

/// An exact run snapshot together with the one DB observation used to decide it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementRunSnapshotResult {
    pub snapshot: RefinementLivenessSnapshot,
    pub observed_at: DbTimestamp,
    pub liveness: RefinementLivenessResult,
}

/// Compact read-only projection for doctor and board-health consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementRunAggregate {
    pub proposal_id: String,
    pub run_id: String,
    pub generation: i32,
    pub state: RefinementRunState,
    pub observed_at: DbTimestamp,
    pub liveness: RefinementLivenessResult,
}

/// Typed contract failures for malformed durable refinement evidence.
#[derive(Debug, thiserror::Error)]
pub enum RefinementRunSnapshotError {
    #[error(transparent)]
    Database(#[from] Error),
    #[error("invalid refinement run evidence: {0}")]
    InvalidEvidence(String),
}

impl From<sqlx::Error> for RefinementRunSnapshotError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(Error::from(error))
    }
}

type SnapshotResult<T> = std::result::Result<T, RefinementRunSnapshotError>;

impl ProposalRepository {
    /// Load one exact run and evaluate it using a repeatable-read database-time
    /// observation. Evidence belonging to any other run is never selected.
    pub async fn load_refinement_run_snapshot(
        &self,
        request: LoadRefinementRunSnapshotRequest,
    ) -> SnapshotResult<Option<RefinementRunSnapshotResult>> {
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let result =
            load_snapshot_in_transaction(&mut tx, &request.run_id, request.heartbeat_grace_millis)
                .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Load the current nonterminal run for a proposal, if present.
    pub async fn load_current_refinement_run_snapshot(
        &self,
        proposal_id: &str,
        heartbeat_grace_millis: i64,
    ) -> SnapshotResult<Option<RefinementRunSnapshotResult>> {
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let run_id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM refinement_runs WHERE proposal_id = $1 \
             AND state IN ('running', 'parked') ORDER BY generation DESC LIMIT 1",
        )
        .bind(proposal_id)
        .fetch_optional(&mut *tx)
        .await?;
        let result = match run_id {
            Some(run_id) => {
                load_snapshot_in_transaction(&mut tx, &run_id, heartbeat_grace_millis).await?
            }
            None => None,
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Read every run for a proposal as a stable, side-effect-free aggregate.
    pub async fn load_refinement_run_aggregates(
        &self,
        proposal_id: &str,
        heartbeat_grace_millis: i64,
    ) -> SnapshotResult<Vec<RefinementRunAggregate>> {
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let run_ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM refinement_runs WHERE proposal_id = $1 ORDER BY generation DESC",
        )
        .bind(proposal_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut aggregates = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            if let Some(result) =
                load_snapshot_in_transaction(&mut tx, &run_id, heartbeat_grace_millis).await?
            {
                let generation = sqlx::query_scalar::<_, i32>(
                    "SELECT generation FROM refinement_runs WHERE id = $1",
                )
                .bind(&run_id)
                .fetch_one(&mut *tx)
                .await?;
                let proposal_id = sqlx::query_scalar::<_, String>(
                    "SELECT proposal_id FROM refinement_runs WHERE id = $1",
                )
                .bind(&run_id)
                .fetch_one(&mut *tx)
                .await?;
                aggregates.push(RefinementRunAggregate {
                    proposal_id,
                    run_id,
                    generation,
                    state: result.snapshot.run.state,
                    observed_at: result.observed_at,
                    liveness: result.liveness,
                });
            }
        }
        tx.commit().await?;
        Ok(aggregates)
    }
}

async fn load_snapshot_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    heartbeat_grace_millis: i64,
) -> SnapshotResult<Option<RefinementRunSnapshotResult>> {
    let observed_at = DbTimestamp(
        sqlx::query_scalar::<_, i64>(
            "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::BIGINT",
        )
        .fetch_one(&mut **tx)
        .await?,
    );
    let run = sqlx::query(
        "SELECT id, state, park_kind, stop_tag, stop_context, heartbeat_at \
         FROM refinement_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(run) = run else {
        return Ok(None);
    };
    let state = run_state(run.get("state"))?;
    let stop_tag: Option<String> = run.get("stop_tag");
    let stop_context: Option<serde_json::Value> = run.get("stop_context");
    let terminal_reason = stop_tag
        .map(|tag| stop_reason(&tag, stop_context))
        .transpose()?;
    let heartbeat_at: String = run.get("heartbeat_at");
    let intents = intent_rows(tx, run_id).await?;
    let between_phase = intents
        .iter()
        .find(|intent| {
            matches!(
                intent.state,
                RefinementIntentState::Pending | RefinementIntentState::Claimed
            )
        })
        .cloned()
        .map(|next_intent| RefinementBetweenPhaseSnapshot {
            run_id: run_id.to_owned(),
            next_intent,
        });
    let snapshot = RefinementLivenessSnapshot {
        run: RefinementRunSnapshot {
            run_id: run.get("id"),
            state,
            terminal_reason,
        },
        park: park_for(&run, run_id)?,
        intents,
        tasks: task_rows(tx, run_id).await?,
        sessions: session_rows(tx, run_id).await?,
        // Keep the handoff projection alongside the intent. The evaluator
        // deliberately applies ordinary intent precedence before this fallback.
        between_phase,
        heartbeat: Some(RefinementHeartbeatSnapshot {
            run_id: run_id.to_owned(),
            heartbeat_at: timestamp(&heartbeat_at)?,
            grace_millis: heartbeat_grace_millis,
        }),
    };
    let liveness = evaluate_refinement_liveness(&snapshot, observed_at);
    Ok(Some(RefinementRunSnapshotResult {
        snapshot,
        observed_at,
        liveness,
    }))
}

fn park_for(row: &PgRow, run_id: &str) -> SnapshotResult<Option<RefinementParkSnapshot>> {
    let state: String = row.get("state");
    let kind: Option<String> = row.get("park_kind");
    if state != "parked" {
        return Ok(None);
    }
    let kind = match kind.as_deref() {
        Some("awaiting_review") => RefinementParkKind::AwaitingReview,
        Some("awaiting_evidence") => RefinementParkKind::AwaitingEvidence,
        other => {
            return Err(RefinementRunSnapshotError::InvalidEvidence(format!(
                "parked run {run_id} has invalid park_kind {other:?}"
            )));
        }
    };
    Ok(Some(RefinementParkSnapshot {
        run_id: run_id.to_owned(),
        kind,
    }))
}

async fn intent_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> SnapshotResult<Vec<RefinementIntentSnapshot>> {
    let rows = sqlx::query("SELECT id, run_id, state, phase, role, claim_expires_at FROM refinement_dispatch_intents WHERE run_id = $1 ORDER BY round, id")
        .bind(run_id).fetch_all(&mut **tx).await?;
    rows.into_iter()
        .map(|row| {
            Ok(RefinementIntentSnapshot {
                intent_id: row.get("id"),
                run_id: row.get("run_id"),
                state: intent_state(row.get("state"))?,
                phase: phase(row.get("phase"))?,
                role: role(row.get("role"))?,
                lease_expires_at: row
                    .get::<Option<String>, _>("claim_expires_at")
                    .map(|v| timestamp(&v))
                    .transpose()?,
            })
        })
        .collect()
}

async fn task_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> SnapshotResult<Vec<RefinementTaskEvidence>> {
    let rows = sqlx::query("SELECT id, refinement_run_id, refinement_intent_id, status FROM tasks WHERE refinement_run_id = $1 ORDER BY id")
        .bind(run_id).fetch_all(&mut **tx).await?;
    rows.into_iter()
        .map(|row| {
            Ok(RefinementTaskEvidence {
                task_id: row.get("id"),
                run_id: row.get("refinement_run_id"),
                intent_id: row.get("refinement_intent_id"),
                state: task_state(row.get("status"))?,
            })
        })
        .collect()
}

async fn session_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> SnapshotResult<Vec<RefinementSessionEvidence>> {
    // Joining tasks is essential: a session cannot become evidence merely by
    // having the same proposal or a late task association.
    let rows = sqlx::query("SELECT s.id, s.task_id, s.status, t.refinement_run_id FROM sessions s JOIN tasks t ON t.id = s.task_id WHERE t.refinement_run_id = $1 ORDER BY s.id")
        .bind(run_id).fetch_all(&mut **tx).await?;
    rows.into_iter()
        .map(|row| {
            Ok(RefinementSessionEvidence {
                session_id: row.get("id"),
                task_id: row.get("task_id"),
                run_id: row.get("refinement_run_id"),
                state: if row.get::<String, _>("status") == "running" {
                    RefinementSessionState::Live
                } else {
                    RefinementSessionState::Ended
                },
            })
        })
        .collect()
}

fn run_state(value: String) -> SnapshotResult<RefinementRunState> {
    match value.as_str() {
        "running" => Ok(RefinementRunState::Active),
        "parked" => Ok(RefinementRunState::Parked),
        "terminal" => Ok(RefinementRunState::Terminal),
        _ => Err(invalid("run state", &value)),
    }
}
fn intent_state(value: String) -> SnapshotResult<RefinementIntentState> {
    match value.as_str() {
        "pending" => Ok(RefinementIntentState::Pending),
        "claimed" => Ok(RefinementIntentState::Claimed),
        "completed" | "materialized" => Ok(RefinementIntentState::Completed),
        "cancelled" => Ok(RefinementIntentState::Cancelled),
        _ => Err(invalid("intent state", &value)),
    }
}
fn phase(value: String) -> SnapshotResult<RefinementPhase> {
    match value.as_str() {
        "adversary_attack" => Ok(RefinementPhase::AdversaryAttack),
        "advocate_revision" => Ok(RefinementPhase::AdvocateRevision),
        "judge_adjudication" => Ok(RefinementPhase::JudgeAdjudication),
        _ => Err(invalid("phase", &value)),
    }
}
fn role(value: String) -> SnapshotResult<RefinementRole> {
    match value.as_str() {
        "adversary" => Ok(RefinementRole::Adversary),
        "advocate" => Ok(RefinementRole::Advocate),
        "judge" => Ok(RefinementRole::Judge),
        _ => Err(invalid("role", &value)),
    }
}
fn task_state(value: String) -> SnapshotResult<RefinementTaskState> {
    match value.as_str() {
        "open" => Ok(RefinementTaskState::Open),
        "queued" => Ok(RefinementTaskState::Queued),
        // `TaskStatus` has role-specific in-flight states while the evaluator
        // has a smaller liveness vocabulary. A task waiting for a role is
        // open; role-owned and PR-completion states remain running evidence.
        "needs_task_review" | "needs_lead_intervention" => Ok(RefinementTaskState::Open),
        "in_progress"
        | "running"
        | "in_task_review"
        | "approved"
        | "pr_draft"
        | "pr_ready"
        | "pr_review"
        | "in_lead_intervention" => Ok(RefinementTaskState::Running),
        "pool_paused" => Ok(RefinementTaskState::PoolPaused),
        "closed" => Ok(RefinementTaskState::Closed),
        "cancelled" => Ok(RefinementTaskState::Cancelled),
        _ => Err(invalid("task status", &value)),
    }
}
fn invalid(field: &str, value: &str) -> RefinementRunSnapshotError {
    RefinementRunSnapshotError::InvalidEvidence(format!("unknown {field} {value:?}"))
}
fn timestamp(value: &str) -> SnapshotResult<DbTimestamp> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|v| DbTimestamp(v.timestamp_millis()))
        .map_err(|e| {
            RefinementRunSnapshotError::InvalidEvidence(format!("invalid timestamp {value:?}: {e}"))
        })
}
fn stop_reason(
    tag: &str,
    context: Option<serde_json::Value>,
) -> SnapshotResult<RefinementStopReason> {
    if let Some(legacy) = context.as_ref().and_then(legacy_stop_context) {
        return Ok(legacy_stop_reason(tag, legacy));
    }
    serde_json::from_value(
        serde_json::json!({"tag": tag, "context": context.unwrap_or(serde_json::Value::Null)}),
    )
    .map_err(|e| {
        RefinementRunSnapshotError::InvalidEvidence(format!("invalid stop reason {tag:?}: {e}"))
    })
}

/// Migration 138 wraps historical lifecycle metadata rather than storing the
/// `RefinementStopReason` serde context directly. Recognize that owned wrapper
/// before attempting the normal modern-row deserialization path.
fn legacy_stop_context(context: &serde_json::Value) -> Option<&serde_json::Value> {
    context
        .as_object()?
        .contains_key("legacy_source_revision_id")
        .then_some(context)
}

fn legacy_stop_reason(tag: &str, context: &serde_json::Value) -> RefinementStopReason {
    let metadata = context
        .get("legacy_metadata")
        .and_then(serde_json::Value::as_object);
    let source_row = context
        .get("legacy_source_revision_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("migration-138")
        .to_owned();
    let text = |key: &str| {
        metadata
            .and_then(|metadata| metadata.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match tag {
        "adversary_dry" => RefinementStopReason::AdversaryDry,
        "round_cap" => RefinementStopReason::RoundCap,
        "spawn_cap" => RefinementStopReason::SpawnCap,
        "human_accepted" => RefinementStopReason::HumanAccepted,
        "human_rejected" => RefinementStopReason::HumanRejected,
        "interrupted" => RefinementStopReason::Interrupted {
            detail: text("detail").or_else(|| text("message")),
        },
        "operator_stop" => RefinementStopReason::OperatorStop {
            actor: text("actor").unwrap_or_else(|| "legacy_migration".to_owned()),
            reason: text("detail").or_else(|| text("message")),
        },
        // These tags require structured fields that historical lifecycle rows
        // did not guarantee. Preserve their normalized tag and source without
        // inventing details that the durable row does not contain.
        _ => RefinementStopReason::UnknownLegacy {
            original_value: tag.to_owned(),
            source_row,
        },
    }
}

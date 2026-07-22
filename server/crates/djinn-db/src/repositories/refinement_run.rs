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

impl ProposalRepository {
    /// Idempotently bind an already-correlated task to an intent and advance heartbeat once.
    pub async fn acknowledge_refinement_task_materialization(
        &self,
        request: AcknowledgeRefinementTaskMaterializationRequest,
    ) -> IntentMutationResult<bool> {
        valid_intent_request(
            &request.run_id,
            &request.intent_id,
            request.generation,
            &request.owner,
        )?;
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        let correlated = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM tasks t JOIN refinement_dispatch_intents i ON i.id = t.refinement_intent_id JOIN refinement_runs r ON r.id = i.run_id WHERE t.id = $1 AND t.refinement_run_id = $2 AND t.refinement_intent_id = $3 AND t.refinement_generation = $4 AND t.refinement_round = i.round AND t.refinement_phase = i.phase AND t.refinement_role = i.role AND r.generation = $4)").bind(&request.task_id).bind(&request.run_id).bind(&request.intent_id).bind(request.generation).fetch_one(&mut *tx).await?;
        if !correlated {
            return Err(RefinementIntentMutationError::TaskCorrelationMismatch {
                task_id: request.task_id,
            });
        }
        let changed = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'materialized', task_id = $4, claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.id = $2 AND i.run_id = $1 AND r.id = i.run_id AND r.generation = $3 AND i.state = 'claimed' AND i.claimed_by = $5").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.task_id).bind(&request.owner).execute(&mut *tx).await?.rows_affected() == 1;
        if changed {
            touch_heartbeat(&mut tx, &request.run_id, request.generation).await?;
        }
        let idempotent = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id = i.run_id WHERE i.id = $2 AND i.run_id = $1 AND r.generation = $3 AND i.state = 'materialized' AND i.task_id = $4)").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.task_id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        if changed || idempotent {
            Ok(changed)
        } else {
            Err(RefinementIntentMutationError::ClaimConflict {
                intent_id: request.intent_id,
                owner: request.owner,
            })
        }
    }

    /// Complete one phase and persist exactly one next-phase intent in the same transaction.
    pub async fn complete_refinement_intent(
        &self,
        request: CompleteRefinementIntentRequest,
    ) -> IntentMutationResult<RefinementNextIntent> {
        valid_intent_request(
            &request.run_id,
            &request.intent_id,
            request.generation,
            &request.owner,
        )?;
        if request.next_round <= 0 || request.next_idempotency_key.trim().is_empty() {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "positive next round and idempotency key are required".into(),
            ));
        }
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        let completed = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'completed', claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.id = $2 AND i.run_id = $1 AND r.id = i.run_id AND r.generation = $3 AND ((i.state = 'claimed' AND i.claimed_by = $4) OR i.state = 'materialized')").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.owner).execute(&mut *tx).await?.rows_affected() == 1;
        if !completed {
            let already = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id = i.run_id WHERE i.id = $2 AND i.run_id = $1 AND r.generation = $3 AND i.state = 'completed')").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).fetch_one(&mut *tx).await?;
            if !already {
                return Err(RefinementIntentMutationError::ClaimConflict {
                    intent_id: request.intent_id,
                    owner: request.owner,
                });
            }
        }
        let next_phase_name = phase_name(request.next_phase);
        let next_role_name = role_name(request.next_role);
        let new_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO refinement_dispatch_intents (id, run_id, round, phase, role, idempotency_key) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (run_id, round, phase, role) DO NOTHING").bind(&new_id).bind(&request.run_id).bind(request.next_round).bind(next_phase_name).bind(next_role_name).bind(&request.next_idempotency_key).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT id, round, phase, role FROM refinement_dispatch_intents WHERE run_id = $1 AND round = $2 AND phase = $3 AND role = $4").bind(&request.run_id).bind(request.next_round).bind(next_phase_name).bind(next_role_name).fetch_one(&mut *tx).await?;
        if completed {
            touch_heartbeat(&mut tx, &request.run_id, request.generation).await?;
        }
        tx.commit().await?;
        Ok(RefinementNextIntent {
            intent_id: row.get("id"),
            round: row.get("round"),
            phase: phase(row.get("phase")).map_err(snapshot_to_mutation)?,
            role: role(row.get("role")).map_err(snapshot_to_mutation)?,
        })
    }
}

async fn touch_heartbeat(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    generation: i32,
) -> IntentMutationResult<()> {
    let updated = sqlx::query("UPDATE refinement_runs SET heartbeat_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND generation = $2").bind(run_id).bind(generation).execute(&mut **tx).await?;
    if updated.rows_affected() == 1 {
        Ok(())
    } else {
        Err(RefinementIntentMutationError::GenerationConflict {
            run_id: run_id.to_owned(),
            generation,
        })
    }
}
fn phase_name(value: RefinementPhase) -> &'static str {
    match value {
        RefinementPhase::AdversaryAttack => "adversary_attack",
        RefinementPhase::AdvocateRevision => "advocate_revision",
        RefinementPhase::JudgeAdjudication => "judge_adjudication",
    }
}
fn role_name(value: RefinementRole) -> &'static str {
    match value {
        RefinementRole::Adversary => "adversary",
        RefinementRole::Advocate => "advocate",
        RefinementRole::Judge => "judge",
    }
}

impl ProposalRepository {
    pub async fn park_refinement_run(
        &self,
        request: ParkRefinementRunRequest,
    ) -> IntentMutationResult<bool> {
        self.db().ensure_initialized().await?;
        let kind = match request.kind {
            RefinementParkKind::AwaitingReview => "awaiting_review",
            RefinementParkKind::AwaitingEvidence => "awaiting_evidence",
        };
        let result = sqlx::query("UPDATE refinement_runs SET state = 'parked', park_kind = $3, parked_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), heartbeat_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND generation = $2 AND state = 'running'").bind(&request.run_id).bind(request.generation).bind(kind).execute(self.db().pool()).await?;
        Ok(result.rows_affected() == 1)
    }
    pub async fn terminal_refinement_run(
        &self,
        request: TerminalRefinementRunRequest,
    ) -> IntentMutationResult<bool> {
        self.db().ensure_initialized().await?;
        let context = serde_json::to_value(&request.reason)
            .map_err(|e| RefinementIntentMutationError::InvalidRequest(e.to_string()))?
            .get("context")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let result = sqlx::query("UPDATE refinement_runs SET state = 'terminal', park_kind = NULL, parked_at = NULL, terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), stop_tag = $3, stop_context = $4, heartbeat_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND generation = $2 AND state IN ('running', 'parked')").bind(&request.run_id).bind(request.generation).bind(request.reason.tag()).bind(context).execute(self.db().pool()).await?;
        Ok(result.rows_affected() == 1)
    }
    pub async fn record_refinement_durable_progress(
        &self,
        run_id: &str,
        generation: i32,
        _progress: RefinementDurableProgress,
    ) -> IntentMutationResult<()> {
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        touch_heartbeat(&mut tx, run_id, generation).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn load_refinement_lifecycle_aggregate(
        &self,
        proposal_id: &str,
        heartbeat_grace_millis: i64,
    ) -> IntentMutationResult<RefinementLifecycleAggregate> {
        if heartbeat_grace_millis < 0 {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "heartbeat grace must not be negative".into(),
            ));
        }
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let run_ids = sqlx::query_scalar::<_, String>("SELECT id FROM refinement_runs WHERE proposal_id = $1 AND state IN ('running', 'parked') ORDER BY generation DESC")
            .bind(proposal_id)
            .fetch_all(&mut *tx)
            .await?;
        let mut stale = 0;
        for run_id in run_ids {
            let snapshot = load_snapshot_in_transaction(&mut tx, &run_id, heartbeat_grace_millis)
                .await
                .map_err(snapshot_to_mutation)?;
            if matches!(
                snapshot.map(|snapshot| snapshot.liveness),
                Some(RefinementLivenessResult::Stale { .. })
            ) {
                stale += 1;
            }
        }
        let reaped = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proposal_revisions WHERE proposal_id = $1 AND refinement_stop_tag = 'reaped_phantom' AND created_at::timestamptz >= transaction_timestamp() - interval '24 hours'").bind(proposal_id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(RefinementLifecycleAggregate {
            stale_run_count: stale,
            reaped_phantom_last_24h: reaped,
        })
    }
}

/// A leased dispatch intent returned after a successful compare-and-swap claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementIntentLease {
    pub intent_id: String,
    pub run_id: String,
    pub generation: i32,
    pub round: i32,
    pub phase: RefinementPhase,
    pub role: RefinementRole,
    pub owner: String,
    pub expires_at: DbTimestamp,
}

/// A pending or currently leased exact-run intent. This projection is read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementPendingIntent {
    pub intent_id: String,
    pub run_id: String,
    pub generation: i32,
    pub round: i32,
    pub phase: RefinementPhase,
    pub role: RefinementRole,
    pub state: RefinementIntentState,
    pub claimed_by: Option<String>,
    pub claim_expires_at: Option<DbTimestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRefinementIntentRequest {
    pub run_id: String,
    pub intent_id: String,
    pub generation: i32,
    pub owner: String,
    pub lease_millis: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRefinementIntentClaimRequest {
    pub run_id: String,
    pub intent_id: String,
    pub generation: i32,
    pub owner: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcknowledgeRefinementTaskMaterializationRequest {
    pub run_id: String,
    pub intent_id: String,
    pub generation: i32,
    pub task_id: String,
    pub owner: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteRefinementIntentRequest {
    pub run_id: String,
    pub intent_id: String,
    pub generation: i32,
    pub owner: String,
    pub next_round: i32,
    pub next_phase: RefinementPhase,
    pub next_role: RefinementRole,
    pub next_idempotency_key: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementNextIntent {
    pub intent_id: String,
    pub round: i32,
    pub phase: RefinementPhase,
    pub role: RefinementRole,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkRefinementRunRequest {
    pub run_id: String,
    pub generation: i32,
    pub kind: RefinementParkKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRefinementRunRequest {
    pub run_id: String,
    pub generation: i32,
    pub reason: RefinementStopReason,
}
/// A named durable append boundary that is permitted to move the heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementDurableProgress {
    DebateAppend,
    VerdictAppend,
    TaskStarted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementLifecycleAggregate {
    pub stale_run_count: i64,
    pub reaped_phantom_last_24h: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RefinementIntentMutationError {
    #[error(transparent)]
    Database(#[from] Error),
    #[error("invalid refinement intent mutation: {0}")]
    InvalidRequest(String),
    #[error("run {run_id} generation {generation} is not current")]
    GenerationConflict { run_id: String, generation: i32 },
    #[error("intent {intent_id} is not claimable by {owner}")]
    ClaimConflict { intent_id: String, owner: String },
    #[error("task {task_id} does not have the required refinement correlation")]
    TaskCorrelationMismatch { task_id: String },
}
impl From<sqlx::Error> for RefinementIntentMutationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(Error::from(error))
    }
}
type IntentMutationResult<T> = std::result::Result<T, RefinementIntentMutationError>;

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

/// Suffix used to derive the first dispatch intent's idempotency key.
///
/// Both the run and intent columns are `VARCHAR(255)`, so admission input must
/// leave room for this durable child key rather than merely fitting the run.
const FIRST_INTENT_IDEMPOTENCY_SUFFIX: &str = "/adversary/1";

/// Durable origin for an admission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefinementAdmissionSource {
    ExplicitStart { actor: String },
    Demand { demand_id: String },
    Revision { revision_id: String },
}

/// Input to the serialized refinement admission authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitRefinementRunRequest {
    pub proposal_id: String,
    pub idempotency_key: String,
    pub source: RefinementAdmissionSource,
    pub heartbeat_grace_millis: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefinementAdmissionOutcome {
    Admitted {
        run_id: String,
        intent_id: String,
        generation: i32,
    },
    Existing {
        run_id: String,
        intent_id: String,
        generation: i32,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RefinementAdmissionError {
    #[error(transparent)]
    Database(#[from] Error),
    #[error("invalid refinement admission: {0}")]
    InvalidRequest(String),
    #[error("proposal not found: {proposal_id}")]
    ProposalNotFound { proposal_id: String },
    #[error("refinement already active for proposal {proposal_id}: run {run_id}")]
    AlreadyActive { proposal_id: String, run_id: String },
    #[error("generation {generation} for proposal {proposal_id} requires stale reap")]
    GenerationConflict {
        proposal_id: String,
        generation: i32,
    },
    #[error("admission conflicted with another transaction")]
    AdmissionConflict,
}

impl From<sqlx::Error> for RefinementAdmissionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(Error::from(error))
    }
}

impl ProposalRepository {
    /// Admit the first generation. A stale current run is left untouched; use
    /// `reap_and_admit` to make a destructive stale replacement explicit.
    pub async fn admit_refinement_run(
        &self,
        request: AdmitRefinementRunRequest,
    ) -> std::result::Result<RefinementAdmissionOutcome, RefinementAdmissionError> {
        self.admit_refinement_run_inner(request, false).await
    }

    /// Atomically reap an evaluator-stale generation and create its successor.
    pub async fn reap_and_admit(
        &self,
        request: AdmitRefinementRunRequest,
    ) -> std::result::Result<RefinementAdmissionOutcome, RefinementAdmissionError> {
        self.admit_refinement_run_inner(request, true).await
    }

    async fn admit_refinement_run_inner(
        &self,
        request: AdmitRefinementRunRequest,
        allow_reap: bool,
    ) -> std::result::Result<RefinementAdmissionOutcome, RefinementAdmissionError> {
        validate_admission(&request)?;
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        let seq = sqlx::query_scalar::<_, i32>(
            "SELECT latest_revision_seq FROM proposals WHERE id = $1 FOR UPDATE",
        )
        .bind(&request.proposal_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| RefinementAdmissionError::ProposalNotFound {
            proposal_id: request.proposal_id.clone(),
        })?;
        if let Some(row) = sqlx::query("SELECT id, generation FROM refinement_runs WHERE proposal_id = $1 AND idempotency_key = $2")
            .bind(&request.proposal_id).bind(&request.idempotency_key).fetch_optional(&mut *tx).await? {
            let run_id: String = row.get("id");
            let intent_id = first_intent_id(&mut tx, &run_id).await?;
            let outcome = RefinementAdmissionOutcome::Existing { run_id, intent_id, generation: row.get("generation") };
            tx.commit().await?;
            return Ok(outcome);
        }
        let current = sqlx::query("SELECT id, generation FROM refinement_runs WHERE proposal_id = $1 AND state IN ('running', 'parked') ORDER BY generation DESC LIMIT 1")
            .bind(&request.proposal_id).fetch_optional(&mut *tx).await?;
        let generation = if let Some(row) = current {
            let run_id: String = row.get("id");
            let generation: i32 = row.get("generation");
            let snapshot =
                load_snapshot_in_transaction(&mut tx, &run_id, request.heartbeat_grace_millis)
                    .await
                    .map_err(snapshot_admission_error)?;
            if !matches!(
                snapshot.map(|snapshot| snapshot.liveness),
                Some(RefinementLivenessResult::Stale { .. })
            ) {
                return Err(RefinementAdmissionError::AlreadyActive {
                    proposal_id: request.proposal_id,
                    run_id,
                });
            }
            if !allow_reap {
                return Err(RefinementAdmissionError::GenerationConflict {
                    proposal_id: request.proposal_id,
                    generation,
                });
            }
            reap_stale_run(&mut tx, &request.proposal_id, seq, &run_id, generation).await?;
            generation
                .checked_add(1)
                .ok_or(RefinementAdmissionError::AdmissionConflict)?
        } else {
            sqlx::query_scalar::<_, Option<i32>>(
                "SELECT max(generation) FROM refinement_runs WHERE proposal_id = $1",
            )
            .bind(&request.proposal_id)
            .fetch_one(&mut *tx)
            .await?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RefinementAdmissionError::AdmissionConflict)?
        };
        let outcome = insert_admission(&mut tx, &request, seq, generation).await?;
        tx.commit().await?;
        Ok(outcome)
    }
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

fn validate_admission(
    request: &AdmitRefinementRunRequest,
) -> std::result::Result<(), RefinementAdmissionError> {
    if request.proposal_id.trim().is_empty()
        || request.idempotency_key.trim().is_empty()
        || request.idempotency_key.len()
            > 255usize.saturating_sub(FIRST_INTENT_IDEMPOTENCY_SUFFIX.len())
        || request.heartbeat_grace_millis < 0
    {
        return Err(RefinementAdmissionError::InvalidRequest(
            "invalid proposal, idempotency key, or heartbeat grace".into(),
        ));
    }
    let identity = match &request.source {
        RefinementAdmissionSource::ExplicitStart { actor } => actor,
        RefinementAdmissionSource::Demand { demand_id } => demand_id,
        RefinementAdmissionSource::Revision { revision_id } => revision_id,
    };
    if identity.trim().is_empty() {
        return Err(RefinementAdmissionError::InvalidRequest(
            "admission source identity must not be blank".into(),
        ));
    }
    Ok(())
}

fn snapshot_admission_error(error: RefinementRunSnapshotError) -> RefinementAdmissionError {
    match error {
        RefinementRunSnapshotError::Database(error) => RefinementAdmissionError::Database(error),
        RefinementRunSnapshotError::InvalidEvidence(detail) => {
            RefinementAdmissionError::InvalidRequest(detail)
        }
    }
}

async fn first_intent_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> std::result::Result<String, RefinementAdmissionError> {
    sqlx::query_scalar(
        "SELECT id FROM refinement_dispatch_intents WHERE run_id = $1 ORDER BY round, id LIMIT 1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| {
        RefinementAdmissionError::InvalidRequest(format!(
            "idempotency winner {run_id} has no first intent"
        ))
    })
}

#[allow(clippy::too_many_arguments)]
async fn lifecycle(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    proposal_id: &str,
    seq: i32,
    kind: &str,
    run_id: &str,
    metadata: serde_json::Value,
    tag: Option<&str>,
    context: Option<serde_json::Value>,
) -> std::result::Result<String, RefinementAdmissionError> {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata, refinement_run_id, refinement_stop_tag, refinement_stop_context) VALUES ($1, $2, $3, '', '', 'markdown', '[]', NULL, $4, $5, $6, $7, $8)")
        .bind(&id).bind(proposal_id).bind(seq).bind(kind).bind(metadata).bind(run_id).bind(tag).bind(context).execute(&mut **tx).await?;
    Ok(id)
}

async fn reap_stale_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    proposal_id: &str,
    seq: i32,
    run_id: &str,
    generation: i32,
) -> std::result::Result<(), RefinementAdmissionError> {
    let context = serde_json::json!({"prior_run_id":run_id,"generation":generation as u64,"evidence_summary":"shared evaluator found no live exact-run evidence"});
    let result = sqlx::query("UPDATE refinement_runs SET state = 'terminal', terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), stop_tag = 'reaped_phantom', stop_context = $3, updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND proposal_id = $2 AND generation = $4 AND state IN ('running', 'parked')")
        .bind(run_id).bind(proposal_id).bind(&context).bind(generation).execute(&mut **tx).await?;
    if result.rows_affected() != 1 {
        return Err(RefinementAdmissionError::AdmissionConflict);
    }
    lifecycle(
        tx,
        proposal_id,
        seq,
        "refinement_stop",
        run_id,
        serde_json::json!({"reason_tag":"reaped_phantom","stop_context":context}),
        Some("reaped_phantom"),
        Some(context),
    )
    .await?;
    Ok(())
}

async fn insert_admission(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &AdmitRefinementRunRequest,
    seq: i32,
    generation: i32,
) -> std::result::Result<RefinementAdmissionOutcome, RefinementAdmissionError> {
    let run_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO refinement_runs (id, proposal_id, generation, idempotency_key) VALUES ($1, $2, $3, $4)").bind(&run_id).bind(&request.proposal_id).bind(generation).bind(&request.idempotency_key).execute(&mut **tx).await?;
    let metadata = serde_json::json!({"run_id":run_id,"generation":generation,"source":request.source,"idempotency_key":request.idempotency_key});
    let start_id = lifecycle(
        tx,
        &request.proposal_id,
        seq,
        "refinement_start",
        &run_id,
        metadata,
        None,
        None,
    )
    .await?;
    sqlx::query("UPDATE refinement_runs SET source_start_revision_id = $2 WHERE id = $1")
        .bind(&run_id)
        .bind(&start_id)
        .execute(&mut **tx)
        .await?;
    let intent_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO refinement_dispatch_intents (id, run_id, round, phase, role, idempotency_key) VALUES ($1, $2, 1, 'adversary_attack', 'adversary', $3)").bind(&intent_id).bind(&run_id).bind(format!("{}{}", request.idempotency_key, FIRST_INTENT_IDEMPOTENCY_SUFFIX)).execute(&mut **tx).await?;
    Ok(RefinementAdmissionOutcome::Admitted {
        run_id,
        intent_id,
        generation,
    })
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

impl ProposalRepository {
    /// Load pending and claimed work without changing claims, rows, or heartbeat.
    pub async fn load_pending_or_claimed_refinement_intents(
        &self,
        run_id: &str,
        generation: i32,
    ) -> IntentMutationResult<Vec<RefinementPendingIntent>> {
        self.db().ensure_initialized().await?;
        ensure_generation(self.db().pool(), run_id, generation).await?;
        let rows = sqlx::query("SELECT i.id, i.run_id, r.generation, i.round, i.phase, i.role, i.state, i.claimed_by, i.claim_expires_at FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id = i.run_id WHERE i.run_id = $1 AND i.state IN ('pending', 'claimed') ORDER BY i.round, i.id").bind(run_id).fetch_all(self.db().pool()).await?;
        rows.into_iter().map(pending_intent_row).collect()
    }

    /// CAS claim pending or DB-time-expired work. Polling does not advance heartbeat.
    pub async fn claim_refinement_intent(
        &self,
        request: ClaimRefinementIntentRequest,
    ) -> IntentMutationResult<Option<RefinementIntentLease>> {
        valid_intent_request(
            &request.run_id,
            &request.intent_id,
            request.generation,
            &request.owner,
        )?;
        if request.lease_millis <= 0 {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "lease must be positive".into(),
            ));
        }
        self.db().ensure_initialized().await?;
        let row = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'claimed', claimed_by = $4, claimed_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), claim_expires_at = to_char((transaction_timestamp() + ($5 * interval '1 millisecond')) AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.id = $2 AND i.run_id = $1 AND r.id = i.run_id AND r.generation = $3 AND r.state = 'running' AND (i.state = 'pending' OR (i.state = 'claimed' AND i.claim_expires_at::timestamptz <= transaction_timestamp())) RETURNING i.id, i.run_id, r.generation, i.round, i.phase, i.role, i.claimed_by, i.claim_expires_at").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.owner).bind(request.lease_millis).fetch_optional(self.db().pool()).await?;
        row.map(lease_row).transpose()
    }

    /// Release only an owned lease; this is not durable progress.
    pub async fn release_refinement_intent_claim(
        &self,
        request: ReleaseRefinementIntentClaimRequest,
    ) -> IntentMutationResult<bool> {
        valid_intent_request(
            &request.run_id,
            &request.intent_id,
            request.generation,
            &request.owner,
        )?;
        self.db().ensure_initialized().await?;
        let result = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'pending', claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.id = $2 AND i.run_id = $1 AND r.id = i.run_id AND r.generation = $3 AND i.state = 'claimed' AND i.claimed_by = $4").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.owner).execute(self.db().pool()).await?;
        Ok(result.rows_affected() == 1)
    }

    /// Make DB-time-expired leases pending again without touching heartbeat.
    pub async fn expire_refinement_intent_claims(
        &self,
        run_id: &str,
        generation: i32,
    ) -> IntentMutationResult<u64> {
        self.db().ensure_initialized().await?;
        let result = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'pending', claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.run_id = $1 AND r.id = i.run_id AND r.generation = $2 AND i.state = 'claimed' AND i.claim_expires_at::timestamptz <= transaction_timestamp()").bind(run_id).bind(generation).execute(self.db().pool()).await?;
        Ok(result.rows_affected())
    }
}

fn valid_intent_request(
    run_id: &str,
    intent_id: &str,
    generation: i32,
    owner: &str,
) -> IntentMutationResult<()> {
    if run_id.trim().is_empty()
        || intent_id.trim().is_empty()
        || owner.trim().is_empty()
        || generation <= 0
    {
        return Err(RefinementIntentMutationError::InvalidRequest(
            "run, intent, owner, and positive generation are required".into(),
        ));
    }
    Ok(())
}
async fn ensure_generation(
    pool: &sqlx::PgPool,
    run_id: &str,
    generation: i32,
) -> IntentMutationResult<()> {
    let present = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM refinement_runs WHERE id = $1 AND generation = $2)",
    )
    .bind(run_id)
    .bind(generation)
    .fetch_one(pool)
    .await?;
    present
        .then_some(())
        .ok_or_else(|| RefinementIntentMutationError::GenerationConflict {
            run_id: run_id.to_owned(),
            generation,
        })
}
fn snapshot_to_mutation(error: RefinementRunSnapshotError) -> RefinementIntentMutationError {
    RefinementIntentMutationError::InvalidRequest(error.to_string())
}
fn pending_intent_row(row: PgRow) -> IntentMutationResult<RefinementPendingIntent> {
    Ok(RefinementPendingIntent {
        intent_id: row.get("id"),
        run_id: row.get("run_id"),
        generation: row.get("generation"),
        round: row.get("round"),
        phase: phase(row.get("phase")).map_err(snapshot_to_mutation)?,
        role: role(row.get("role")).map_err(snapshot_to_mutation)?,
        state: intent_state(row.get("state")).map_err(snapshot_to_mutation)?,
        claimed_by: row.get("claimed_by"),
        claim_expires_at: row
            .get::<Option<String>, _>("claim_expires_at")
            .map(|v| timestamp(&v))
            .transpose()
            .map_err(snapshot_to_mutation)?,
    })
}
fn lease_row(row: PgRow) -> IntentMutationResult<RefinementIntentLease> {
    Ok(RefinementIntentLease {
        intent_id: row.get("id"),
        run_id: row.get("run_id"),
        generation: row.get("generation"),
        round: row.get("round"),
        phase: phase(row.get("phase")).map_err(snapshot_to_mutation)?,
        role: role(row.get("role")).map_err(snapshot_to_mutation)?,
        owner: row.get("claimed_by"),
        expires_at: timestamp(&row.get::<String, _>("claim_expires_at"))
            .map_err(snapshot_to_mutation)?,
    })
}

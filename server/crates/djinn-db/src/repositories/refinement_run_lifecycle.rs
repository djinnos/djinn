//! Leased refinement-intent lifecycle and durable progress mutations.
//!
//! Public request/response types remain in `refinement_run`; this focused module
//! contains the repository implementation so the snapshot/admission module stays
//! within the changed-Rust-file size guard.

use djinn_core::refinement_liveness::{
    RefinementLivenessResult, RefinementParkKind, RefinementPhase, RefinementRole,
};
use sqlx::{Row, postgres::PgRow};

use crate::repositories::proposal::ProposalRepository;

use super::refinement_run::*;

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
        let changed = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'materialized', task_id = $4, claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.id = $2 AND i.run_id = $1 AND r.id = i.run_id AND r.generation = $3 AND i.state = 'claimed' AND i.claimed_by = $5 AND i.claim_expires_at::timestamptz > transaction_timestamp()").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.task_id).bind(&request.owner).execute(&mut *tx).await?.rows_affected() == 1;
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
        let next_phase_name = phase_name(request.next_phase);
        let next_role_name = role_name(request.next_role);
        // Locking the source serializes its one durable successor selection.
        let source = sqlx::query("SELECT i.state, i.claimed_by, i.claim_expires_at::timestamptz > transaction_timestamp() AS claim_unexpired, i.next_intent_id FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id = i.run_id WHERE i.id = $2 AND i.run_id = $1 AND r.generation = $3 FOR UPDATE").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).fetch_optional(&mut *tx).await?;
        let Some(source) = source else {
            return Err(RefinementIntentMutationError::GenerationConflict {
                run_id: request.run_id,
                generation: request.generation,
            });
        };
        let source_state: String = source.get("state");
        if source_state == "completed" {
            let Some(successor_id) = source.get::<Option<String>, _>("next_intent_id") else {
                return Err(RefinementIntentMutationError::InvalidRequest(
                    "completed intent has no durable successor".into(),
                ));
            };
            let row = sqlx::query("SELECT id, round, phase, role, idempotency_key FROM refinement_dispatch_intents WHERE id = $1 AND run_id = $2").bind(successor_id).bind(&request.run_id).fetch_one(&mut *tx).await?;
            let matches_request = row.get::<i32, _>("round") == request.next_round
                && row.get::<String, _>("phase") == next_phase_name
                && row.get::<String, _>("role") == next_role_name
                && row.get::<String, _>("idempotency_key") == request.next_idempotency_key;
            if !matches_request {
                return Err(RefinementIntentMutationError::InvalidRequest(
                    "completed intent retry selects a different successor".into(),
                ));
            }
            tx.commit().await?;
            return Ok(RefinementNextIntent {
                intent_id: row.get("id"),
                round: row.get("round"),
                phase: phase(row.get("phase")).map_err(snapshot_to_mutation)?,
                role: role(row.get("role")).map_err(snapshot_to_mutation)?,
            });
        }
        let claimed_by_owner = source.get::<Option<String>, _>("claimed_by").as_deref()
            == Some(request.owner.as_str());
        let claim_unexpired: Option<bool> = source.get("claim_unexpired");
        if source_state != "materialized"
            && !(source_state == "claimed" && claimed_by_owner && claim_unexpired == Some(true))
        {
            return Err(RefinementIntentMutationError::ClaimConflict {
                intent_id: request.intent_id,
                owner: request.owner,
            });
        }
        let completed = sqlx::query("UPDATE refinement_dispatch_intents SET state = 'completed', claimed_by = NULL, claimed_at = NULL, claim_expires_at = NULL, terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND ((state = 'claimed' AND claimed_by = $2 AND claim_expires_at::timestamptz > transaction_timestamp()) OR state = 'materialized')").bind(&request.intent_id).bind(&request.owner).execute(&mut *tx).await?.rows_affected() == 1;
        if !completed {
            return Err(RefinementIntentMutationError::ClaimConflict {
                intent_id: request.intent_id,
                owner: request.owner,
            });
        }
        let new_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO refinement_dispatch_intents (id, run_id, round, phase, role, idempotency_key) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (run_id, round, phase, role) DO NOTHING").bind(&new_id).bind(&request.run_id).bind(request.next_round).bind(next_phase_name).bind(next_role_name).bind(&request.next_idempotency_key).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT id, round, phase, role, idempotency_key FROM refinement_dispatch_intents WHERE run_id = $1 AND round = $2 AND phase = $3 AND role = $4").bind(&request.run_id).bind(request.next_round).bind(next_phase_name).bind(next_role_name).fetch_one(&mut *tx).await?;
        if row.get::<String, _>("idempotency_key") != request.next_idempotency_key {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "next intent identity is already bound to a different idempotency key".into(),
            ));
        }
        sqlx::query("UPDATE refinement_dispatch_intents SET next_intent_id = $2 WHERE id = $1 AND next_intent_id IS NULL").bind(&request.intent_id).bind(row.get::<String, _>("id")).execute(&mut *tx).await?;
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

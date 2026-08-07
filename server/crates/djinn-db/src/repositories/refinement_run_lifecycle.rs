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
    /// Read-only enumeration keyed only on durable task terminal state and the
    /// absence of a successor. No age or heartbeat participates.
    pub async fn load_refinement_stalled_handoffs(
        &self,
    ) -> IntentMutationResult<Vec<RefinementStalledHandoff>> {
        self.db().ensure_initialized().await?;
        let rows = sqlx::query(
            "SELECT r.proposal_id, r.id AS run_id, r.generation, i.id AS intent_id, \
                    t.id AS task_id, t.status AS task_status, COALESCE(t.closed_at, t.updated_at) AS task_terminal_at, \
                    i.outcome_attempts, (i.next_intent_id IS NOT NULL) AS successor_present, GREATEST(0, EXTRACT(EPOCH FROM \
                    (transaction_timestamp() - COALESCE(t.closed_at, t.updated_at)::timestamptz))::BIGINT) AS terminal_elapsed_seconds \
             FROM refinement_dispatch_intents i \
             JOIN refinement_runs r ON r.id = i.run_id \
             JOIN tasks t ON t.id = i.task_id \
             WHERE r.state = 'running' AND i.state = 'materialized' \
             ORDER BY t.updated_at, i.id",
        )
        .fetch_all(self.db().pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let task_status: String = row.get("task_status");
                if !is_refinement_stalled_handoff(
                    true,
                    task_status == "closed",
                    row.get("successor_present"),
                ) {
                    return None;
                }
                Some(RefinementStalledHandoff {
                    proposal_id: row.get("proposal_id"),
                    run_id: row.get("run_id"),
                    generation: row.get("generation"),
                    intent_id: row.get("intent_id"),
                    task_id: row.get("task_id"),
                    task_status,
                    task_terminal_at: row.get("task_terminal_at"),
                    terminal_elapsed_seconds: row.get("terminal_elapsed_seconds"),
                    outcome_attempts: row.get("outcome_attempts"),
                })
            })
            .collect())
    }

    /// Atomically consume one durable retry from the exact stalled handoff.
    pub async fn increment_refinement_outcome_attempt(
        &self,
        run_id: &str,
        generation: i32,
        task_id: &str,
    ) -> IntentMutationResult<i32> {
        self.db().ensure_initialized().await?;
        let attempts = sqlx::query_scalar::<_, i32>(
            "UPDATE refinement_dispatch_intents i SET outcome_attempts = outcome_attempts + 1, \
                    updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
             FROM refinement_runs r, tasks t \
             WHERE i.task_id = $1 AND i.run_id = $2 AND r.id = i.run_id AND r.generation = $3 \
               AND r.state = 'running' AND i.state = 'materialized' AND t.id = i.task_id \
               AND t.status = 'closed' AND i.next_intent_id IS NULL \
             RETURNING i.outcome_attempts",
        )
        .bind(task_id).bind(run_id).bind(generation)
        .fetch_optional(self.db().pool()).await?;
        attempts.ok_or_else(|| RefinementIntentMutationError::NotStalledHandoff {
            run_id: run_id.to_owned(),
        })
    }

    /// Guarded operator escape hatch. It uses the same generation fence as all
    /// other terminal transitions and refuses live/open role work.
    pub async fn operator_stop_stalled_refinement(
        &self,
        run_id: &str,
        generation: i32,
        actor: &str,
        reason: Option<String>,
    ) -> IntentMutationResult<bool> {
        if run_id.trim().is_empty() || generation <= 0 || actor.trim().is_empty() {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "exact run, positive generation, and actor are required".into(),
            ));
        }
        self.db().ensure_initialized().await?;
        let stop = djinn_core::refinement_liveness::RefinementStopReason::OperatorStop {
            actor: actor.to_owned(),
            reason,
        };
        let context = serde_json::to_value(&stop)
            .map_err(|e| RefinementIntentMutationError::InvalidRequest(e.to_string()))?
            .get("context")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let mut tx = self.db().pool().begin().await?;
        let proposal_id = sqlx::query_scalar::<_, String>(
            "UPDATE refinement_runs r SET state = 'terminal', terminal_at = \
                    to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                    stop_tag = 'operator_stop', stop_context = $3, updated_at = \
                    to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
             WHERE r.id = $1 AND r.generation = $2 AND r.state = 'running' AND EXISTS ( \
               SELECT 1 FROM refinement_dispatch_intents i JOIN tasks t ON t.id = i.task_id \
               WHERE i.run_id = r.id AND i.state = 'materialized' AND i.next_intent_id IS NULL \
                 AND t.status = 'closed') RETURNING r.proposal_id",
        )
        .bind(run_id).bind(generation).bind(&context)
        .fetch_optional(&mut *tx).await?;
        let Some(proposal_id) = proposal_id else {
            return Err(RefinementIntentMutationError::NotStalledHandoff {
                run_id: run_id.to_owned(),
            });
        };
        sqlx::query(
            "UPDATE refinement_dispatch_intents SET state='cancelled', terminal_at=\
             to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
             updated_at=to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
             WHERE run_id=$1 AND state='materialized' AND next_intent_id IS NULL AND task_id IN \
             (SELECT id FROM tasks WHERE status='closed')",
        ).bind(run_id).execute(&mut *tx).await?;
        let seq = sqlx::query_scalar::<_, i32>(
            "SELECT latest_revision_seq FROM proposals WHERE id = $1 FOR UPDATE",
        )
        .bind(&proposal_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata, refinement_run_id, refinement_stop_tag, refinement_stop_context) VALUES ($1, $2, $3, '', '', 'markdown', '[]', NULL, 'refinement_stop', $4, $5, 'operator_stop', $6)")
            .bind(uuid::Uuid::now_v7().to_string()).bind(&proposal_id).bind(seq)
            .bind(serde_json::json!({"reason_tag":"operator_stop","stop_context":context}))
            .bind(run_id).bind(&context).execute(&mut *tx).await?;
        drain_pending_feedback_handoff(&mut tx, &proposal_id, generation).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Discover all nonterminal exact runs for restart recovery. Unlike the
    /// dispatcher list this includes parks, which are live but not dispatchable.
    pub async fn load_recoverable_refinement_runs(
        &self,
    ) -> IntentMutationResult<Vec<ActiveRefinementRun>> {
        self.db().ensure_initialized().await?;
        let rows = sqlx::query("SELECT id, proposal_id, generation FROM refinement_runs WHERE state IN ('running', 'parked') ORDER BY created_at")
            .fetch_all(self.db().pool())
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| ActiveRefinementRun {
                run_id: row.get("id"),
                proposal_id: row.get("proposal_id"),
                generation: row.get("generation"),
            })
            .collect())
    }

    /// The spec revision this run was admitted from — the pre-refinement
    /// snapshot the human's reject reverts to, and the `before` side of the
    /// reviewed diff.
    ///
    /// Disposable in-memory projections must read it from here rather than from
    /// the proposal's current head: the head advances with every revision the
    /// tribunal writes, so a projection rebuilt mid-run would otherwise adopt
    /// the refined spec as its own "pre-refinement" snapshot.
    pub async fn refinement_run_captured_snapshot_seq(
        &self,
        run_id: &str,
    ) -> IntentMutationResult<Option<i32>> {
        self.db().ensure_initialized().await?;
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT start.seq FROM refinement_runs r \
             JOIN proposal_revisions start ON start.id = r.source_start_revision_id \
             WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_optional(self.db().pool())
        .await?)
    }

    /// Discover durable running runs without changing leases or heartbeat.
    pub async fn load_active_refinement_runs(
        &self,
    ) -> IntentMutationResult<Vec<ActiveRefinementRun>> {
        self.db().ensure_initialized().await?;
        let rows = sqlx::query("SELECT id, proposal_id, generation FROM refinement_runs WHERE state = 'running' ORDER BY created_at").fetch_all(self.db().pool()).await?;
        Ok(rows
            .into_iter()
            .map(|row| ActiveRefinementRun {
                run_id: row.get("id"),
                proposal_id: row.get("proposal_id"),
                generation: row.get("generation"),
            })
            .collect())
    }

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

/// Atomically convert the elected pending cohort into one dispatchable run.
async fn drain_pending_feedback_handoff(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    proposal_id: &str,
    previous_generation: i32,
) -> IntentMutationResult<()> {
    let row = sqlx::query("SELECT boundary_feedback_id FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending' AND cohort_owner FOR UPDATE")
        .bind(proposal_id).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let boundary: String = row.get("boundary_feedback_id");
    let key = format!("pending-feedback/{boundary}");
    let seq = sqlx::query_scalar::<_, i32>(
        "SELECT latest_revision_seq FROM proposals WHERE id=$1 FOR UPDATE",
    )
    .bind(proposal_id)
    .fetch_one(&mut **tx)
    .await?;
    let outcome = insert_admission(
        tx,
        &AdmitRefinementRunRequest {
            proposal_id: proposal_id.to_owned(),
            idempotency_key: key,
            source: RefinementAdmissionSource::Demand {
                demand_id: boundary.clone(),
            },
            heartbeat_grace_millis: 60_000,
        },
        seq,
        previous_generation + 1,
    )
    .await
    .map_err(|error| match error {
        RefinementAdmissionError::Database(error) => RefinementIntentMutationError::Database(error),
        error => RefinementIntentMutationError::InvalidRequest(error.to_string()),
    })?;
    let RefinementAdmissionOutcome::Admitted { run_id, .. } = outcome else {
        return Err(RefinementIntentMutationError::InvalidRequest(
            "pending successor identity unexpectedly existed".into(),
        ));
    };
    capture_pending_feedback_sources(tx, proposal_id, seq).await?;
    sqlx::query("UPDATE pending_feedback_refinement_handoffs SET state='admitted', successor_run_id=$2 WHERE proposal_id=$1 AND state='pending'")
        .bind(proposal_id).bind(&run_id).execute(&mut **tx).await?;
    Ok(())
}

/// Consume the elected cohort at the awaiting-review boundary. Unlike a
/// terminal boundary, this preserves the parked run: normal demand admission
/// resumes the exact run and appends a single successor intent instead of
/// inserting a second nonterminal run for the proposal.
async fn drain_pending_feedback_handoff_from_awaiting_review_park(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    proposal_id: &str,
    run_id: &str,
    generation: i32,
) -> IntentMutationResult<()> {
    let row = sqlx::query("SELECT boundary_feedback_id FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending' AND cohort_owner FOR UPDATE")
        .bind(proposal_id).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let boundary: String = row.get("boundary_feedback_id");
    let key = format!("pending-feedback/{boundary}");
    let seq = sqlx::query_scalar::<_, i32>(
        "SELECT latest_revision_seq FROM proposals WHERE id=$1 FOR UPDATE",
    )
    .bind(proposal_id)
    .fetch_one(&mut **tx)
    .await?;
    resume_awaiting_review_park(
        tx,
        &AdmitRefinementRunRequest {
            proposal_id: proposal_id.to_owned(),
            idempotency_key: key,
            source: RefinementAdmissionSource::Demand {
                demand_id: boundary,
            },
            heartbeat_grace_millis: 60_000,
        },
        run_id,
        generation,
    )
    .await
    .map_err(|error| match error {
        RefinementAdmissionError::Database(error) => RefinementIntentMutationError::Database(error),
        error => RefinementIntentMutationError::InvalidRequest(error.to_string()),
    })?;
    capture_pending_feedback_sources(tx, proposal_id, seq).await?;
    sqlx::query("UPDATE pending_feedback_refinement_handoffs SET state='admitted', successor_run_id=$2 WHERE proposal_id=$1 AND state='pending'")
        .bind(proposal_id)
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Materialize every member of the elected cohort before its successor intent
/// becomes visible.  The source-table existence check makes a retry after a
/// crash/restart idempotent and prevents a boundary from being captured twice.
async fn capture_pending_feedback_sources(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    proposal_id: &str,
    revision_seq: i32,
) -> IntentMutationResult<()> {
    let boundaries = sqlx::query_scalar::<_, String>(
        "SELECT boundary_feedback_id FROM pending_feedback_refinement_handoffs WHERE proposal_id=$1 AND state='pending' ORDER BY created_at,id FOR UPDATE",
    )
    .bind(proposal_id)
    .fetch_all(&mut **tx)
    .await?;
    for boundary in boundaries {
        let captured = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM proposal_feedback_refinement_sources WHERE source_feedback_id=$1)",
        )
        .bind(&boundary)
        .fetch_one(&mut **tx)
        .await?;
        if captured {
            continue;
        }
        let feedback = sqlx::query(
            "SELECT parent_id,author_kind,author_user_id,author_model,body,severity,created_at FROM proposal_feedback WHERE id=$1 AND proposal_id=$2 FOR UPDATE",
        )
        .bind(&boundary)
        .bind(proposal_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| RefinementIntentMutationError::InvalidRequest("pending feedback boundary is missing".into()))?;
        let generation = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT max(generation) FROM proposal_feedback_refinement_injections WHERE root_feedback_id=$1",
        )
        .bind(&boundary)
        .fetch_one(&mut **tx)
        .await?
        .unwrap_or(0)
        + 1;
        let round = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT max(round) FROM proposal_debate_trail WHERE proposal_id=$1",
        )
        .bind(proposal_id)
        .fetch_one(&mut **tx)
        .await?
        .unwrap_or(0)
            + 1;
        let injection_id = uuid::Uuid::now_v7().to_string();
        let metadata = serde_json::json!({"kind":"feedback_refinement_generation_v1","root_feedback_id":boundary,"injection_id":injection_id,"generation":generation});
        let debate_id: String = sqlx::query_scalar(
            "INSERT INTO proposal_debate_trail (id,proposal_id,kind,body,blocking,agent_role,author_kind,against_revision_seq,round,body_metadata) VALUES ($1,$2,'human_feedback',$3,true,'human_feedback','agent',$4,$5,$6) RETURNING id",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(proposal_id)
        .bind(feedback.get::<String, _>("body"))
        .bind(revision_seq)
        .bind(round)
        .bind(&metadata)
        .fetch_one(&mut **tx)
        .await?;
        let cutoff_at: String = sqlx::query_scalar("SELECT to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')")
            .fetch_one(&mut **tx).await?;
        sqlx::query("INSERT INTO proposal_feedback_refinement_injections (id,proposal_id,root_feedback_id,generation,state,cutoff_at,cutoff_feedback_id,round,debate_entry_id) VALUES ($1,$2,$3,$4,'injected',$5,$6,$7,$8)")
            .bind(&injection_id).bind(proposal_id).bind(&boundary).bind(generation).bind(&cutoff_at).bind(&boundary).bind(round).bind(&debate_id).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO proposal_feedback_refinement_sources (injection_id,source_feedback_id,source_ordinal,source_parent_id,source_author_kind,source_author_user_id,source_author_model,source_body,source_severity,source_created_at,captured_at) VALUES ($1,$2,1,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(&injection_id).bind(&boundary).bind(feedback.get::<Option<String>, _>("parent_id")).bind(feedback.get::<String, _>("author_kind")).bind(feedback.get::<Option<String>, _>("author_user_id")).bind(feedback.get::<Option<String>, _>("author_model")).bind(feedback.get::<String, _>("body")).bind(feedback.get::<String, _>("severity")).bind(feedback.get::<String, _>("created_at")).bind(&cutoff_at).execute(&mut **tx).await?;
    }
    Ok(())
}

async fn touch_heartbeat(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    generation: i32,
) -> IntentMutationResult<()> {
    let updated = sqlx::query("UPDATE refinement_runs SET heartbeat_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND generation = $2 AND state IN ('running', 'parked')").bind(run_id).bind(generation).execute(&mut **tx).await?;
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
    pub async fn park_refinement_run_from_intent(
        &self,
        request: ParkRefinementRunFromIntentRequest,
    ) -> IntentMutationResult<bool> {
        self.source_intent_transition(request.source, Some(request.kind), None)
            .await
    }
    pub async fn terminal_refinement_run_from_intent(
        &self,
        request: TerminalRefinementRunFromIntentRequest,
    ) -> IntentMutationResult<bool> {
        self.source_intent_transition(request.source, None, Some(request.reason))
            .await
    }

    /// Resolve one exact generation parked for the human review boundary.
    /// Rejection writes the captured spec snapshot and terminal state in one
    /// transaction, so neither side can become visible independently.
    pub async fn resolve_refinement_human_review(
        &self,
        request: ResolveRefinementHumanReviewRequest,
    ) -> IntentMutationResult<bool> {
        if request.run_id.trim().is_empty() || request.generation <= 0 {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "exact run and positive generation are required".into(),
            ));
        }
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        let run = sqlx::query(
            "SELECT r.proposal_id, r.state, r.park_kind, start.seq AS captured_seq \
             FROM refinement_runs r JOIN proposal_revisions start ON start.id = r.source_start_revision_id \
             WHERE r.id = $1 AND r.generation = $2 FOR UPDATE",
        )
        .bind(&request.run_id)
        .bind(request.generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(run) = run else {
            return Err(RefinementIntentMutationError::GenerationConflict {
                run_id: request.run_id,
                generation: request.generation,
            });
        };
        let run_state: String = run.get("state");
        if run_state == "terminal" {
            tx.commit().await?;
            return Ok(false);
        }
        if run_state != "parked"
            || run.get::<Option<String>, _>("park_kind").as_deref() != Some("awaiting_review")
        {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "human review resolution requires an exact awaiting-review park".into(),
            ));
        }
        // The run row itself is the only authority for which revision the
        // refinement started from. Callers hold a disposable in-memory
        // projection that is rebuilt from the *live* proposal head whenever the
        // coordinator restarts or re-hydrates mid-run, so any caller-supplied
        // snapshot seq drifts forward with every tribunal revision. Comparing
        // against it wedged parked runs permanently: neither accept nor reject
        // could ever match once a projection had been rebuilt.
        let captured_seq: i32 = run.get("captured_seq");
        let proposal_id: String = run.get("proposal_id");
        let reason = if request.accept {
            djinn_core::refinement_liveness::RefinementStopReason::HumanAccepted
        } else {
            djinn_core::refinement_liveness::RefinementStopReason::HumanRejected
        };
        if !request.accept {
            let head =
                sqlx::query("SELECT latest_revision_seq FROM proposals WHERE id = $1 FOR UPDATE")
                    .bind(&proposal_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| {
                        RefinementIntentMutationError::InvalidRequest(
                            "run proposal no longer exists".into(),
                        )
                    })?;
            let snapshot = sqlx::query(
                "SELECT title, body, body_format, acceptance_criteria FROM proposal_revisions \
                 WHERE proposal_id = $1 AND seq = $2 AND event_kind = 'spec_revision' \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(&proposal_id)
            .bind(captured_seq)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                RefinementIntentMutationError::InvalidRequest(
                    "captured proposal snapshot is missing".into(),
                )
            })?;
            let next_seq = head
                .get::<i32, _>("latest_revision_seq")
                .checked_add(1)
                .ok_or_else(|| {
                    RefinementIntentMutationError::InvalidRequest(
                        "proposal revision sequence overflow".into(),
                    )
                })?;
            let title: String = snapshot.get("title");
            let body: String = snapshot.get("body");
            let body_format: String = snapshot.get("body_format");
            let acceptance_criteria: serde_json::Value = snapshot.get("acceptance_criteria");
            sqlx::query(
                "UPDATE proposals SET title = $2, body = $3, body_format = $4, acceptance_criteria = $5, \
                 status = 'draft', latest_revision_seq = $6, closed_at = NULL, \
                 updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
            )
            .bind(&proposal_id)
            .bind(&title)
            .bind(&body)
            .bind(&body_format)
            .bind(&acceptance_criteria)
            .bind(next_seq)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, 'spec_revision', $8)",
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&proposal_id)
            .bind(next_seq)
            .bind(&title)
            .bind(&body)
            .bind(&body_format)
            .bind(&acceptance_criteria)
            .bind(serde_json::json!({"source":"human_review_rejection","snapshot_revision_seq":captured_seq}))
            .execute(&mut *tx)
            .await?;
        }
        let context = serde_json::to_value(&reason)
            .map_err(|error| RefinementIntentMutationError::InvalidRequest(error.to_string()))?
            .get("context")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let terminalized = sqlx::query(
            "UPDATE refinement_runs SET state = 'terminal', park_kind = NULL, parked_at = NULL, \
             terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
             stop_tag = $3, stop_context = $4 WHERE id = $1 AND generation = $2 \
             AND state = 'parked' AND park_kind = 'awaiting_review'",
        )
        .bind(&request.run_id)
        .bind(request.generation)
        .bind(reason.tag())
        .bind(context)
        .execute(&mut *tx)
        .await?;
        if terminalized.rows_affected() != 1 {
            return Err(RefinementIntentMutationError::GenerationConflict {
                run_id: request.run_id,
                generation: request.generation,
            });
        }
        drain_pending_feedback_handoff(&mut tx, &proposal_id, request.generation).await?;
        tx.commit().await?;
        Ok(true)
    }
    async fn source_intent_transition(
        &self,
        source: SourceIntentTransitionRequest,
        park: Option<RefinementParkKind>,
        stop: Option<djinn_core::refinement_liveness::RefinementStopReason>,
    ) -> IntentMutationResult<bool> {
        if source.run_id.trim().is_empty()
            || source.intent_id.trim().is_empty()
            || source.generation <= 0
            || source.expected_round <= 0
        {
            return Err(RefinementIntentMutationError::InvalidRequest(
                "exact run, intent, generation, and round are required".into(),
            ));
        }
        self.db().ensure_initialized().await?;
        let mut tx = self.db().pool().begin().await?;
        let row=sqlx::query("SELECT r.state AS run_state,i.state AS intent_state,i.round,i.phase,i.role FROM refinement_runs r JOIN refinement_dispatch_intents i ON i.run_id=r.id WHERE r.id=$1 AND r.generation=$2 AND i.id=$3 FOR UPDATE").bind(&source.run_id).bind(source.generation).bind(&source.intent_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Err(RefinementIntentMutationError::GenerationConflict {
                run_id: source.run_id,
                generation: source.generation,
            });
        };
        if row.get::<i32, _>("round") != source.expected_round
            || row.get::<String, _>("phase") != phase_name(source.expected_phase)
            || row.get::<String, _>("role") != role_name(source.expected_role)
        {
            return Err(RefinementIntentMutationError::SourceIntentMismatch {
                intent_id: source.intent_id,
            });
        }
        if row.get::<String, _>("run_state") != "running" {
            tx.commit().await?;
            return Ok(false);
        }
        if !matches!(
            row.get::<String, _>("intent_state").as_str(),
            "claimed" | "materialized"
        ) {
            return Err(RefinementIntentMutationError::SourceIntentMismatch {
                intent_id: source.intent_id,
            });
        }
        sqlx::query("UPDATE refinement_dispatch_intents SET state='completed',claimed_by=NULL,claimed_at=NULL,claim_expires_at=NULL,terminal_at=to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$1 AND state IN ('claimed','materialized')").bind(&source.intent_id).execute(&mut *tx).await?;
        let changed = if let Some(kind) = park {
            sqlx::query("UPDATE refinement_runs SET state='parked',park_kind=$3,parked_at=to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$1 AND generation=$2 AND state='running'").bind(&source.run_id).bind(source.generation).bind(match kind {RefinementParkKind::AwaitingReview=>"awaiting_review",RefinementParkKind::AwaitingEvidence=>"awaiting_evidence"}).execute(&mut *tx).await?.rows_affected()==1
        } else {
            let reason = stop.expect("terminal reason");
            let context = serde_json::to_value(&reason)
                .map_err(|e| RefinementIntentMutationError::InvalidRequest(e.to_string()))?
                .get("context")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            sqlx::query("UPDATE refinement_runs SET state='terminal',terminal_at=to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),stop_tag=$3,stop_context=$4 WHERE id=$1 AND generation=$2 AND state='running'").bind(&source.run_id).bind(source.generation).bind(reason.tag()).bind(context).execute(&mut *tx).await?.rows_affected()==1
        };
        if !changed {
            return Err(RefinementIntentMutationError::GenerationConflict {
                run_id: source.run_id,
                generation: source.generation,
            });
        }
        // An awaiting-review park is a demand-admissible boundary: no role work
        // remains in flight, so a feedback cohort committed while this intent
        // was running must resume through the same transaction and same run.
        // Awaiting evidence remains deliberately excluded because its evidence
        // spike owns a separate resume path.
        if park == Some(RefinementParkKind::AwaitingReview) {
            let proposal_id = sqlx::query_scalar::<_, String>(
                "SELECT proposal_id FROM refinement_runs WHERE id=$1",
            )
            .bind(&source.run_id)
            .fetch_one(&mut *tx)
            .await?;
            drain_pending_feedback_handoff_from_awaiting_review_park(
                &mut tx,
                &proposal_id,
                &source.run_id,
                source.generation,
            )
            .await?;
        } else if park.is_none() {
            let proposal_id = sqlx::query_scalar::<_, String>(
                "SELECT proposal_id FROM refinement_runs WHERE id=$1",
            )
            .bind(&source.run_id)
            .fetch_one(&mut *tx)
            .await?;
            drain_pending_feedback_handoff(&mut tx, &proposal_id, source.generation).await?;
        }
        tx.commit().await?;
        Ok(true)
    }
}

impl ProposalRepository {
    /// Read the board-wide phantom projection using durable snapshots and the
    /// shared evaluator. This transaction is read-only by construction.
    pub async fn load_board_refinement_lifecycle_aggregate(
        &self,
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
        let run_ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM refinement_runs WHERE state IN ('running', 'parked') ORDER BY id",
        )
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
        let reaped = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM proposal_revisions WHERE refinement_stop_tag = 'reaped_phantom' AND created_at::timestamptz >= transaction_timestamp() - interval '24 hours'",
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RefinementLifecycleAggregate {
            stale_run_count: stale,
            reaped_phantom_last_24h: reaped,
        })
    }

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
        let mut tx = self.db().pool().begin().await?;
        let proposal_id = sqlx::query_scalar::<_, String>("UPDATE refinement_runs SET state = 'terminal', park_kind = NULL, parked_at = NULL, terminal_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), stop_tag = $3, stop_context = $4, heartbeat_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND generation = $2 AND state IN ('running', 'parked') RETURNING proposal_id").bind(&request.run_id).bind(request.generation).bind(request.reason.tag()).bind(&context).fetch_optional(&mut *tx).await?;
        let Some(proposal_id) = proposal_id else {
            tx.commit().await?;
            return Ok(false);
        };
        let seq = sqlx::query_scalar::<_, i32>(
            "SELECT latest_revision_seq FROM proposals WHERE id = $1 FOR UPDATE",
        )
        .bind(&proposal_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata, refinement_run_id, refinement_stop_tag, refinement_stop_context) VALUES ($1, $2, $3, '', '', 'markdown', '[]', NULL, 'refinement_stop', $4, $5, $6, $7)")
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&proposal_id)
            .bind(seq)
            .bind(serde_json::json!({"reason_tag": request.reason.tag(), "stop_context": context}))
            .bind(&request.run_id)
            .bind(request.reason.tag())
            .bind(&context)
            .execute(&mut *tx)
            .await?;
        drain_pending_feedback_handoff(&mut tx, &proposal_id, request.generation).await?;
        tx.commit().await?;
        Ok(true)
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
    /// Load pending, claimed, and materialized work without changing claims, rows, or heartbeat.
    pub async fn load_pending_or_claimed_refinement_intents(
        &self,
        run_id: &str,
        generation: i32,
    ) -> IntentMutationResult<Vec<RefinementPendingIntent>> {
        self.db().ensure_initialized().await?;
        ensure_generation(self.db().pool(), run_id, generation).await?;
        let rows = sqlx::query("SELECT i.id, i.run_id, r.generation, i.round, i.phase, i.role, i.state, i.claimed_by, i.claim_expires_at FROM refinement_dispatch_intents i JOIN refinement_runs r ON r.id = i.run_id WHERE i.run_id = $1 AND i.state IN ('pending', 'claimed', 'materialized') ORDER BY i.round, i.id").bind(run_id).fetch_all(self.db().pool()).await?;
        rows.into_iter().map(pending_intent_row).collect()
    }

    /// Read dispatchable exact-run work, including materialized enqueue retries, without moving heartbeat.
    pub async fn load_dispatchable_refinement_intents(
        &self,
        run_id: &str,
        generation: i32,
    ) -> IntentMutationResult<Vec<RefinementPendingIntent>> {
        self.load_pending_or_claimed_refinement_intents(run_id, generation)
            .await
    }

    /// CAS claim pending or DB-time-expired work, **or renew a claim this owner
    /// already holds**. Polling does not advance heartbeat.
    ///
    /// The renewal arm is load-bearing, not a convenience. An unexpired claim is
    /// the only liveness evidence an intent has between `claimed` and
    /// `materialized`, and the durable poll is the only thing that can refresh
    /// it. Without a same-owner arm, expiry is a *precondition* of re-claiming:
    /// the holder cannot extend a lease it still holds, so the lease must first
    /// lapse — and for the whole span between that lapse and the next poll the
    /// live run matches no evidence class in `evaluate_refinement_liveness` and
    /// `reap_and_admit` terminalizes it as `reaped_phantom`. That window exists
    /// for ANY lease length, so it cannot be closed by lengthening the lease;
    /// only renewing before expiry closes it.
    ///
    /// Mutual exclusion is unaffected: the new arm matches solely on
    /// `claimed_by = owner`, so a foreign coordinator still cannot take an
    /// unexpired claim. `claimed_at` is preserved across a renewal so it keeps
    /// meaning "when this owner first took the claim".
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
        let row = sqlx::query("UPDATE refinement_dispatch_intents i SET state = 'claimed', claimed_by = $4, claimed_at = CASE WHEN i.state = 'claimed' AND i.claimed_by = $4 AND i.claim_expires_at::timestamptz > transaction_timestamp() THEN i.claimed_at ELSE to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') END, claim_expires_at = to_char((transaction_timestamp() + ($5 * interval '1 millisecond')) AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at = to_char(transaction_timestamp() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM refinement_runs r WHERE i.id = $2 AND i.run_id = $1 AND r.id = i.run_id AND r.generation = $3 AND r.state = 'running' AND (i.state = 'pending' OR (i.state = 'claimed' AND (i.claimed_by = $4 OR i.claim_expires_at::timestamptz <= transaction_timestamp()))) RETURNING i.id, i.run_id, r.generation, i.round, i.phase, i.role, i.claimed_by, i.claim_expires_at").bind(&request.run_id).bind(&request.intent_id).bind(request.generation).bind(&request.owner).bind(request.lease_millis).fetch_optional(self.db().pool()).await?;
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

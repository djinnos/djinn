//! Proposal-owned authority for typed tribunal evidence lifecycle.
//!
//! APIs take a caller-owned transaction so a coordinator can compose them with
//! proposal mutations. Attempts and transitions are append-only database facts.

use djinn_core::models::task::is_evidence_spike;
use djinn_core::models::{
    TribunalEvidenceAnchorMethod, TribunalEvidenceDisposition, TribunalEvidenceFinding,
    TribunalEvidenceLifecycle, TribunalEvidenceOutcome, TribunalEvidencePlannedCheck,
};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::collections::{HashMap, HashSet};

use crate::{Database, Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemandTypedEvidenceInput {
    pub finding_id: String,
    pub proposal_id: String,
    pub demand_hash: String,
    pub claim: serde_json::Value,
    pub demanded_revision_seq: i32,
    pub judge_task_id: String,
}
/// Worker wire payload. Anchor health is intentionally absent: it is derived
/// from immutable database facts rather than trusted from the caller.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TribunalEvidenceReturnV1 {
    pub version: String,
    pub finding_id: String,
    pub spike_task_id: String,
    pub attempt_id: String,
    pub conclusion: String,
    #[serde(default)]
    pub checks: Vec<TribunalEvidenceReturnCheckV1>,
    #[serde(default)]
    pub findings: Vec<TribunalEvidenceReturnFindingV1>,
    #[serde(default)]
    pub failures: Vec<TribunalEvidenceReturnFailureV1>,
    #[serde(default)]
    pub gaps: Vec<TribunalEvidenceReturnGapV1>,
}
/// Minimal identity recovered independently of full V1 decoding so a malformed
/// but attributable return still records the required failed lifecycle fact.
#[derive(serde::Deserialize)]
struct TribunalEvidenceReturnEnvelopeV1 {
    #[serde(default)]
    attempt_id: Option<String>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TribunalEvidenceReturnCheckV1 {
    pub check_id: String,
    pub method: String,
    pub status: String,
    pub detail: Option<String>,
    pub invocation_id: Option<String>,
    #[serde(default)]
    pub anchors: Vec<TribunalEvidenceReturnAnchorV1>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TribunalEvidenceReturnFindingV1 {
    pub check_id: String,
    pub conclusion: String,
    #[serde(default)]
    pub anchors: Vec<TribunalEvidenceReturnAnchorV1>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TribunalEvidenceReturnAnchorV1 {
    pub method: String,
    pub locator: String,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TribunalEvidenceReturnFailureV1 {
    pub check_id: String,
    pub code: String,
    pub detail: String,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct TribunalEvidenceReturnGapV1 {
    pub check_id: String,
    pub code: String,
    pub detail: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TribunalEvidenceReturnResultV1 {
    pub validation_id: String,
    pub outcome: TribunalEvidenceOutcome,
    pub lifecycle: TribunalEvidenceLifecycle,
    pub replayed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocateTypedEvidenceAttemptInput {
    pub attempt_id: String,
    pub finding_id: String,
    pub spike_task_id: String,
    pub evidence_plan_id: Option<String>,
    pub planned_checks: Vec<PlannedTypedEvidenceCheckInput>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedTypedEvidenceCheckInput {
    pub id: String,
    pub ordinal: i32,
    pub check_id: String,
    pub method: TribunalEvidenceAnchorMethod,
    pub evidence_plan_id: Option<String>,
    pub evidence_plan_check_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendTypedEvidenceTransitionInput {
    pub id: String,
    pub finding_id: String,
    pub ordinal: i32,
    pub from_lifecycle: Option<TribunalEvidenceLifecycle>,
    pub to_lifecycle: TribunalEvidenceLifecycle,
    pub actor_task_id: Option<String>,
    pub metadata: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistTypedEvidenceValidationInput {
    pub validation_id: String,
    pub attempt_id: String,
    pub payload_sha256: String,
    pub outcome: TribunalEvidenceOutcome,
    pub validator_facts: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocateTypedEvidenceRetryInput {
    pub finding_id: String,
    pub failed_transition_id: String,
    pub retry_attempt_id: String,
    /// Reserved before dispatch so dispatch recovery never allocates another task.
    pub retry_spike_task_id: String,
    /// Retry attempts retain a distinct immutable plan for return validation.
    pub evidence_plan_id: Option<String>,
    pub planned_checks: Vec<PlannedTypedEvidenceCheckInput>,
    pub demanded_transition_id: String,
    pub actor_task_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchTypedEvidenceRetryInput {
    pub finding_id: String,
    pub attempt_id: String,
    pub spike_task_id: String,
    pub transition_id: String,
    pub actor_task_id: Option<String>,
}
/// Coordinator confirmation that the originally demanded, non-retry attempt
/// was accepted by the slot pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchTypedEvidenceDemandInput {
    pub finding_id: String,
    pub attempt_id: String,
    pub spike_task_id: String,
    pub transition_id: String,
    pub actor_task_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceRetryDispatchErrorInput {
    pub finding_id: String,
    pub attempt_id: String,
    pub spike_task_id: String,
    pub error: String,
}
/// Append-only enqueue failure for the original demanded attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceDemandDispatchErrorInput {
    pub finding_id: String,
    pub attempt_id: String,
    pub spike_task_id: String,
    pub error: String,
}
/// A Judge-owned terminal decision. `folding_revision` must be an existing spec revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisposeTypedEvidenceInput {
    pub disposition_id: String,
    pub transition_id: String,
    pub finding_id: String,
    pub validation_result_id: Option<String>,
    pub folding_revision: i32,
    pub outcome: TribunalEvidenceOutcome,
    pub disposition: TribunalEvidenceLifecycle,
    pub judge_task_id: String,
    pub rationale: String,
    /// A withdrawal needs an explicit, machine-checkable assertion in addition
    /// to its rationale: it may only remove non-load-bearing uncertainty.
    pub withdrawal_is_non_load_bearing: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceFindingProjection {
    pub finding: TribunalEvidenceFinding,
    pub active_attempt_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceAttemptAllocation {
    pub attempt_id: String,
    pub spike_task_id: String,
    pub sequence: i32,
    pub planned_checks: Vec<TribunalEvidencePlannedCheck>,
}
/// Exact allocated attempt awaiting coordinator enqueue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemandedTypedEvidenceDispatch {
    pub finding_id: String,
    pub attempt_id: String,
    pub spike_task_id: String,
    pub is_retry: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceDispositionProjection {
    pub disposition: TribunalEvidenceDisposition,
    pub finding_lifecycle: TribunalEvidenceLifecycle,
}

/// The only projection a mixed-version consumer may use. `None` deliberately
/// conflates absent and invalid typed state: legacy authority remains usable
/// during rollback, while typed-mode callers must fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyEvidenceParityProjection {
    pub finding: TribunalEvidenceFinding,
    pub attempt_id: Option<String>,
    pub spike_task_id: Option<String>,
}

/// Coordinator-facing lifecycle authority. The repository owns the typed read
/// and the mixed-version fence so consumers cannot recreate parity rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypedEvidenceLifecycleProjection {
    Absent,
    Valid(Box<TribunalEvidenceFinding>),
    Invalid,
}

/// Summary of a re-runnable legacy migration. Rows that cannot be represented
/// (notably malformed claims) are skipped rather than guessed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypedEvidenceBackfillReport {
    pub scanned: usize,
    pub created_findings: usize,
    pub created_attempts: usize,
    pub skipped_malformed: usize,
}

const LEGACY_LINK_ONLY_CLAIM: &str = "__typed_evidence_legacy_link_only";

pub struct TypedEvidenceRepository {
    db: Database,
}
impl TypedEvidenceRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Whether this server-authenticated task is the exact active typed
    /// evidence spike for an allocated attempt. Producers use this only to
    /// fence durable delivery; `submit_return_v1` remains the authority for
    /// finding, task, attempt, and payload validation.
    pub async fn has_active_attempt_for_task(&self, spike_task_id: &str) -> Result<bool> {
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM typed_evidence_attempts a JOIN typed_evidence_findings f ON f.id=a.finding_id WHERE a.spike_task_id=$1 AND f.lifecycle='spike_active')",
        )
        .bind(spike_task_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(active)
    }

    pub async fn planned_checks_for_attempt_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        attempt_id: &str,
    ) -> Result<Vec<TribunalEvidencePlannedCheck>> {
        checks(tx, attempt_id).await
    }

    /// Durable coordinator inventory; identity is read from the typed attempt,
    /// never recreated from an open task.
    pub async fn demanded_dispatches(&self) -> Result<Vec<DemandedTypedEvidenceDispatch>> {
        let rows = sqlx::query("SELECT f.id AS finding_id,a.id AS attempt_id,a.spike_task_id, EXISTS(SELECT 1 FROM typed_evidence_retry_idempotency r WHERE r.retry_attempt_id=a.id) AS is_retry FROM typed_evidence_findings f JOIN typed_evidence_attempts a ON a.finding_id=f.id JOIN tasks t ON t.id=a.spike_task_id WHERE f.lifecycle='demanded' AND t.status <> 'closed' ORDER BY f.created_at,a.sequence").fetch_all(self.db.pool()).await?;
        Ok(rows
            .into_iter()
            .map(|row| DemandedTypedEvidenceDispatch {
                finding_id: row.get("finding_id"),
                attempt_id: row.get("attempt_id"),
                spike_task_id: row.get("spike_task_id"),
                is_retry: row.get("is_retry"),
            })
            .collect())
    }

    pub async fn copy_planned_checks_for_latest_attempt_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        finding_id: &str,
    ) -> Result<Vec<PlannedTypedEvidenceCheckInput>> {
        let rows = sqlx::query("SELECT check_id,method,evidence_plan_id,evidence_plan_check_id,ordinal FROM typed_evidence_planned_checks WHERE attempt_id=(SELECT id FROM typed_evidence_attempts WHERE finding_id=$1 ORDER BY sequence DESC LIMIT 1) ORDER BY ordinal").bind(finding_id).fetch_all(&mut **tx).await?;
        Ok(rows
            .into_iter()
            .map(|row| PlannedTypedEvidenceCheckInput {
                id: uuid::Uuid::now_v7().to_string(),
                ordinal: row.get("ordinal"),
                check_id: row.get("check_id"),
                method: match row.get::<String, _>("method").as_str() {
                    "code" => TribunalEvidenceAnchorMethod::Code,
                    "graph" => TribunalEvidenceAnchorMethod::Graph,
                    _ => TribunalEvidenceAnchorMethod::Command,
                },
                evidence_plan_id: row.get("evidence_plan_id"),
                evidence_plan_check_id: row.get("evidence_plan_check_id"),
            })
            .collect())
    }

    /// Backfill only actively parked legacy rows. It is re-runnable and never
    /// changes legacy authority, which keeps rollback recovery intact.
    pub async fn backfill_active_legacy_evidence(&self) -> Result<TypedEvidenceBackfillReport> {
        let mut tx = self.db.pool().begin().await?;
        let rows = sqlx::query("SELECT id,linked_spike_task_id,needs_evidence_claim,latest_revision_seq FROM proposals WHERE NULLIF(btrim(linked_spike_task_id),'') IS NOT NULL OR NULLIF(btrim(needs_evidence_claim),'') IS NOT NULL FOR UPDATE")
            .fetch_all(&mut *tx).await?;
        let mut report = TypedEvidenceBackfillReport {
            scanned: rows.len(),
            ..Default::default()
        };
        for row in rows {
            let proposal_id: String = row.get("id");
            let link: Option<String> = row.get("linked_spike_task_id");
            // A linked row is active authority only while the task remains
            // active. Leave terminal or missing-task legacy state untouched so
            // rollback readers can recover it without invented typed history.
            if let Some(task_id) = link.as_deref() {
                let task_is_active: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=$1 AND status <> 'closed')",
                )
                .bind(task_id)
                .fetch_one(&mut *tx)
                .await?;
                if !task_is_active {
                    report.skipped_malformed += 1;
                    continue;
                }
            }
            let Some(claim) = legacy_claim(
                row.get::<Option<String>, _>("needs_evidence_claim")
                    .as_deref(),
            ) else {
                report.skipped_malformed += 1;
                continue;
            };
            let judge = claim
                .get("created_by_task_id")
                .and_then(serde_json::Value::as_str)
                .or(link.as_deref());
            let Some(judge) = judge else {
                report.skipped_malformed += 1;
                continue;
            };
            let revision = claim
                .get("against_revision_seq")
                .and_then(serde_json::Value::as_i64)
                .map(|v| v as i32)
                .filter(|v| *v > 0)
                .unwrap_or(row.get("latest_revision_seq"));
            let hash = legacy_demand_hash(&claim, link.as_deref());
            let existing = sqlx::query(
                "SELECT id,claim,lifecycle FROM typed_evidence_findings WHERE proposal_id=$1 AND demand_hash=$2 FOR UPDATE",
            )
            .bind(&proposal_id)
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?;
            let finding_id = if let Some(existing) = existing {
                let id: String = existing.get("id");
                let lifecycle = parse(&existing.get::<String, _>("lifecycle"))?;
                let attempts = sqlx::query(
                    "SELECT spike_task_id FROM typed_evidence_attempts WHERE finding_id=$1 ORDER BY sequence",
                )
                .bind(&id)
                .fetch_all(&mut *tx)
                .await?;
                let parity_matches = existing.get::<serde_json::Value, _>("claim") == claim
                    && match link.as_deref() {
                        None => {
                            lifecycle == TribunalEvidenceLifecycle::Demanded && attempts.is_empty()
                        }
                        Some(task) => {
                            (lifecycle == TribunalEvidenceLifecycle::Demanded
                                && attempts.is_empty())
                                || (lifecycle == TribunalEvidenceLifecycle::SpikeActive
                                    && attempts.len() == 1
                                    && attempts[0].get::<String, _>("spike_task_id") == task)
                        }
                    };
                if !parity_matches {
                    report.skipped_malformed += 1;
                    continue;
                }
                id
            } else {
                if Self::has_unresolved_in_transaction(&mut tx, &proposal_id).await? {
                    report.skipped_malformed += 1;
                    continue;
                }
                let id = uuid::Uuid::now_v7().to_string();
                sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'demanded',$4,$5,$6)").bind(&id).bind(&proposal_id).bind(&hash).bind(&claim).bind(revision).bind(judge).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,1,NULL,'demanded',$3,$4)").bind(uuid::Uuid::now_v7().to_string()).bind(&id).bind(judge).bind(serde_json::json!({"source":"legacy_backfill"})).execute(&mut *tx).await?;
                report.created_findings += 1;
                id
            };
            if let Some(task) = link {
                let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM typed_evidence_attempts WHERE finding_id=$1 AND spike_task_id=$2)").bind(&finding_id).bind(&task).fetch_one(&mut *tx).await?;
                if !exists {
                    let seq: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(sequence),0)+1 FROM typed_evidence_attempts WHERE finding_id=$1").bind(&finding_id).fetch_one(&mut *tx).await?;
                    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,$3,$4)").bind(uuid::Uuid::now_v7().to_string()).bind(&finding_id).bind(seq).bind(&task).execute(&mut *tx).await?;
                    report.created_attempts += 1;
                }
                if lock_state(&mut tx, &finding_id).await? == TribunalEvidenceLifecycle::Demanded {
                    Self::append_transition(
                        &mut tx,
                        AppendTypedEvidenceTransitionInput {
                            id: uuid::Uuid::now_v7().to_string(),
                            finding_id: finding_id.clone(),
                            ordinal: 2,
                            from_lifecycle: Some(TribunalEvidenceLifecycle::Demanded),
                            to_lifecycle: TribunalEvidenceLifecycle::SpikeActive,
                            actor_task_id: Some(task),
                            metadata: serde_json::json!({"source":"legacy_backfill"}),
                        },
                    )
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(report)
    }

    /// Typed-mode read. Any absent, malformed, ambiguous, or mismatched state
    /// yields `None` and does not repair either authority representation.
    pub async fn dual_read_legacy_parity(
        &self,
        proposal_id: &str,
    ) -> Result<Option<LegacyEvidenceParityProjection>> {
        let mut tx = self.db.pool().begin().await?;
        let result = Self::dual_read_legacy_parity_in_transaction(&mut tx, proposal_id).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Project the single live finding for coordinator dispatch. Ambiguous or
    /// legacy-mismatched authority is invalid rather than inferred from tasks.
    pub async fn coordinator_lifecycle_projection(
        &self,
        proposal_id: &str,
    ) -> Result<TypedEvidenceLifecycleProjection> {
        let rows = sqlx::query("SELECT id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id,created_at,updated_at FROM typed_evidence_findings WHERE proposal_id=$1 AND lifecycle IN ('demanded','spike_active','evidence_received','failed')")
            .bind(proposal_id).fetch_all(self.db.pool()).await?;
        if rows.is_empty() {
            return Ok(TypedEvidenceLifecycleProjection::Absent);
        }
        if rows.len() != 1 {
            return Ok(TypedEvidenceLifecycleProjection::Invalid);
        }
        let finding = finding(&rows[0])?;
        match finding.lifecycle {
            TribunalEvidenceLifecycle::Demanded | TribunalEvidenceLifecycle::SpikeActive => {
                match self.dual_read_legacy_parity(proposal_id).await? {
                    Some(parity) if parity.finding.id == finding.id => {
                        Ok(TypedEvidenceLifecycleProjection::Valid(Box::new(finding)))
                    }
                    _ => Ok(TypedEvidenceLifecycleProjection::Invalid),
                }
            }
            TribunalEvidenceLifecycle::EvidenceReceived | TribunalEvidenceLifecycle::Failed => {
                let legacy = sqlx::query(
                    "SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1",
                )
                .bind(proposal_id)
                .fetch_optional(self.db.pool())
                .await?;
                match legacy {
                    Some(row)
                        if row
                            .get::<Option<String>, _>("linked_spike_task_id")
                            .is_none()
                            && row
                                .get::<Option<String>, _>("needs_evidence_claim")
                                .is_none() =>
                    {
                        Ok(TypedEvidenceLifecycleProjection::Valid(Box::new(finding)))
                    }
                    _ => Ok(TypedEvidenceLifecycleProjection::Invalid),
                }
            }
            TribunalEvidenceLifecycle::Resolved | TribunalEvidenceLifecycle::Withdrawn => {
                Ok(TypedEvidenceLifecycleProjection::Absent)
            }
        }
    }

    pub async fn dual_read_legacy_parity_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        proposal_id: &str,
    ) -> Result<Option<LegacyEvidenceParityProjection>> {
        let Some(legacy) = sqlx::query(
            "SELECT linked_spike_task_id,needs_evidence_claim FROM proposals WHERE id=$1 FOR SHARE",
        )
        .bind(proposal_id)
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Ok(None);
        };
        let link: Option<String> = legacy.get("linked_spike_task_id");
        let Some(claim) = legacy_claim(
            legacy
                .get::<Option<String>, _>("needs_evidence_claim")
                .as_deref(),
        ) else {
            return Ok(None);
        };
        let rows = sqlx::query("SELECT id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id,created_at,updated_at FROM typed_evidence_findings WHERE proposal_id=$1 AND lifecycle IN ('demanded','spike_active','evidence_received','failed')").bind(proposal_id).fetch_all(&mut **tx).await?;
        if rows.len() != 1 || rows[0].get::<serde_json::Value, _>("claim") != claim {
            return Ok(None);
        }
        let finding = finding(&rows[0])?;
        if finding.demand_hash != legacy_demand_hash(&claim, link.as_deref())
            && finding.demand_hash != normalized_demand_hash(&claim)
        {
            return Ok(None);
        }
        let attempts = sqlx::query("SELECT id,spike_task_id FROM typed_evidence_attempts WHERE finding_id=$1 ORDER BY sequence DESC").bind(&finding.id).fetch_all(&mut **tx).await?;
        let linked_task_is_active = match link.as_deref() {
            Some(task) => {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=$1 AND status <> 'closed')",
                )
                .bind(task)
                .fetch_one(&mut **tx)
                .await?
            }
            None => false,
        };
        match (link, finding.lifecycle, attempts.as_slice()) {
            (None, TribunalEvidenceLifecycle::Demanded, []) => {
                Ok(Some(LegacyEvidenceParityProjection {
                    finding,
                    attempt_id: None,
                    spike_task_id: None,
                }))
            }
            (Some(task), TribunalEvidenceLifecycle::SpikeActive, [attempt])
                if attempt.get::<String, _>("spike_task_id") == task && linked_task_is_active =>
            {
                Ok(Some(LegacyEvidenceParityProjection {
                    finding,
                    attempt_id: Some(attempt.get("id")),
                    spike_task_id: Some(task),
                }))
            }
            _ => Ok(None),
        }
    }

    /// Dual-write the initial typed demand and rollback-compatible legacy
    /// authority under one caller-owned transaction. Callers that allocate a
    /// spike append `demanded -> spike_active` in that same transaction.
    pub async fn demand_and_set_legacy_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: DemandTypedEvidenceInput,
        linked_spike_task_id: Option<&str>,
    ) -> Result<TypedEvidenceFindingProjection> {
        let proposal_id = input.proposal_id.clone();
        if input.demand_hash != legacy_demand_hash(&input.claim, linked_spike_task_id)
            && input.demand_hash != normalized_demand_hash(&input.claim)
        {
            return Err(Error::InvalidData(
                "legacy dual-write demand hash mismatch".into(),
            ));
        }
        let claim = serde_json::to_string(&input.claim).map_err(|error| {
            Error::InvalidData(format!("legacy claim serialization failed: {error}"))
        })?;
        let projection = Self::demand_in_transaction(tx, input).await?;
        let updated = sqlx::query(
            "UPDATE proposals SET linked_spike_task_id=$1,needs_evidence_claim=$2 WHERE id=$3",
        )
        .bind(linked_spike_task_id)
        .bind(claim)
        .bind(&proposal_id)
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(Error::InvalidData("proposal not found".into()));
        }
        Ok(projection)
    }

    /// Atomically materialize an active legacy spike as a typed demand, its
    /// current attempt, and `demanded -> spike_active`. This prevents new
    /// mixed-version links from existing without typed authority.
    pub async fn demand_activate_and_set_legacy_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: DemandTypedEvidenceInput,
        spike_task_id: &str,
    ) -> Result<TypedEvidenceFindingProjection> {
        nonempty(&[spike_task_id])?;
        let task_is_active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=$1 AND status <> 'closed')",
        )
        .bind(spike_task_id)
        .fetch_one(&mut **tx)
        .await?;
        if !task_is_active {
            return Err(Error::InvalidTransition(
                "legacy_typed_task_not_active".into(),
            ));
        }
        let projection =
            Self::demand_and_set_legacy_in_transaction(tx, input, Some(spike_task_id)).await?;
        let finding_id = projection.finding.id.clone();
        let attempts = sqlx::query(
            "SELECT id FROM typed_evidence_attempts WHERE finding_id=$1 AND spike_task_id=$2",
        )
        .bind(&finding_id)
        .bind(spike_task_id)
        .fetch_all(&mut **tx)
        .await?;
        if attempts.len() > 1 {
            return Err(Error::InvalidTransition(
                "legacy_typed_parity_mismatch".into(),
            ));
        }
        if attempts.is_empty() {
            let sequence: i32 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM typed_evidence_attempts WHERE finding_id=$1",
            )
            .bind(&finding_id)
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,$3,$4)",
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&finding_id)
            .bind(sequence)
            .bind(spike_task_id)
            .execute(&mut **tx)
            .await?;
        }
        let lifecycle = lock_state(tx, &finding_id).await?;
        if lifecycle == TribunalEvidenceLifecycle::Demanded
            || lifecycle == TribunalEvidenceLifecycle::Failed
        {
            let ordinal: i32 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
            )
            .bind(&finding_id)
            .fetch_one(&mut **tx)
            .await?;
            Self::append_transition(
                tx,
                AppendTypedEvidenceTransitionInput {
                    id: uuid::Uuid::now_v7().to_string(),
                    finding_id: finding_id.clone(),
                    ordinal,
                    from_lifecycle: Some(lifecycle),
                    to_lifecycle: TribunalEvidenceLifecycle::SpikeActive,
                    actor_task_id: Some(spike_task_id.to_owned()),
                    metadata: serde_json::json!({"source":"legacy_dual_write_demand"}),
                },
            )
            .await?;
        }
        Ok(projection)
    }

    /// Clearing legacy authority is inseparable from the corresponding typed
    /// non-terminal transition in this caller-owned transaction.
    pub async fn transition_and_clear_legacy_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        proposal_id: &str,
        expected_spike_task_id: &str,
        transition: AppendTypedEvidenceTransitionInput,
    ) -> Result<()> {
        let Some(parity) = Self::dual_read_legacy_parity_in_transaction(tx, proposal_id).await?
        else {
            return Err(Error::InvalidTransition(
                "legacy_typed_parity_mismatch".into(),
            ));
        };
        if parity.finding.id != transition.finding_id
            || parity.spike_task_id.as_deref() != Some(expected_spike_task_id)
        {
            return Err(Error::InvalidTransition(
                "legacy_typed_parity_mismatch".into(),
            ));
        }
        Self::append_transition_in_transaction(tx, transition).await?;
        let updated = sqlx::query("UPDATE proposals SET linked_spike_task_id=NULL,needs_evidence_claim=NULL WHERE id=$1 AND linked_spike_task_id=$2").bind(proposal_id).bind(expected_spike_task_id).execute(&mut **tx).await?;
        if updated.rows_affected() != 1 {
            return Err(Error::InvalidTransition(
                "legacy_typed_parity_mismatch".into(),
            ));
        }
        Ok(())
    }

    /// Commit the `spike_active -> evidence_received` fact and legacy clearing
    /// together. Unlike the live projection, this path requires the linked
    /// task to be terminal; any missing or ambiguous authority rolls back.
    pub async fn evidence_received_and_clear_legacy(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        let result = async {
            let legacy = sqlx::query("SELECT needs_evidence_claim FROM proposals WHERE id=$1 AND linked_spike_task_id=$2 FOR UPDATE")
                .bind(proposal_id).bind(spike_task_id).fetch_optional(&mut *tx).await?
                .ok_or_else(|| Error::InvalidTransition("legacy_typed_parity_mismatch".into()))?;
            let claim = legacy_claim(legacy.get::<Option<String>, _>("needs_evidence_claim").as_deref())
                .ok_or_else(|| Error::InvalidTransition("legacy_typed_parity_mismatch".into()))?;
            let terminal: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=$1 AND status='closed')")
                .bind(spike_task_id).fetch_one(&mut *tx).await?;
            if !terminal {
                return Err(Error::InvalidTransition("legacy_typed_task_not_terminal".into()));
            }
            let findings = sqlx::query("SELECT id,lifecycle FROM typed_evidence_findings WHERE proposal_id=$1 AND demand_hash=$2 FOR UPDATE")
                .bind(proposal_id).bind(legacy_demand_hash(&claim, Some(spike_task_id)))
                .fetch_all(&mut *tx).await?;
            if findings.len() != 1 || findings[0].get::<String, _>("lifecycle") != "spike_active" {
                return Err(Error::InvalidTransition("legacy_typed_parity_mismatch".into()));
            }
            let finding_id: String = findings[0].get("id");
            // Inspect the complete immutable attempt history, rather than only
            // the expected task. Filtering here would hide a second attempt
            // for another task and allow ambiguous authority to clear the
            // rollback-compatible legacy fields.
            let attempts = sqlx::query(
                "SELECT id,spike_task_id FROM typed_evidence_attempts WHERE finding_id=$1 ORDER BY sequence",
            )
            .bind(&finding_id)
            .fetch_all(&mut *tx)
            .await?;
            if attempts.len() != 1
                || attempts[0].get::<String, _>("spike_task_id") != spike_task_id
            {
                return Err(Error::InvalidTransition("legacy_typed_parity_mismatch".into()));
            }
            let ordinal: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1")
                .bind(&finding_id).fetch_one(&mut *tx).await?;
            Self::append_transition(&mut tx, AppendTypedEvidenceTransitionInput {
                id: uuid::Uuid::now_v7().to_string(), finding_id, ordinal,
                from_lifecycle: Some(TribunalEvidenceLifecycle::SpikeActive),
                to_lifecycle: TribunalEvidenceLifecycle::EvidenceReceived,
                actor_task_id: Some(spike_task_id.to_owned()),
                metadata: serde_json::json!({"source":"legacy_dual_write_clear", "attempt_id": attempts[0].get::<String, _>("id")}),
            }).await?;
            let update = sqlx::query("UPDATE proposals SET linked_spike_task_id=NULL, needs_evidence_claim=NULL WHERE id=$1 AND linked_spike_task_id=$2")
                .bind(proposal_id).bind(spike_task_id).execute(&mut *tx).await?;
            if update.rows_affected() != 1 {
                return Err(Error::InvalidTransition("legacy_typed_parity_mismatch".into()));
            }
            Ok(())
        }.await;
        match result {
            Ok(()) => tx.commit().await?,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Database-owned entry point: a rejected normalized payload is rolled
    /// back before its append-only failure fact is committed separately.
    pub async fn submit_return_v1(
        &self,
        payload_bytes: &[u8],
    ) -> Result<TribunalEvidenceReturnResultV1> {
        // Required fields may be missing in a malformed body while its attempt
        // remains authoritative and must receive one failure transition.
        let rejection_attempt_id =
            serde_json::from_slice::<TribunalEvidenceReturnEnvelopeV1>(payload_bytes)
                .ok()
                .and_then(|envelope| envelope.attempt_id)
                .filter(|attempt_id| !attempt_id.trim().is_empty());
        let mut tx = self.db.pool().begin().await?;
        match Self::submit_return_v1_in_transaction(&mut tx, payload_bytes).await {
            Ok(result) => {
                tx.commit().await?;
                Ok(result)
            }
            Err(error) => {
                tx.rollback().await?;
                if let (Some(attempt_id), Error::InvalidData(code)) =
                    (&rejection_attempt_id, &error)
                {
                    self.record_rejected_return(attempt_id, code).await?;
                }
                Err(error)
            }
        }
    }

    async fn record_rejected_return(&self, attempt_id: &str, code: &str) -> Result<()> {
        // The attempt is the only caller-supplied identity required to locate
        // the authoritative finding/task pair. An incorrect claimed binding
        // must not suppress the required failed lifecycle fact.
        if attempt_id.trim().is_empty() {
            return Ok(());
        }
        let mut tx = self.db.pool().begin().await?;
        let attempt = sqlx::query(
            "SELECT finding_id,spike_task_id FROM typed_evidence_attempts WHERE id=$1 FOR UPDATE",
        )
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(attempt) = attempt else {
            tx.commit().await?;
            return Ok(());
        };
        let already_terminal: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM typed_evidence_validation_results WHERE attempt_id=$1)",
        )
        .bind(attempt_id)
        .fetch_one(&mut *tx)
        .await?;
        if already_terminal {
            tx.commit().await?;
            return Ok(());
        }
        let finding_id: String = attempt.get("finding_id");
        let spike_task_id: String = attempt.get("spike_task_id");
        require_active_return_attempt(&mut tx, attempt_id, &finding_id, &spike_task_id).await?;
        let ordinal: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&finding_id)
        .fetch_one(&mut *tx)
        .await?;
        Self::append_transition(&mut tx, AppendTypedEvidenceTransitionInput {
            id: uuid::Uuid::now_v7().to_string(),
            finding_id,
            ordinal,
            from_lifecycle: Some(TribunalEvidenceLifecycle::SpikeActive),
            to_lifecycle: TribunalEvidenceLifecycle::Failed,
            actor_task_id: Some(spike_task_id),
            metadata: serde_json::json!({"validator_version":"TribunalEvidenceReturnV1", "validation_error":code}),
        }).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Validate a return against the frozen attempt and atomically persist the
    /// normalized result. The unique attempt result is the replay fence.
    pub async fn submit_return_v1_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        payload_bytes: &[u8],
    ) -> Result<TribunalEvidenceReturnResultV1> {
        if payload_bytes.len() > 256 * 1024 {
            return Err(v1("payload_too_large"));
        }
        let payload: TribunalEvidenceReturnV1 =
            serde_json::from_slice(payload_bytes).map_err(|_| v1("invalid_json"))?;
        let hash = format!("{:x}", Sha256::digest(payload_bytes));
        validate_return_shape(&payload)?;
        let attempt = sqlx::query(
            "SELECT finding_id,spike_task_id FROM typed_evidence_attempts WHERE id=$1 FOR UPDATE",
        )
        .bind(&payload.attempt_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| v1("unknown_attempt"))?;
        if attempt.get::<String, _>("finding_id") != payload.finding_id {
            return Err(v1("finding_binding_mismatch"));
        }
        if attempt.get::<String, _>("spike_task_id") != payload.spike_task_id {
            return Err(v1("source_task_binding_mismatch"));
        }
        if let Some(row)=sqlx::query("SELECT id,payload_sha256,outcome FROM typed_evidence_validation_results WHERE attempt_id=$1").bind(&payload.attempt_id).fetch_optional(&mut **tx).await? {
            if row.get::<String,_>("payload_sha256") != hash { return Err(v1("replay_payload_conflict")); }
            return Ok(TribunalEvidenceReturnResultV1 { validation_id:row.get("id"), outcome:parse_outcome(&row.get::<String,_>("outcome"))?, lifecycle:lock_state(tx,&payload.finding_id).await?, replayed:true });
        }
        require_active_return_attempt(
            tx,
            &payload.attempt_id,
            &payload.finding_id,
            &payload.spike_task_id,
        )
        .await?;
        let planned = checks(tx, &payload.attempt_id).await?;
        let expected: HashMap<_, _> = planned.iter().map(|c| (c.check_id.as_str(), c)).collect();
        if payload.checks.len() != expected.len() {
            return Err(v1("missing_expected_check"));
        }
        let mut seen = HashSet::new();
        for c in &payload.checks {
            limit_check(c)?;
            let Some(p) = expected.get(c.check_id.as_str()) else {
                return Err(v1("unknown_check"));
            };
            if !seen.insert(c.check_id.as_str()) {
                return Err(v1("duplicate_check"));
            }
            if c.method != planned_method(p.method) {
                return Err(v1("check_method_mismatch"));
            }
        }
        let mut finding_checks = HashSet::new();
        for f in &payload.findings {
            if !expected.contains_key(f.check_id.as_str()) {
                return Err(v1("dangling_finding_check"));
            }
            if !finding_checks.insert(f.check_id.as_str()) {
                return Err(v1("duplicate_finding_check"));
            }
            if f.conclusion.trim().is_empty() {
                return Err(v1("finding_conclusion_required"));
            }
            if bytes(&f.conclusion) > 8192 {
                return Err(v1("conclusion_too_large"));
            }
            if bytes(&f.conclusion) > 2048 && bytes(&f.conclusion) > 8192 {
                return Err(v1("string_too_large"));
            }
            if f.anchors.len() > 16 {
                return Err(v1("too_many_anchors"));
            }
            for a in &f.anchors {
                limit_anchor(a)?;
            }
        }
        let mut failures = HashSet::new();
        for i in &payload.failures {
            limit_failure(i, &expected)?;
            if !failures.insert(i.check_id.as_str()) {
                return Err(v1("duplicate_issue_check"));
            }
        }
        let mut gaps = HashSet::new();
        for i in &payload.gaps {
            limit_gap(i, &expected)?;
            if !gaps.insert(i.check_id.as_str()) {
                return Err(v1("duplicate_issue_check"));
            }
        }
        for c in &payload.checks {
            let has_failure = failures.contains(c.check_id.as_str());
            let has_gap = gaps.contains(c.check_id.as_str());
            match c.status.as_str() {
                "passed" if has_failure || has_gap || c.detail.is_some() => {
                    return Err(v1("passed_status_fields_forbidden"));
                }
                "failed" if !has_failure || has_gap => {
                    return Err(v1("failed_status_failure_required"));
                }
                "not_run" if !has_gap || has_failure => {
                    return Err(v1("not_run_status_gap_required"));
                }
                _ => {}
            }
        }
        // This parent must precede its normalized children: the FK is immediate.
        let validation_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO typed_evidence_validation_results (id,attempt_id,payload_sha256,outcome,validator_facts) VALUES ($1,$2,$3,'unresolved',$4)")
            .bind(&validation_id).bind(&payload.attempt_id).bind(&hash)
            .bind(serde_json::json!({"validator_version":"TribunalEvidenceReturnV1","raw_payload_sha256":hash,"server_hydrated":true}))
            .execute(&mut **tx).await?;
        let mut check_healthy = HashMap::new();
        for c in &payload.checks {
            let p = expected[c.check_id.as_str()];
            let result_id = uuid::Uuid::now_v7().to_string();
            sqlx::query("INSERT INTO typed_evidence_check_results (id,validation_result_id,planned_check_id,status,detail) VALUES ($1,$2,$3,$4,$5)").bind(&result_id).bind(&validation_id).bind(&p.id).bind(&c.status).bind(&c.detail).execute(&mut **tx).await?;
            let mut positive = false;
            if let Some(invocation_id) = &c.invocation_id {
                let hydrated =
                    hydrate_command_invocation(tx, &payload.attempt_id, p, invocation_id).await?;
                sqlx::query("INSERT INTO typed_evidence_invocation_provenance (validation_result_id,check_result_id,invocation_id,usable) VALUES ($1,$2,$3,$4)")
                    .bind(&validation_id).bind(&result_id).bind(invocation_id).bind(hydrated.healthy && hydrated.method_compatible)
                    .execute(&mut **tx).await?;
                positive |= hydrated.healthy && hydrated.method_compatible;
            }
            for a in &c.anchors {
                let id = uuid::Uuid::now_v7().to_string();
                let hydrated = hydrate_anchor(tx, &payload.attempt_id, p, a).await?;
                positive |= hydrated.healthy && hydrated.method_compatible;
                sqlx::query("INSERT INTO typed_evidence_anchors (id,check_result_id,method,locator) VALUES ($1,$2,$3,$4)").bind(&id).bind(&result_id).bind(&a.method).bind(&a.locator).execute(&mut **tx).await?;
                sqlx::query("INSERT INTO typed_evidence_anchor_health (anchor_id,health,detail,immutable_identity,method_compatible) VALUES ($1,$2,$3,$4,$5)")
                    .bind(&id).bind(hydrated.health()).bind(&hydrated.detail).bind(&hydrated.identity).bind(hydrated.method_compatible).execute(&mut **tx).await?;
            }
            check_healthy.insert(c.check_id.as_str(), c.status == "passed" && positive);
        }
        for f in &payload.findings {
            let p = expected[f.check_id.as_str()];
            let finding_id = uuid::Uuid::now_v7().to_string();
            // The anchor FK is immediate, but this outer transaction remains
            // atomic: create the normalized parent before its hydrated rows.
            sqlx::query("INSERT INTO typed_evidence_return_findings (id,validation_result_id,planned_check_id,conclusion,usable) VALUES ($1,$2,$3,$4,FALSE)")
                .bind(&finding_id).bind(&validation_id).bind(&p.id).bind(&f.conclusion).execute(&mut **tx).await?;
            let mut positive = false;
            for a in &f.anchors {
                let hydrated = hydrate_anchor(tx, &payload.attempt_id, p, a).await?;
                positive |= hydrated.healthy && hydrated.method_compatible;
                sqlx::query("INSERT INTO typed_evidence_return_finding_anchors (id,finding_id,method,locator,health,immutable_identity,detail,method_compatible) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
                    .bind(uuid::Uuid::now_v7().to_string()).bind(&finding_id).bind(&a.method).bind(&a.locator).bind(hydrated.health()).bind(&hydrated.identity).bind(&hydrated.detail).bind(hydrated.method_compatible).execute(&mut **tx).await?;
            }
            sqlx::query("UPDATE typed_evidence_return_findings SET usable=$1 WHERE id=$2")
                .bind(positive)
                .bind(&finding_id)
                .execute(&mut **tx)
                .await?;
        }
        let result_outcome = derive_outcome(&payload.checks, &check_healthy);
        sqlx::query("UPDATE typed_evidence_validation_results SET outcome=$1,validator_facts=$2 WHERE id=$3").bind(outcome(result_outcome)).bind(serde_json::json!({"validator_version":"TribunalEvidenceReturnV1","raw_payload_sha256":hash,"server_hydrated":true,"outcome":outcome(result_outcome)})).bind(&validation_id).execute(&mut **tx).await?;
        for i in &payload.failures {
            let p = expected[i.check_id.as_str()];
            sqlx::query("INSERT INTO typed_evidence_issues (id,validation_result_id,planned_check_id,kind,code,detail) VALUES ($1,$2,$3,'failure',$4,$5)").bind(uuid::Uuid::now_v7().to_string()).bind(&validation_id).bind(&p.id).bind(&i.code).bind(&i.detail).execute(&mut **tx).await?;
        }
        for i in &payload.gaps {
            let p = expected[i.check_id.as_str()];
            sqlx::query("INSERT INTO typed_evidence_issues (id,validation_result_id,planned_check_id,kind,code,detail) VALUES ($1,$2,$3,'gap',$4,$5)").bind(uuid::Uuid::now_v7().to_string()).bind(&validation_id).bind(&p.id).bind(&i.code).bind(&i.detail).execute(&mut **tx).await?;
        }
        let state = lock_state(tx, &payload.finding_id).await?;
        let ordinal: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&payload.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        Self::append_transition(tx,AppendTypedEvidenceTransitionInput{id:uuid::Uuid::now_v7().to_string(),finding_id:payload.finding_id,ordinal,from_lifecycle:Some(state),to_lifecycle:TribunalEvidenceLifecycle::EvidenceReceived,actor_task_id:Some(payload.spike_task_id),metadata:serde_json::json!({"validation_result_id":validation_id,"outcome":outcome(result_outcome)})}).await?;
        Ok(TribunalEvidenceReturnResultV1 {
            validation_id,
            outcome: result_outcome,
            lifecycle: TribunalEvidenceLifecycle::EvidenceReceived,
            replayed: false,
        })
    }

    /// Same normalized demand is idempotent; a different unresolved demand
    /// returns `active_evidence_conflict` without a write.
    pub async fn demand_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: DemandTypedEvidenceInput,
    ) -> Result<TypedEvidenceFindingProjection> {
        nonempty(&[
            &input.finding_id,
            &input.proposal_id,
            &input.demand_hash,
            &input.judge_task_id,
        ])?;
        if !input.claim.is_object() || input.demanded_revision_seq <= 0 {
            return Err(Error::InvalidData(
                "typed evidence demand requires object claim and positive revision".into(),
            ));
        }
        sqlx::query("SELECT id FROM proposals WHERE id = $1 FOR UPDATE")
            .bind(&input.proposal_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| Error::InvalidData("proposal not found".into()))?;
        if let Some(row) = sqlx::query("SELECT id, proposal_id, demand_hash, lifecycle, claim, demanded_revision_seq, created_by_task_id, created_at, updated_at FROM typed_evidence_findings WHERE proposal_id=$1 AND demand_hash=$2").bind(&input.proposal_id).bind(&input.demand_hash).fetch_optional(&mut **tx).await? {
            let id: String = row.get("id");
            return Ok(TypedEvidenceFindingProjection { finding: finding(&row)?, active_attempt_id: active_attempt(tx, &id).await? });
        }
        if Self::has_unresolved_in_transaction(tx, &input.proposal_id).await? {
            return Err(Error::InvalidTransition("active_evidence_conflict".into()));
        }
        let row = sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'demanded',$4,$5,$6) RETURNING id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id,created_at,updated_at").bind(&input.finding_id).bind(&input.proposal_id).bind(&input.demand_hash).bind(&input.claim).bind(input.demanded_revision_seq).bind(&input.judge_task_id).fetch_one(&mut **tx).await?;
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,1,NULL,'demanded',$3,'{}'::jsonb)").bind(uuid::Uuid::now_v7().to_string()).bind(&input.finding_id).bind(&input.judge_task_id).execute(&mut **tx).await?;
        Ok(TypedEvidenceFindingProjection {
            finding: finding(&row)?,
            active_attempt_id: None,
        })
    }

    /// Returns the proposal-wide unresolved projection. `demanded_revision_seq`
    /// is provenance only, so a finding remains unresolved across later
    /// proposal revisions until a terminal disposition is recorded.
    pub async fn has_unresolved_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        proposal_id: &str,
    ) -> Result<bool> {
        nonempty(&[proposal_id])?;
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM typed_evidence_findings WHERE proposal_id=$1 AND lifecycle IN ('demanded','spike_active','evidence_received','failed'))",
        )
        .bind(proposal_id)
        .fetch_one(&mut **tx)
        .await?)
    }

    pub async fn allocate_attempt_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: AllocateTypedEvidenceAttemptInput,
    ) -> Result<TypedEvidenceAttemptAllocation> {
        nonempty(&[&input.attempt_id, &input.finding_id, &input.spike_task_id])?;
        let state = lock_state(tx, &input.finding_id).await?;
        if !matches!(
            state,
            TribunalEvidenceLifecycle::Demanded | TribunalEvidenceLifecycle::Failed
        ) {
            return Err(Error::InvalidTransition(
                "attempt allocation requires demanded or failed finding".into(),
            ));
        }
        if let Some(row) = sqlx::query("SELECT id,sequence FROM typed_evidence_attempts WHERE finding_id=$1 AND spike_task_id=$2").bind(&input.finding_id).bind(&input.spike_task_id).fetch_optional(&mut **tx).await? { let id: String=row.get("id"); return Ok(TypedEvidenceAttemptAllocation { attempt_id:id.clone(), spike_task_id: input.spike_task_id.clone(), sequence:row.get("sequence"), planned_checks:checks(tx,&id).await? }); }
        if input.planned_checks.is_empty() {
            return Err(Error::InvalidData(
                "typed evidence attempt requires planned checks".into(),
            ));
        }
        let sequence: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM typed_evidence_attempts WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id,evidence_plan_id) VALUES ($1,$2,$3,$4,$5)").bind(&input.attempt_id).bind(&input.finding_id).bind(sequence).bind(&input.spike_task_id).bind(&input.evidence_plan_id).execute(&mut **tx).await?;
        for check in &input.planned_checks {
            sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method,evidence_plan_id,evidence_plan_check_id) VALUES ($1,$2,$3,$4,$5,$6,$7)").bind(&check.id).bind(&input.attempt_id).bind(check.ordinal).bind(&check.check_id).bind(planned_method(check.method)).bind(&check.evidence_plan_id).bind(&check.evidence_plan_check_id).execute(&mut **tx).await?;
        }
        Ok(TypedEvidenceAttemptAllocation {
            attempt_id: input.attempt_id.clone(),
            spike_task_id: input.spike_task_id,
            sequence,
            planned_checks: checks(tx, &input.attempt_id).await?,
        })
    }

    /// Allocate a second immutable attempt only from the latest failure. The
    /// idempotency row is checked first, so duplicate calls return its identity.
    pub async fn allocate_retry_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: AllocateTypedEvidenceRetryInput,
    ) -> Result<TypedEvidenceAttemptAllocation> {
        nonempty(&[
            &input.finding_id,
            &input.failed_transition_id,
            &input.retry_attempt_id,
            &input.retry_spike_task_id,
            &input.demanded_transition_id,
        ])?;
        if let Some(row) = sqlx::query("SELECT a.id,a.sequence,a.spike_task_id FROM typed_evidence_retry_idempotency r JOIN typed_evidence_attempts a ON a.id=r.retry_attempt_id WHERE r.finding_id=$1 AND r.failed_transition_id=$2").bind(&input.finding_id).bind(&input.failed_transition_id).fetch_optional(&mut **tx).await? {
            let id: String = row.get("id");
            return Ok(TypedEvidenceAttemptAllocation { attempt_id:id.clone(), spike_task_id:row.get("spike_task_id"), sequence:row.get("sequence"), planned_checks:checks(tx,&id).await? });
        }
        if lock_state(tx, &input.finding_id).await? != TribunalEvidenceLifecycle::Failed {
            return Err(Error::InvalidTransition(
                "retry_requires_latest_failed_transition".into(),
            ));
        }
        let latest = sqlx::query("SELECT id,to_lifecycle FROM typed_evidence_transitions WHERE finding_id=$1 ORDER BY ordinal DESC LIMIT 1 FOR UPDATE").bind(&input.finding_id).fetch_optional(&mut **tx).await?;
        if latest
            .as_ref()
            .map(|r| (r.get::<String, _>("id"), r.get::<String, _>("to_lifecycle")))
            != Some((input.failed_transition_id.clone(), "failed".into()))
        {
            return Err(Error::InvalidTransition(
                "retry_requires_latest_failed_transition".into(),
            ));
        }
        let proposal: String =
            sqlx::query_scalar("SELECT proposal_id FROM typed_evidence_findings WHERE id=$1")
                .bind(&input.finding_id)
                .fetch_one(&mut **tx)
                .await?;
        let legacy: Option<String> =
            sqlx::query_scalar("SELECT linked_spike_task_id FROM proposals WHERE id=$1 FOR UPDATE")
                .bind(&proposal)
                .fetch_one(&mut **tx)
                .await?;
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM typed_evidence_attempts a JOIN tasks t ON t.id=a.spike_task_id WHERE a.finding_id=$1 AND t.status <> 'closed')").bind(&input.finding_id).fetch_one(&mut **tx).await?;
        if legacy.is_some() || active {
            return Err(Error::InvalidTransition("active_evidence_conflict".into()));
        }
        let retry_task_labels: Option<String> = sqlx::query_scalar("SELECT t.labels::text FROM tasks t WHERE t.id=$1 AND t.status <> 'closed' AND NOT EXISTS(SELECT 1 FROM typed_evidence_attempts a WHERE a.spike_task_id=t.id)")
            .bind(&input.retry_spike_task_id)
            .fetch_optional(&mut **tx)
            .await?;
        if !retry_task_labels.as_deref().is_some_and(is_evidence_spike) {
            return Err(Error::InvalidTransition(
                "retry_spike_task_not_new_and_active".into(),
            ));
        }
        if input.planned_checks.is_empty() {
            return Err(Error::InvalidData(
                "typed evidence attempt requires planned checks".into(),
            ));
        }
        let sequence: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM typed_evidence_attempts WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id,evidence_plan_id) VALUES ($1,$2,$3,$4,$5)").bind(&input.retry_attempt_id).bind(&input.finding_id).bind(sequence).bind(&input.retry_spike_task_id).bind(&input.evidence_plan_id).execute(&mut **tx).await?;
        for check in &input.planned_checks {
            sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method,evidence_plan_id,evidence_plan_check_id) VALUES ($1,$2,$3,$4,$5,$6,$7)").bind(&check.id).bind(&input.retry_attempt_id).bind(check.ordinal).bind(&check.check_id).bind(planned_method(check.method)).bind(&check.evidence_plan_id).bind(&check.evidence_plan_check_id).execute(&mut **tx).await?;
        }
        sqlx::query("INSERT INTO typed_evidence_retry_idempotency (finding_id,failed_transition_id,retry_attempt_id) VALUES ($1,$2,$3)").bind(&input.finding_id).bind(&input.failed_transition_id).bind(&input.retry_attempt_id).execute(&mut **tx).await?;
        let ordinal: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        Self::append_transition(tx, AppendTypedEvidenceTransitionInput { id:input.demanded_transition_id, finding_id:input.finding_id, ordinal, from_lifecycle:Some(TribunalEvidenceLifecycle::Failed), to_lifecycle:TribunalEvidenceLifecycle::Demanded, actor_task_id:input.actor_task_id, metadata:serde_json::json!({"retry_attempt_id":input.retry_attempt_id,"failed_transition_id":input.failed_transition_id}) }).await?;
        let planned_checks = checks(tx, &input.retry_attempt_id).await?;
        Ok(TypedEvidenceAttemptAllocation {
            attempt_id: input.retry_attempt_id,
            spike_task_id: input.retry_spike_task_id,
            sequence,
            planned_checks,
        })
    }

    pub async fn dispatch_retry_success_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: DispatchTypedEvidenceRetryInput,
    ) -> Result<()> {
        nonempty(&[
            &input.finding_id,
            &input.attempt_id,
            &input.spike_task_id,
            &input.transition_id,
        ])?;
        let reserved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM typed_evidence_retry_idempotency r JOIN typed_evidence_attempts a ON a.id=r.retry_attempt_id JOIN typed_evidence_findings f ON f.id=r.finding_id JOIN typed_evidence_transitions d ON d.finding_id=r.finding_id WHERE r.finding_id=$1 AND a.id=$2 AND a.spike_task_id=$3 AND f.lifecycle='demanded' AND d.ordinal=(SELECT MAX(ordinal) FROM typed_evidence_transitions WHERE finding_id=r.finding_id) AND d.to_lifecycle='demanded' AND d.metadata->>'retry_attempt_id'=a.id AND d.metadata->>'failed_transition_id'=r.failed_transition_id)").bind(&input.finding_id).bind(&input.attempt_id).bind(&input.spike_task_id).fetch_one(&mut **tx).await?;
        if !reserved {
            return Err(Error::InvalidTransition(
                "retry_attempt_identity_mismatch".into(),
            ));
        }
        let ordinal: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        Self::append_transition(tx, AppendTypedEvidenceTransitionInput { id:input.transition_id, finding_id:input.finding_id, ordinal, from_lifecycle:Some(TribunalEvidenceLifecycle::Demanded), to_lifecycle:TribunalEvidenceLifecycle::SpikeActive, actor_task_id:input.actor_task_id, metadata:serde_json::json!({"attempt_id":input.attempt_id,"spike_task_id":input.spike_task_id}) }).await
    }

    /// Activate precisely the initial attempt that was committed as demanded.
    /// Retry allocations use their reservation-specific primitive instead.
    pub async fn dispatch_demand_success_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: DispatchTypedEvidenceDemandInput,
    ) -> Result<()> {
        nonempty(&[
            &input.finding_id,
            &input.attempt_id,
            &input.spike_task_id,
            &input.transition_id,
        ])?;
        let reserved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM typed_evidence_attempts a JOIN typed_evidence_findings f ON f.id=a.finding_id JOIN typed_evidence_transitions d ON d.finding_id=f.id WHERE f.id=$1 AND a.id=$2 AND a.spike_task_id=$3 AND f.lifecycle='demanded' AND NOT EXISTS(SELECT 1 FROM typed_evidence_retry_idempotency r WHERE r.retry_attempt_id=a.id) AND d.ordinal=(SELECT MAX(ordinal) FROM typed_evidence_transitions WHERE finding_id=f.id) AND d.to_lifecycle='demanded')").bind(&input.finding_id).bind(&input.attempt_id).bind(&input.spike_task_id).fetch_one(&mut **tx).await?;
        if !reserved {
            return Err(Error::InvalidTransition(
                "demand_attempt_identity_mismatch".into(),
            ));
        }
        let ordinal: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        Self::append_transition(tx, AppendTypedEvidenceTransitionInput { id: input.transition_id, finding_id: input.finding_id, ordinal, from_lifecycle: Some(TribunalEvidenceLifecycle::Demanded), to_lifecycle: TribunalEvidenceLifecycle::SpikeActive, actor_task_id: input.actor_task_id, metadata: serde_json::json!({"attempt_id":input.attempt_id,"spike_task_id":input.spike_task_id}) }).await
    }

    pub async fn append_retry_dispatch_error(
        &self,
        input: TypedEvidenceRetryDispatchErrorInput,
    ) -> Result<()> {
        nonempty(&[
            &input.finding_id,
            &input.attempt_id,
            &input.spike_task_id,
            &input.error,
        ])?;
        let written = sqlx::query("INSERT INTO typed_evidence_retry_dispatch_errors (id,finding_id,attempt_id,spike_task_id,error) SELECT $1,$2,$3,$4,$5 WHERE EXISTS (SELECT 1 FROM typed_evidence_retry_idempotency r JOIN typed_evidence_attempts a ON a.id=r.retry_attempt_id JOIN typed_evidence_findings f ON f.id=r.finding_id JOIN typed_evidence_transitions d ON d.finding_id=r.finding_id WHERE r.finding_id=$2 AND a.id=$3 AND a.spike_task_id=$4 AND f.lifecycle='demanded' AND d.ordinal=(SELECT MAX(ordinal) FROM typed_evidence_transitions WHERE finding_id=r.finding_id) AND d.to_lifecycle='demanded' AND d.metadata->>'retry_attempt_id'=a.id AND d.metadata->>'failed_transition_id'=r.failed_transition_id)").bind(uuid::Uuid::now_v7().to_string()).bind(&input.finding_id).bind(&input.attempt_id).bind(&input.spike_task_id).bind(&input.error).execute(self.db.pool()).await?;
        if written.rows_affected() != 1 {
            return Err(Error::InvalidTransition(
                "retry_attempt_identity_mismatch".into(),
            ));
        }
        Ok(())
    }

    /// Record an enqueue failure for the exact initial allocation without
    /// changing lifecycle or manufacturing another task/attempt.
    pub async fn append_demand_dispatch_error(
        &self,
        input: TypedEvidenceDemandDispatchErrorInput,
    ) -> Result<()> {
        nonempty(&[
            &input.finding_id,
            &input.attempt_id,
            &input.spike_task_id,
            &input.error,
        ])?;
        let written = sqlx::query("INSERT INTO typed_evidence_retry_dispatch_errors (id,finding_id,attempt_id,spike_task_id,error) SELECT $1,$2,$3,$4,$5 WHERE EXISTS (SELECT 1 FROM typed_evidence_attempts a JOIN typed_evidence_findings f ON f.id=a.finding_id JOIN typed_evidence_transitions d ON d.finding_id=f.id WHERE f.id=$2 AND a.id=$3 AND a.spike_task_id=$4 AND f.lifecycle='demanded' AND NOT EXISTS(SELECT 1 FROM typed_evidence_retry_idempotency r WHERE r.retry_attempt_id=a.id) AND d.ordinal=(SELECT MAX(ordinal) FROM typed_evidence_transitions WHERE finding_id=f.id) AND d.to_lifecycle='demanded')").bind(uuid::Uuid::now_v7().to_string()).bind(&input.finding_id).bind(&input.attempt_id).bind(&input.spike_task_id).bind(&input.error).execute(self.db.pool()).await?;
        if written.rows_affected() != 1 {
            return Err(Error::InvalidTransition(
                "demand_attempt_identity_mismatch".into(),
            ));
        }
        Ok(())
    }

    pub async fn retry_attempt_for_failure(
        &self,
        finding_id: &str,
        failed_transition_id: &str,
    ) -> Result<Option<TypedEvidenceAttemptAllocation>> {
        let mut tx = self.db.pool().begin().await?;
        let row = sqlx::query("SELECT a.id,a.sequence,a.spike_task_id FROM typed_evidence_retry_idempotency r JOIN typed_evidence_attempts a ON a.id=r.retry_attempt_id WHERE r.finding_id=$1 AND r.failed_transition_id=$2").bind(finding_id).bind(failed_transition_id).fetch_optional(&mut *tx).await?;
        if let Some(row) = row {
            let attempt_id: String = row.get("id");
            Ok(Some(TypedEvidenceAttemptAllocation {
                spike_task_id: row.get("spike_task_id"),
                sequence: row.get("sequence"),
                planned_checks: checks(&mut tx, &attempt_id).await?,
                attempt_id,
            }))
        } else {
            Ok(None)
        }
    }

    /// Appends the fact and advances only the materialized current lifecycle.
    pub async fn append_transition_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: AppendTypedEvidenceTransitionInput,
    ) -> Result<()> {
        if input.to_lifecycle.is_terminal() {
            return Err(Error::InvalidTransition(
                "terminal transitions require dispose_in_transaction".into(),
            ));
        }
        Self::append_transition(tx, input).await
    }

    async fn append_transition(
        tx: &mut Transaction<'_, Postgres>,
        input: AppendTypedEvidenceTransitionInput,
    ) -> Result<()> {
        if input.ordinal <= 0 || !input.metadata.is_object() {
            return Err(Error::InvalidData(
                "transition requires positive ordinal and object metadata".into(),
            ));
        }
        let state = lock_state(tx, &input.finding_id).await?;
        if input.from_lifecycle != Some(state) || !allowed(state, input.to_lifecycle) {
            return Err(Error::InvalidTransition(format!(
                "{} -> {}",
                state.as_str(),
                input.to_lifecycle.as_str()
            )));
        }
        let next: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        if next != input.ordinal {
            return Err(Error::InvalidData(
                "typed evidence transition ordinal is not next".into(),
            ));
        }
        sqlx::query("INSERT INTO typed_evidence_transitions (id,finding_id,ordinal,from_lifecycle,to_lifecycle,actor_task_id,metadata) VALUES ($1,$2,$3,$4,$5,$6,$7)").bind(&input.id).bind(&input.finding_id).bind(input.ordinal).bind(state.as_str()).bind(input.to_lifecycle.as_str()).bind(&input.actor_task_id).bind(&input.metadata).execute(&mut **tx).await?;
        sqlx::query("UPDATE typed_evidence_findings SET lifecycle=$1,updated_at=to_char(now() AT TIME ZONE 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$2").bind(input.to_lifecycle.as_str()).bind(&input.finding_id).execute(&mut **tx).await?;
        Ok(())
    }

    pub async fn dispose_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        input: DisposeTypedEvidenceInput,
    ) -> Result<TypedEvidenceDispositionProjection> {
        if !matches!(
            input.disposition,
            TribunalEvidenceLifecycle::Resolved | TribunalEvidenceLifecycle::Withdrawn
        ) {
            return Err(Error::InvalidTransition(
                "disposition must be terminal".into(),
            ));
        }
        nonempty(&[
            &input.disposition_id,
            &input.transition_id,
            &input.finding_id,
            &input.judge_task_id,
            &input.rationale,
        ])?;
        if input.disposition == TribunalEvidenceLifecycle::Withdrawn
            && !input.withdrawal_is_non_load_bearing
        {
            return Err(Error::InvalidData(
                "withdrawal requires non-load-bearing assertion".into(),
            ));
        }
        let row = sqlx::query(
            "SELECT proposal_id,lifecycle FROM typed_evidence_findings WHERE id=$1 FOR UPDATE",
        )
        .bind(&input.finding_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::InvalidData("finding not found".into()))?;
        let proposal_id: String = row.get("proposal_id");
        // A finding may be demanded by an Adversary. Terminal attribution is
        // the active Judge assigned to this proposal, not the demand-origin
        // task. Preserve repository authority by validating that assignment in
        // this transaction before any lifecycle or legacy-link mutation.
        let active_judge: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks t JOIN refinement_dispatch_intents i ON i.id=t.refinement_intent_id JOIN refinement_runs r ON r.id=i.run_id WHERE t.id=$1 AND r.proposal_id=$2 AND r.state='running' AND i.state='materialized' AND i.task_id=t.id AND t.status IN ('open','in_progress') AND t.agent_type='judge' AND t.refinement_run_id=r.id AND t.refinement_generation=r.generation AND t.refinement_round=i.round AND t.refinement_phase=i.phase AND t.refinement_role=i.role)").bind(&input.judge_task_id).bind(&proposal_id).fetch_one(&mut **tx).await?;
        if !active_judge {
            return Err(Error::InvalidData(
                "active Judge attribution required".into(),
            ));
        }
        let legacy_link: Option<String> =
            sqlx::query_scalar("SELECT linked_spike_task_id FROM proposals WHERE id=$1 FOR UPDATE")
                .bind(&proposal_id)
                .fetch_one(&mut **tx)
                .await?;
        if let Some(linked_spike_task_id) = legacy_link.as_deref() {
            let Some(parity) =
                Self::dual_read_legacy_parity_in_transaction(tx, &proposal_id).await?
            else {
                return Err(Error::InvalidTransition(
                    "legacy_typed_parity_mismatch".into(),
                ));
            };
            if parity.finding.id != input.finding_id
                || parity.spike_task_id.as_deref() != Some(linked_spike_task_id)
            {
                return Err(Error::InvalidTransition(
                    "legacy_typed_parity_mismatch".into(),
                ));
            }
        }
        let committed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM proposal_revisions WHERE proposal_id=$1 AND seq=$2 AND event_kind='spec_revision')").bind(&proposal_id).bind(input.folding_revision).fetch_one(&mut **tx).await?;
        if !committed {
            return Err(Error::InvalidData(
                "existing committed folding revision required".into(),
            ));
        }
        let state = parse(&row.get::<String, _>("lifecycle"))?;
        let latest_transition: Option<String> = sqlx::query_scalar(
            "SELECT to_lifecycle FROM typed_evidence_transitions WHERE finding_id=$1 ORDER BY ordinal DESC LIMIT 1",
        )
        .bind(&input.finding_id)
        .fetch_optional(&mut **tx)
        .await?;
        if latest_transition.as_deref() != Some(state.as_str()) {
            return Err(Error::InvalidTransition(
                "legacy_typed_parity_mismatch".into(),
            ));
        }
        let ordinal: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM typed_evidence_transitions WHERE finding_id=$1",
        )
        .bind(&input.finding_id)
        .fetch_one(&mut **tx)
        .await?;
        Self::append_transition(
            tx,
            AppendTypedEvidenceTransitionInput {
                id: input.transition_id,
                finding_id: input.finding_id.clone(),
                ordinal,
                from_lifecycle: Some(state),
                to_lifecycle: input.disposition,
                actor_task_id: Some(input.judge_task_id.clone()),
                metadata: serde_json::json!({
                    "rationale": input.rationale,
                    "withdrawal_is_non_load_bearing": input.withdrawal_is_non_load_bearing,
                }),
            },
        )
        .await?;
        let clear = sqlx::query(
            "UPDATE proposals SET linked_spike_task_id=NULL,needs_evidence_claim=NULL WHERE id=$1 AND linked_spike_task_id IS NOT DISTINCT FROM $2",
        )
        .bind(&proposal_id)
        .bind(&legacy_link)
        .execute(&mut **tx)
        .await?;
        if clear.rows_affected() != 1 {
            return Err(Error::InvalidTransition(
                "legacy_typed_parity_mismatch".into(),
            ));
        }
        let row=sqlx::query("INSERT INTO typed_evidence_dispositions (id,finding_id,validation_result_id,folding_revision,outcome,disposition,judge_task_id,rationale) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING created_at").bind(&input.disposition_id).bind(&input.finding_id).bind(&input.validation_result_id).bind(input.folding_revision).bind(outcome(input.outcome)).bind(input.disposition.as_str()).bind(&input.judge_task_id).bind(&input.rationale).fetch_one(&mut **tx).await?;
        Ok(TypedEvidenceDispositionProjection {
            disposition: TribunalEvidenceDisposition {
                id: input.disposition_id,
                finding_id: input.finding_id,
                validation_result_id: input.validation_result_id,
                folding_revision: input.folding_revision,
                outcome: input.outcome,
                disposition: input.disposition,
                judge_task_id: input.judge_task_id,
                rationale: input.rationale,
                created_at: row.get("created_at"),
            },
            finding_lifecycle: input.disposition,
        })
    }
}
fn allowed(from: TribunalEvidenceLifecycle, to: TribunalEvidenceLifecycle) -> bool {
    matches!(
        (from, to),
        (
            TribunalEvidenceLifecycle::Demanded,
            TribunalEvidenceLifecycle::SpikeActive | TribunalEvidenceLifecycle::Withdrawn
        ) | (
            TribunalEvidenceLifecycle::SpikeActive,
            TribunalEvidenceLifecycle::EvidenceReceived
                | TribunalEvidenceLifecycle::Failed
                | TribunalEvidenceLifecycle::Withdrawn
        ) | (
            TribunalEvidenceLifecycle::EvidenceReceived,
            TribunalEvidenceLifecycle::Resolved | TribunalEvidenceLifecycle::Withdrawn
        ) | (
            TribunalEvidenceLifecycle::Failed,
            TribunalEvidenceLifecycle::Demanded
                | TribunalEvidenceLifecycle::SpikeActive
                | TribunalEvidenceLifecycle::Withdrawn
        )
    )
}
fn parse(s: &str) -> Result<TribunalEvidenceLifecycle> {
    match s {
        "demanded" => Ok(TribunalEvidenceLifecycle::Demanded),
        "spike_active" => Ok(TribunalEvidenceLifecycle::SpikeActive),
        "evidence_received" => Ok(TribunalEvidenceLifecycle::EvidenceReceived),
        "failed" => Ok(TribunalEvidenceLifecycle::Failed),
        "resolved" => Ok(TribunalEvidenceLifecycle::Resolved),
        "withdrawn" => Ok(TribunalEvidenceLifecycle::Withdrawn),
        _ => Err(Error::InvalidData(
            "unknown typed evidence lifecycle".into(),
        )),
    }
}
fn outcome(value: TribunalEvidenceOutcome) -> &'static str {
    match value {
        TribunalEvidenceOutcome::Resolved => "resolved",
        TribunalEvidenceOutcome::Partial => "partial",
        TribunalEvidenceOutcome::Unresolved => "unresolved",
    }
}
fn derive_outcome(
    checks: &[TribunalEvidenceReturnCheckV1],
    check_healthy: &HashMap<&str, bool>,
) -> TribunalEvidenceOutcome {
    let any_positive = check_healthy.values().any(|usable| *usable);
    let all_passed_usable = checks.iter().all(|check| {
        check.status == "passed"
            && check_healthy
                .get(check.check_id.as_str())
                .copied()
                .unwrap_or(false)
    });
    if all_passed_usable {
        TribunalEvidenceOutcome::Resolved
    } else if any_positive {
        TribunalEvidenceOutcome::Partial
    } else {
        TribunalEvidenceOutcome::Unresolved
    }
}
fn planned_method(value: TribunalEvidenceAnchorMethod) -> &'static str {
    match value {
        TribunalEvidenceAnchorMethod::Code => "code",
        TribunalEvidenceAnchorMethod::Graph => "graph",
        TribunalEvidenceAnchorMethod::Command => "command",
        _ => unreachable!("planned checks only permit code, graph, or command"),
    }
}
#[derive(Debug)]
struct HydratedAnchor {
    healthy: bool,
    method_compatible: bool,
    identity: serde_json::Value,
    detail: String,
}
impl HydratedAnchor {
    fn unusable(detail: impl Into<String>, identity: serde_json::Value) -> Self {
        Self {
            healthy: false,
            method_compatible: false,
            identity,
            detail: detail.into(),
        }
    }
    fn health(&self) -> &'static str {
        if self.healthy && self.method_compatible {
            "healthy"
        } else {
            "unusable"
        }
    }
}
fn unobserved_repository_anchor(
    attempt_id: &str,
    planned_check_id: &str,
    plan_id: &str,
    commit: &str,
) -> HydratedAnchor {
    HydratedAnchor::unusable(
        "no immutable server-owned repository observation bound to this attempt and planned check",
        serde_json::json!({
            "attempt_id": attempt_id,
            "planned_check_id": planned_check_id,
            "evidence_plan_id": plan_id,
            "captured_commit_sha": commit,
        }),
    )
}
/// A locator is merely a caller selector. Its grammar is family-specific so
/// one family cannot be reinterpreted as another family's evidence.
enum CanonicalAnchorLocator<'a> {
    Repository {
        plan_id: &'a str,
        commit: &'a str,
    },
    Command {
        invocation_id: &'a str,
    },
    Code {
        path: &'a str,
        commit: &'a str,
        start_line: u32,
        end_line: u32,
    },
    Graph {
        generation_id: &'a str,
    },
    Artifact {
        artifact_id: &'a str,
    },
    Memory {
        note_id: &'a str,
        content_sha256: &'a str,
    },
    External {
        uri: &'a str,
        content_sha256: &'a str,
    },
}
fn canonical_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn canonical_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn canonical_commit(value: &str) -> bool {
    (7..=128).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn parse_line_range(value: &str) -> Option<(u32, u32)> {
    let (start, end) = match value.split_once('-') {
        Some((start, end)) => (start.parse().ok()?, end.parse().ok()?),
        None => {
            let line = value.parse().ok()?;
            (line, line)
        }
    };
    (start > 0 && start <= end).then_some((start, end))
}
fn parse_canonical_locator<'a>(
    method: &str,
    locator: &'a str,
) -> Option<CanonicalAnchorLocator<'a>> {
    match method {
        "repository" => {
            let (plan_id, commit) = locator.strip_prefix("repository:")?.split_once(':')?;
            (canonical_id(plan_id) && canonical_commit(commit))
                .then_some(CanonicalAnchorLocator::Repository { plan_id, commit })
        }
        "command" => {
            let id = locator.strip_prefix("command:")?;
            canonical_id(id).then_some(CanonicalAnchorLocator::Command { invocation_id: id })
        }
        "code" => {
            let (path, revision) = locator.strip_prefix("code:")?.split_once('@')?;
            let (commit, lines) = revision.split_once("#L")?;
            let (start_line, end_line) = parse_line_range(lines)?;
            (!path.is_empty()
                && !path.starts_with('/')
                && !path.contains("..")
                && !path.chars().any(char::is_whitespace)
                && canonical_commit(commit))
            .then_some(CanonicalAnchorLocator::Code {
                path,
                commit,
                start_line,
                end_line,
            })
        }
        "graph" => {
            let id = locator.strip_prefix("graph:")?;
            uuid::Uuid::parse_str(id)
                .ok()
                .map(|_| CanonicalAnchorLocator::Graph { generation_id: id })
        }
        "artifact" => {
            let id = locator.strip_prefix("artifact:")?;
            uuid::Uuid::parse_str(id)
                .ok()
                .map(|_| CanonicalAnchorLocator::Artifact { artifact_id: id })
        }
        "memory" => {
            let (note_id, content_sha256) = locator.strip_prefix("memory:")?.split_once('@')?;
            (uuid::Uuid::parse_str(note_id).is_ok() && canonical_sha256(content_sha256)).then_some(
                CanonicalAnchorLocator::Memory {
                    note_id,
                    content_sha256,
                },
            )
        }
        "external" => {
            let (uri, content_sha256) =
                locator.strip_prefix("external:")?.rsplit_once("#sha256=")?;
            (uri.starts_with("https://") && canonical_sha256(content_sha256)).then_some(
                CanonicalAnchorLocator::External {
                    uri,
                    content_sha256,
                },
            )
        }
        _ => None,
    }
}
/// Resolve only canonical locators against server-owned rows scoped to this
/// frozen attempt/check. Finalized JSON is never searched for caller text.
async fn hydrate_anchor(
    tx: &mut Transaction<'_, Postgres>,
    attempt_id: &str,
    planned: &TribunalEvidencePlannedCheck,
    anchor: &TribunalEvidenceReturnAnchorV1,
) -> Result<HydratedAnchor> {
    let scope = serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"evidence_plan_id":planned.evidence_plan_id});
    let compatible = matches!(
        (planned_method(planned.method), anchor.method.as_str()),
        ("code", "code" | "repository")
            | ("graph", "graph" | "repository")
            | ("command", "command" | "artifact" | "memory" | "external")
    );
    if !compatible {
        return Ok(HydratedAnchor::unusable(
            "anchor method incompatible with planned check",
            scope,
        ));
    }
    let Some(locator) = parse_canonical_locator(&anchor.method, &anchor.locator) else {
        return Ok(HydratedAnchor::unusable(
            "locator is not canonical for its anchor family",
            scope,
        ));
    };
    match locator {
        CanonicalAnchorLocator::Command { invocation_id } => {
            hydrate_command_invocation(tx, attempt_id, planned, invocation_id).await
        }
        // A frozen plan and captured commit establish planning context, not an
        // observation that performed this check. There is currently no
        // immutable repository-observation row owned by an exact
        // attempt/planned-check pair, so even an exact canonical selector must
        // remain unusable. Do not infer health from `evidence_plans`.
        CanonicalAnchorLocator::Repository { plan_id, commit } => Ok(unobserved_repository_anchor(
            attempt_id,
            &planned.id,
            plan_id,
            commit,
        )),
        // Graph generations and galaxy artifacts are immutable, but their
        // repository records are project-wide. Commit equality is not exact
        // attempt/check provenance and would allow cross-attempt evidence.
        CanonicalAnchorLocator::Graph { generation_id } => Ok(HydratedAnchor::unusable(
            "no immutable server-owned graph provenance bound to this attempt and planned check",
            serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"generation_id":generation_id}),
        )),
        CanonicalAnchorLocator::Artifact { artifact_id } => Ok(HydratedAnchor::unusable(
            "no immutable server-owned artifact provenance bound to this attempt and planned check",
            serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"artifact_id":artifact_id}),
        )),
        // The repository has no immutable file-content, memory-revision, or
        // external-provenance source bound to an evidence plan. These parsed
        // selectors remain persisted as unusable rather than trusted.
        CanonicalAnchorLocator::Code {
            path,
            commit,
            start_line,
            end_line,
        } => Ok(HydratedAnchor::unusable(
            "no immutable server-owned code source",
            serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"path":path,"captured_commit_sha":commit,"start_line":start_line,"end_line":end_line}),
        )),
        CanonicalAnchorLocator::Memory {
            note_id,
            content_sha256,
        } => Ok(HydratedAnchor::unusable(
            "no immutable server-owned memory revision source",
            serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"note_id":note_id,"content_sha256":content_sha256}),
        )),
        CanonicalAnchorLocator::External {
            uri,
            content_sha256,
        } => Ok(HydratedAnchor::unusable(
            "no immutable server-owned external provenance source",
            serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"uri":uri,"content_sha256":content_sha256}),
        )),
    }
}
async fn hydrate_command_invocation(
    tx: &mut Transaction<'_, Postgres>,
    attempt_id: &str,
    planned: &TribunalEvidencePlannedCheck,
    invocation_id: &str,
) -> Result<HydratedAnchor> {
    let scope = serde_json::json!({"attempt_id":attempt_id,"planned_check_id":planned.id,"invocation_id":invocation_id});
    if planned_method(planned.method) != "command" {
        return Ok(HydratedAnchor::unusable(
            "command invocation incompatible with planned check",
            scope,
        ));
    }
    let row = sqlx::query("SELECT plan_id,check_id,launch_state,process_state,exit_code,timed_out,captured_commit_sha FROM evidence_command_invocations WHERE id=$1").bind(invocation_id).fetch_optional(&mut **tx).await?.ok_or_else(|| v1("unknown_invocation"))?;
    let plan_id: String = row.get("plan_id");
    let check_id: String = row.get("check_id");
    let owned = planned.evidence_plan_id.as_deref() == Some(plan_id.as_str())
        && planned.evidence_plan_check_id.as_deref() == Some(check_id.as_str());
    let healthy = owned
        && row.get::<String, _>("launch_state") == "launched"
        && row.get::<String, _>("process_state") == "exited"
        && row.get::<Option<i32>, _>("exit_code") == Some(0)
        && !row.get::<bool, _>("timed_out");
    Ok(HydratedAnchor {
        healthy,
        method_compatible: owned,
        identity: serde_json::json!({"invocation_id":invocation_id,"evidence_plan_id":plan_id,"evidence_plan_check_id":check_id,"captured_commit_sha":row.get::<String,_>("captured_commit_sha")}),
        detail: if healthy {
            "exact successful command invocation".into()
        } else {
            "invocation is cross-plan, cross-check, or unhealthy".into()
        },
    })
}
fn method(s: &str) -> Result<TribunalEvidenceAnchorMethod> {
    match s {
        "code" => Ok(TribunalEvidenceAnchorMethod::Code),
        "graph" => Ok(TribunalEvidenceAnchorMethod::Graph),
        "command" => Ok(TribunalEvidenceAnchorMethod::Command),
        _ => Err(Error::InvalidData("invalid planned check method".into())),
    }
}
async fn lock_state(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> Result<TribunalEvidenceLifecycle> {
    let s: Option<String> =
        sqlx::query_scalar("SELECT lifecycle FROM typed_evidence_findings WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    parse(&s.ok_or_else(|| Error::InvalidData("finding not found".into()))?)
}

/// Fence a first terminal return to the finding's current reservation. Callers
/// check the immutable validation-result replay fence before entering here.
async fn require_active_return_attempt(
    tx: &mut Transaction<'_, Postgres>,
    attempt_id: &str,
    finding_id: &str,
    spike_task_id: &str,
) -> Result<()> {
    let state = lock_state(tx, finding_id).await?;
    let latest = sqlx::query(
        "SELECT id,spike_task_id FROM typed_evidence_attempts \
         WHERE finding_id=$1 ORDER BY sequence DESC LIMIT 1 FOR UPDATE",
    )
    .bind(finding_id)
    .fetch_optional(&mut **tx)
    .await?;
    let authorized = latest.is_some_and(|row| {
        row.get::<String, _>("id") == attempt_id
            && row.get::<String, _>("spike_task_id") == spike_task_id
    });
    if state != TribunalEvidenceLifecycle::SpikeActive || !authorized {
        return Err(v1("attempt_not_active"));
    }
    Ok(())
}

async fn active_attempt(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM typed_evidence_attempts WHERE finding_id=$1 ORDER BY sequence DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?)
}
async fn checks(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> Result<Vec<TribunalEvidencePlannedCheck>> {
    let rows=sqlx::query("SELECT id,attempt_id,ordinal,check_id,method,evidence_plan_id,evidence_plan_check_id FROM typed_evidence_planned_checks WHERE attempt_id=$1 ORDER BY ordinal").bind(id).fetch_all(&mut **tx).await?;
    rows.into_iter()
        .map(|r| {
            Ok(TribunalEvidencePlannedCheck {
                id: r.get("id"),
                attempt_id: r.get("attempt_id"),
                ordinal: r.get("ordinal"),
                check_id: r.get("check_id"),
                method: method(&r.get::<String, _>("method"))?,
                evidence_plan_id: r.get("evidence_plan_id"),
                evidence_plan_check_id: r.get("evidence_plan_check_id"),
            })
        })
        .collect()
}
fn finding(r: &sqlx::postgres::PgRow) -> Result<TribunalEvidenceFinding> {
    Ok(TribunalEvidenceFinding {
        id: r.get("id"),
        proposal_id: r.get("proposal_id"),
        demand_hash: r.get("demand_hash"),
        lifecycle: parse(&r.get::<String, _>("lifecycle"))?,
        claim: r.get("claim"),
        demanded_revision_seq: r.get("demanded_revision_seq"),
        created_by_task_id: r.get("created_by_task_id"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}
fn legacy_claim(raw: Option<&str>) -> Option<serde_json::Value> {
    match raw.filter(|value| !value.trim().is_empty()) {
        Some(raw) => {
            let value: serde_json::Value = serde_json::from_str(raw).ok()?;
            (value.is_object()
                && value
                    .get("created_by_task_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| !id.trim().is_empty())
                && value
                    .get("against_revision_seq")
                    .and_then(serde_json::Value::as_i64)
                    .is_some_and(|seq| seq > 0))
            .then_some(value)
        }
        None => Some(serde_json::json!({ LEGACY_LINK_ONLY_CLAIM: true })),
    }
}
pub fn legacy_demand_hash(claim: &serde_json::Value, spike_task_id: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"typed_evidence_legacy_backfill_v1\0");
    hasher.update(serde_json::to_vec(claim).expect("JSON value serialization cannot fail"));
    hasher.update(b"\0");
    hasher.update(spike_task_id.unwrap_or_default().as_bytes());
    format!("legacy:{:x}", hasher.finalize())
}

/// Stable caller-delivery identity; task allocation is intentionally excluded.
pub fn normalized_demand_hash(claim: &serde_json::Value) -> String {
    fn normalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(s.trim().to_owned()),
            serde_json::Value::Array(xs) => {
                serde_json::Value::Array(xs.iter().map(normalize).collect())
            }
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort_unstable();
                for key in keys {
                    out.insert(key.clone(), normalize(&map[key]));
                }
                serde_json::Value::Object(out)
            }
            _ => value.clone(),
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(b"typed_evidence_normalized_demand_v1\0");
    hasher.update(serde_json::to_vec(&normalize(claim)).expect("JSON serialization cannot fail"));
    format!("demand:{:x}", hasher.finalize())
}
fn nonempty(values: &[&str]) -> Result<()> {
    if values.iter().any(|v| v.trim().is_empty()) {
        Err(Error::InvalidData(
            "typed evidence identity fields must be non-empty".into(),
        ))
    } else {
        Ok(())
    }
}

fn v1(code: &str) -> Error {
    Error::InvalidData(code.into())
}
fn bytes(value: &str) -> usize {
    value.len()
}
fn parse_outcome(value: &str) -> Result<TribunalEvidenceOutcome> {
    match value {
        "resolved" => Ok(TribunalEvidenceOutcome::Resolved),
        "partial" => Ok(TribunalEvidenceOutcome::Partial),
        "unresolved" => Ok(TribunalEvidenceOutcome::Unresolved),
        _ => Err(v1("invalid_persisted_outcome")),
    }
}
fn validate_return_shape(p: &TribunalEvidenceReturnV1) -> Result<()> {
    if p.version != "TribunalEvidenceReturnV1" {
        return Err(v1("unsupported_version"));
    }
    if [
        p.finding_id.as_str(),
        p.spike_task_id.as_str(),
        p.attempt_id.as_str(),
        p.conclusion.as_str(),
    ]
    .iter()
    .any(|s| s.trim().is_empty())
    {
        return Err(v1("missing_identity"));
    }
    if [
        p.finding_id.as_str(),
        p.spike_task_id.as_str(),
        p.attempt_id.as_str(),
        p.version.as_str(),
    ]
    .iter()
    .any(|s| bytes(s) > 2048)
    {
        return Err(v1("string_too_large"));
    }
    if bytes(&p.conclusion) > 8192 {
        return Err(v1("conclusion_too_large"));
    }
    if p.checks.len() > 32 || p.findings.len() > 32 || p.failures.len() > 32 || p.gaps.len() > 32 {
        return Err(v1("return_limit_exceeded"));
    }
    Ok(())
}
fn limit_check(c: &TribunalEvidenceReturnCheckV1) -> Result<()> {
    if bytes(&c.check_id) > 2048
        || bytes(&c.method) > 2048
        || bytes(&c.status) > 2048
        || c.invocation_id.as_ref().is_some_and(|s| bytes(s) > 2048)
        || c.anchors.len() > 16
    {
        return Err(v1("check_limit_exceeded"));
    }
    if !matches!(c.status.as_str(), "passed" | "failed" | "not_run") {
        return Err(v1("invalid_check_status"));
    }
    if c.status != "passed" && c.detail.as_deref().is_none_or(str::is_empty) {
        return Err(v1("status_detail_required"));
    }
    if c.invocation_id.is_some() && c.method != "command" {
        return Err(v1("invocation_method_mismatch"));
    }
    if c.method == "command"
        && c.status == "passed"
        && c.invocation_id.as_deref().is_none_or(str::is_empty)
    {
        return Err(v1("command_invocation_required"));
    }
    if c.method == "command" && c.status != "passed" && c.invocation_id.is_some() {
        return Err(v1("command_invocation_forbidden"));
    }
    if c.detail.as_ref().is_some_and(|d| bytes(d) > 8192) {
        return Err(v1("detail_too_large"));
    }
    for a in &c.anchors {
        limit_anchor(a)?;
    }
    Ok(())
}
fn limit_anchor(a: &TribunalEvidenceReturnAnchorV1) -> Result<()> {
    if a.locator.trim().is_empty()
        || bytes(&a.locator) > 2048
        || bytes(&a.method) > 2048
        || !matches!(
            a.method.as_str(),
            "code" | "graph" | "command" | "artifact" | "memory" | "external" | "repository"
        )
    {
        Err(v1("invalid_anchor"))
    } else {
        Ok(())
    }
}
fn limit_failure(
    i: &TribunalEvidenceReturnFailureV1,
    expected: &HashMap<&str, &TribunalEvidencePlannedCheck>,
) -> Result<()> {
    limit_typed_issue(&i.check_id, &i.code, &i.detail, expected)
}
fn limit_gap(
    i: &TribunalEvidenceReturnGapV1,
    expected: &HashMap<&str, &TribunalEvidencePlannedCheck>,
) -> Result<()> {
    limit_typed_issue(&i.check_id, &i.code, &i.detail, expected)
}
fn limit_typed_issue(
    check_id: &str,
    code: &str,
    detail: &str,
    expected: &HashMap<&str, &TribunalEvidencePlannedCheck>,
) -> Result<()> {
    if check_id.trim().is_empty()
        || code.trim().is_empty()
        || detail.trim().is_empty()
        || bytes(check_id) > 2048
        || bytes(code) > 2048
        || bytes(detail) > 8192
    {
        Err(v1("invalid_issue"))
    } else if !expected.contains_key(check_id) {
        Err(v1("dangling_issue_check"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_evidence_lifecycle_v1() {
        use TribunalEvidenceLifecycle::*;
        let allowed_edges = [
            (Demanded, SpikeActive),
            (Demanded, Withdrawn),
            (SpikeActive, EvidenceReceived),
            (SpikeActive, Failed),
            (SpikeActive, Withdrawn),
            (EvidenceReceived, Resolved),
            (EvidenceReceived, Withdrawn),
            (Failed, Demanded),
            (Failed, SpikeActive),
            (Failed, Withdrawn),
        ];
        for from in [
            Demanded,
            SpikeActive,
            EvidenceReceived,
            Failed,
            Resolved,
            Withdrawn,
        ] {
            for to in [
                Demanded,
                SpikeActive,
                EvidenceReceived,
                Failed,
                Resolved,
                Withdrawn,
            ] {
                assert_eq!(
                    allowed(from, to),
                    allowed_edges.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
        assert!(!Demanded.is_terminal());
        assert!(Resolved.is_terminal());
        assert!(Withdrawn.is_terminal());
    }

    #[test]
    fn malformed_return_envelope_retains_attempt_identity() {
        let malformed = br#"{
            "version":"TribunalEvidenceReturnV1",
            "finding_id":"finding",
            "spike_task_id":"task",
            "attempt_id":"attempt",
            "checks":[{"check_id":"check","method":"code"}]
        }"#;

        assert!(serde_json::from_slice::<TribunalEvidenceReturnV1>(malformed).is_err());
        let envelope = serde_json::from_slice::<TribunalEvidenceReturnEnvelopeV1>(malformed)
            .expect("minimal envelope must decode despite malformed V1 body");
        assert_eq!(envelope.attempt_id.as_deref(), Some("attempt"));
    }

    #[test]
    fn matching_plan_repository_locators_do_not_resolve_passed_checks() {
        let plan_id = "019fcad4-4b3f-7ce2-8527-93c5cfab897a";
        let commit = "abcdef0123456789";
        let locator = format!("repository:{plan_id}:{commit}");
        let parsed = parse_canonical_locator("repository", &locator)
            .expect("matching plan repository locator is canonical");
        let CanonicalAnchorLocator::Repository {
            plan_id: parsed_plan_id,
            commit: parsed_commit,
        } = parsed
        else {
            panic!("repository parser returned a different anchor family");
        };
        assert_eq!(parsed_plan_id, plan_id);
        assert_eq!(parsed_commit, commit);

        let checks = vec![
            TribunalEvidenceReturnCheckV1 {
                check_id: "code-check".into(),
                method: "code".into(),
                status: "passed".into(),
                detail: None,
                invocation_id: None,
                anchors: vec![TribunalEvidenceReturnAnchorV1 {
                    method: "repository".into(),
                    locator: locator.clone(),
                }],
            },
            TribunalEvidenceReturnCheckV1 {
                check_id: "graph-check".into(),
                method: "graph".into(),
                status: "passed".into(),
                detail: None,
                invocation_id: None,
                anchors: vec![TribunalEvidenceReturnAnchorV1 {
                    method: "repository".into(),
                    locator: locator.clone(),
                }],
            },
        ];
        let code =
            unobserved_repository_anchor("attempt", "planned-code", parsed_plan_id, parsed_commit);
        let graph =
            unobserved_repository_anchor("attempt", "planned-graph", parsed_plan_id, parsed_commit);
        for hydrated in [&code, &graph] {
            assert!(!hydrated.healthy);
            assert!(!hydrated.method_compatible);
            assert_eq!(hydrated.health(), "unusable");
            assert_eq!(hydrated.identity["evidence_plan_id"], plan_id);
            assert_eq!(hydrated.identity["captured_commit_sha"], commit);
        }

        let check_healthy = HashMap::from([
            ("code-check", code.healthy && code.method_compatible),
            ("graph-check", graph.healthy && graph.method_compatible),
        ]);
        assert_eq!(
            derive_outcome(&checks, &check_healthy),
            TribunalEvidenceOutcome::Unresolved,
            "planning context alone cannot make passed code/graph checks positive",
        );
    }
}

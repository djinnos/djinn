//! Proposal-owned authority for typed tribunal evidence lifecycle.
//!
//! APIs take a caller-owned transaction so a coordinator can compose them with
//! proposal mutations. Attempts and transitions are append-only database facts.

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
    pub failures: Vec<TribunalEvidenceReturnIssueV1>,
    #[serde(default)]
    pub gaps: Vec<TribunalEvidenceReturnIssueV1>,
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
pub struct TribunalEvidenceReturnIssueV1 {
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
    pub sequence: i32,
    pub planned_checks: Vec<TribunalEvidencePlannedCheck>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceDispositionProjection {
    pub disposition: TribunalEvidenceDisposition,
    pub finding_lifecycle: TribunalEvidenceLifecycle,
}

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
        for f in &payload.findings {
            if !expected.contains_key(f.check_id.as_str()) {
                return Err(v1("dangling_finding_check"));
            }
            if f.anchors.len() > 16 {
                return Err(v1("too_many_anchors"));
            }
            for a in &f.anchors {
                limit_anchor(a)?;
            }
        }
        for i in payload.failures.iter().chain(&payload.gaps) {
            limit_issue(i)?;
        }
        let validation_id = uuid::Uuid::now_v7().to_string();
        let mut usable = 0;
        let mut resolved = true;
        for c in &payload.checks {
            let p = expected[c.check_id.as_str()];
            let result_id = uuid::Uuid::now_v7().to_string();
            sqlx::query("INSERT INTO typed_evidence_check_results (id,validation_result_id,planned_check_id,status,detail) VALUES ($1,$2,$3,$4,$5)").bind(&result_id).bind(&validation_id).bind(&p.id).bind(&c.status).bind(&c.detail).execute(&mut **tx).await?;
            let invocation_ok=match &c.invocation_id {Some(id)=>sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM evidence_command_invocations WHERE id=$1 AND check_id=$2)").bind(id).bind(&c.check_id).fetch_one(&mut **tx).await?,None=>false};
            let mut healthy = false;
            for a in &c.anchors {
                let h = a.method == "command" && c.method == "command" && invocation_ok;
                let id = uuid::Uuid::now_v7().to_string();
                sqlx::query("INSERT INTO typed_evidence_anchors (id,check_result_id,method,locator) VALUES ($1,$2,$3,$4)").bind(&id).bind(&result_id).bind(&a.method).bind(&a.locator).execute(&mut **tx).await?;
                sqlx::query("INSERT INTO typed_evidence_anchor_health (anchor_id,health,detail) VALUES ($1,$2,$3)").bind(&id).bind(if h{"healthy"}else{"unusable"}).bind(if h{None}else{Some("server hydration missing or incompatible")}).execute(&mut **tx).await?;
                healthy |= h;
            }
            if c.status == "passed" && healthy {
                usable += 1;
            }
            resolved &= c.status == "passed" && healthy;
        }
        let result_outcome = if resolved {
            TribunalEvidenceOutcome::Resolved
        } else if usable > 0 {
            TribunalEvidenceOutcome::Partial
        } else {
            TribunalEvidenceOutcome::Unresolved
        };
        sqlx::query("INSERT INTO typed_evidence_validation_results (id,attempt_id,payload_sha256,outcome,validator_facts) VALUES ($1,$2,$3,$4,$5)").bind(&validation_id).bind(&payload.attempt_id).bind(&hash).bind(outcome(result_outcome)).bind(serde_json::json!({"validator_version":"TribunalEvidenceReturnV1","raw_payload_sha256":hash,"server_hydrated":true})).execute(&mut **tx).await?;
        for (kind, issues) in [("failure", &payload.failures), ("gap", &payload.gaps)] {
            for i in issues {
                sqlx::query("INSERT INTO typed_evidence_issues (id,validation_result_id,kind,code,detail) VALUES ($1,$2,$3,$4,$5)").bind(uuid::Uuid::now_v7().to_string()).bind(&validation_id).bind(kind).bind(&i.code).bind(&i.detail).execute(&mut **tx).await?;
            }
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
        if let Some(row) = sqlx::query("SELECT id,sequence FROM typed_evidence_attempts WHERE finding_id=$1 AND spike_task_id=$2").bind(&input.finding_id).bind(&input.spike_task_id).fetch_optional(&mut **tx).await? { let id: String=row.get("id"); return Ok(TypedEvidenceAttemptAllocation { attempt_id:id.clone(), sequence:row.get("sequence"), planned_checks:checks(tx,&id).await? }); }
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
            sequence,
            planned_checks: checks(tx, &input.attempt_id).await?,
        })
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
        let row=sqlx::query("SELECT proposal_id,created_by_task_id,lifecycle FROM typed_evidence_findings WHERE id=$1 FOR UPDATE").bind(&input.finding_id).fetch_optional(&mut **tx).await?.ok_or_else(|| Error::InvalidData("finding not found".into()))?;
        if row.get::<String, _>("created_by_task_id") != input.judge_task_id {
            return Err(Error::InvalidData("Judge attribution required".into()));
        }
        let committed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM proposal_revisions WHERE proposal_id=$1 AND seq=$2 AND event_kind='spec_revision')").bind(row.get::<String,_>("proposal_id")).bind(input.folding_revision).fetch_one(&mut **tx).await?;
        if !committed {
            return Err(Error::InvalidData(
                "existing committed folding revision required".into(),
            ));
        }
        let state = parse(&row.get::<String, _>("lifecycle"))?;
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
            TribunalEvidenceLifecycle::SpikeActive | TribunalEvidenceLifecycle::Withdrawn
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
fn planned_method(value: TribunalEvidenceAnchorMethod) -> &'static str {
    match value {
        TribunalEvidenceAnchorMethod::Code => "code",
        TribunalEvidenceAnchorMethod::Graph => "graph",
        TribunalEvidenceAnchorMethod::Command => "command",
        _ => unreachable!("planned checks only permit code, graph, or command"),
    }
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
    if bytes(&p.conclusion) > 8192 {
        return Err(v1("conclusion_too_large"));
    }
    if p.checks.len() > 32 || p.findings.len() > 32 || p.gaps.len() > 32 {
        return Err(v1("return_limit_exceeded"));
    }
    Ok(())
}
fn limit_check(c: &TribunalEvidenceReturnCheckV1) -> Result<()> {
    if bytes(&c.check_id) > 2048 || bytes(&c.method) > 2048 || c.anchors.len() > 16 {
        return Err(v1("check_limit_exceeded"));
    }
    if !matches!(c.status.as_str(), "passed" | "failed" | "not_run") {
        return Err(v1("invalid_check_status"));
    }
    if c.status != "passed" && c.detail.as_deref().is_none_or(str::is_empty) {
        return Err(v1("status_detail_required"));
    }
    if c.status == "passed" && c.invocation_id.is_some() && c.method != "command" {
        return Err(v1("invocation_method_mismatch"));
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
fn limit_issue(i: &TribunalEvidenceReturnIssueV1) -> Result<()> {
    if i.code.trim().is_empty()
        || i.detail.trim().is_empty()
        || bytes(&i.code) > 2048
        || bytes(&i.detail) > 8192
    {
        Err(v1("invalid_issue"))
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
}

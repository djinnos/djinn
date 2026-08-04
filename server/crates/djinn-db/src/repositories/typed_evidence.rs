//! Repository-facing typed tribunal evidence contracts.
//!
//! Lifecycle authority is intentionally implemented by the proposal repository
//! layer. This focused module supplies its transaction-safe request/response
//! vocabulary without duplicating migration 156's evidence plan or invocation
//! persistence APIs.

use djinn_core::models::{
    TribunalEvidenceAnchorMethod, TribunalEvidenceDisposition, TribunalEvidenceFinding,
    TribunalEvidenceLifecycle, TribunalEvidenceOutcome, TribunalEvidencePlannedCheck,
};

/// Input for atomically recording a Judge demand and its first transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemandTypedEvidenceInput {
    pub finding_id: String,
    pub proposal_id: String,
    pub demand_hash: String,
    pub claim: serde_json::Value,
    pub demanded_revision_seq: i32,
    pub judge_task_id: String,
}

/// Input for allocating one ordered spike attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocateTypedEvidenceAttemptInput {
    pub attempt_id: String,
    pub finding_id: String,
    pub spike_task_id: String,
    /// Existing frozen plan from `EvidenceRepository`; no duplicated plan data.
    pub evidence_plan_id: Option<String>,
    pub planned_checks: Vec<PlannedTypedEvidenceCheckInput>,
}

/// Expected check supplied with an attempt allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedTypedEvidenceCheckInput {
    pub id: String,
    pub ordinal: i32,
    pub check_id: String,
    pub method: TribunalEvidenceAnchorMethod,
    /// Existing `evidence_plan_checks` identity from migration 156 when present.
    pub evidence_plan_id: Option<String>,
    pub evidence_plan_check_id: Option<String>,
}

/// Input for appending a lifecycle transition. Callers must use a transaction.
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

/// Input for persisting the terminal validation projection of an attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistTypedEvidenceValidationInput {
    pub validation_id: String,
    pub attempt_id: String,
    pub payload_sha256: String,
    pub outcome: TribunalEvidenceOutcome,
    pub validator_facts: serde_json::Value,
}

/// Input for a replay-safe retry allocation keyed by the failed transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocateTypedEvidenceRetryInput {
    pub finding_id: String,
    pub failed_transition_id: String,
    pub retry_attempt_id: String,
}

/// Projection returned after a demand mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceFindingProjection {
    pub finding: TribunalEvidenceFinding,
    pub active_attempt_id: Option<String>,
}

/// Projection returned after an attempt allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceAttemptAllocation {
    pub attempt_id: String,
    pub sequence: i32,
    pub planned_checks: Vec<TribunalEvidencePlannedCheck>,
}

/// Folded terminal result intended for Judge-authored resolution or withdrawal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedEvidenceDispositionProjection {
    pub disposition: TribunalEvidenceDisposition,
    pub finding_lifecycle: TribunalEvidenceLifecycle,
}

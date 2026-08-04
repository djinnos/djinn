//! Typed durable records for the tribunal evidence lifecycle.
//!
//! These records are additive to the legacy proposal parking columns. They do
//! not drive a coordinator or control-plane transition by themselves.

use serde::{Deserialize, Serialize};

/// Durable lifecycle vocabulary for a tribunal evidence finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TribunalEvidenceLifecycle {
    Demanded,
    SpikeActive,
    EvidenceReceived,
    Failed,
    Resolved,
    Withdrawn,
}

impl TribunalEvidenceLifecycle {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Withdrawn)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Demanded => "demanded",
            Self::SpikeActive => "spike_active",
            Self::EvidenceReceived => "evidence_received",
            Self::Failed => "failed",
            Self::Resolved => "resolved",
            Self::Withdrawn => "withdrawn",
        }
    }
}

/// Deterministic assessment of returned evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TribunalEvidenceOutcome {
    Resolved,
    Partial,
    Unresolved,
}

/// Status reported for an expected planned check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TribunalEvidenceCheckStatus {
    Passed,
    Failed,
    NotRun,
}

/// The server-compatible method used by a normalized result anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TribunalEvidenceAnchorMethod {
    Code,
    Graph,
    Command,
    Artifact,
    Memory,
    External,
    Repository,
}

/// Health derived by the server after dereferencing an anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TribunalEvidenceAnchorHealthStatus {
    Healthy,
    Unusable,
    Unavailable,
}

/// A normalized Judge demand, retained after it becomes terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceFinding {
    pub id: String,
    pub proposal_id: String,
    pub demand_hash: String,
    pub lifecycle: TribunalEvidenceLifecycle,
    pub claim: serde_json::Value,
    pub demanded_revision_seq: i32,
    pub created_by_task_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One ordered spike attempt for a finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceAttempt {
    pub id: String,
    pub finding_id: String,
    pub sequence: i32,
    pub spike_task_id: String,
    /// Frozen plan from migration 156; no plan or invocation data is copied here.
    pub evidence_plan_id: Option<String>,
    pub created_at: String,
}

/// An append-only lifecycle fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceTransition {
    pub id: String,
    pub finding_id: String,
    pub ordinal: i32,
    pub from_lifecycle: Option<TribunalEvidenceLifecycle>,
    pub to_lifecycle: TribunalEvidenceLifecycle,
    pub actor_task_id: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// A check expected from a frozen plan for one attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidencePlannedCheck {
    pub id: String,
    pub attempt_id: String,
    pub ordinal: i32,
    pub check_id: String,
    pub method: TribunalEvidenceAnchorMethod,
    /// References `evidence_plan_checks(plan_id, check_id)` from migration 156.
    pub evidence_plan_id: Option<String>,
    pub evidence_plan_check_id: Option<String>,
}

/// Terminal validated result for an attempt, with the hash of the submitted payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceValidationResult {
    pub id: String,
    pub attempt_id: String,
    pub payload_sha256: String,
    pub outcome: TribunalEvidenceOutcome,
    pub validator_facts: serde_json::Value,
    pub validated_at: String,
}

/// Normalized result for one planned check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceCheckResult {
    pub id: String,
    pub validation_result_id: String,
    pub planned_check_id: String,
    pub status: TribunalEvidenceCheckStatus,
    pub detail: Option<String>,
}

/// A result anchor and its server-derived health fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceAnchor {
    pub id: String,
    pub check_result_id: String,
    pub method: TribunalEvidenceAnchorMethod,
    pub locator: String,
    pub health: TribunalEvidenceAnchorHealthStatus,
    pub health_detail: Option<String>,
    pub observed_at: String,
}

/// A normalized evidence failure or gap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceIssue {
    pub id: String,
    pub validation_result_id: String,
    pub kind: TribunalEvidenceIssueKind,
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TribunalEvidenceIssueKind {
    Failure,
    Gap,
}

/// Folded evidence assessment and the Judge's final disposition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceDisposition {
    pub id: String,
    pub finding_id: String,
    pub validation_result_id: Option<String>,
    pub folding_revision: i32,
    pub outcome: TribunalEvidenceOutcome,
    pub disposition: TribunalEvidenceLifecycle,
    pub judge_task_id: String,
    pub rationale: String,
    pub created_at: String,
}

/// Replay-safe retry binding for a failed transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TribunalEvidenceRetryIdempotency {
    pub finding_id: String,
    pub failed_transition_id: String,
    pub retry_attempt_id: String,
    pub created_at: String,
}

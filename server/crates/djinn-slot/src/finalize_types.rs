//! Finalize tool payload types.
//!
//! Extracted from `djinn-agent::roles::finalize` so the slot crate can parse
//! finalize tool payloads without depending on djinn-agent.

use djinn_control_plane::tools::evidence_findings::EvidenceCompletionV1;
use serde::Deserialize;

/// Per-criterion verdict from a reviewer's `submit_review` call.
#[derive(Debug, Deserialize)]
pub struct AcVerdict {
    #[serde(default)]
    pub criterion: String,
    pub met: bool,
}

/// Entry from a planner's `submit_grooming` call.
#[derive(Debug, Deserialize)]
pub struct TaskGroomingEntry {
    pub task_id: String,
    pub action: String,
    pub changes: Option<String>,
}

/// Payload for a worker submitting completed work.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitWork {
    pub task_id: String,
    pub commit_title: String,
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub remaining_concerns: Vec<String>,
    /// Available only to the exact linked refinement-evidence spike.
    pub evidence_completion: Option<EvidenceCompletionV1>,
    /// Raw canonical `TribunalEvidenceReturnV1` delivery. It deliberately
    /// remains JSON here: malformed returns must reach the typed repository so
    /// it can durably record their failed validation outcome.
    pub tribunal_evidence_return_v1: Option<serde_json::Value>,
}

/// Payload for a reviewer submitting their review outcome.
#[derive(Debug, Deserialize)]
pub struct SubmitReview {
    pub task_id: String,
    pub verdict: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<AcVerdict>,
    pub feedback: Option<String>,
}

/// Payload for a Lead/arbiter submitting an intervention decision.
#[derive(Debug, Deserialize)]
pub struct SubmitDecision {
    pub task_id: String,
    pub decision: String,
    pub rationale: Option<String>,
    /// Evidence citation — required for `approve` and `approve_conflict`.
    pub evidence: Option<serde_json::Value>,
    /// Park dossier — required for `park`.
    pub park_dossier: Option<serde_json::Value>,
    /// Directive — required for `reopen`.
    pub directive: Option<String>,
    /// Verification command — required for a repair `reopen`, forbidden for a
    /// diagnostic one.
    pub verification_command: Option<String>,
    /// Closed diagnostic reason — required for a diagnostic `reopen` on a CI
    /// route, forbidden for a repair (proposal `nafu`). Mirrors
    /// `djinn_db::CiDiagnosticReason`; kept a `String` here because
    /// `djinn-slot` must not depend on `djinn-db`, and the supervisor
    /// validator is what parses it into the closed set.
    pub diagnostic_reason: Option<String>,
    /// Models excluded from next dispatch — optional for `reopen`.
    #[serde(default)]
    pub exclude_models: Vec<String>,
    #[serde(default)]
    pub created_tasks: Vec<String>,
}

/// Payload for a Planner submitting planning results.
#[derive(Debug, Deserialize)]
pub struct SubmitGrooming {
    #[serde(default)]
    pub tasks_reviewed: Vec<TaskGroomingEntry>,
    pub summary: Option<String>,
    pub decision: Option<String>,
    /// Epics (UUID or short_id) that block this planning epic. When the planner
    /// concludes "blocked on epic X, no tasks created", listing X here durably
    /// records the epic-blocker edge so the coordinator parks this epic's
    /// planning until X closes — instead of re-deriving "blocked" via a fresh
    /// LLM session on every stale-sweep. Idempotent; ignored when empty.
    #[serde(default)]
    pub blocked_on: Vec<String>,
}

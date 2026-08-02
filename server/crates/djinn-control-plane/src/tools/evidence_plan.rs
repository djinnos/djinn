//! Frozen evidence-plan capture and exact terminal-result reconciliation.
//!
//! This is deliberately a control-plane contract, not an executor or a
//! lifecycle hand-off. Callers provide only the ordered investigation checks;
//! the task/session/provenance identity is supplied by the server context.

use std::collections::HashSet;

use djinn_core::{
    events::EventBus,
    models::{EvidencePlan, EvidencePlanHydration},
};
use djinn_db::{
    EvidenceRepository, InsertEvidencePlan, InsertEvidencePlanCheck, SessionRepository,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Server-derived identity frozen with an evidence plan. This type is produced
/// by the control plane after it has authenticated the active task session and
/// captured the worktree state; it is never deserialized from caller input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidencePlanIdentity {
    pub spike_task_id: String,
    pub session_id: String,
    pub captured_commit_sha: String,
    pub worktree_fingerprint: String,
}

/// Caller-owned portion of plan capture. No identity or provenance fields are
/// accepted from the caller.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidencePlanCapture {
    pub checks: Vec<EvidencePlanCheckInput>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidencePlanCheckInput {
    pub check_id: String,
    pub question: String,
    pub method: EvidenceMethod,
}

/// The complete, closed set of investigation methods.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceMethod {
    Code,
    Graph,
    Command,
}

impl EvidenceMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Graph => "graph",
            Self::Command => "command",
        }
    }
}

/// One claimed terminal result submitted by a later execution/completion
/// operation. Findings remain out of scope; reconciliation only freezes the
/// exact method-compatible coverage shape.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceTerminalResult {
    pub check_id: String,
    pub method: EvidenceMethod,
    pub terminal: bool,
}

/// Stable validation failures suitable for callers to map to their own error
/// envelopes without mutating a plan or creating a hand-off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidencePlanError {
    InvalidPlan(&'static str),
    NoFrozenPlan,
    EmptyResults,
    NonTerminalResult {
        check_id: String,
    },
    DuplicateResult {
        check_id: String,
    },
    UnknownResult {
        check_id: String,
    },
    MethodMismatch {
        check_id: String,
        expected: String,
        actual: String,
    },
    OmittedResult {
        check_id: String,
    },
    Persistence(String),
}

impl std::fmt::Display for EvidencePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan(reason) => write!(f, "invalid evidence plan: {reason}"),
            Self::NoFrozenPlan => write!(f, "a frozen evidence plan is required"),
            Self::EmptyResults => write!(f, "terminal evidence results must not be empty"),
            Self::NonTerminalResult { check_id } => {
                write!(f, "result for '{check_id}' is not terminal")
            }
            Self::DuplicateResult { check_id } => {
                write!(f, "multiple terminal results for '{check_id}'")
            }
            Self::UnknownResult { check_id } => {
                write!(f, "result references unknown check '{check_id}'")
            }
            Self::MethodMismatch {
                check_id,
                expected,
                actual,
            } => write!(
                f,
                "result for '{check_id}' uses method '{actual}', expected '{expected}'"
            ),
            Self::OmittedResult { check_id } => {
                write!(f, "missing terminal result for '{check_id}'")
            }
            Self::Persistence(error) => write!(f, "evidence persistence failed: {error}"),
        }
    }
}

impl std::error::Error for EvidencePlanError {}

/// Capture exactly one validated plan for the supplied server-owned identity.
/// The durable repository unique constraint makes concurrent second captures
/// fail rather than replace the original frozen plan.
pub async fn capture_evidence_plan(
    repository: &EvidenceRepository,
    identity: EvidencePlanIdentity,
    capture: EvidencePlanCapture,
) -> Result<String, EvidencePlanError> {
    validate_capture(&identity, &capture)?;
    validate_session_binding(repository, &identity).await?;
    let plan = repository
        .insert_plan(InsertEvidencePlan {
            id: uuid::Uuid::now_v7().to_string(),
            spike_task_id: identity.spike_task_id,
            session_id: identity.session_id,
            captured_commit_sha: identity.captured_commit_sha,
            worktree_fingerprint: identity.worktree_fingerprint,
            checks: capture
                .checks
                .into_iter()
                .map(|check| InsertEvidencePlanCheck {
                    check_id: check.check_id.trim().to_owned(),
                    question: check.question.trim().to_owned(),
                    method: check.method.as_str().to_owned(),
                })
                .collect(),
        })
        .await
        .map_err(|error| EvidencePlanError::Persistence(error.to_string()))?;
    djinn_telemetry::evidence_metrics::record(
        djinn_telemetry::evidence_metrics::EvidenceStage::Plan,
        djinn_telemetry::evidence_metrics::EvidenceOutcome::Captured,
    );
    Ok(plan.id)
}

/// The repository has independent task and session foreign keys, so verify the
/// server-authenticated session is actually the task's session before freezing
/// their combined identity.
async fn validate_session_binding(
    repository: &EvidenceRepository,
    identity: &EvidencePlanIdentity,
) -> Result<(), EvidencePlanError> {
    let sessions = SessionRepository::new(repository.db().clone(), EventBus::noop());
    let session = sessions
        .get(&identity.session_id)
        .await
        .map_err(|error| EvidencePlanError::Persistence(error.to_string()))?;
    if session.and_then(|session| session.task_id).as_deref() != Some(&identity.spike_task_id) {
        return Err(EvidencePlanError::InvalidPlan(
            "authenticated session is not bound to the evidence spike task",
        ));
    }
    Ok(())
}

/// Load the frozen plan for an action identity. Execution and completion code
/// must call this before accepting any work; an identity mismatch is treated as
/// no plan rather than allowing a plan from another session to leak across.
pub async fn require_frozen_plan(
    repository: &EvidenceRepository,
    identity: &EvidencePlanIdentity,
) -> Result<EvidencePlanHydration, EvidencePlanError> {
    repository
        .hydrate_by_identity(&identity.spike_task_id, &identity.session_id)
        .await
        .map_err(|error| EvidencePlanError::Persistence(error.to_string()))?
        .filter(|hydration| {
            hydration.plan.captured_commit_sha == identity.captured_commit_sha
                && hydration.plan.worktree_fingerprint == identity.worktree_fingerprint
        })
        .ok_or(EvidencePlanError::NoFrozenPlan)
}

/// Reconcile the terminal result set against the frozen ordered plan.
///
/// This is intentionally pure: a failure cannot alter the plan or write a
/// finalized projection. Later lifecycle code can call it inside its own
/// transaction immediately before its atomic hand-off insert.
pub fn reconcile_terminal_results(
    plan: &EvidencePlan,
    results: &[EvidenceTerminalResult],
) -> Result<(), EvidencePlanError> {
    if results.is_empty() {
        return Err(EvidencePlanError::EmptyResults);
    }
    let planned_ids: HashSet<&str> = plan
        .checks
        .iter()
        .map(|check| check.check_id.as_str())
        .collect();
    let mut seen = HashSet::with_capacity(results.len());
    for result in results {
        let check_id = result.check_id.trim();
        if check_id.is_empty() || !planned_ids.contains(check_id) {
            return Err(EvidencePlanError::UnknownResult {
                check_id: check_id.to_owned(),
            });
        }
        if !result.terminal {
            return Err(EvidencePlanError::NonTerminalResult {
                check_id: check_id.to_owned(),
            });
        }
        if !seen.insert(check_id) {
            return Err(EvidencePlanError::DuplicateResult {
                check_id: check_id.to_owned(),
            });
        }
        let planned = plan
            .checks
            .iter()
            .find(|check| check.check_id == check_id)
            .expect("checked above");
        let actual = result.method.as_str();
        if planned.method != actual {
            return Err(EvidencePlanError::MethodMismatch {
                check_id: check_id.to_owned(),
                expected: planned.method.clone(),
                actual: actual.to_owned(),
            });
        }
    }
    for check in &plan.checks {
        if !seen.contains(check.check_id.as_str()) {
            return Err(EvidencePlanError::OmittedResult {
                check_id: check.check_id.clone(),
            });
        }
    }
    Ok(())
}

fn validate_capture(
    identity: &EvidencePlanIdentity,
    capture: &EvidencePlanCapture,
) -> Result<(), EvidencePlanError> {
    if [
        identity.spike_task_id.as_str(),
        identity.session_id.as_str(),
        identity.captured_commit_sha.as_str(),
        identity.worktree_fingerprint.as_str(),
    ]
    .iter()
    .any(|value| value.trim().is_empty())
    {
        return Err(EvidencePlanError::InvalidPlan(
            "server identity fields must be nonempty",
        ));
    }
    if capture.checks.is_empty() {
        return Err(EvidencePlanError::InvalidPlan(
            "at least one check is required",
        ));
    }
    let mut ids = HashSet::new();
    let mut questions = HashSet::new();
    for check in &capture.checks {
        let id = check.check_id.trim();
        let question = check.question.trim();
        if id.is_empty() || question.is_empty() {
            return Err(EvidencePlanError::InvalidPlan(
                "check ids and questions must be nonempty",
            ));
        }
        if !ids.insert(id) || !questions.insert(question) {
            return Err(EvidencePlanError::InvalidPlan(
                "check ids and questions must be unique",
            ));
        }
    }
    Ok(())
}

//! Immutable command provenance for frozen evidence-plan command checks.
//!
//! The executor supplies facts observed by server execution code. Completion
//! callers receive only an opaque invocation selector and hydrate all outcome
//! and transcript fields from the immutable ledger.

use djinn_core::models::EvidenceCommandInvocation;
use djinn_db::{AppendEvidenceInvocation, EvidenceRepository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::evidence_plan::{EvidencePlanError, EvidencePlanIdentity, require_frozen_plan};

/// Trusted server execution facts. This is intentionally not deserializable,
/// so callers cannot submit process, outcome, or transcript claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommandObservation {
    pub argv: Vec<String>,
    pub canonical_cwd: String,
    pub launch_state: String,
    pub process_state: String,
    pub launched_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub runner_failure: Option<String>,
    pub elapsed_millis: Option<i64>,
    pub timeout_millis: Option<i64>,
    pub timed_out: bool,
    pub stdout_digest: Option<String>,
    pub stdout_excerpt: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_digest: Option<String>,
    pub stderr_excerpt: Option<String>,
    pub stderr_truncated: bool,
}

/// The sole command-related completion input. It cannot override provenance.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCommandInvocationSelection {
    pub invocation_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCommandHealth {
    Timeout,
    Error,
    Broken,
    Ok,
    Degraded,
}

/// Complete immutable provenance returned to finalizers and projection code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedCommandProvenance {
    pub invocation: EvidenceCommandInvocation,
    pub health: EvidenceCommandHealth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceCommandError {
    Plan(EvidencePlanError),
    UnknownCheck {
        check_id: String,
    },
    UnknownInvocation {
        invocation_id: String,
    },
    InvocationCheckMismatch {
        expected_check_id: String,
        actual_check_id: String,
    },
    MethodMismatch {
        check_id: String,
        actual: String,
    },
    Persistence(String),
}

impl std::fmt::Display for EvidenceCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(error) => error.fmt(f),
            Self::UnknownCheck { check_id } => write!(f, "unknown evidence check '{check_id}'"),
            Self::UnknownInvocation { invocation_id } => {
                write!(f, "unknown command invocation '{invocation_id}'")
            }
            Self::InvocationCheckMismatch {
                expected_check_id,
                actual_check_id,
            } => write!(
                f,
                "command invocation belongs to '{actual_check_id}', not expected check '{expected_check_id}'"
            ),
            Self::MethodMismatch { check_id, actual } => write!(
                f,
                "command invocation for '{check_id}' requires method 'command', found '{actual}'"
            ),
            Self::Persistence(error) => write!(f, "evidence command persistence failed: {error}"),
        }
    }
}
impl std::error::Error for EvidenceCommandError {}

/// Append one distinct server-owned event for an existing command check.
/// UUIDv7 is allocated here, so retries can never overwrite prior events.
pub async fn record_command_observation(
    repository: &EvidenceRepository,
    identity: &EvidencePlanIdentity,
    check_id: &str,
    observation: ServerCommandObservation,
) -> Result<EvidenceCommandInvocation, EvidenceCommandError> {
    let hydration = require_frozen_plan(repository, identity)
        .await
        .map_err(EvidenceCommandError::Plan)?;
    let check_id = check_id.trim();
    let check = hydration
        .plan
        .checks
        .iter()
        .find(|check| check.check_id == check_id)
        .ok_or_else(|| EvidenceCommandError::UnknownCheck {
            check_id: check_id.to_owned(),
        })?;
    if check.method != "command" {
        return Err(EvidenceCommandError::MethodMismatch {
            check_id: check_id.to_owned(),
            actual: check.method.clone(),
        });
    }
    repository
        .append_invocation(AppendEvidenceInvocation {
            id: uuid::Uuid::now_v7().to_string(),
            plan_id: hydration.plan.id,
            spike_task_id: identity.spike_task_id.clone(),
            session_id: identity.session_id.clone(),
            captured_commit_sha: identity.captured_commit_sha.clone(),
            worktree_fingerprint: identity.worktree_fingerprint.clone(),
            check_id: check_id.to_owned(),
            argv: observation.argv,
            canonical_cwd: observation.canonical_cwd,
            launch_state: observation.launch_state,
            process_state: observation.process_state,
            launched_at: observation.launched_at,
            finished_at: observation.finished_at,
            exit_code: observation.exit_code,
            signal: observation.signal,
            runner_failure: observation.runner_failure,
            elapsed_millis: observation.elapsed_millis,
            timeout_millis: observation.timeout_millis,
            timed_out: observation.timed_out,
            stdout_digest: observation.stdout_digest,
            stdout_excerpt: observation.stdout_excerpt,
            stdout_truncated: observation.stdout_truncated,
            stderr_digest: observation.stderr_digest,
            stderr_excerpt: observation.stderr_excerpt,
            stderr_truncated: observation.stderr_truncated,
        })
        .await
        .map_err(|error| EvidenceCommandError::Persistence(error.to_string()))
}

/// Hydrate every immutable event associated with this exact frozen identity.
pub async fn hydrate_command_provenance(
    repository: &EvidenceRepository,
    identity: &EvidencePlanIdentity,
) -> Result<Vec<HydratedCommandProvenance>, EvidenceCommandError> {
    let hydration = require_frozen_plan(repository, identity)
        .await
        .map_err(EvidenceCommandError::Plan)?;
    Ok(hydration
        .invocations
        .into_iter()
        .map(|invocation| HydratedCommandProvenance {
            health: derive_command_health(&invocation),
            invocation,
        })
        .collect())
}

/// Resolve a completion selector through the immutable ledger for one
/// server-supplied command check. An invented or cross-check id is rejected,
/// and callers cannot replace any hydrated provenance field.
pub async fn hydrate_selected_command_provenance(
    repository: &EvidenceRepository,
    identity: &EvidencePlanIdentity,
    expected_check_id: &str,
    selection: &EvidenceCommandInvocationSelection,
) -> Result<HydratedCommandProvenance, EvidenceCommandError> {
    let expected_check_id = expected_check_id.trim();
    let invocation_id = selection.invocation_id.trim();
    let provenance = hydrate_command_provenance(repository, identity)
        .await?
        .into_iter()
        .find(|provenance| provenance.invocation.id == invocation_id)
        .ok_or_else(|| EvidenceCommandError::UnknownInvocation {
            invocation_id: invocation_id.to_owned(),
        })?;
    if provenance.invocation.check_id != expected_check_id {
        return Err(EvidenceCommandError::InvocationCheckMismatch {
            expected_check_id: expected_check_id.to_owned(),
            actual_check_id: provenance.invocation.check_id,
        });
    }
    Ok(provenance)
}

/// Strict precedence: timeout, runner/sandbox failure, broken process, zero,
/// then nonzero exit. Thus an exit code can never hide a timeout or failure.
pub fn derive_command_health(invocation: &EvidenceCommandInvocation) -> EvidenceCommandHealth {
    if invocation.timed_out || invocation.process_state == "timed_out" {
        EvidenceCommandHealth::Timeout
    } else if invocation.runner_failure.is_some()
        || invocation.launch_state == "failed_to_launch"
        || invocation.process_state == "runner_failed"
    {
        EvidenceCommandHealth::Error
    } else if invocation.signal.is_some() || invocation.process_state == "signaled" {
        EvidenceCommandHealth::Broken
    } else {
        match invocation.exit_code {
            Some(0) => EvidenceCommandHealth::Ok,
            Some(_) => EvidenceCommandHealth::Degraded,
            None => EvidenceCommandHealth::Broken,
        }
    }
}

//! Agent-facing `run_verification` client surface (epic hexh).
//!
//! The tool is a pure client of `coordinate_final_verification` — the same
//! authoritative consult-or-run entry the completion-intent path uses. It never
//! opens a second attempt, never writes a passing row, and never clones the
//! recording logic. The types below are: a per-session pre-lease gate (rate
//! limiting), typed per-check results, and a typed tool outcome. The shared
//! consult-or-run core stays in [`crate::final_verification`]; this module only
//! adds the agent-facing outcome, telemetry, and gate plumbing on top of it.

use djinn_core::canonical_verify::ResolvedVerificationSelectionV1;
use djinn_sandbox::final_verification_execution::{
    FinalVerificationCommandEvidence, FinalVerificationIneligibilityReason,
};

use crate::final_verification::{
    FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
    FinalVerificationSuccessEvidence, coordinate_final_verification_core,
};
use crate::host::SlotContext;

/// One per-check result surfaced to the agent. IDs stay bounded; full command
/// output is never carried here (it remains in structured audit/log storage).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AgentVerificationCheckResult {
    pub check_id: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_millis: u64,
}

/// A typed rate-limit denial. It is produced BEFORE any lease acquisition and
/// never consumes a lease or executes a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalVerificationRateLimited {
    /// Bounded scope label, e.g. `"concurrent"` or `"hourly"`.
    pub scope: String,
    pub detail: String,
    pub retry_after_seconds: Option<u64>,
}

/// A held run permit. The gate returns it immediately before lease acquisition;
/// it is held for the whole run and released (via `Drop`) when the coordinator
/// returns. Dropping it decrements the per-session concurrency count.
pub struct FinalVerificationRunPermit {
    _guard: Box<dyn Send>,
}

impl FinalVerificationRunPermit {
    pub fn new(guard: Box<dyn Send>) -> Self {
        Self { _guard: guard }
    }
}

impl std::fmt::Debug for FinalVerificationRunPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FinalVerificationRunPermit")
    }
}

/// Per-session admission gate consulted exactly once, after a consult miss and
/// immediately before invocation-lease acquisition. A fresh consult hit never
/// reaches this gate (hits are free and never counted against the budget).
pub trait FinalVerificationPreLeaseGate: Send {
    /// Reserve a run slot. `Ok(permit)` allows lease acquisition and execution;
    /// `Err(_)` is a typed rate-limit denial that acquires no lease and runs no
    /// command.
    fn acquire(&mut self) -> Result<FinalVerificationRunPermit, FinalVerificationRateLimited>;
}

/// Typed outcome of one agent-facing `run_verification` tool call. Exactly one
/// bounded telemetry outcome is emitted per call by the coordinator client.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum AgentRunVerificationOutcome {
    /// Fresh consult hit: stored evidence returned with no lease and no run.
    Hit {
        evidence: Box<FinalVerificationSuccessEvidence>,
        checks: Vec<AgentVerificationCheckResult>,
    },
    /// A run executed, every required check passed, and a durable row was
    /// recorded by the single writer.
    RanPass {
        evidence: Box<FinalVerificationSuccessEvidence>,
        checks: Vec<AgentVerificationCheckResult>,
    },
    /// A run executed and at least one required check failed. No passing row was
    /// written. Per-check failure results are returned.
    RanFail {
        checks: Vec<AgentVerificationCheckResult>,
        reason: String,
    },
    /// An infrastructure outcome distinct from a real check failure (resolution,
    /// lease, hermeticity/consistency boundary, sandbox, or persistence error).
    Error { detail: String },
    /// A typed per-session rate-limit outcome. No lease was acquired.
    RateLimited {
        scope: String,
        detail: String,
        retry_after_seconds: Option<u64>,
    },
    /// The project declares no final-verification plan. Nothing was leased,
    /// executed, or recorded.
    NotConfigured,
}

/// Internal capture of run-scoped facts the agent client needs but that the
/// authoritative recording outcome does not carry.
#[derive(Default)]
pub(crate) struct AgentRunCapture {
    /// `"full"` or `"subset"`, computed once material resolves.
    pub(crate) selection: Option<&'static str>,
    /// Per-check results from the single execution (empty on a reuse hit).
    pub(crate) checks: Vec<AgentVerificationCheckResult>,
    /// Structured ineligibility reason from the single execution, used to
    /// distinguish a real check failure from an infrastructure outcome.
    pub(crate) ineligibility: Option<FinalVerificationIneligibilityReason>,
}

/// Result of the shared consult-or-run core. `RateLimited` is only reachable
/// when a gate is supplied (the completion-intent path passes none).
pub(crate) enum CoordinateResult {
    Outcome(FinalVerificationRecordingOutcome),
    RateLimited(FinalVerificationRateLimited),
}

/// Classify the `"full"`/`"subset"` command-group selection surface for one run.
pub(crate) fn selection_surface(selection: &ResolvedVerificationSelectionV1) -> &'static str {
    match selection {
        ResolvedVerificationSelectionV1::LegacyFlatPlan => "full",
        ResolvedVerificationSelectionV1::SelectedGroups {
            selection_rules,
            selected_command_groups,
        } => {
            let all: std::collections::BTreeSet<&str> = selection_rules
                .iter()
                .flat_map(|rule| rule.command_groups.iter().map(String::as_str))
                .collect();
            let selected: std::collections::BTreeSet<&str> =
                selected_command_groups.iter().map(String::as_str).collect();
            if !all.is_empty() && selected == all {
                "full"
            } else {
                "subset"
            }
        }
    }
}

pub(crate) fn check_from_command(
    command: &FinalVerificationCommandEvidence,
) -> AgentVerificationCheckResult {
    AgentVerificationCheckResult {
        check_id: command.descriptor.check_id.clone(),
        passed: command.exit_code == Some(0) && !command.timed_out,
        exit_code: command.exit_code,
        timed_out: command.timed_out,
        duration_millis: command
            .completed_at_unix_millis
            .saturating_sub(command.started_at_unix_millis) as u64,
    }
}

/// Reconstruct per-check pass results from reusable success evidence, which
/// carries no live command evidence. Prefers the persisted `ordered_commands`
/// shape and falls back to `covered_checks`.
fn checks_from_success_evidence(
    evidence: &FinalVerificationSuccessEvidence,
) -> Vec<AgentVerificationCheckResult> {
    if let Some(commands) = evidence.ordered_commands.as_array()
        && !commands.is_empty()
    {
        let checks: Vec<AgentVerificationCheckResult> = commands
            .iter()
            .filter_map(|command| {
                let check_id = command.get("descriptor_id")?.as_str()?.to_owned();
                let started = command
                    .get("started_at_unix_millis")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let completed = command
                    .get("completed_at_unix_millis")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                Some(AgentVerificationCheckResult {
                    check_id,
                    passed: command
                        .get("passed")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true),
                    exit_code: Some(0),
                    timed_out: false,
                    duration_millis: completed.saturating_sub(started),
                })
            })
            .collect();
        if !checks.is_empty() {
            return checks;
        }
    }
    evidence
        .required_checks
        .iter()
        .map(|check_id| AgentVerificationCheckResult {
            check_id: check_id.clone(),
            passed: true,
            exit_code: Some(0),
            timed_out: false,
            duration_millis: 0,
        })
        .collect()
}

fn is_check_level_failure(reason: &Option<FinalVerificationIneligibilityReason>) -> bool {
    matches!(
        reason,
        Some(
            FinalVerificationIneligibilityReason::CommandFailed { .. }
                | FinalVerificationIneligibilityReason::CommandTimedOut { .. }
                | FinalVerificationIneligibilityReason::RequiredChecksNotCovered { .. }
        )
    )
}

/// Run the authoritative coordinator as an agent tool client. Consults first;
/// on a fresh hit returns `Hit` with no lease. On a miss it checks the supplied
/// per-session gate BEFORE lease acquisition — a denial returns `RateLimited`
/// with no lease — then routes through the one lease, one execution, F0=F1 and
/// coverage eligibility, and the single durable-pass writer. Exactly one bounded
/// tool-attempt telemetry outcome is emitted per call.
pub async fn coordinate_final_verification_for_agent(
    request: FinalVerificationCoordinatorRequest,
    ctx: &SlotContext,
    gate: &mut dyn FinalVerificationPreLeaseGate,
) -> AgentRunVerificationOutcome {
    let task_id = request.task_id.clone();
    let task_run_id = request.task_run_id.clone();
    let mut capture = AgentRunCapture::default();
    let recording =
        match coordinate_final_verification_core(request, ctx, Some(gate), &mut capture).await {
            CoordinateResult::RateLimited(rate_limited) => {
                let outcome = AgentRunVerificationOutcome::RateLimited {
                    scope: rate_limited.scope,
                    detail: rate_limited.detail,
                    retry_after_seconds: rate_limited.retry_after_seconds,
                };
                emit_tool_attempt(&outcome, &capture, &task_id, &task_run_id);
                return outcome;
            }
            CoordinateResult::Outcome(recording) => recording,
        };
    let outcome = match recording {
        FinalVerificationRecordingOutcome::Reused { evidence, .. } => {
            let checks = checks_from_success_evidence(&evidence);
            AgentRunVerificationOutcome::Hit { evidence, checks }
        }
        FinalVerificationRecordingOutcome::Stored { evidence, .. } => {
            let checks = if capture.checks.is_empty() {
                checks_from_success_evidence(&evidence)
            } else {
                capture.checks.clone()
            };
            AgentRunVerificationOutcome::RanPass { evidence, checks }
        }
        FinalVerificationRecordingOutcome::Ineligible { reason, .. } => {
            if is_check_level_failure(&capture.ineligibility) {
                let mut checks = capture.checks.clone();
                if let Some(FinalVerificationIneligibilityReason::RequiredChecksNotCovered {
                    missing,
                }) = &capture.ineligibility
                {
                    for check_id in missing {
                        checks.push(AgentVerificationCheckResult {
                            check_id: check_id.clone(),
                            passed: false,
                            exit_code: None,
                            timed_out: false,
                            duration_millis: 0,
                        });
                    }
                }
                AgentRunVerificationOutcome::RanFail { checks, reason }
            } else {
                AgentRunVerificationOutcome::Error { detail: reason }
            }
        }
        FinalVerificationRecordingOutcome::Error { detail, .. } => {
            AgentRunVerificationOutcome::Error { detail }
        }
        // A service-provisioning failure is an infrastructure outcome distinct
        // from a real check failure, so it maps to the tool's `Error` result
        // (bucketed with other infrastructure errors) with a bounded detail that
        // carries no URL, credential, tenant name, or raw backend error.
        FinalVerificationRecordingOutcome::InfrastructureIneligible { phase, code, .. } => {
            AgentRunVerificationOutcome::Error {
                detail: format!("catalog service provisioning failed ({phase:?}/{code:?})"),
            }
        }
        FinalVerificationRecordingOutcome::NotConfigured { .. } => {
            AgentRunVerificationOutcome::NotConfigured
        }
    };
    emit_tool_attempt(&outcome, &capture, &task_id, &task_run_id);
    outcome
}

fn outcome_checks(outcome: &AgentRunVerificationOutcome) -> &[AgentVerificationCheckResult] {
    match outcome {
        AgentRunVerificationOutcome::Hit { checks, .. }
        | AgentRunVerificationOutcome::RanPass { checks, .. }
        | AgentRunVerificationOutcome::RanFail { checks, .. } => checks,
        _ => &[],
    }
}

/// Emit exactly one bounded tool-attempt telemetry outcome, plus the bounded
/// full/subset selection and per-check pass/fail counters. Correlation IDs
/// remain structured audit fields only.
fn emit_tool_attempt(
    outcome: &AgentRunVerificationOutcome,
    capture: &AgentRunCapture,
    task_id: &str,
    task_run_id: &str,
) {
    use djinn_telemetry::final_verification as tel;
    let label = match outcome {
        AgentRunVerificationOutcome::Hit { .. } => tel::TOOL_HIT,
        AgentRunVerificationOutcome::RanPass { .. } => tel::TOOL_RAN_PASS,
        AgentRunVerificationOutcome::RanFail { .. } => tel::TOOL_RAN_FAIL,
        AgentRunVerificationOutcome::RateLimited { .. } => tel::TOOL_RATE_LIMITED,
        // NotConfigured has nothing to verify; it buckets with error for the
        // bounded tool-attempt metric while staying a distinct typed result.
        AgentRunVerificationOutcome::Error { .. } | AgentRunVerificationOutcome::NotConfigured => {
            tel::TOOL_ERROR
        }
    };
    if tel::increment_tool_attempt(label).is_err() {
        tracing::warn!(
            run_verification_outcome = label,
            task_id = %task_id,
            task_run_id = %task_run_id,
            "run_verification tool-attempt telemetry emission failed"
        );
    }
    if let Some(selection) = capture.selection {
        tel::increment_selection(selection);
    }
    let checks = outcome_checks(outcome);
    let passed = checks.iter().filter(|check| check.passed).count() as u64;
    let failed = checks.len() as u64 - passed;
    tel::add_check_results(tel::CHECK_PASS, passed);
    tel::add_check_results(tel::CHECK_FAIL, failed);
    tracing::info!(
        run_verification_outcome = label,
        task_id = %task_id,
        task_run_id = %task_run_id,
        selection = capture.selection.unwrap_or("unknown"),
        checks_passed = passed,
        checks_failed = failed,
        "run_verification tool attempt completed"
    );
}

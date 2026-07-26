//! Bounded telemetry and outcome construction for the final-verification
//! coordinator.
//!
//! Split out of `final_verification.rs` so the coordinator keeps only the
//! resolve/consult/lease/execute/persist protocol. Every metric here carries a
//! single bounded label; rich identifiers, the evidence tier, and diagnostics
//! ride the structured tracing events instead, so no per-project or per-task
//! value can reach a metric's cardinality.

use djinn_telemetry::final_verification as telemetry;

use super::{
    FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
    provisioning_outcome_label, provisioning_phase_label,
};
use crate::host::SlotContext;

pub(super) fn lookup_none<T>(
    outcome: &'static str,
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &'static str,
    detail: &str,
    ctx: &SlotContext,
) -> Option<T> {
    emit_lookup_outcome_with_test_observation(outcome, request, attempt_id, reason, detail, ctx);
    None
}

pub(super) fn emit_lookup_outcome(
    outcome: &'static str,
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &str,
    detail: &str,
) {
    // The metric has only the bounded outcome label. Rich identifiers and
    // diagnostics are emitted in the structured audit event below.
    if telemetry::increment_lookup(outcome).is_err() {
        tracing::warn!(verify_run_lookup_outcome = outcome, task_id = %request.task_id,
            task_run_id = %request.task_run_id, verification_attempt_id = %attempt_id,
            audit_reason = reason, audit_detail = detail,
            "final verification reuse telemetry emission failed");
    }
    tracing::info!(verify_run_lookup_outcome = outcome, task_id = %request.task_id,
        task_run_id = %request.task_run_id, verification_attempt_id = %attempt_id,
        audit_reason = reason, audit_detail = detail, "final verification reuse consultation");
}

pub(super) fn emit_lookup_outcome_with_test_observation(
    outcome: &'static str,
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &'static str,
    detail: &str,
    ctx: &SlotContext,
) {
    emit_lookup_outcome(outcome, request, attempt_id, reason, detail);
    ctx.callbacks
        .record_final_verification_consultation_outcome_for_test(outcome, reason);
}

pub(super) fn ineligible_outcome(
    attempt_id: &str,
    reason: &str,
) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Ineligible {
        verification_attempt_id: attempt_id.to_owned(),
        reason: reason.to_owned(),
    }
}
pub(super) fn error_outcome(attempt_id: &str, detail: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Error {
        verification_attempt_id: attempt_id.to_owned(),
        detail: detail.to_owned(),
    }
}
pub(super) fn emit_ineligible(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &str,
) -> FinalVerificationRecordingOutcome {
    emit_outcome(request, ineligible_outcome(attempt_id, reason))
}
pub(super) fn emit_error(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    detail: &str,
) -> FinalVerificationRecordingOutcome {
    emit_outcome(request, error_outcome(attempt_id, detail))
}
pub(super) fn emit_outcome(
    request: &FinalVerificationCoordinatorRequest,
    outcome: FinalVerificationRecordingOutcome,
) -> FinalVerificationRecordingOutcome {
    match &outcome {
        FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id,
            verify_run_id,
            evidence,
        } => {
            let _ = telemetry::increment_record(telemetry::RECORD_STORED);
            tracing::info!(
                recording_outcome = "stored", task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id, verify_run_id = %verify_run_id,
                persisted_run_id = %evidence.persisted_run_id,
                ordered_commands = %evidence.ordered_commands,
                covered_checks = %evidence.covered_checks,
                required_checks = ?evidence.required_checks,
                verification_input_fingerprint = %evidence.verification_input_fingerprint,
                manifest_version = %evidence.manifest_version,
                environment_identity_digest = %evidence.environment_identity_digest,
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::Reused {
            verification_attempt_id,
            evidence,
        } => tracing::info!(
            recording_outcome = "reused", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id, verify_run_id = %evidence.persisted_run_id,
            ordered_commands = %evidence.ordered_commands,
            covered_checks = %evidence.covered_checks,
            required_checks = ?evidence.required_checks,
            verification_input_fingerprint = %evidence.verification_input_fingerprint,
            manifest_version = %evidence.manifest_version,
            environment_identity_digest = %evidence.environment_identity_digest,
            "final verification recording completed"
        ),
        FinalVerificationRecordingOutcome::Ineligible {
            verification_attempt_id,
            reason,
        } => {
            let _ = telemetry::increment_record(telemetry::RECORD_INELIGIBLE);
            tracing::info!(
                recording_outcome = "ineligible", task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id, reason = %reason,
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::InfrastructureIneligible {
            verification_attempt_id,
            phase,
            code,
        } => {
            // Infrastructure ineligibility records no passing row, so it counts
            // as an ineligible writer outcome; the bounded provisioning phase/code
            // ride the structured audit fields, never the record metric labels.
            let _ = telemetry::increment_record(telemetry::RECORD_INELIGIBLE);
            tracing::info!(
                recording_outcome = "infrastructure_ineligible",
                task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id,
                provisioning_phase = provisioning_phase_label(*phase),
                provisioning_outcome = provisioning_outcome_label(*code),
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::Error {
            verification_attempt_id,
            detail,
        } => {
            let _ = telemetry::increment_record(telemetry::RECORD_ERROR);
            tracing::error!(
                recording_outcome = "error", task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id, detail = %detail,
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::NotConfigured {
            verification_attempt_id,
        } => tracing::info!(
            recording_outcome = "not_configured", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id,
            "final verification recording completed"
        ),
    }
    outcome
}

//! Authoritative final-verification recording coordinator.
//!
//! This module deliberately owns no fingerprinting, identity derivation, or
//! sandbox policy. Those correctness boundaries live in `djinn-sandbox`; this
//! coordinator owns the one lease, one execution, and one possible durable row.

use std::future::Future;
use std::pin::Pin;

use djinn_core::canonical_verify::EnvironmentIdentityV1;
use djinn_core::models::VerifySource;
use djinn_db::repositories::verify_run::{
    RecordEligibleFinalVerificationPassParams, RequiredFinalVerificationCommand,
    VerifyRunRepository,
};
use djinn_sandbox::final_verification_execution::{
    FinalVerificationExecutionEvidence, FinalVerificationExecutionRequest,
    execute_final_verification,
};
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;

/// Material the host resolves from the current canonical plan, manifest, and
/// environment. It is intentionally an execution request rather than a second
/// copy of the identity/fingerprint contract.
#[derive(Clone)]
pub struct FinalVerificationResolvedMaterial {
    pub execution_request: FinalVerificationExecutionRequest,
    pub verify_source: VerifySource,
    /// Required check IDs from the same canonical plan used by the request.
    pub required_checks: Vec<String>,
    /// Legacy audit fingerprint. It is not used for final-verification reuse.
    pub diff_fingerprint: String,
}

/// The normal invocation lease. The coordinator explicitly releases it before
/// every return, including ineligible, insert-error, and cancellation paths.
pub trait FinalVerificationInvocationLease: Send {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

#[derive(Clone, Debug)]
pub struct FinalVerificationCoordinatorRequest {
    pub task_id: String,
    pub task_run_id: String,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalVerificationRecordingOutcome {
    Stored {
        verification_attempt_id: String,
        verify_run_id: String,
    },
    Ineligible {
        verification_attempt_id: String,
        reason: String,
    },
    Error {
        verification_attempt_id: String,
        detail: String,
    },
}

/// Resolve, lease, execute once, and conditionally record exactly one pass.
pub async fn coordinate_final_verification(
    request: FinalVerificationCoordinatorRequest,
    ctx: &SlotContext,
) -> FinalVerificationRecordingOutcome {
    let verification_attempt_id = uuid::Uuid::now_v7().to_string();
    let verify_run_id = uuid::Uuid::now_v7().to_string();

    if request.cancellation.is_cancelled() {
        return emit_ineligible(
            &request,
            &verification_attempt_id,
            "cancelled before resolution",
        );
    }
    let material = match ctx
        .callbacks
        .resolve_final_verification(
            &request.task_id,
            &request.task_run_id,
            &verification_attempt_id,
            &verify_run_id,
            ctx,
        )
        .await
    {
        Ok(material) => material,
        Err(detail) => return emit_error(&request, &verification_attempt_id, &detail),
    };
    if request.cancellation.is_cancelled() {
        return emit_ineligible(&request, &verification_attempt_id, "cancelled before lease");
    }
    let mut lease = match ctx
        .callbacks
        .acquire_final_verification_lease(
            &request.task_id,
            &request.task_run_id,
            &verification_attempt_id,
            ctx,
        )
        .await
    {
        Ok(lease) => lease,
        Err(detail) => return emit_error(&request, &verification_attempt_id, &detail),
    };

    let outcome = if request.cancellation.is_cancelled() {
        ineligible_outcome(&verification_attempt_id, "cancelled before execution")
    } else {
        // The delivered executor performs every descriptor in order and returns
        // evidence rather than persistence side effects.
        let evidence = execute_final_verification(material.execution_request.clone()).await;
        if request.cancellation.is_cancelled() {
            ineligible_outcome(&verification_attempt_id, "cancelled during execution")
        } else if !evidence.eligible() {
            ineligible_outcome(&verification_attempt_id, &format_evidence_reason(&evidence))
        } else {
            persist_evidence(
                &request,
                &verification_attempt_id,
                &verify_run_id,
                &material,
                &evidence,
                ctx,
            )
            .await
        }
    };

    // A release failure cannot turn execution into a successful record. A row
    // may only be reported stored after the lease has also released normally.
    match lease.release().await {
        Ok(()) => emit_outcome(&request, outcome),
        Err(detail) => emit_error(
            &request,
            &verification_attempt_id,
            &format!("lease release failed: {detail}"),
        ),
    }
}

async fn persist_evidence(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    verify_run_id: &str,
    material: &FinalVerificationResolvedMaterial,
    evidence: &FinalVerificationExecutionEvidence,
    ctx: &SlotContext,
) -> FinalVerificationRecordingOutcome {
    let (Some(f0), Some(f1), Some(pre), Some(post)) = (
        evidence.fingerprint_f0.as_ref(),
        evidence.fingerprint_f1.as_ref(),
        evidence.pre_environment_identity.as_ref(),
        evidence.post_environment_identity.as_ref(),
    ) else {
        return ineligible_outcome(attempt_id, "eligible evidence was incomplete");
    };
    if f0 != f1 || pre != post {
        return ineligible_outcome(attempt_id, "verification consistency boundary changed");
    }
    let ordered_commands = serde_json::Value::Array(
        evidence
            .commands
            .iter()
            .map(|command| {
                serde_json::json!({
                    "descriptor_id": command.descriptor.check_id,
                    "result": "pass",
                    "passed": true,
                    "started_at_unix_millis": command.started_at_unix_millis,
                    "completed_at_unix_millis": command.completed_at_unix_millis,
                })
            })
            .collect(),
    );
    let covered_checks = serde_json::Value::Array(
        evidence
            .commands
            .iter()
            .map(|command| serde_json::Value::String(command.descriptor.check_id.clone()))
            .collect(),
    );
    let required_commands: Vec<_> = evidence
        .commands
        .iter()
        .map(|command| RequiredFinalVerificationCommand {
            descriptor_id: &command.descriptor.check_id,
        })
        .collect();
    let required_checks = material.required_checks.clone();
    let identity_json = identity_json(pre);
    let completed_at = match time::OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(timestamp) => timestamp,
        Err(error) => {
            return error_outcome(attempt_id, &format!("timestamp formatting failed: {error}"));
        }
    };
    let repo = VerifyRunRepository::new(ctx.db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    match repo
        .record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
            id: &id,
            task_run_id: &request.task_run_id,
            verify_source: material.verify_source.as_str(),
            verify_run_id,
            verification_attempt_id: attempt_id,
            required_commands: &required_commands,
            ordered_commands: &ordered_commands,
            covered_checks: &covered_checks,
            required_checks: &required_checks,
            verification_input_fingerprint: &f0.fingerprint,
            manifest_version: &format!("manifest-v{}", evidence.manifest_version),
            environment_identity_json: &identity_json,
            environment_identity_digest: &pre.digest,
            environment_identity_version: "identity-v1",
            completed_at: &completed_at,
            diff_fingerprint: &material.diff_fingerprint,
        })
        .await
    {
        Ok(_) => FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id: attempt_id.to_owned(),
            verify_run_id: verify_run_id.to_owned(),
        },
        Err(error) => error_outcome(
            attempt_id,
            &format!("final verification insert failed: {error}"),
        ),
    }
}

fn identity_json(identity: &EnvironmentIdentityV1) -> serde_json::Value {
    serde_json::from_str(&identity.canonical_json).unwrap_or_else(|_| {
        serde_json::json!({
            "canonical_json": identity.canonical_json,
            "schema_version": identity.schema_version,
            "canonicalization_version": identity.canonicalization_version,
        })
    })
}

fn format_evidence_reason(evidence: &FinalVerificationExecutionEvidence) -> String {
    evidence.eligibility_reason.as_ref().map_or_else(
        || "malformed final-verification evidence".to_owned(),
        |reason| format!("{reason:?}"),
    )
}

fn ineligible_outcome(attempt_id: &str, reason: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Ineligible {
        verification_attempt_id: attempt_id.to_owned(),
        reason: reason.to_owned(),
    }
}
fn error_outcome(attempt_id: &str, detail: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Error {
        verification_attempt_id: attempt_id.to_owned(),
        detail: detail.to_owned(),
    }
}
fn emit_ineligible(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &str,
) -> FinalVerificationRecordingOutcome {
    emit_outcome(request, ineligible_outcome(attempt_id, reason))
}
fn emit_error(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    detail: &str,
) -> FinalVerificationRecordingOutcome {
    emit_outcome(request, error_outcome(attempt_id, detail))
}
fn emit_outcome(
    request: &FinalVerificationCoordinatorRequest,
    outcome: FinalVerificationRecordingOutcome,
) -> FinalVerificationRecordingOutcome {
    match &outcome {
        FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id,
            verify_run_id,
        } => tracing::info!(
            recording_outcome = "stored", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id, verify_run_id = %verify_run_id,
            "final verification recording completed"
        ),
        FinalVerificationRecordingOutcome::Ineligible {
            verification_attempt_id,
            reason,
        } => tracing::info!(
            recording_outcome = "ineligible", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id, reason = %reason,
            "final verification recording completed"
        ),
        FinalVerificationRecordingOutcome::Error {
            verification_attempt_id,
            detail,
        } => tracing::error!(
            recording_outcome = "error", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id, detail = %detail,
            "final verification recording completed"
        ),
    }
    outcome
}

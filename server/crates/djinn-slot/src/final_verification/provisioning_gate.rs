//! Bounded catalog-service provisioning helpers for the final-verification
//! coordinator: classify a service-lifecycle failure distinctly from a
//! command/check failure, derive the coarse bounded `service_type` label, and
//! emit exactly one bounded provisioning metric plus one structured audit
//! summary per serviced attempt.

use djinn_sandbox::final_verification_execution::{
    FinalVerificationExecutionEvidence, FinalVerificationIneligibilityReason,
};
use djinn_sandbox::service_provisioning::{ServiceProvisioningCode, ServiceProvisioningPhase};

use super::{FinalVerificationCoordinatorRequest, FinalVerificationResolvedMaterial};

/// Return the bounded provisioning `(phase, code)` when the execution evidence
/// is ineligible specifically because of the catalog-service lifecycle, so the
/// coordinator can surface a typed infrastructure outcome distinct from a
/// command/check failure. Any other (or no) ineligibility returns `None`.
pub(super) fn service_provisioning_failure(
    evidence: &FinalVerificationExecutionEvidence,
) -> Option<(ServiceProvisioningPhase, ServiceProvisioningCode)> {
    match &evidence.eligibility_reason {
        Some(FinalVerificationIneligibilityReason::ServiceProvisioning { phase, code }) => {
            Some((*phase, *code))
        }
        _ => None,
    }
}

/// Coarse, bounded classification of a plan's declared catalog services for the
/// provisioning metric's `service_type` label. Preset identifiers are never used
/// as labels directly (they are project-defined and unbounded); this collapses
/// them to a fixed enum. Distinct types collapse to `multiple`; an empty plan to
/// `none`.
pub(super) fn classify_service_type(material: &FinalVerificationResolvedMaterial) -> &'static str {
    use djinn_telemetry::final_verification as tel;
    let mut seen: Option<&'static str> = None;
    for provisioner in &material.execution_request.service_provisioners {
        let preset = provisioner.preset_id().to_ascii_lowercase();
        let classified = if preset.contains("postgres") {
            tel::PROVISION_SERVICE_POSTGRES
        } else if preset.contains("redis") {
            tel::PROVISION_SERVICE_REDIS
        } else if preset.contains("rabbitmq") || preset.contains("amqp") {
            tel::PROVISION_SERVICE_RABBITMQ
        } else {
            tel::PROVISION_SERVICE_OTHER
        };
        match seen {
            None => seen = Some(classified),
            Some(existing) if existing == classified => {}
            Some(_) => return tel::PROVISION_SERVICE_MULTIPLE,
        }
    }
    seen.unwrap_or(tel::PROVISION_SERVICE_NONE)
}

pub(super) fn provisioning_phase_label(phase: ServiceProvisioningPhase) -> &'static str {
    use djinn_telemetry::final_verification as tel;
    match phase {
        ServiceProvisioningPhase::Resolve => tel::PROVISION_PHASE_RESOLVE,
        ServiceProvisioningPhase::Proxy => tel::PROVISION_PHASE_PROXY,
        ServiceProvisioningPhase::Create => tel::PROVISION_PHASE_CREATE,
        ServiceProvisioningPhase::Readiness => tel::PROVISION_PHASE_READINESS,
        ServiceProvisioningPhase::Teardown => tel::PROVISION_PHASE_TEARDOWN,
    }
}

pub(super) fn provisioning_outcome_label(code: ServiceProvisioningCode) -> &'static str {
    use djinn_telemetry::final_verification as tel;
    match code {
        ServiceProvisioningCode::Unavailable => tel::PROVISION_OUTCOME_UNAVAILABLE,
        ServiceProvisioningCode::ProtocolMismatch => tel::PROVISION_OUTCOME_PROTOCOL_MISMATCH,
        ServiceProvisioningCode::Timeout => tel::PROVISION_OUTCOME_TIMEOUT,
        ServiceProvisioningCode::Rejected => tel::PROVISION_OUTCOME_REJECTED,
        ServiceProvisioningCode::InvalidResponse => tel::PROVISION_OUTCOME_INVALID_RESPONSE,
    }
}

/// Emit exactly one bounded provisioning metric and one structured audit summary
/// for an attempt whose plan declared catalog services. Serviceless plans emit
/// nothing (no provisioning occurred). Success (evidence not blocked by the
/// service lifecycle) records `complete`/`ok`; a service failure records the
/// failing bounded `phase`/`outcome`. The metric carries only bounded enum
/// labels; task/run/attempt/fingerprint/environment identifiers are confined to
/// the structured audit event and are never metric labels.
pub(super) fn emit_service_provisioning_outcome(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    material: &FinalVerificationResolvedMaterial,
    evidence: &FinalVerificationExecutionEvidence,
) {
    use djinn_telemetry::final_verification as tel;
    if material.execution_request.service_provisioners.is_empty() {
        return;
    }
    let service_type = classify_service_type(material);
    let (phase, outcome) = match service_provisioning_failure(evidence) {
        Some((phase, code)) => (
            provisioning_phase_label(phase),
            provisioning_outcome_label(code),
        ),
        // Provisioning + teardown both succeeded whenever the evidence is not
        // blocked by the service lifecycle, even if a command later failed.
        None => (tel::PROVISION_PHASE_COMPLETE, tel::PROVISION_OUTCOME_OK),
    };
    if tel::increment_provisioning(phase, outcome, service_type).is_err() {
        tracing::warn!(
            provisioning_phase = phase,
            provisioning_outcome = outcome,
            service_type = service_type,
            task_id = %request.task_id,
            task_run_id = %request.task_run_id,
            verification_attempt_id = %attempt_id,
            "service provisioning telemetry emission failed"
        );
    }
    // The one structured attempt summary. Fingerprint and environment identity
    // are present only when execution progressed past provisioning; a pure
    // provisioning failure leaves them empty rather than fabricating a value.
    let fingerprint = evidence
        .fingerprint_f0
        .as_ref()
        .map_or("", |digest| digest.fingerprint.as_str());
    let environment_identity_digest = evidence
        .pre_environment_identity
        .as_ref()
        .map_or("", |identity| identity.digest.as_str());
    tracing::info!(
        provisioning_phase = phase,
        provisioning_outcome = outcome,
        service_type = service_type,
        task_id = %request.task_id,
        task_run_id = %request.task_run_id,
        verification_attempt_id = %attempt_id,
        verification_input_fingerprint = %fingerprint,
        environment_identity_digest = %environment_identity_digest,
        "final verification service provisioning attempt completed"
    );
}

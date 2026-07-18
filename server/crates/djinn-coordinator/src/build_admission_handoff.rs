//! Emergency-side policy for the durable build-admission authority handoff.
//!
//! This module deliberately contains no invocation controller.  The invocation
//! side is represented by [`InvocationAuthorityObservation`] so startup and
//! telemetry can use one deterministic protocol interpretation without making
//! goxi a runtime dependency.

use djinn_db::{AdmissionHandoffPhase, AdmissionHandoffRow};

use crate::build_admission::{BuildAdmissionMode, BuildAdmissionReadiness};

/// Observation supplied by the invocation authority (or a deterministic fake).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InvocationAuthorityObservation {
    pub enforcing: bool,
}

/// Why an emergency controller remains required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmergencyAuthorityDecision {
    /// The configured standalone v0 mode remains authoritative.
    ConfiguredStandalone,
    /// A durable row requires v0, or is not safe enough to release it.
    RequiredFailClosed,
    /// The committed invocation-primary phase is the sole release point.
    MayDisable,
}

/// Bounded protocol classification consumed by later telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffState {
    MissingRow,
    EmergencyPrimary,
    ForwardOverlap,
    InvocationPrimary,
    RollbackOverlap,
    IncompleteEpoch,
    EpochUnreadable,
    UnexpectedOverlap,
}

/// Data-only handoff result.  `row` preserves the exact durable epoch and
/// acknowledgement evidence for callers that need to report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffSnapshot {
    pub state: HandoffState,
    pub emergency: EmergencyAuthorityDecision,
    pub row: Option<AdmissionHandoffRow>,
    pub emergency_acknowledgement_allowed: bool,
}

/// Evaluate the durable row before changing the emergency controller.
///
/// Any read failure, stale acknowledgement, or overlap that is not the durable
/// overlap phase is conservative.  Only a *committed* `InvocationPrimary` row
/// releases emergency enforcement.  A missing row intentionally does not
/// invent rollout state: it retains the configured standalone mode, except
/// that two simultaneously observed authorities are an anomaly.
#[must_use]
pub fn evaluate_handoff(
    row: Result<Option<AdmissionHandoffRow>, ()>,
    configured_mode: BuildAdmissionMode,
    emergency_enforcing: bool,
    emergency_readiness: BuildAdmissionReadiness,
    invocation: InvocationAuthorityObservation,
) -> HandoffSnapshot {
    let ack_allowed = configured_mode == BuildAdmissionMode::Enforce
        && emergency_enforcing
        && emergency_readiness.is_healthy();
    let (state, emergency, row) = match row {
        Err(()) => (HandoffState::EpochUnreadable, EmergencyAuthorityDecision::RequiredFailClosed, None),
        Ok(None) if emergency_enforcing && invocation.enforcing => (
            HandoffState::UnexpectedOverlap,
            EmergencyAuthorityDecision::ConfiguredStandalone,
            None,
        ),
        Ok(None) => (HandoffState::MissingRow, EmergencyAuthorityDecision::ConfiguredStandalone, None),
        Ok(Some(row)) => {
            let emergency_current = row.emergency_ack_epoch == Some(row.epoch);
            let invocation_current = row.invocation_ack_epoch == Some(row.epoch);
            let complete = match row.phase {
                AdmissionHandoffPhase::EmergencyPrimary => emergency_current,
                AdmissionHandoffPhase::ForwardOverlap | AdmissionHandoffPhase::RollbackOverlap => {
                    emergency_current && invocation_current
                }
                AdmissionHandoffPhase::InvocationPrimary => invocation_current,
            };
            if !complete {
                (HandoffState::IncompleteEpoch, EmergencyAuthorityDecision::RequiredFailClosed, Some(row))
            } else {
                match row.phase {
                    AdmissionHandoffPhase::EmergencyPrimary => (HandoffState::EmergencyPrimary, EmergencyAuthorityDecision::RequiredFailClosed, Some(row)),
                    AdmissionHandoffPhase::ForwardOverlap => (HandoffState::ForwardOverlap, EmergencyAuthorityDecision::RequiredFailClosed, Some(row)),
                    AdmissionHandoffPhase::InvocationPrimary => (HandoffState::InvocationPrimary, EmergencyAuthorityDecision::MayDisable, Some(row)),
                    AdmissionHandoffPhase::RollbackOverlap => (HandoffState::RollbackOverlap, EmergencyAuthorityDecision::RequiredFailClosed, Some(row)),
                }
            }
        }
    };
    HandoffSnapshot { state, emergency, row, emergency_acknowledgement_allowed: ack_allowed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(phase: AdmissionHandoffPhase, emergency: bool, invocation: bool) -> AdmissionHandoffRow {
        AdmissionHandoffRow { phase, epoch: 7, emergency_ack_epoch: emergency.then_some(7), invocation_ack_epoch: invocation.then_some(7), updated_at: "now".into() }
    }

    #[test]
    fn every_persisted_phase_has_a_deterministic_emergency_decision() {
        let cases = [
            (AdmissionHandoffPhase::EmergencyPrimary, true, false, HandoffState::EmergencyPrimary, EmergencyAuthorityDecision::RequiredFailClosed),
            (AdmissionHandoffPhase::ForwardOverlap, true, true, HandoffState::ForwardOverlap, EmergencyAuthorityDecision::RequiredFailClosed),
            (AdmissionHandoffPhase::InvocationPrimary, false, true, HandoffState::InvocationPrimary, EmergencyAuthorityDecision::MayDisable),
            (AdmissionHandoffPhase::RollbackOverlap, true, true, HandoffState::RollbackOverlap, EmergencyAuthorityDecision::RequiredFailClosed),
        ];
        for (phase, emergency_ack, invocation_ack, state, decision) in cases {
            let snapshot = evaluate_handoff(Ok(Some(row(phase, emergency_ack, invocation_ack))), BuildAdmissionMode::Enforce, true, BuildAdmissionReadiness::Healthy, InvocationAuthorityObservation::default());
            assert_eq!(snapshot.state, state);
            assert_eq!(snapshot.emergency, decision);
        }
    }

    #[test]
    fn failures_incomplete_epochs_and_restart_before_commit_stay_closed() {
        for input in [
            Err(()),
            Ok(Some(row(AdmissionHandoffPhase::ForwardOverlap, true, false))),
            Ok(Some(row(AdmissionHandoffPhase::InvocationPrimary, false, false))),
        ] {
            assert_eq!(evaluate_handoff(input, BuildAdmissionMode::Enforce, true, BuildAdmissionReadiness::Healthy, InvocationAuthorityObservation::default()).emergency, EmergencyAuthorityDecision::RequiredFailClosed);
        }
    }

    #[test]
    fn missing_row_preserves_mode_but_simultaneous_authorities_are_anomalous() {
        let standalone = evaluate_handoff(Ok(None), BuildAdmissionMode::Observe, false, BuildAdmissionReadiness::Healthy, InvocationAuthorityObservation::default());
        assert_eq!(standalone.state, HandoffState::MissingRow);
        assert_eq!(standalone.emergency, EmergencyAuthorityDecision::ConfiguredStandalone);
        let anomaly = evaluate_handoff(Ok(None), BuildAdmissionMode::Enforce, true, BuildAdmissionReadiness::Healthy, InvocationAuthorityObservation { enforcing: true });
        assert_eq!(anomaly.state, HandoffState::UnexpectedOverlap);
    }

    #[test]
    fn acknowledgement_requires_actual_healthy_enforcement() {
        for readiness in [BuildAdmissionReadiness::JournalRecoveryIncomplete, BuildAdmissionReadiness::Healthy] {
            let snapshot = evaluate_handoff(Ok(Some(row(AdmissionHandoffPhase::EmergencyPrimary, false, false))), BuildAdmissionMode::Enforce, readiness.is_healthy(), readiness, InvocationAuthorityObservation::default());
            assert_eq!(snapshot.emergency_acknowledgement_allowed, readiness.is_healthy());
        }
    }
}

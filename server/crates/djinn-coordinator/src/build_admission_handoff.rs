//! Emergency-side policy for the durable build-admission authority handoff.
//!
//! This module deliberately contains no invocation controller.  The invocation
//! side is represented by [`InvocationAuthorityObservation`] so startup and
//! telemetry can use one deterministic protocol interpretation without making
//! goxi a runtime dependency.

use djinn_db::{AdmissionHandoffPhase, AdmissionHandoffRow};

use crate::build_admission::{BuildAdmissionMode, BuildAdmissionReadiness};

/// Observation supplied by the invocation authority (or a deterministic fake).
///
/// **Deliberately not `Default`.** Every production call site once passed
/// `::default()` — `enforcing: false` hard-coded — so the server never read the
/// real v1 authority and emitted a permanent, unclearable `stale_epoch` warning
/// throughout the forward cutover. Requiring the field to be named forces each
/// caller to state where its value came from: production derives it from
/// `evaluate_invocation_lift` over the durable row, and any caller that
/// genuinely cannot observe v1 must write `enforcing: false` with a comment
/// saying why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvocationAuthorityObservation {
    pub enforcing: bool,
}

impl HandoffSnapshot {
    /// Convert the typed policy result plus authority observations into the
    /// intentionally small warning contract.
    #[must_use]
    pub fn warning_reason(
        &self,
        emergency_enforcing: bool,
        invocation: InvocationAuthorityObservation,
    ) -> Option<HandoffWarningReason> {
        match self.state {
            HandoffState::UnexpectedOverlap => Some(HandoffWarningReason::UnexpectedOverlap),
            HandoffState::IncompleteEpoch => Some(HandoffWarningReason::StaleEpoch),
            HandoffState::EpochUnreadable => Some(HandoffWarningReason::EpochUnreadable),
            // A row where neither authority enforces is a fail-closed
            // misconfiguration that must surface for attention.
            HandoffState::IllegalModeCombo => Some(HandoffWarningReason::StaleEpoch),
            HandoffState::ForwardOverlap | HandoffState::RollbackOverlap => {
                if emergency_enforcing && invocation.enforcing {
                    None
                } else {
                    Some(HandoffWarningReason::StaleEpoch)
                }
            }
            // Baseline and shadow are both the v0-primary steady state.
            HandoffState::EmergencyPrimary | HandoffState::Shadow => {
                if emergency_enforcing {
                    invocation
                        .enforcing
                        .then_some(HandoffWarningReason::UnexpectedOverlap)
                } else {
                    Some(HandoffWarningReason::StaleEpoch)
                }
            }
            HandoffState::InvocationPrimary => {
                if invocation.enforcing {
                    emergency_enforcing.then_some(HandoffWarningReason::UnexpectedOverlap)
                } else {
                    Some(HandoffWarningReason::StaleEpoch)
                }
            }
            HandoffState::MissingRow => (emergency_enforcing && invocation.enforcing)
                .then_some(HandoffWarningReason::UnexpectedOverlap),
        }
    }
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
///
/// `EmergencyPrimary` is the v0-only baseline (v1 off); `Shadow` is that same
/// v0 baseline with v1 observing without enforcing; `ForwardOverlap` /
/// `RollbackOverlap` are the both-enforcing overlaps. `IllegalModeCombo` is the
/// fail-closed classification for a row in which neither authority enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffState {
    MissingRow,
    /// v0 baseline: v0 enforcing, v1 off.
    EmergencyPrimary,
    /// v0 baseline with v1 shadowing (observing, not enforcing).
    Shadow,
    ForwardOverlap,
    InvocationPrimary,
    RollbackOverlap,
    IncompleteEpoch,
    EpochUnreadable,
    UnexpectedOverlap,
    /// Neither authority enforces — a misconfiguration that fails closed.
    IllegalModeCombo,
}

/// The bounded reasons used by handoff telemetry and persistent lifecycle logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffWarningReason {
    UnexpectedOverlap,
    StaleEpoch,
    EpochUnreadable,
}

impl HandoffWarningReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedOverlap => "unexpected_overlap",
            Self::StaleEpoch => "stale_epoch",
            Self::EpochUnreadable => "epoch_unreadable",
        }
    }
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

/// Exact bounded values for `djinn_build_admission_handoff_warning{reason}`.
///
/// Keeping this projection beside the protocol interpreter means startup,
/// telemetry, and deterministic fakes cannot disagree about which incomplete
/// durable state is a stale-epoch warning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandoffWarningGauges {
    pub unexpected_overlap: u8,
    pub stale_epoch: u8,
    pub epoch_unreadable: u8,
}

impl HandoffSnapshot {
    /// Project the protocol result into the three and only three warning gauges.
    #[must_use]
    pub fn warning_gauges(
        &self,
        emergency_enforcing: bool,
        invocation: InvocationAuthorityObservation,
    ) -> HandoffWarningGauges {
        match self.warning_reason(emergency_enforcing, invocation) {
            Some(HandoffWarningReason::UnexpectedOverlap) => HandoffWarningGauges {
                unexpected_overlap: 1,
                stale_epoch: 0,
                epoch_unreadable: 0,
            },
            Some(HandoffWarningReason::StaleEpoch) => HandoffWarningGauges {
                unexpected_overlap: 0,
                stale_epoch: 1,
                epoch_unreadable: 0,
            },
            Some(HandoffWarningReason::EpochUnreadable) => HandoffWarningGauges {
                unexpected_overlap: 0,
                stale_epoch: 0,
                epoch_unreadable: 1,
            },
            None => HandoffWarningGauges {
                unexpected_overlap: 0,
                stale_epoch: 0,
                epoch_unreadable: 0,
            },
        }
    }
}

/// Evaluate the durable row before changing the emergency controller.
///
/// Any read failure, stale acknowledgement, or overlap that is not the durable
/// overlap phase is conservative.  Only a *committed* `InvocationPrimary` row
/// **whose recorded `v1_mode` is enforcing** releases emergency enforcement; an
/// invocation acknowledgement on a row where v1 is off or shadowing never
/// does, because such a row would leave no enforcing authority at all.  A
/// missing row intentionally does not invent rollout state: it retains the
/// configured standalone mode, except that two simultaneously observed
/// authorities are an anomaly.
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
        Err(()) => (
            HandoffState::EpochUnreadable,
            EmergencyAuthorityDecision::RequiredFailClosed,
            None,
        ),
        Ok(None) if emergency_enforcing && invocation.enforcing => (
            HandoffState::UnexpectedOverlap,
            EmergencyAuthorityDecision::ConfiguredStandalone,
            None,
        ),
        Ok(None) => (
            HandoffState::MissingRow,
            EmergencyAuthorityDecision::ConfiguredStandalone,
            None,
        ),
        Ok(Some(row)) if !row.v0_mode.is_enforcing() && !row.v1_mode.is_enforcing() => (
            // Neither authority enforces: no admission control at all. This is a
            // misconfiguration and fails closed regardless of phase or acks.
            HandoffState::IllegalModeCombo,
            EmergencyAuthorityDecision::RequiredFailClosed,
            Some(row),
        ),
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
                (
                    HandoffState::IncompleteEpoch,
                    EmergencyAuthorityDecision::RequiredFailClosed,
                    Some(row),
                )
            } else {
                match row.phase {
                    // The v0 baseline distinguishes pure baseline (v1 off) from
                    // v1 shadowing; both keep v0 enforcing.
                    AdmissionHandoffPhase::EmergencyPrimary => {
                        let state = if row.v1_mode == djinn_db::V1Mode::Shadow {
                            HandoffState::Shadow
                        } else {
                            HandoffState::EmergencyPrimary
                        };
                        (
                            state,
                            EmergencyAuthorityDecision::RequiredFailClosed,
                            Some(row),
                        )
                    }
                    AdmissionHandoffPhase::ForwardOverlap => (
                        HandoffState::ForwardOverlap,
                        EmergencyAuthorityDecision::RequiredFailClosed,
                        Some(row),
                    ),
                    // Releasing v0 requires v1 to be *both* acknowledged and
                    // actually enforcing. An acknowledgement alone is never
                    // sufficient evidence: the invocation-side projection
                    // refuses to lift a v1 that is off or merely shadowing, so
                    // a durable row that reached invocation_primary with a
                    // non-enforcing v1 would otherwise leave zero enforcing
                    // authorities. That row is out of protocol, and the
                    // invariant does not depend on it being unreachable.
                    AdmissionHandoffPhase::InvocationPrimary => (
                        HandoffState::InvocationPrimary,
                        if row.v1_mode.is_enforcing() {
                            EmergencyAuthorityDecision::MayDisable
                        } else {
                            EmergencyAuthorityDecision::RequiredFailClosed
                        },
                        Some(row),
                    ),
                    AdmissionHandoffPhase::RollbackOverlap => (
                        HandoffState::RollbackOverlap,
                        EmergencyAuthorityDecision::RequiredFailClosed,
                        Some(row),
                    ),
                }
            }
        }
    };
    HandoffSnapshot {
        state,
        emergency,
        emergency_acknowledgement_allowed: ack_allowed
            && emergency == EmergencyAuthorityDecision::RequiredFailClosed
            && row
                .as_ref()
                .is_some_and(|row| row.emergency_ack_epoch != Some(row.epoch)),
        row,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(phase: AdmissionHandoffPhase, emergency: bool, invocation: bool) -> AdmissionHandoffRow {
        row_with_modes(
            phase,
            emergency,
            invocation,
            djinn_db::V0Mode::Enforce,
            djinn_db::V1Mode::Off,
        )
    }

    fn row_with_modes(
        phase: AdmissionHandoffPhase,
        emergency: bool,
        invocation: bool,
        v0_mode: djinn_db::V0Mode,
        v1_mode: djinn_db::V1Mode,
    ) -> AdmissionHandoffRow {
        AdmissionHandoffRow {
            phase,
            epoch: 7,
            emergency_ack_epoch: emergency.then_some(7),
            invocation_ack_epoch: invocation.then_some(7),
            v0_mode,
            v1_mode,
            cap: None,
            updated_at: "now".into(),
        }
    }

    #[test]
    fn evaluate_handoff_distinguishes_baseline_shadow_overlap_and_fails_closed_on_illegal_combo() {
        use djinn_db::{V0Mode, V1Mode};
        // Baseline: v0 enforce, v1 off.
        let baseline = evaluate_handoff(
            Ok(Some(row_with_modes(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Off,
            ))),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation { enforcing: false },
        );
        assert_eq!(baseline.state, HandoffState::EmergencyPrimary);
        assert_eq!(
            baseline.emergency,
            EmergencyAuthorityDecision::RequiredFailClosed
        );

        // Shadow: v0 enforce, v1 shadow — distinct from baseline.
        let shadow = evaluate_handoff(
            Ok(Some(row_with_modes(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Shadow,
            ))),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation { enforcing: false },
        );
        assert_eq!(shadow.state, HandoffState::Shadow);
        assert_eq!(
            shadow.emergency,
            EmergencyAuthorityDecision::RequiredFailClosed
        );

        // Overlap: both enforce.
        let overlap = evaluate_handoff(
            Ok(Some(row_with_modes(
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                true,
                V0Mode::Enforce,
                V1Mode::Enforce,
            ))),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation { enforcing: true },
        );
        assert_eq!(overlap.state, HandoffState::ForwardOverlap);

        // Illegal combo: neither authority enforces — fails closed regardless of acks.
        for (v0, v1) in [
            (V0Mode::Observe, V1Mode::Off),
            (V0Mode::Observe, V1Mode::Shadow),
            (V0Mode::Disabled, V1Mode::Off),
            (V0Mode::Disabled, V1Mode::Shadow),
        ] {
            let illegal = evaluate_handoff(
                Ok(Some(row_with_modes(
                    AdmissionHandoffPhase::EmergencyPrimary,
                    true,
                    false,
                    v0,
                    v1,
                ))),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                InvocationAuthorityObservation { enforcing: false },
            );
            assert_eq!(
                illegal.state,
                HandoffState::IllegalModeCombo,
                "{v0:?}/{v1:?}"
            );
            assert_eq!(
                illegal.emergency,
                EmergencyAuthorityDecision::RequiredFailClosed,
                "illegal combo must fail closed for {v0:?}/{v1:?}"
            );
            assert_eq!(
                illegal.warning_reason(true, InvocationAuthorityObservation { enforcing: false }),
                Some(HandoffWarningReason::StaleEpoch)
            );
        }

        // Unreadable and incomplete epochs also fail closed.
        assert_eq!(
            evaluate_handoff(
                Err(()),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                InvocationAuthorityObservation { enforcing: false },
            )
            .emergency,
            EmergencyAuthorityDecision::RequiredFailClosed
        );
        assert_eq!(
            evaluate_handoff(
                Ok(Some(row_with_modes(
                    AdmissionHandoffPhase::ForwardOverlap,
                    true,
                    false,
                    V0Mode::Enforce,
                    V1Mode::Enforce,
                ))),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                InvocationAuthorityObservation { enforcing: true },
            )
            .state,
            HandoffState::IncompleteEpoch
        );
    }

    #[test]
    fn every_persisted_phase_has_a_deterministic_emergency_decision() {
        use djinn_db::{V0Mode, V1Mode};
        // Each row carries the v1 mode the operator executor actually records
        // for that phase: off at the v0 baseline, enforcing from the moment the
        // overlap is armed through the rollback overlap.
        let cases = [
            (
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V1Mode::Off,
                HandoffState::EmergencyPrimary,
                EmergencyAuthorityDecision::RequiredFailClosed,
            ),
            (
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                true,
                V1Mode::Enforce,
                HandoffState::ForwardOverlap,
                EmergencyAuthorityDecision::RequiredFailClosed,
            ),
            (
                AdmissionHandoffPhase::InvocationPrimary,
                false,
                true,
                V1Mode::Enforce,
                HandoffState::InvocationPrimary,
                EmergencyAuthorityDecision::MayDisable,
            ),
            (
                AdmissionHandoffPhase::RollbackOverlap,
                true,
                true,
                V1Mode::Enforce,
                HandoffState::RollbackOverlap,
                EmergencyAuthorityDecision::RequiredFailClosed,
            ),
        ];
        for (phase, emergency_ack, invocation_ack, v1_mode, state, decision) in cases {
            let snapshot = evaluate_handoff(
                Ok(Some(row_with_modes(
                    phase,
                    emergency_ack,
                    invocation_ack,
                    V0Mode::Enforce,
                    v1_mode,
                ))),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                InvocationAuthorityObservation { enforcing: false },
            );
            assert_eq!(snapshot.state, state);
            assert_eq!(snapshot.emergency, decision);
        }
    }

    /// The out-of-protocol row goxi's invariant must survive: a committed
    /// `invocation_primary` phase carrying a current invocation acknowledgement
    /// while the recorded `v1_mode` is not enforcing. The invocation-side
    /// projection refuses to lift such a row, so releasing v0 here would leave
    /// zero enforcing authorities.
    #[test]
    fn invocation_primary_never_releases_v0_while_v1_is_not_enforcing() {
        use djinn_db::{V0Mode, V1Mode};
        for v1_mode in [V1Mode::Off, V1Mode::Shadow] {
            let out_of_protocol = row_with_modes(
                AdmissionHandoffPhase::InvocationPrimary,
                false,
                true,
                V0Mode::Enforce,
                v1_mode,
            );
            // The row is complete for its phase: the invocation authority has
            // acknowledged the exact current epoch, so nothing else fails it
            // closed.
            assert_eq!(
                out_of_protocol.invocation_ack_epoch,
                Some(out_of_protocol.epoch)
            );
            let snapshot = evaluate_handoff(
                Ok(Some(out_of_protocol)),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                // The invocation authority is not lifting, exactly as
                // `evaluate_invocation_lift` projects a non-enforcing v1.
                InvocationAuthorityObservation { enforcing: false },
            );
            assert_eq!(snapshot.state, HandoffState::InvocationPrimary);
            assert_ne!(
                snapshot.emergency,
                EmergencyAuthorityDecision::MayDisable,
                "v0 must not be released while v1 is {v1_mode:?}"
            );
            assert_eq!(
                snapshot.emergency,
                EmergencyAuthorityDecision::RequiredFailClosed,
                "{v1_mode:?}: the emergency authority stays required"
            );
            // The invariant itself: at least one authority still enforces.
            let v0_enforcing = snapshot.emergency != EmergencyAuthorityDecision::MayDisable;
            assert!(
                v0_enforcing,
                "{v1_mode:?}: at least one enforcing authority must remain"
            );
            // The condition is the recorded mode, not the observation: even an
            // authority that claims to be enforcing cannot release v0 while the
            // durable row says v1 is not.
            assert_eq!(
                evaluate_handoff(
                    Ok(Some(row_with_modes(
                        AdmissionHandoffPhase::InvocationPrimary,
                        false,
                        true,
                        V0Mode::Enforce,
                        v1_mode,
                    ))),
                    BuildAdmissionMode::Enforce,
                    true,
                    BuildAdmissionReadiness::Healthy,
                    InvocationAuthorityObservation { enforcing: true },
                )
                .emergency,
                EmergencyAuthorityDecision::RequiredFailClosed,
            );
        }
        // The genuine cutover is unchanged: an enforcing v1 still releases v0.
        assert_eq!(
            evaluate_handoff(
                Ok(Some(row_with_modes(
                    AdmissionHandoffPhase::InvocationPrimary,
                    false,
                    true,
                    V0Mode::Observe,
                    V1Mode::Enforce,
                ))),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                InvocationAuthorityObservation { enforcing: true },
            )
            .emergency,
            EmergencyAuthorityDecision::MayDisable,
        );
    }

    #[test]
    fn failures_incomplete_epochs_and_restart_before_commit_stay_closed() {
        for input in [
            Err(()),
            Ok(Some(row(
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                false,
            ))),
            Ok(Some(row(
                AdmissionHandoffPhase::InvocationPrimary,
                false,
                false,
            ))),
        ] {
            assert_eq!(
                evaluate_handoff(
                    input,
                    BuildAdmissionMode::Enforce,
                    true,
                    BuildAdmissionReadiness::Healthy,
                    InvocationAuthorityObservation { enforcing: false }
                )
                .emergency,
                EmergencyAuthorityDecision::RequiredFailClosed
            );
        }
    }

    #[test]
    fn missing_row_preserves_mode_but_simultaneous_authorities_are_anomalous() {
        let standalone = evaluate_handoff(
            Ok(None),
            BuildAdmissionMode::Observe,
            false,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation { enforcing: false },
        );
        assert_eq!(standalone.state, HandoffState::MissingRow);
        assert_eq!(
            standalone.emergency,
            EmergencyAuthorityDecision::ConfiguredStandalone
        );
        let anomaly = evaluate_handoff(
            Ok(None),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation { enforcing: true },
        );
        assert_eq!(anomaly.state, HandoffState::UnexpectedOverlap);
    }

    #[test]
    fn acknowledgement_requires_actual_healthy_enforcement() {
        for readiness in [
            BuildAdmissionReadiness::JournalRecoveryIncomplete,
            BuildAdmissionReadiness::Healthy,
        ] {
            let snapshot = evaluate_handoff(
                Ok(Some(row(
                    AdmissionHandoffPhase::EmergencyPrimary,
                    false,
                    false,
                ))),
                BuildAdmissionMode::Enforce,
                readiness.is_healthy(),
                readiness,
                InvocationAuthorityObservation { enforcing: false },
            );
            assert_eq!(
                snapshot.emergency_acknowledgement_allowed,
                readiness.is_healthy()
            );
        }
    }
    #[test]
    fn warning_classification_has_only_the_contract_reasons() {
        let invocation = InvocationAuthorityObservation { enforcing: true };
        for phase in [
            AdmissionHandoffPhase::ForwardOverlap,
            AdmissionHandoffPhase::RollbackOverlap,
        ] {
            let snapshot = evaluate_handoff(
                Ok(Some(row(phase, true, true))),
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                invocation,
            );
            assert_eq!(snapshot.warning_reason(true, invocation), None, "{phase:?}");
            assert_eq!(
                snapshot.warning_reason(true, InvocationAuthorityObservation { enforcing: false }),
                Some(HandoffWarningReason::StaleEpoch)
            );
        }
        let unreadable = evaluate_handoff(
            Err(()),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation { enforcing: false },
        );
        assert_eq!(
            unreadable.warning_reason(true, InvocationAuthorityObservation { enforcing: false }),
            Some(HandoffWarningReason::EpochUnreadable)
        );
        let overlap = evaluate_handoff(
            Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
            ))),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            invocation,
        );
        assert_eq!(
            overlap.warning_reason(true, invocation),
            Some(HandoffWarningReason::UnexpectedOverlap)
        );
    }
}

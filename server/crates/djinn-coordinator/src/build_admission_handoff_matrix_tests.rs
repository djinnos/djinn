//! Table-driven crash/restart proof for the durable admission-epoch handoff.
//!
//! Split out of `build_admission_integration_tests` purely for file size; it is
//! the same suite and shares its intent: deterministic, barrier-driven, and
//! asserted against durable state rather than wall-clock ordering.

use djinn_db::{
    AdmissionHandoffAuthority, AdmissionHandoffPhase, AdmissionHandoffRepository, Database,
};

use crate::build_admission::{BuildAdmissionMode, BuildAdmissionReadiness};
use crate::build_admission_handoff::{
    EmergencyAuthorityDecision, HandoffState, HandoffWarningGauges, InvocationAuthorityObservation,
    evaluate_handoff,
};

#[derive(Clone, Copy, Debug)]
enum HandoffMatrixAction {
    Acknowledge(AdmissionHandoffAuthority),
    Commit(AdmissionHandoffPhase),
}

#[derive(Clone, Copy, Debug)]
struct HandoffMatrixExpectation {
    phase: AdmissionHandoffPhase,
    state: HandoffState,
    emergency_acknowledged: bool,
    invocation_acknowledged: bool,
    // The scenario table owns each authority's required enforcement.
    emergency_enforcing: bool,
    invocation_enforcing: bool,
    legal_next: AdmissionHandoffPhase,
    advance_allowed: bool,
}

#[derive(Clone, Copy, Debug)]
struct HandoffMatrixScenario {
    expected: HandoffMatrixExpectation,
    // A deterministic fake supplied independently of the expected policy result.
    invocation_observation: InvocationAuthorityObservation,
}

fn handoff_next(phase: AdmissionHandoffPhase) -> AdmissionHandoffPhase {
    match phase {
        AdmissionHandoffPhase::EmergencyPrimary => AdmissionHandoffPhase::ForwardOverlap,
        AdmissionHandoffPhase::ForwardOverlap => AdmissionHandoffPhase::InvocationPrimary,
        AdmissionHandoffPhase::InvocationPrimary => AdmissionHandoffPhase::RollbackOverlap,
        AdmissionHandoffPhase::RollbackOverlap => AdmissionHandoffPhase::EmergencyPrimary,
    }
}

async fn assert_handoff_restart_snapshot(
    repo: &AdmissionHandoffRepository,
    scenario: HandoffMatrixScenario,
) {
    let expected = scenario.expected;
    let invocation_observation = scenario.invocation_observation;
    let row = repo.read().await.unwrap().unwrap();
    assert_eq!(row.phase, expected.phase);
    assert_eq!(
        row.emergency_ack_epoch,
        expected.emergency_acknowledged.then_some(row.epoch)
    );
    assert_eq!(
        row.invocation_ack_epoch,
        expected.invocation_acknowledged.then_some(row.epoch)
    );
    let snapshot = evaluate_handoff(
        Ok(Some(row.clone())),
        BuildAdmissionMode::Enforce,
        expected.emergency_enforcing,
        BuildAdmissionReadiness::Healthy,
        invocation_observation,
    );
    assert_eq!(snapshot.state, expected.state);
    assert_eq!(
        snapshot.emergency == EmergencyAuthorityDecision::RequiredFailClosed,
        expected.emergency_enforcing,
        "emergency authority requirement must match the matrix"
    );
    assert_eq!(
        invocation_observation.enforcing, expected.invocation_enforcing,
        "invocation authority observation must match the matrix"
    );
    assert!(
        snapshot.emergency == EmergencyAuthorityDecision::RequiredFailClosed
            || invocation_observation.enforcing,
        "every crash/restart state retains at least one enforcing authority"
    );
    assert_eq!(
        snapshot.emergency_acknowledgement_allowed,
        expected.emergency_enforcing
            && !expected.emergency_acknowledged
            && snapshot.emergency == EmergencyAuthorityDecision::RequiredFailClosed,
    );
    assert_eq!(
        snapshot.emergency == EmergencyAuthorityDecision::MayDisable,
        !expected.emergency_enforcing,
    );
    assert_eq!(
        snapshot.warning_gauges(expected.emergency_enforcing, invocation_observation),
        if snapshot.state == HandoffState::IncompleteEpoch {
            HandoffWarningGauges {
                stale_epoch: 1,
                ..HandoffWarningGauges::default()
            }
        } else {
            HandoffWarningGauges::default()
        },
    );
    for authority in [
        AdmissionHandoffAuthority::Emergency,
        AdmissionHandoffAuthority::Invocation,
    ] {
        assert!(matches!(
            repo.acknowledge(authority, row.epoch - 1).await,
            Err(djinn_db::Error::InvalidTransition(_))
        ));
    }
    assert!(matches!(
        repo.advance(row.epoch, handoff_next(expected.legal_next), &[])
            .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));
    if !expected.advance_allowed {
        assert!(
            matches!(
                repo.advance(row.epoch, expected.legal_next, &[]).await,
                Err(djinn_db::Error::InvalidTransition(_))
            ),
            "current-epoch acknowledgement guard rejects phase advance"
        );
    }
}

/// Table-driven crash/restart proof for the complete forward and rollback cycle.
/// Its invocation authority is a typed observation, never a deployed service.
#[tokio::test]
async fn handoff_crash_matrix_preserves_authority_and_epoch_guards() {
    let repo = AdmissionHandoffRepository::new(Database::open_in_memory().unwrap());
    // The matrix walks a genuine forward cutover, so the invocation authority is
    // armed to enforce before any phase advances — exactly as the operator
    // executor arms it while the row is still emergency-primary. Without it the
    // invocation-primary rows below would be an out-of-protocol state that can
    // never release v0, because v1 would not actually be enforcing. Arming
    // clears both acknowledgements and bumps the epoch, which is precisely the
    // un-acknowledged state the first expectation asserts.
    let seeded = repo.read().await.unwrap().unwrap();
    repo.set_modes_and_cap(
        seeded.epoch,
        djinn_db::V0Mode::Enforce,
        djinn_db::V1Mode::Enforce,
        None,
    )
    .await
    .unwrap();
    let expectations = [
        (
            AdmissionHandoffPhase::EmergencyPrimary,
            HandoffState::IncompleteEpoch,
            false,
            false,
            true,
            false,
            false,
            false,
        ),
        (
            AdmissionHandoffPhase::EmergencyPrimary,
            HandoffState::EmergencyPrimary,
            true,
            false,
            true,
            false,
            false,
            true,
        ),
        (
            AdmissionHandoffPhase::ForwardOverlap,
            HandoffState::IncompleteEpoch,
            false,
            false,
            true,
            true,
            true,
            false,
        ),
        (
            AdmissionHandoffPhase::ForwardOverlap,
            HandoffState::IncompleteEpoch,
            true,
            false,
            true,
            true,
            true,
            false,
        ),
        (
            AdmissionHandoffPhase::ForwardOverlap,
            HandoffState::ForwardOverlap,
            true,
            true,
            true,
            true,
            true,
            true,
        ),
        (
            AdmissionHandoffPhase::InvocationPrimary,
            HandoffState::IncompleteEpoch,
            false,
            false,
            true,
            true,
            true,
            false,
        ),
        (
            AdmissionHandoffPhase::InvocationPrimary,
            HandoffState::InvocationPrimary,
            false,
            true,
            false,
            true,
            true,
            true,
        ),
        (
            AdmissionHandoffPhase::RollbackOverlap,
            HandoffState::IncompleteEpoch,
            false,
            false,
            true,
            true,
            true,
            false,
        ),
        (
            AdmissionHandoffPhase::RollbackOverlap,
            HandoffState::IncompleteEpoch,
            true,
            false,
            true,
            true,
            true,
            false,
        ),
        (
            AdmissionHandoffPhase::RollbackOverlap,
            HandoffState::RollbackOverlap,
            true,
            true,
            true,
            true,
            true,
            true,
        ),
        (
            AdmissionHandoffPhase::EmergencyPrimary,
            HandoffState::IncompleteEpoch,
            false,
            false,
            true,
            false,
            false,
            false,
        ),
    ]
    .map(
        |(
            phase,
            state,
            emergency_acknowledged,
            invocation_acknowledged,
            emergency_enforcing,
            invocation_observed,
            invocation_enforcing,
            advance_allowed,
        )| HandoffMatrixScenario {
            expected: HandoffMatrixExpectation {
                phase,
                state,
                emergency_acknowledged,
                invocation_acknowledged,
                emergency_enforcing,
                invocation_enforcing,
                legal_next: handoff_next(phase),
                advance_allowed,
            },
            invocation_observation: InvocationAuthorityObservation {
                enforcing: invocation_observed,
            },
        },
    );
    let actions = [
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Emergency),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::ForwardOverlap),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Emergency),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Invocation),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::InvocationPrimary),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Invocation),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::RollbackOverlap),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Emergency),
        HandoffMatrixAction::Acknowledge(AdmissionHandoffAuthority::Invocation),
        HandoffMatrixAction::Commit(AdmissionHandoffPhase::EmergencyPrimary),
    ];
    for (index, scenario) in expectations.into_iter().enumerate() {
        // Crash/restart before every action and, on the next iteration, after it.
        assert_handoff_restart_snapshot(&repo, scenario).await;
        let Some(action) = actions.get(index).copied() else {
            continue;
        };
        let before = repo.read().await.unwrap().unwrap();
        match action {
            HandoffMatrixAction::Acknowledge(authority) => {
                repo.acknowledge(authority, before.epoch).await.unwrap();
            }
            HandoffMatrixAction::Commit(next) => {
                assert_eq!(next, handoff_next(before.phase));
                repo.advance(before.epoch, next, &[]).await.unwrap();
            }
        }
    }

    let current = repo.read().await.unwrap().unwrap();
    assert_eq!(current.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert!(matches!(
        repo.acknowledge(AdmissionHandoffAuthority::Emergency, current.epoch - 1)
            .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));
    assert!(matches!(
        repo.advance(current.epoch, AdmissionHandoffPhase::InvocationPrimary, &[])
            .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));
    assert!(matches!(
        repo.advance(
            current.epoch - 1,
            AdmissionHandoffPhase::ForwardOverlap,
            &[]
        )
        .await,
        Err(djinn_db::Error::InvalidTransition(_))
    ));

    let warning_cases = [
        (
            "read failure",
            Err(()),
            true,
            true,
            HandoffWarningGauges {
                epoch_unreadable: 1,
                ..HandoffWarningGauges::default()
            },
        ),
        (
            "missing row",
            Ok(None),
            true,
            false,
            HandoffWarningGauges::default(),
        ),
        (
            "steady overlap",
            Ok(None),
            true,
            true,
            HandoffWarningGauges {
                unexpected_overlap: 1,
                ..HandoffWarningGauges::default()
            },
        ),
        (
            "emergency primary overlap",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::EmergencyPrimary,
                epoch: 9,
                emergency_ack_epoch: Some(9),
                invocation_ack_epoch: None,
                v0_mode: djinn_db::V0Mode::Enforce,
                v1_mode: djinn_db::V1Mode::Off,
                cap: None,
                updated_at: "test".into(),
            })),
            true,
            true,
            HandoffWarningGauges {
                unexpected_overlap: 1,
                ..HandoffWarningGauges::default()
            },
        ),
        (
            "invocation primary overlap",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::InvocationPrimary,
                epoch: 9,
                emergency_ack_epoch: None,
                invocation_ack_epoch: Some(9),
                v0_mode: djinn_db::V0Mode::Enforce,
                v1_mode: djinn_db::V1Mode::Off,
                cap: None,
                updated_at: "test".into(),
            })),
            true,
            true,
            HandoffWarningGauges {
                unexpected_overlap: 1,
                ..HandoffWarningGauges::default()
            },
        ),
        (
            "recovered emergency primary",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::EmergencyPrimary,
                epoch: 9,
                emergency_ack_epoch: Some(9),
                invocation_ack_epoch: None,
                v0_mode: djinn_db::V0Mode::Enforce,
                v1_mode: djinn_db::V1Mode::Off,
                cap: None,
                updated_at: "test".into(),
            })),
            true,
            false,
            HandoffWarningGauges::default(),
        ),
        (
            "valid overlap",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::ForwardOverlap,
                epoch: 9,
                emergency_ack_epoch: Some(9),
                invocation_ack_epoch: Some(9),
                v0_mode: djinn_db::V0Mode::Enforce,
                v1_mode: djinn_db::V1Mode::Off,
                cap: None,
                updated_at: "test".into(),
            })),
            true,
            true,
            HandoffWarningGauges::default(),
        ),
        (
            "stale acknowledgement",
            Ok(Some(djinn_db::AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::RollbackOverlap,
                epoch: 9,
                emergency_ack_epoch: Some(8),
                invocation_ack_epoch: Some(9),
                v0_mode: djinn_db::V0Mode::Enforce,
                v1_mode: djinn_db::V1Mode::Off,
                cap: None,
                updated_at: "test".into(),
            })),
            true,
            true,
            HandoffWarningGauges {
                stale_epoch: 1,
                ..HandoffWarningGauges::default()
            },
        ),
    ];
    for (name, row, emergency, invocation, expected) in warning_cases {
        let snapshot = evaluate_handoff(
            row,
            BuildAdmissionMode::Enforce,
            emergency,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation {
                enforcing: invocation,
            },
        );
        assert_eq!(
            snapshot.warning_gauges(
                emergency,
                InvocationAuthorityObservation {
                    enforcing: invocation
                },
            ),
            expected,
            "{name}"
        );
    }
    assert_eq!(
        evaluate_handoff(
            Ok(Some(current)),
            BuildAdmissionMode::Enforce,
            true,
            BuildAdmissionReadiness::Healthy,
            InvocationAuthorityObservation::default(),
        )
        .state,
        HandoffState::IncompleteEpoch,
    );
}

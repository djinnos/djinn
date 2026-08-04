//! Additive, redaction-safe provider-attempt admission vocabulary.
//!
//! This module deliberately plans only from an already serialized request body.
//! It never retains that body (which can contain user content) or credential
//! material. Slot acquisition, lease ownership, and retry ownership remain
//! outside this provider-side contract.

use std::collections::BTreeSet;
use std::fmt;

use djinn_db::{ModelTurnAuthoritativeUsage, ModelTurnBucketDebit, ModelTurnBucketKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// The largest output reservation accepted by the v1 admission contract.
pub const MAX_OUTPUT_RESERVATION_UNITS_V1: i64 = 16_384;

/// A stable, non-reversible reference to a durable credential record.
///
/// The durable credential row ID is intentionally accepted only at construction
/// time and is not retained by this type's `Debug` or serialization forms.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCredentialRecordScopeV1 {
    fingerprint: String,
}

impl ProviderCredentialRecordScopeV1 {
    #[must_use]
    pub fn from_credential_record_id(credential_record_id: &str) -> Self {
        let fingerprint = format!(
            "sha256:{:x}",
            Sha256::digest(credential_record_id.as_bytes())
        );
        Self { fingerprint }
    }

    /// A non-reversible identifier safe for diagnostics and persisted plans.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl fmt::Debug for ProviderCredentialRecordScopeV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentialRecordScopeV1")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// The durable credential record and provider/model route for one attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderAttemptScopeV1 {
    pub credential: ProviderCredentialRecordScopeV1,
    pub provider_id: String,
    pub model_id: String,
}

/// The provider route's admission capabilities. A covered v1 route must make
/// both no-hidden-retry and abort behavior explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHiddenRetryCapabilityV1 {
    Disabled,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAbortCapabilityV1 {
    Supported,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderAttemptCapabilitiesV1 {
    pub hidden_retries: ProviderHiddenRetryCapabilityV1,
    pub abort: ProviderAbortCapabilityV1,
}

/// Whether a route may make predictive capacity claims before a response.
/// Gemini target-1 is deliberately reactive-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAdmissionPolicyV1 {
    Proactive,
    ReactiveOnlyTarget1,
}

/// A route is either fully covered by the additive v1 contract or explicitly
/// excluded from enforcement. `Uncovered` is fail-closed, never a zero debit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "reason")]
pub enum ProviderAttemptRouteCoverageV1 {
    Covered {
        capabilities: ProviderAttemptCapabilitiesV1,
        supported_bucket_bindings: Vec<ModelTurnBucketKind>,
        policy: ProviderAdmissionPolicyV1,
    },
    Uncovered(ProviderAttemptUncoveredReasonV1),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptUncoveredReasonV1 {
    SerializationUnavailable,
    InputEstimateOverflow,
    InvalidProviderInputEstimate,
    MissingOutputReservation,
    OutputReservationOverflow,
    CombinedReservationOverflow,
    MissingBucketBinding { bucket_kind: ModelTurnBucketKind },
    HiddenRetriesNotDisabled,
    AbortUnsupported,
    MissingCredentialRecordIdentity,
}

/// The source chosen for the output reservation, retained for auditability
/// without retaining the request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderOutputReservationSourceV1 {
    ExplicitLimit,
    ModelDefault,
}

/// An abort signal owned by the caller wrapping this one provider attempt.
/// It has no serializable or debug representation containing request identity.
#[derive(Clone)]
pub struct ProviderAttemptAbortHandleV1 {
    cancellation: CancellationToken,
}

impl ProviderAttemptAbortHandleV1 {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
        }
    }

    pub fn abort(&self) {
        self.cancellation.cancel();
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Default for ProviderAttemptAbortHandleV1 {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ProviderAttemptAbortHandleV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderAttemptAbortHandleV1(..)")
    }
}

/// An enforceable, conservative reservation for exactly one provider attempt.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderAttemptPlanV1 {
    pub scope: ProviderAttemptScopeV1,
    pub coverage: ProviderAttemptRouteCoverageV1,
    /// Request=1, input=conservative serialized-body estimate, output=selected
    /// output limit, and combined=input+output.
    pub debits: Vec<ModelTurnBucketDebit>,
    pub output_reservation_source: ProviderOutputReservationSourceV1,
    #[serde(skip_serializing)]
    pub abort: ProviderAttemptAbortHandleV1,
}

/// A terminal result for exactly one wrapped provider attempt. It intentionally
/// contains no raw request, lease, user, account, project, or credential IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderOutcomeV1 {
    pub terminal: ProviderAttemptTerminalV1,
    pub authoritative_usage: Option<ModelTurnAuthoritativeUsage>,
    pub abort: ProviderAttemptAbortResultV1,
    pub token_emission: ProviderTokenEmissionV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptTerminalV1 {
    Completed,
    Failed(ProviderAttemptLossV1),
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptLossV1 {
    Transport,
    ProviderRejected,
    RateLimited,
    EmptyTurn,
    CodexEmptyTurn,
    Protocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptAbortResultV1 {
    NotRequested,
    Requested,
    Confirmed,
    Unsupported,
}

/// Timing needed by later throughput windows, represented without request IDs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProviderTokenEmissionV1 {
    pub first_token_monotonic_ms: Option<u64>,
    pub last_token_monotonic_ms: Option<u64>,
}

/// Build a v1 plan from the exact serialized UTF-8 body sent by a provider
/// format. `None` means serialization was unavailable and is explicitly
/// uncovered. The provider estimate is optional because the byte fallback is
/// always conservative enough to cover formats without a tokenizer estimate.
pub fn plan_provider_attempt_v1(
    scope: ProviderAttemptScopeV1,
    serialized_request_utf8: Option<&[u8]>,
    provider_input_estimate: Option<i64>,
    explicit_output_limit: Option<i64>,
    model_output_default: Option<i64>,
    supported_bucket_bindings: impl IntoIterator<Item = ModelTurnBucketKind>,
    capabilities: ProviderAttemptCapabilitiesV1,
) -> Result<ProviderAttemptPlanV1, ProviderAttemptRouteCoverageV1> {
    plan_provider_attempt_with_policy_v1(
        scope,
        serialized_request_utf8,
        provider_input_estimate,
        explicit_output_limit,
        model_output_default,
        supported_bucket_bindings,
        capabilities,
        ProviderAdmissionPolicyV1::Proactive,
    )
}

/// Build a v1 plan with its predictive-capacity policy made explicit.
pub fn plan_provider_attempt_with_policy_v1(
    scope: ProviderAttemptScopeV1,
    serialized_request_utf8: Option<&[u8]>,
    provider_input_estimate: Option<i64>,
    explicit_output_limit: Option<i64>,
    model_output_default: Option<i64>,
    supported_bucket_bindings: impl IntoIterator<Item = ModelTurnBucketKind>,
    capabilities: ProviderAttemptCapabilitiesV1,
    policy: ProviderAdmissionPolicyV1,
) -> Result<ProviderAttemptPlanV1, ProviderAttemptRouteCoverageV1> {
    if capabilities.hidden_retries != ProviderHiddenRetryCapabilityV1::Disabled {
        return Err(ProviderAttemptRouteCoverageV1::Uncovered(
            ProviderAttemptUncoveredReasonV1::HiddenRetriesNotDisabled,
        ));
    }
    if capabilities.abort != ProviderAbortCapabilityV1::Supported {
        return Err(ProviderAttemptRouteCoverageV1::Uncovered(
            ProviderAttemptUncoveredReasonV1::AbortUnsupported,
        ));
    }

    let bindings: BTreeSet<_> = supported_bucket_bindings.into_iter().collect();
    let required_bindings = match policy {
        ProviderAdmissionPolicyV1::Proactive => vec![
            ModelTurnBucketKind::Request,
            ModelTurnBucketKind::Input,
            ModelTurnBucketKind::Output,
            ModelTurnBucketKind::Combined,
        ],
        ProviderAdmissionPolicyV1::ReactiveOnlyTarget1 => vec![ModelTurnBucketKind::Request],
    };
    for &bucket_kind in &required_bindings {
        if !bindings.contains(&bucket_kind) {
            return Err(ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::MissingBucketBinding { bucket_kind },
            ));
        }
    }

    let Some(serialized_request_utf8) = serialized_request_utf8 else {
        return Err(uncovered(
            ProviderAttemptUncoveredReasonV1::SerializationUnavailable,
        ));
    };
    let input_units = match policy {
        ProviderAdmissionPolicyV1::Proactive => {
            match conservative_input_units(serialized_request_utf8, provider_input_estimate) {
                Ok(value) => Some(value),
                Err(reason) => return Err(uncovered(reason)),
            }
        }
        ProviderAdmissionPolicyV1::ReactiveOnlyTarget1 => None,
    };
    let (output_units, output_reservation_source) = match policy {
        ProviderAdmissionPolicyV1::Proactive => {
            match output_reservation(explicit_output_limit, model_output_default) {
                Ok((value, source)) => (Some(value), source),
                Err(reason) => return Err(uncovered(reason)),
            }
        }
        ProviderAdmissionPolicyV1::ReactiveOnlyTarget1 => {
            (None, ProviderOutputReservationSourceV1::ModelDefault)
        }
    };
    let combined_units = match (input_units, output_units) {
        (Some(input), Some(output)) => match input.checked_add(output) {
            Some(value) => Some(value),
            None => {
                return Err(uncovered(
                    ProviderAttemptUncoveredReasonV1::CombinedReservationOverflow,
                ));
            }
        },
        _ => None,
    };

    Ok(ProviderAttemptPlanV1 {
        scope,
        coverage: ProviderAttemptRouteCoverageV1::Covered {
            capabilities,
            supported_bucket_bindings: bindings.into_iter().collect(),
            policy,
        },
        debits: required_bindings
            .into_iter()
            .map(|bucket_kind| ModelTurnBucketDebit {
                units: match bucket_kind {
                    ModelTurnBucketKind::Request => 1,
                    ModelTurnBucketKind::Input => input_units.expect("proactive input binding"),
                    ModelTurnBucketKind::Output => output_units.expect("proactive output binding"),
                    ModelTurnBucketKind::Combined => {
                        combined_units.expect("proactive combined binding")
                    }
                },
                bucket_kind,
            })
            .collect(),
        output_reservation_source,
        abort: ProviderAttemptAbortHandleV1::new(),
    })
}

fn uncovered(reason: ProviderAttemptUncoveredReasonV1) -> ProviderAttemptRouteCoverageV1 {
    ProviderAttemptRouteCoverageV1::Uncovered(reason)
}

fn conservative_input_units(
    serialized_request_utf8: &[u8],
    provider_input_estimate: Option<i64>,
) -> Result<i64, ProviderAttemptUncoveredReasonV1> {
    let byte_count = i64::try_from(serialized_request_utf8.len())
        .map_err(|_| ProviderAttemptUncoveredReasonV1::InputEstimateOverflow)?;
    let fallback = byte_count
        .checked_add(2)
        .ok_or(ProviderAttemptUncoveredReasonV1::InputEstimateOverflow)?
        / 3;
    let provider_estimate = match provider_input_estimate {
        Some(value) if value < 0 => {
            return Err(ProviderAttemptUncoveredReasonV1::InvalidProviderInputEstimate);
        }
        Some(value) => value,
        None => 0,
    };
    let estimate = fallback.max(provider_estimate);
    estimate
        .checked_mul(115)
        .and_then(|value| value.checked_add(99))
        .map(|value| value / 100)
        .ok_or(ProviderAttemptUncoveredReasonV1::InputEstimateOverflow)
}

fn output_reservation(
    explicit_output_limit: Option<i64>,
    model_output_default: Option<i64>,
) -> Result<(i64, ProviderOutputReservationSourceV1), ProviderAttemptUncoveredReasonV1> {
    let selected = explicit_output_limit
        .map(|value| (value, ProviderOutputReservationSourceV1::ExplicitLimit))
        .or_else(|| {
            model_output_default
                .map(|value| (value, ProviderOutputReservationSourceV1::ModelDefault))
        });
    let Some((output, source)) = selected else {
        return Err(ProviderAttemptUncoveredReasonV1::MissingOutputReservation);
    };
    if output <= 0 {
        return Err(ProviderAttemptUncoveredReasonV1::OutputReservationOverflow);
    }
    Ok((output.min(MAX_OUTPUT_RESERVATION_UNITS_V1), source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ProviderAttemptScopeV1 {
        ProviderAttemptScopeV1 {
            credential: ProviderCredentialRecordScopeV1::from_credential_record_id(
                "credential-raw",
            ),
            provider_id: "openai".to_string(),
            model_id: "gpt-test".to_string(),
        }
    }

    fn capabilities() -> ProviderAttemptCapabilitiesV1 {
        ProviderAttemptCapabilitiesV1 {
            hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
            abort: ProviderAbortCapabilityV1::Supported,
        }
    }

    fn all_bindings() -> [ModelTurnBucketKind; 4] {
        [
            ModelTurnBucketKind::Request,
            ModelTurnBucketKind::Input,
            ModelTurnBucketKind::Output,
            ModelTurnBucketKind::Combined,
        ]
    }

    #[test]
    fn planning_uses_exact_bytes_provider_max_uplift_and_output_cap() {
        let plan = plan_provider_attempt_v1(
            scope(),
            Some("é".as_bytes()),
            Some(10),
            Some(20_000),
            Some(64),
            all_bindings(),
            capabilities(),
        )
        .expect("covered plan");

        assert_eq!(
            plan.debits,
            vec![
                ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Request,
                    units: 1
                },
                ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Input,
                    units: 12
                },
                ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Output,
                    units: 16_384
                },
                ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Combined,
                    units: 16_396
                },
            ]
        );
        assert_eq!(
            plan.output_reservation_source,
            ProviderOutputReservationSourceV1::ExplicitLimit
        );
    }

    #[test]
    fn planning_uses_ceiling_byte_fallback_and_model_default() {
        let plan = plan_provider_attempt_v1(
            scope(),
            Some(b"rust"),
            None,
            None,
            Some(40),
            all_bindings(),
            capabilities(),
        )
        .expect("covered plan");
        assert_eq!(plan.debits[1].units, 3); // ceil(4 / 3), then ceil(2 * 1.15)
        assert_eq!(plan.debits[2].units, 40);
        assert_eq!(
            plan.output_reservation_source,
            ProviderOutputReservationSourceV1::ModelDefault
        );
    }

    #[test]
    fn missing_or_uncomputable_values_are_explicitly_uncovered() {
        let unavailable = plan_provider_attempt_v1(
            scope(),
            None,
            None,
            Some(1),
            None,
            all_bindings(),
            capabilities(),
        );
        assert!(matches!(
            unavailable,
            Err(ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::SerializationUnavailable
            ))
        ));

        let missing_output = plan_provider_attempt_v1(
            scope(),
            Some(b"{}"),
            None,
            None,
            None,
            all_bindings(),
            capabilities(),
        );
        assert!(matches!(
            missing_output,
            Err(ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::MissingOutputReservation
            ))
        ));

        let overflowing_input = plan_provider_attempt_v1(
            scope(),
            Some(b"{}"),
            Some(i64::MAX),
            Some(1),
            None,
            all_bindings(),
            capabilities(),
        );
        assert!(matches!(
            overflowing_input,
            Err(ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::InputEstimateOverflow
            ))
        ));

        let invalid_output = plan_provider_attempt_v1(
            scope(),
            Some(b"{}"),
            None,
            Some(-1),
            None,
            all_bindings(),
            capabilities(),
        );
        assert!(matches!(
            invalid_output,
            Err(ProviderAttemptRouteCoverageV1::Uncovered(
                ProviderAttemptUncoveredReasonV1::OutputReservationOverflow
            ))
        ));
    }

    #[test]
    fn diagnostics_and_serialization_do_not_expose_forbidden_identifiers() {
        let plan = plan_provider_attempt_v1(
            scope(),
            Some(b"{}"),
            None,
            Some(1),
            None,
            all_bindings(),
            capabilities(),
        )
        .expect("covered plan");
        let outcome = ProviderOutcomeV1 {
            terminal: ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::CodexEmptyTurn),
            authoritative_usage: None,
            abort: ProviderAttemptAbortResultV1::NotRequested,
            token_emission: ProviderTokenEmissionV1::default(),
        };
        let diagnostics = format!(
            "{plan:?} {outcome:?} {} {}",
            serde_json::to_string(&plan).unwrap(),
            serde_json::to_string(&outcome).unwrap()
        );
        for forbidden in [
            "credential-raw",
            "request-raw",
            "lease-raw",
            "user-raw",
            "account-raw",
            "project-raw",
            "secret-raw",
        ] {
            assert!(
                !diagnostics.contains(forbidden),
                "diagnostic exposed {forbidden}"
            );
        }
    }
}

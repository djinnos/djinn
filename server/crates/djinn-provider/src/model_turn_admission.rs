//! Additive, redaction-safe provider-attempt admission vocabulary.
//!
//! This module deliberately plans only from an already serialized request body.
//! It never retains that body (which can contain user content) or credential
//! material. Slot acquisition, lease ownership, and retry ownership remain
//! outside this provider-side contract.

use std::collections::BTreeSet;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use djinn_db::{ModelTurnAuthoritativeUsage, ModelTurnBucketDebit, ModelTurnBucketKind};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
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

/// Receipt clocks injected at the transport boundary. The monotonic value is an
/// opaque millisecond counter so normalized values are deterministic and safe
/// to persist without retaining an `Instant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderReceiptTimeV1 {
    pub wall: SystemTime,
    pub monotonic_ms: u64,
}

/// Authoritative provider usage before reconciliation into the Phase A
/// request/input/output/combined vocabulary. It contains counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderUsageObservationV1 {
    pub input_units: Option<i64>,
    pub output_units: Option<i64>,
    pub combined_units: Option<i64>,
}

/// Bounded, redaction-safe reasons for ignoring an observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderObservationIgnoreReasonV1 {
    Malformed,
    Stale,
    Regressing,
    Incomplete,
    Impossible,
}

/// Saturating diagnostic counters; raw header names and values are never kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProviderObservationDiagnosticsV1 {
    pub malformed: u32,
    pub stale: u32,
    pub regressing: u32,
    pub incomplete: u32,
    pub impossible: u32,
}

impl ProviderObservationDiagnosticsV1 {
    fn record(&mut self, reason: ProviderObservationIgnoreReasonV1) {
        let counter = match reason {
            ProviderObservationIgnoreReasonV1::Malformed => &mut self.malformed,
            ProviderObservationIgnoreReasonV1::Stale => &mut self.stale,
            ProviderObservationIgnoreReasonV1::Regressing => &mut self.regressing,
            ProviderObservationIgnoreReasonV1::Incomplete => &mut self.incomplete,
            ProviderObservationIgnoreReasonV1::Impossible => &mut self.impossible,
        };
        *counter = counter.saturating_add(1);
    }
}

/// Available capacity in the Phase A bucket vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderAvailableCapacityV1 {
    pub request_units: i64,
    pub input_units: i64,
    pub output_units: i64,
    pub combined_units: i64,
}

/// Cold pools permit one discovery response owner, compatible with Phase A's
/// `DiscoveryRequired` state. Repository acquisition is intentionally absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDiscoveryOwnershipV1 {
    DiscoveryRequired,
    DiscoveryOwned { request_sequence: u64 },
    Known,
}

/// Sanitized outcome of one API-key response. It retains no raw headers,
/// request IDs, account IDs, credentials, URLs, or bodies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderNormalizedObservationV1 {
    pub authoritative_usage: Option<ModelTurnAuthoritativeUsage>,
    pub available_capacity: Option<ProviderAvailableCapacityV1>,
    pub reset_epoch: Option<u64>,
    pub retry_after_deadline_monotonic_ms: Option<u64>,
    pub ignored: Option<ProviderObservationIgnoreReasonV1>,
    pub diagnostics: ProviderObservationDiagnosticsV1,
    pub discovery: ProviderDiscoveryOwnershipV1,
}

/// Stateful normalizer for API-key response observations. Raw headers are
/// consumed only by [`Self::observe`] and are never stored in this type.
#[derive(Clone, Debug, Default)]
pub struct ProviderApiKeyNormalizerV1 {
    last_sequence: Option<u64>,
    reset_epoch: Option<u64>,
    capacity: Option<ProviderAvailableCapacityV1>,
    diagnostics: ProviderObservationDiagnosticsV1,
    discovery: Option<u64>,
    reactive_only: bool,
}

impl ProviderApiKeyNormalizerV1 {
    #[must_use]
    pub fn new(policy: ProviderAdmissionPolicyV1) -> Self {
        Self {
            reactive_only: policy == ProviderAdmissionPolicyV1::ReactiveOnlyTarget1,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn discovery_ownership(&self) -> ProviderDiscoveryOwnershipV1 {
        match (self.capacity, self.discovery) {
            (Some(_), _) => ProviderDiscoveryOwnershipV1::Known,
            (None, Some(request_sequence)) => {
                ProviderDiscoveryOwnershipV1::DiscoveryOwned { request_sequence }
            }
            (None, None) => ProviderDiscoveryOwnershipV1::DiscoveryRequired,
        }
    }

    pub fn claim_discovery(&mut self, request_sequence: u64) -> ProviderDiscoveryOwnershipV1 {
        if self.capacity.is_none() && self.discovery.is_none() {
            self.discovery = Some(request_sequence);
        }
        self.discovery_ownership()
    }

    /// Supported capacity headers are
    /// `x-ratelimit-remaining-{requests,input-tokens,output-tokens,tokens}` and
    /// `x-ratelimit-reset` (an unsigned reset epoch). Header matching is ASCII
    /// case-insensitive. `retry-after` accepts delta seconds and HTTP-date.
    pub fn observe(
        &mut self,
        request_sequence: u64,
        headers: &[(&str, &str)],
        usage: ProviderUsageObservationV1,
        receipt: ProviderReceiptTimeV1,
    ) -> ProviderNormalizedObservationV1 {
        let retry_after_deadline_monotonic_ms = match retry_after_deadline(headers, receipt) {
            Ok(value) => value,
            Err(reason) => return self.ignored(reason, None),
        };
        let authoritative_usage = match normalize_usage(usage) {
            Ok(value) => value,
            Err(reason) => return self.ignored(reason, retry_after_deadline_monotonic_ms),
        };
        let parsed = match parse_capacity_headers(headers) {
            Ok(value) => value,
            Err(reason) => return self.ignored(reason, retry_after_deadline_monotonic_ms),
        };
        if let Some((epoch, _)) = parsed
            && self.reset_epoch.is_some_and(|previous| epoch < previous)
        {
            return self.ignored(
                ProviderObservationIgnoreReasonV1::Regressing,
                retry_after_deadline_monotonic_ms,
            );
        }

        if let Some((epoch, candidate)) = parsed {
            let reset_transition = self.reset_epoch != Some(epoch);
            if reset_transition {
                // A larger explicit epoch authoritatively starts a new window;
                // its capacity is not compared to the preceding window.
                self.reset_epoch = Some(epoch);
                self.last_sequence = Some(request_sequence);
                if !self.reactive_only {
                    self.capacity = Some(candidate);
                    self.discovery = None;
                }
            } else if self
                .last_sequence
                .is_none_or(|last| request_sequence > last)
            {
                // A newer response may report either direction for every
                // bucket. Keep its sequence as the epoch's growth watermark.
                self.last_sequence = Some(request_sequence);
                if !self.reactive_only {
                    self.capacity = Some(candidate);
                    self.discovery = None;
                }
            } else if self.reactive_only {
                return self.ignored(
                    ProviderObservationIgnoreReasonV1::Stale,
                    retry_after_deadline_monotonic_ms,
                );
            } else {
                // An out-of-order response cannot increase enforceable
                // capacity, but each bucket may still safely decrease. Do not
                // move the growth watermark backwards after applying a lower
                // observation.
                let capacity = self.capacity.unwrap_or(candidate);
                if !capacity_decreases(candidate, capacity) {
                    return self.ignored(
                        ProviderObservationIgnoreReasonV1::Stale,
                        retry_after_deadline_monotonic_ms,
                    );
                }
                self.capacity = Some(capacity_min(candidate, capacity));
                self.discovery = None;
            }
        }
        self.outcome(
            authoritative_usage,
            if self.reactive_only {
                None
            } else {
                self.capacity
            },
            retry_after_deadline_monotonic_ms,
            None,
        )
    }

    fn ignored(
        &mut self,
        reason: ProviderObservationIgnoreReasonV1,
        retry_after_deadline_monotonic_ms: Option<u64>,
    ) -> ProviderNormalizedObservationV1 {
        self.diagnostics.record(reason);
        self.outcome(None, None, retry_after_deadline_monotonic_ms, Some(reason))
    }

    fn outcome(
        &self,
        authoritative_usage: Option<ModelTurnAuthoritativeUsage>,
        available_capacity: Option<ProviderAvailableCapacityV1>,
        retry_after_deadline_monotonic_ms: Option<u64>,
        ignored: Option<ProviderObservationIgnoreReasonV1>,
    ) -> ProviderNormalizedObservationV1 {
        ProviderNormalizedObservationV1 {
            authoritative_usage,
            available_capacity,
            reset_epoch: self.reset_epoch,
            retry_after_deadline_monotonic_ms,
            ignored,
            diagnostics: self.diagnostics,
            discovery: self.discovery_ownership(),
        }
    }
}

fn capacity_decreases(
    candidate: ProviderAvailableCapacityV1,
    current: ProviderAvailableCapacityV1,
) -> bool {
    candidate.request_units < current.request_units
        || candidate.input_units < current.input_units
        || candidate.output_units < current.output_units
        || candidate.combined_units < current.combined_units
}

fn capacity_min(
    candidate: ProviderAvailableCapacityV1,
    current: ProviderAvailableCapacityV1,
) -> ProviderAvailableCapacityV1 {
    ProviderAvailableCapacityV1 {
        request_units: candidate.request_units.min(current.request_units),
        input_units: candidate.input_units.min(current.input_units),
        output_units: candidate.output_units.min(current.output_units),
        combined_units: candidate.combined_units.min(current.combined_units),
    }
}

fn normalize_usage(
    usage: ProviderUsageObservationV1,
) -> Result<Option<ModelTurnAuthoritativeUsage>, ProviderObservationIgnoreReasonV1> {
    let fields = [usage.input_units, usage.output_units, usage.combined_units];
    if fields.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [Some(input_units), Some(output_units), Some(combined_units)] = fields else {
        return Err(ProviderObservationIgnoreReasonV1::Incomplete);
    };
    if input_units < 0
        || output_units < 0
        || combined_units < 0
        || input_units.checked_add(output_units) != Some(combined_units)
    {
        return Err(ProviderObservationIgnoreReasonV1::Impossible);
    }
    Ok(Some(ModelTurnAuthoritativeUsage {
        request_units: 1,
        input_units,
        output_units,
        combined_units,
    }))
}

fn parse_capacity_headers(
    headers: &[(&str, &str)],
) -> Result<Option<(u64, ProviderAvailableCapacityV1)>, ProviderObservationIgnoreReasonV1> {
    let value = |name: &str| {
        headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(*value))
    };
    let fields = [
        value("x-ratelimit-remaining-requests"),
        value("x-ratelimit-remaining-input-tokens"),
        value("x-ratelimit-remaining-output-tokens"),
        value("x-ratelimit-remaining-tokens"),
        value("x-ratelimit-reset"),
    ];
    if fields.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [
        Some(request),
        Some(input),
        Some(output),
        Some(combined),
        Some(epoch),
    ] = fields
    else {
        return Err(ProviderObservationIgnoreReasonV1::Incomplete);
    };
    let parse_i64 = |value: &str| {
        value
            .parse::<i64>()
            .map_err(|_| ProviderObservationIgnoreReasonV1::Malformed)
    };
    let capacity = ProviderAvailableCapacityV1 {
        request_units: parse_i64(request)?,
        input_units: parse_i64(input)?,
        output_units: parse_i64(output)?,
        combined_units: parse_i64(combined)?,
    };
    if capacity.request_units < 0
        || capacity.input_units < 0
        || capacity.output_units < 0
        || capacity.combined_units < 0
    {
        return Err(ProviderObservationIgnoreReasonV1::Impossible);
    }
    let epoch = epoch
        .parse::<u64>()
        .map_err(|_| ProviderObservationIgnoreReasonV1::Malformed)?;
    Ok(Some((epoch, capacity)))
}

fn retry_after_deadline(
    headers: &[(&str, &str)],
    receipt: ProviderReceiptTimeV1,
) -> Result<Option<u64>, ProviderObservationIgnoreReasonV1> {
    let value = headers
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case("retry-after").then_some(*value));
    let Some(value) = value else {
        return Ok(None);
    };
    let delay_ms = if let Ok(seconds) = value.parse::<u64>() {
        seconds
            .checked_mul(1_000)
            .ok_or(ProviderObservationIgnoreReasonV1::Impossible)?
    } else {
        let date = OffsetDateTime::parse(value, &Rfc2822)
            .map_err(|_| ProviderObservationIgnoreReasonV1::Malformed)?;
        let date_ms = u64::try_from(date.unix_timestamp())
            .map_err(|_| ProviderObservationIgnoreReasonV1::Impossible)?
            .checked_mul(1_000)
            .ok_or(ProviderObservationIgnoreReasonV1::Impossible)?;
        let receipt_ms = u64::try_from(
            receipt
                .wall
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ProviderObservationIgnoreReasonV1::Impossible)?
                .as_millis(),
        )
        .map_err(|_| ProviderObservationIgnoreReasonV1::Impossible)?;
        date_ms.saturating_sub(receipt_ms)
    };
    receipt
        .monotonic_ms
        .checked_add(delay_ms)
        .map(Some)
        .ok_or(ProviderObservationIgnoreReasonV1::Impossible)
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
// The individual fields deliberately mirror the admission boundary: keeping
// them separate makes it impossible for an adapter to replace exact wire bytes
// or route capabilities with an opaque request object.
#[allow(clippy::too_many_arguments)]
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

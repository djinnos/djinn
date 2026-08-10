use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Lifecycle of one immutable proposal build attempt.
///
/// A new attempt is first reserved against an exact observed main head, becomes
/// active once its branch/PR identity is established, and is retired when the
/// build is stopped or superseded. Attempts never return to an earlier state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalBuildAttemptLifecycle {
    Reserved,
    Active,
    Retired,
}

impl ProposalBuildAttemptLifecycle {
    pub const ALL: [Self; 3] = [Self::Reserved, Self::Active, Self::Retired];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

impl fmt::Display for ProposalBuildAttemptLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProposalBuildAttemptLifecycle {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            other => Err(format!("unknown proposal build attempt lifecycle: {other}")),
        }
    }
}

/// Machine-readable reason direct delivery was parked rather than retried.
///
/// This is deliberately closed: an unrecognised persisted reason must fail
/// parsing instead of silently being treated as a retryable condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectDeliveryParkReason {
    BranchIdentityMismatch,
    ProposalPrIdentityMismatch,
    UnexpectedBranchHead,
    MappedHeadRetryBound,
    DeliveryConflict,
    NoProposalOwner,
    CapabilityUnavailable,
    EpochDisabled,
    LeaseLost,
}

impl DirectDeliveryParkReason {
    pub const ALL: [Self; 9] = [
        Self::BranchIdentityMismatch,
        Self::ProposalPrIdentityMismatch,
        Self::UnexpectedBranchHead,
        Self::MappedHeadRetryBound,
        Self::DeliveryConflict,
        Self::NoProposalOwner,
        Self::CapabilityUnavailable,
        Self::EpochDisabled,
        Self::LeaseLost,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BranchIdentityMismatch => "branch_identity_mismatch",
            Self::ProposalPrIdentityMismatch => "proposal_pr_identity_mismatch",
            Self::UnexpectedBranchHead => "unexpected_branch_head",
            Self::MappedHeadRetryBound => "mapped_head_retry_bound",
            Self::DeliveryConflict => "delivery_conflict",
            Self::NoProposalOwner => "no_proposal_owner",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::EpochDisabled => "epoch_disabled",
            Self::LeaseLost => "lease_lost",
        }
    }
}

impl fmt::Display for DirectDeliveryParkReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DirectDeliveryParkReason {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "branch_identity_mismatch" => Ok(Self::BranchIdentityMismatch),
            "proposal_pr_identity_mismatch" => Ok(Self::ProposalPrIdentityMismatch),
            "unexpected_branch_head" => Ok(Self::UnexpectedBranchHead),
            "mapped_head_retry_bound" => Ok(Self::MappedHeadRetryBound),
            "delivery_conflict" => Ok(Self::DeliveryConflict),
            "no_proposal_owner" => Ok(Self::NoProposalOwner),
            "capability_unavailable" => Ok(Self::CapabilityUnavailable),
            "epoch_disabled" => Ok(Self::EpochDisabled),
            "lease_lost" => Ok(Self::LeaseLost),
            other => Err(format!("unknown direct delivery park reason: {other}")),
        }
    }
}

/// Persisted identity and lifecycle state for a proposal build attempt.
///
/// Repository code decodes the closed enum fields explicitly rather than
/// accepting arbitrary database strings through an untyped row mapping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalBuildAttempt {
    pub id: String,
    pub proposal_id: String,
    pub short_id: String,
    pub lifecycle: ProposalBuildAttemptLifecycle,
    /// Exact main SHA observed when the attempt's branch was reserved.
    pub base_sha: String,
    /// Current exact branch head, if publication has completed.
    pub branch_head_sha: Option<String>,
    pub branch_name: String,
    /// Exact draft proposal PR identity. Written once and retained after
    /// retirement so a later attempt cannot adopt a historical PR.
    pub proposal_pr_number: Option<i64>,
    pub proposal_pr_url: Option<String>,
    pub park_reason: Option<DirectDeliveryParkReason>,
    pub created_at: String,
    pub activated_at: Option<String>,
    pub retired_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_wire_round_trip<T>(value: T, wire: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
    {
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            format!("\"{wire}\"")
        );
        assert_eq!(
            serde_json::from_str::<T>(&format!("\"{wire}\"")).unwrap(),
            value
        );
    }

    #[test]
    fn attempt_lifecycle_wire_values_are_closed() {
        for value in ProposalBuildAttemptLifecycle::ALL {
            assert_wire_round_trip(value, value.as_str());
        }
        assert!("unknown".parse::<ProposalBuildAttemptLifecycle>().is_err());
        assert!(serde_json::from_str::<ProposalBuildAttemptLifecycle>("\"unknown\"").is_err());
    }

    #[test]
    fn park_reason_wire_values_are_closed() {
        for value in DirectDeliveryParkReason::ALL {
            assert_wire_round_trip(value, value.as_str());
        }
        assert!("unknown".parse::<DirectDeliveryParkReason>().is_err());
        assert!(serde_json::from_str::<DirectDeliveryParkReason>("\"unknown\"").is_err());
    }
}

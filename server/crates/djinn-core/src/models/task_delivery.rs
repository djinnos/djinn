use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Persisted activation state for the `direct_delivery_v1` epoch.
///
/// Nullable attempt, delivery, capability, or lease fields are not an activation
/// signal. Only the explicit `active` epoch permits direct-delivery writers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectDeliveryEpochState {
    #[default]
    Disabled,
    Active,
}

/// CAS identity for retrying a clean candidate after its expected-old ref update
/// lost to a ledger-mapped first-parent head. Source facts must stay unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappedHeadRetryDelivery {
    pub transition_id: String,
    pub build_attempt_id: String,
    pub task_id: String,
    pub expected_generation: i64,
    pub delivery_generation: i64,
}

impl MappedHeadRetryDelivery {
    pub fn new(
        transition_id: impl Into<String>,
        build_attempt_id: impl Into<String>,
        task_id: impl Into<String>,
        expected_generation: i64,
        delivery_generation: i64,
    ) -> Result<Self> {
        let result = Self {
            transition_id: transition_id.into(),
            build_attempt_id: build_attempt_id.into(),
            task_id: task_id.into(),
            expected_generation,
            delivery_generation,
        };
        require_nonblank("transition_id", &result.transition_id)?;
        require_nonblank("build_attempt_id", &result.build_attempt_id)?;
        require_nonblank("task_id", &result.task_id)?;
        require_positive("expected_generation", result.expected_generation)?;
        require_positive("delivery_generation", result.delivery_generation)?;
        if result.delivery_generation != result.expected_generation + 1 {
            return Err(Error::InvalidTransition(
                "mapped-head retry delivery_generation must be exactly expected_generation + 1"
                    .into(),
            ));
        }
        Ok(result)
    }
}

impl DirectDeliveryEpochState {
    pub const ALL: [Self; 2] = [Self::Disabled, Self::Active];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Active => "active",
        }
    }

    pub const fn permits_direct_delivery(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl fmt::Display for DirectDeliveryEpochState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DirectDeliveryEpochState {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "active" => Ok(Self::Active),
            other => Err(format!("unknown direct delivery epoch state: {other}")),
        }
    }
}

/// The explicitly persisted epoch fence. The name is fixed for schema and
/// mixed-version compatibility: `direct_delivery_v1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectDeliveryEpoch {
    pub name: String,
    pub state: DirectDeliveryEpochState,
    /// Monotonically allocated activation generation used by activation CAS.
    pub generation: i64,
}

impl Default for DirectDeliveryEpoch {
    fn default() -> Self {
        Self {
            name: "direct_delivery_v1".to_owned(),
            state: DirectDeliveryEpochState::Disabled,
            generation: 0,
        }
    }
}

impl DirectDeliveryEpoch {
    pub const NAME: &'static str = "direct_delivery_v1";

    pub fn new(state: DirectDeliveryEpochState, generation: i64) -> Result<Self> {
        if generation < 0 {
            return Err(Error::Internal(
                "direct delivery epoch generation must not be negative".into(),
            ));
        }
        Ok(Self {
            name: Self::NAME.to_owned(),
            state,
            generation,
        })
    }

    /// No inference is allowed from any neighbouring nullable persistence field.
    pub const fn permits_direct_delivery(&self) -> bool {
        self.state.permits_direct_delivery()
    }
}

/// A required implementation capability for activation of direct delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectDeliveryCapability {
    Schema,
    Provider,
    Repository,
    Orchestrator,
    ConsumerCutover,
}

impl DirectDeliveryCapability {
    pub const ALL: [Self; 5] = [
        Self::Schema,
        Self::Provider,
        Self::Repository,
        Self::Orchestrator,
        Self::ConsumerCutover,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Provider => "provider",
            Self::Repository => "repository",
            Self::Orchestrator => "orchestrator",
            Self::ConsumerCutover => "consumer_cutover",
        }
    }
}

impl fmt::Display for DirectDeliveryCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DirectDeliveryCapability {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "schema" => Ok(Self::Schema),
            "provider" => Ok(Self::Provider),
            "repository" => Ok(Self::Repository),
            "orchestrator" => Ok(Self::Orchestrator),
            "consumer_cutover" => Ok(Self::ConsumerCutover),
            other => Err(format!("unknown direct delivery capability: {other}")),
        }
    }
}

/// A process declaration used by the activation census.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectDeliveryCapabilityRecord {
    pub process_incarnation_id: String,
    pub capability: DirectDeliveryCapability,
    pub epoch_generation: i64,
    pub observed_at: String,
}

/// A leased delivery mutation ownership record. `epoch_generation` binds a lease
/// to an activation generation and prevents a stale process from writing after a
/// later activation decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectDeliveryLease {
    pub id: String,
    pub build_attempt_id: String,
    pub task_id: String,
    pub delivery_generation: i64,
    pub owner_incarnation_id: String,
    pub epoch_generation: i64,
    pub acquired_at: String,
    pub expires_at: String,
}

/// Immutable identity of one ledger generation.
///
/// No generation zero exists: positive generations make an omitted/default CAS
/// value invalid instead of accidentally addressing the first delivery.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskDeliveryIdentity {
    pub build_attempt_id: String,
    pub task_id: String,
    pub delivery_generation: i64,
}

impl TaskDeliveryIdentity {
    pub fn new(
        build_attempt_id: impl Into<String>,
        task_id: impl Into<String>,
        delivery_generation: i64,
    ) -> Result<Self> {
        let identity = Self {
            build_attempt_id: build_attempt_id.into(),
            task_id: task_id.into(),
            delivery_generation,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn validate(&self) -> Result<()> {
        require_nonblank("build_attempt_id", &self.build_attempt_id)?;
        require_nonblank("task_id", &self.task_id)?;
        require_positive("delivery_generation", self.delivery_generation)
    }
}

/// Immutable state of a task-delivery ledger generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDeliveryState {
    Prepared,
    Applying,
    Applied,
    Conflict,
    Superseded,
}

impl TaskDeliveryState {
    pub const ALL: [Self; 5] = [
        Self::Prepared,
        Self::Applying,
        Self::Applied,
        Self::Conflict,
        Self::Superseded,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Conflict => "conflict",
            Self::Superseded => "superseded",
        }
    }

    /// Terminal rows are historical facts. Retry creates a new generation;
    /// callers must never mutate a terminal row back into an applying state.
    pub const fn is_immutable(self) -> bool {
        matches!(self, Self::Conflict | Self::Applied | Self::Superseded)
    }
}

impl fmt::Display for TaskDeliveryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskDeliveryState {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "applying" => Ok(Self::Applying),
            "applied" => Ok(Self::Applied),
            "conflict" => Ok(Self::Conflict),
            "superseded" => Ok(Self::Superseded),
            other => Err(format!("unknown task delivery state: {other}")),
        }
    }
}

/// Persisted immutable ledger record for one candidate delivery generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDelivery {
    #[serde(flatten)]
    pub identity: TaskDeliveryIdentity,
    pub state: TaskDeliveryState,
    /// The deterministic candidate commit generated for this exact identity.
    pub candidate_sha: String,
    /// Immutable source revision and normalized patch identity used to prepare it.
    pub source_sha: String,
    pub patch_digest: String,
    pub selected_parent_sha: String,
    pub prepare_transition_id: String,
    pub base_sha: String,
    pub applied_at: Option<String>,
    pub conflict_reason: Option<String>,
    pub supersede_transition_id: Option<String>,
    pub created_at: String,
}

/// Typed system-only finalization input. This is intentionally separate from
/// `TransitionAction`: it may only be issued by repository/orchestrator code
/// after remote observation has proved that this exact candidate was applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIntegrated {
    pub identity: TaskDeliveryIdentity,
    pub candidate_sha: String,
    pub observed_applied_candidate_sha: String,
    pub merge_commit_sha: String,
}

impl TaskIntegrated {
    pub fn new(
        identity: TaskDeliveryIdentity,
        candidate_sha: impl Into<String>,
        observed_applied_candidate_sha: impl Into<String>,
        merge_commit_sha: impl Into<String>,
    ) -> Result<Self> {
        identity.validate()?;
        let candidate_sha = candidate_sha.into();
        let observed_applied_candidate_sha = observed_applied_candidate_sha.into();
        let merge_commit_sha = merge_commit_sha.into();
        require_nonblank("candidate_sha", &candidate_sha)?;
        require_nonblank(
            "observed_applied_candidate_sha",
            &observed_applied_candidate_sha,
        )?;
        require_nonblank("merge_commit_sha", &merge_commit_sha)?;
        if candidate_sha != observed_applied_candidate_sha {
            return Err(Error::InvalidTransition(
                "task integration requires observation of the exact delivery candidate".into(),
            ));
        }
        if candidate_sha != merge_commit_sha {
            return Err(Error::InvalidTransition(
                "task integration requires the merge commit to be the exact delivery candidate"
                    .into(),
            ));
        }
        Ok(Self {
            identity,
            candidate_sha,
            observed_applied_candidate_sha,
            merge_commit_sha,
        })
    }
}

/// Typed CAS input for a rework transition. The repository allocates the new
/// immutable generation only if `expected_generation` still names the conflict
/// generation observed by the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReworkDelivery {
    pub transition_id: String,
    pub build_attempt_id: String,
    pub task_id: String,
    pub expected_generation: i64,
    pub delivery_generation: i64,
}

impl ReworkDelivery {
    pub fn new(
        transition_id: impl Into<String>,
        build_attempt_id: impl Into<String>,
        task_id: impl Into<String>,
        expected_generation: i64,
        delivery_generation: i64,
    ) -> Result<Self> {
        let result = Self {
            transition_id: transition_id.into(),
            build_attempt_id: build_attempt_id.into(),
            task_id: task_id.into(),
            expected_generation,
            delivery_generation,
        };
        require_nonblank("transition_id", &result.transition_id)?;
        require_nonblank("build_attempt_id", &result.build_attempt_id)?;
        require_nonblank("task_id", &result.task_id)?;
        require_positive("expected_generation", result.expected_generation)?;
        require_positive("delivery_generation", result.delivery_generation)?;
        if result.delivery_generation <= result.expected_generation {
            return Err(Error::InvalidTransition(
                "rework delivery_generation must advance expected_generation".into(),
            ));
        }
        Ok(result)
    }
}

fn require_nonblank(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::InvalidTransition(format!("{name} must be nonblank")));
    }
    Ok(())
}

fn require_positive(name: &str, value: i64) -> Result<()> {
    if value <= 0 {
        return Err(Error::InvalidTransition(format!("{name} must be positive")));
    }
    Ok(())
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
    fn epoch_capability_and_delivery_state_wire_values_are_closed() {
        for value in DirectDeliveryEpochState::ALL {
            assert_wire_round_trip(value, value.as_str());
        }
        for value in DirectDeliveryCapability::ALL {
            assert_wire_round_trip(value, value.as_str());
        }
        for value in TaskDeliveryState::ALL {
            assert_wire_round_trip(value, value.as_str());
        }
        assert!(serde_json::from_str::<DirectDeliveryEpochState>("\"unknown\"").is_err());
        assert!(serde_json::from_str::<DirectDeliveryCapability>("\"unknown\"").is_err());
        assert!(serde_json::from_str::<TaskDeliveryState>("\"unknown\"").is_err());
    }

    #[test]
    fn epoch_is_disabled_by_default_and_never_inferred_from_identity() {
        let epoch = DirectDeliveryEpoch::default();
        assert_eq!(epoch.name, DirectDeliveryEpoch::NAME);
        assert!(!epoch.permits_direct_delivery());
        let identity = TaskDeliveryIdentity::new("attempt", "task", 1).unwrap();
        assert!(
            !epoch.permits_direct_delivery(),
            "{identity:?} must not activate delivery"
        );
    }

    #[test]
    fn task_integrated_requires_exact_positive_candidate_identity() {
        let identity = TaskDeliveryIdentity::new("attempt", "task", 1).unwrap();
        assert!(
            TaskIntegrated::new(identity.clone(), "candidate", "candidate", "candidate").is_ok()
        );
        assert!(TaskIntegrated::new(identity.clone(), "candidate", "other", "candidate").is_err());
        assert!(TaskIntegrated::new(identity.clone(), "candidate", "candidate", "merge").is_err());
        assert!(TaskIntegrated::new(identity, "candidate", "candidate", " ").is_err());
        assert!(TaskDeliveryIdentity::new("attempt", "task", 0).is_err());
    }

    #[test]
    fn rework_requires_transition_identity_and_generation_cas() {
        assert!(ReworkDelivery::new("transition", "attempt", "task", 1, 2).is_ok());
        assert!(ReworkDelivery::new(" ", "attempt", "task", 1, 2).is_err());
        assert!(ReworkDelivery::new("transition", "attempt", "task", 0, 2).is_err());
        assert!(ReworkDelivery::new("transition", "attempt", "task", 2, 2).is_err());
    }
}

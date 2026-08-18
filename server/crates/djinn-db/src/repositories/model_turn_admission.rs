//! Durable model-turn admission records, acquisition, and schema readiness.
//!
//! Admission serializes through Postgres; provider normalization and fenced
//! reconciliation remain owned by later phases.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::{Database, Result};

/// The durable model-turn admission schema revision understood by this binary.
pub const MODEL_TURN_ADMISSION_SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnAdmissionPhase {
    Off,
    Shadow,
    Draining,
    Enforce,
}

/// A requested debit of a persisted admission bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnBucketDebit {
    pub bucket_kind: ModelTurnBucketKind,
    pub units: i64,
}

/// Input to the atomic model-turn acquisition operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnAcquireInput {
    pub pool_id: i64,
    pub request_id: String,
    pub owner_pod_uid: Option<String>,
    /// Generation is copied once into the new lease and is never updated.
    pub generation: i64,
    pub debits: Vec<ModelTurnBucketDebit>,
}

pub type ModelTurnAcquireTurnInput = ModelTurnAcquireInput;

/// A durable condition which may change and permits a later retry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnAdmissionWait {
    Concurrency {
        target: i64,
        in_flight: i64,
    },
    ResetAt {
        bucket_kind: ModelTurnBucketKind,
        reset_at: String,
    },
    /// Capability discovery is durably assigned to exactly one request while
    /// the pool remains unknown. The owner must publish a capability result;
    /// non-owners retry after that state changes.
    DiscoveryRequired {
        owner_request_id: String,
        is_owner: bool,
    },
    Draining,
    BindingUnavailable {
        bucket_kind: ModelTurnBucketKind,
    },
    BucketUnavailable {
        bucket_kind: ModelTurnBucketKind,
        available_units: i64,
        required_units: i64,
        reset_at: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnAdmissionRejection {
    PoolUnavailable,
    Off,
    ShadowOnly,
    IneligibleIdentity { state: ModelTurnIdentityState },
    UnsupportedCapability { state: ModelTurnCapabilityState },
    InvalidRequest,
    RequestConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnAcquireOutcome {
    Admitted {
        reservation: ModelTurnReservation,
        lease: ModelTurnLease,
        idempotent: bool,
    },
    Wait(ModelTurnAdmissionWait),
    Rejected(ModelTurnAdmissionRejection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnIdentityState {
    Eligible,
    Revoked,
    Ambiguous,
    Colliding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnCapabilityState {
    Unknown,
    Supported,
    Unsupported,
    Degraded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnBucketKind {
    Request,
    Input,
    Output,
    Combined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnReservationState {
    Reserved,
    Dispatched,
    Reconciled,
    Expired,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnLeaseLifecycle {
    Reserved,
    Dispatching,
    Active,
    Reconciled,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnLeaseTerminalOutcome {
    Completed,
    Cancelled,
    Expired,
    Failed,
}

/// Result of a fenced lease mutation. `Fenced` never changes another lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnLeaseMutationOutcome {
    Applied,
    Idempotent,
    Fenced,
}

/// The exact observation a watchdog must compare before expiring a lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLeaseExpiryInput {
    pub identity: ModelTurnLeaseIdentity,
    pub observed_lifecycle: ModelTurnLeaseLifecycle,
    pub observed_heartbeat_at: Option<String>,
    pub boundary_at: String,
}

/// Provider usage which replaces the reservation estimate at reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnAuthoritativeUsage {
    pub request_units: i64,
    pub input_units: i64,
    pub output_units: i64,
    pub combined_units: i64,
}

/// One fenced terminal decision. Missing usage quarantines possibly-sent spend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLeaseReconciliationInput {
    pub identity: ModelTurnLeaseIdentity,
    pub outcome: ModelTurnLeaseTerminalOutcome,
    pub authoritative_usage: Option<ModelTurnAuthoritativeUsage>,
    pub detail: Option<String>,
}

/// A pool is keyed exclusively by the durable credential row and provider/model
/// scope. It intentionally carries neither credential material nor user IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnPool {
    pub id: i64,
    pub credential_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub phase: ModelTurnAdmissionPhase,
    pub identity_state: ModelTurnIdentityState,
    pub capability_state: ModelTurnCapabilityState,
    pub learned_concurrency: i64,
    pub in_flight: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnBucketBinding {
    pub pool_id: i64,
    pub bucket_kind: ModelTurnBucketKind,
    pub capacity_units: i64,
    pub available_units: i64,
    pub authoritative_epoch: i64,
    pub quarantined_units: i64,
    pub observation_sequence: i64,
    pub reset_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnReservation {
    pub id: String,
    pub pool_id: i64,
    pub request_id: String,
    pub state: ModelTurnReservationState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnReservationBucket {
    pub reservation_id: String,
    pub pool_id: i64,
    pub bucket_kind: ModelTurnBucketKind,
    pub reserved_units: i64,
}

/// A random lease id paired with its immutable generation and stable request
/// identity. `new` is the only constructor in Phase A and does not persist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLeaseIdentity {
    pub lease_id: String,
    pub generation: i64,
    pub request_id: String,
}

impl ModelTurnLeaseIdentity {
    #[must_use]
    pub fn new(generation: i64, request_id: impl Into<String>) -> Self {
        Self {
            lease_id: uuid::Uuid::new_v4().to_string(),
            generation,
            request_id: request_id.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLease {
    pub identity: ModelTurnLeaseIdentity,
    pub pool_id: i64,
    pub reservation_id: String,
    pub owner_pod_uid: Option<String>,
    pub lifecycle: ModelTurnLeaseLifecycle,
    pub heartbeat_at: Option<String>,
}

/// Closed, redaction-safe reason code persisted with a Phase-C window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnControllerWindowDiagnosticCode {
    EmptyExpectedDenominator,
    MissingCapability,
    UnexpectedCapability,
    DuplicateCapability,
    UncoveredCapability,
    PartialCapabilityCoverage,
    StaleHeartbeat,
    UnknownAttemptPath,
    MissingUsage,
    ExpiredLease,
    OpenBreaker,
    MissingStage,
    DuplicateStage,
    MissingStageOutcome,
    StageOutsideWindow,
    ReversedStages,
    InvalidStageOutcome,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelTurnControllerWindowDiagnostic {
    pub pool_id: i64,
    pub code: ModelTurnControllerWindowDiagnosticCode,
}
/// Labels are correlations only; the coordinator's live catalog owns authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTurnControllerWindowSummary {
    pub provider_id: String,
    pub model_id: String,
    pub trainable: bool,
    pub diagnostics: Vec<ModelTurnControllerWindowDiagnostic>,
}
/// The durable leadership fence a controller write must clear.
///
/// It is the existing coordinator-incarnation lease, not a competing leadership
/// mechanism: `incarnation_id` is the writer's own immutable incarnation and
/// `live_since_at` is the renewal floor below which that incarnation is
/// considered to have stopped renewing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnControllerFence {
    pub incarnation_id: String,
    pub live_since_at: String,
}

/// The upper bound this storage boundary accepts for a learned target.
///
/// The subscription controller's own contract is `[1, 32]`; this is the
/// storage-side sanity bound, deliberately looser so the two are not one
/// constant pretending to be two. Zero is rejected outright: `acquire_turn`
/// compares `learned_concurrency` against `in_flight`, so committing zero would
/// silently close a pool, and the only thing that may stop a pool admitting is
/// the mode ledger.
pub const MODEL_TURN_LEARNED_CONCURRENCY_MAX: i64 = 1_024;

/// One fenced write of a pool's learned concurrency target.
///
/// `learned_concurrency` is the target the subscription controller arrived at,
/// and `fence` is the same durable coordinator-incarnation lease every other
/// controller write has to clear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnLearnedConcurrencyInput {
    pub pool_id: i64,
    pub learned_concurrency: i64,
    /// The writing leader's controller generation.
    ///
    /// **It is not part of the fence, and it is not persisted.** It is
    /// validated (`>= 0`) and then goes nowhere:
    /// [`ModelTurnAdmissionRepository::apply_learned_concurrency`] binds
    /// `pool_id`, `learned_concurrency` and the two
    /// [`ModelTurnControllerFence`] fields, and nothing else. This field
    /// previously claimed that it was "carried so a caller cannot commit a
    /// target without naming the tick it came from", which was true only in the
    /// sense that the caller must type a number; adversarial verification of
    /// proposal `96fy` flagged the comment as asserting a fence that does not
    /// exist, and a doc comment claiming a guarantee is how the surrounding
    /// gaps stayed invisible.
    ///
    /// Contrast [`ModelTurnModeChangeInput::controller_generation`], which is
    /// persisted verbatim into `model_turn_pool_mode_transitions` and *is*
    /// attributable. Making this one attributable would need a column on
    /// `model_turn_pools` (or its own ledger) to write it to; until that
    /// exists, the only durable authority on this write is the incarnation
    /// fence.
    pub controller_generation: i64,
    pub fence: ModelTurnControllerFence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnControllerWindowInput {
    pub pool_id: i64,
    pub window_sequence: i64,
    pub started_at: String,
    pub ended_at: String,
    pub admitted_turns: i64,
    pub completed_turns: i64,
    pub summary: ModelTurnControllerWindowSummary,
    pub fence: ModelTurnControllerFence,
}
/// Exact-bound DB projection. It deliberately contains no diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnLearnerWindow {
    pub pool_id: i64,
    pub window_sequence: i64,
    pub started_at: String,
    pub ended_at: String,
    pub admitted_turns: i64,
    pub completed_turns: i64,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnControllerWindow {
    pub pool_id: i64,
    pub window_sequence: i64,
    pub admitted_turns: i64,
    pub completed_turns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnObservation {
    pub pool_id: i64,
    pub sequence: i64,
    pub kind: String,
    pub request_units: i64,
    pub input_units: i64,
    pub output_units: i64,
    /// A bounded, non-secret diagnostic string.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnAdmissionSchemaReadiness {
    pub model_turn_admission_schema: i64,
}

/// A bounded, redaction-safe record written at the slot send boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnDecisionRecordInput {
    pub pool_id: i64,
    pub request_fingerprint: String,
    pub generation: i64,
    pub decision: ModelTurnDecisionKind,
    /// Closed diagnostic vocabulary; arbitrary diagnostic text is deliberately
    /// unrepresentable at this durable boundary.
    pub diagnostic: Option<ModelTurnDecisionDiagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnDecisionKind {
    ShadowPermit,
    EnforceAdmitted,
    Wait,
    Rejected,
}

/// Stable, non-identifying decision diagnostics persisted by their canonical
/// code. There is intentionally no string/catch-all variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnDecisionDiagnostic {
    CapabilityUnknown,
    CapabilityUnsupported,
    PoolUnavailable,
    PolicyDraining,
    RequestInvalid,
}

impl ModelTurnDecisionDiagnostic {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::CapabilityUnknown => "capability_unknown",
            Self::CapabilityUnsupported => "capability_unsupported",
            Self::PoolUnavailable => "pool_unavailable",
            Self::PolicyDraining => "policy_draining",
            Self::RequestInvalid => "request_invalid",
        }
    }
}

/// Slot-bound, route-qualified report. It contains no request, lease, credential, or account identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnCapabilityHeartbeatInput {
    pub pool_id: i64,
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider_id: String,
    pub model_id: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnPhaseCEvidenceStage {
    Decision,
    Dispatch,
    Heartbeat,
    ProviderOutcome,
    Reconcile,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnPhaseCEvidenceOutcome {
    Recorded,
    Succeeded,
    Failed,
    Missing,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnPhaseCEvidenceInput {
    pub pool_id: i64,
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider_id: String,
    pub model_id: String,
    pub attempt_fingerprint: String,
    pub stage: ModelTurnPhaseCEvidenceStage,
    pub outcome: ModelTurnPhaseCEvidenceOutcome,
}

/// A bounded attempt-chain edge with the timestamp needed for an aligned,
/// half-open controller window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModelTurnPhaseCEvidence {
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider_id: String,
    pub model_id: String,
    pub attempt_fingerprint: String,
    pub stage: String,
    pub outcome: String,
    pub recorded_at: String,
}

// ── Phase D: the persisted A→B→C→D compatibility phase and its guard ───────

/// The persisted deployment compatibility phase of one pool.
///
/// This is deliberately **not** [`ModelTurnAdmissionPhase`], which is the
/// per-pool admission *mode* (`off|shadow|draining|enforce`). A pool carries
/// both: the mode says what admission does right now, the compatibility phase
/// says how far the A→B→C→D rollout has been proven for that pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnCompatibilityPhase {
    A,
    B,
    C,
    D,
}

impl ModelTurnCompatibilityPhase {
    /// The persisted spelling. The column is `VARCHAR(1)` with a CHECK.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
            Self::C => "c",
            Self::D => "d",
        }
    }

    /// The single phase this one may advance to, or `None` at the end.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::A => Some(Self::B),
            Self::B => Some(Self::C),
            Self::C => Some(Self::D),
            Self::D => None,
        }
    }
}

fn parse_compatibility_phase(value: &str) -> Result<ModelTurnCompatibilityPhase> {
    match value {
        "a" => Ok(ModelTurnCompatibilityPhase::A),
        "b" => Ok(ModelTurnCompatibilityPhase::B),
        "c" => Ok(ModelTurnCompatibilityPhase::C),
        "d" => Ok(ModelTurnCompatibilityPhase::D),
        other => Err(crate::Error::InvalidData(format!(
            "unknown model-turn compatibility phase `{other}`"
        ))),
    }
}

/// Every prerequisite a requested compatibility phase must satisfy.
///
/// The variant list *is* the closed key set of the persisted
/// `predicate_results` object; migration 211 enforces the same set with a
/// CHECK constraint, so the two cannot drift apart without a storage error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnPhasePredicate {
    /// The durable admission schema marker is installed at the version this
    /// binary understands.
    SchemaMarker,
    /// Every fresh capability report for the pool carries exactly the pool's
    /// own B1 route labels, and at least one such report exists.
    CapabilityReports,
    /// The requesting coordinator incarnation still holds a live, non-draining
    /// leadership lease at or after the fence's floor.
    LeadershipGeneration,
    /// Every attempt observed in the freshness window has a complete stage
    /// chain with no missing edge. An empty history fails closed.
    ObservationHistory,
    /// The set of routes reporting fresh coverage equals the caller's live
    /// expected-path set exactly, and is non-empty.
    ExpectedPathCoverage,
    /// The pool's durable identity state is `eligible`.
    IdentityEligibility,
}

impl ModelTurnPhasePredicate {
    /// The closed allow-list, in persisted key order.
    pub const ALL: [Self; 6] = [
        Self::SchemaMarker,
        Self::CapabilityReports,
        Self::LeadershipGeneration,
        Self::ObservationHistory,
        Self::ExpectedPathCoverage,
        Self::IdentityEligibility,
    ];

    /// The persisted JSON key. Bounded vocabulary; never an identifier.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::SchemaMarker => "schema_marker",
            Self::CapabilityReports => "capability_reports",
            Self::LeadershipGeneration => "leadership_generation",
            Self::ObservationHistory => "observation_history",
            Self::ExpectedPathCoverage => "expected_path_coverage",
            Self::IdentityEligibility => "identity_eligibility",
        }
    }
}

/// The freshness bound every ageing predicate is measured against.
pub const MODEL_TURN_PHASE_PREDICATE_FRESHNESS_SECONDS: i64 = 60;

/// One live expected attempt path, as the coordinator's own inventory sees it.
///
/// It is a slot identity plus a deployment revision — deliberately no request,
/// lease, credential, account, project, or user identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelTurnExpectedPathKey {
    pub slot_pod_uid: String,
    pub deployment_revision: String,
}

/// A request to make one compatibility phase effective for one pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnPhaseTransitionRequest {
    pub pool_id: i64,
    pub requested_phase: ModelTurnCompatibilityPhase,
    /// The requesting leader's controller generation, persisted verbatim.
    pub controller_generation: i64,
    /// The durable leadership fence the guard re-checks inside its own
    /// transaction, so a superseded generation cannot make a phase effective.
    pub fence: ModelTurnControllerFence,
    /// RFC 3339 instant every freshness bound is measured from. It is supplied
    /// rather than read from a local clock so a fake-time caller and a
    /// production caller evaluate the identical predicate.
    pub evaluated_at: String,
    /// The live expected attempt paths for this pool. Empty fails closed.
    pub expected_paths: Vec<ModelTurnExpectedPathKey>,
}

/// The persisted, closed-shape predicate verdict.
pub type ModelTurnPhasePredicateResults = BTreeMap<String, bool>;

/// What one guarded phase request actually did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelTurnPhaseTransitionOutcome {
    /// Every predicate held. Exactly one ledger row was written and
    /// `compatibility_phase` advanced by exactly one step.
    Advanced {
        effective_phase: ModelTurnCompatibilityPhase,
        predicate_results: ModelTurnPhasePredicateResults,
    },
    /// At least one predicate failed. One ledger row was written naming the
    /// failures; the effective phase is unchanged.
    Denied {
        effective_phase: ModelTurnCompatibilityPhase,
        failed: Vec<ModelTurnPhasePredicate>,
        predicate_results: ModelTurnPhasePredicateResults,
    },
    /// The pool already stands at the requested phase. No row is written, so
    /// re-issuing an accepted request is idempotent.
    AlreadyEffective {
        effective_phase: ModelTurnCompatibilityPhase,
    },
    /// The request would skip a prerequisite phase (or move backwards). No
    /// predicate is evaluated and **no row is written**: a phase cannot become
    /// effective without its predecessor having become effective first.
    NotAdjacent {
        effective_phase: ModelTurnCompatibilityPhase,
        requested_phase: ModelTurnCompatibilityPhase,
    },
    /// No such pool row.
    PoolUnavailable,
}

// ── Phase D: the per-pool admission-mode writer ────────────────────────────

/// Why a mode changed. A closed vocabulary, mirrored by migration 212's CHECK.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnModeChangeReason {
    OperatorRequest,
    CapabilityCoverageLoss,
    IdentityIneligible,
    Rollback,
    /// The last in-flight lease reached a terminal state, so a draining pool
    /// has nothing left to drain.
    DrainSettled,
    /// The leader's guarded enforcement pass advanced the pool.
    EnforcementAdvance,
}

impl ModelTurnModeChangeReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::OperatorRequest => "operator_request",
            Self::CapabilityCoverageLoss => "capability_coverage_loss",
            Self::IdentityIneligible => "identity_ineligible",
            Self::Rollback => "rollback",
            Self::DrainSettled => "drain_settled",
            Self::EnforcementAdvance => "enforcement_advance",
        }
    }
}

/// Why a requested mode was refused. Bounded; never a free-text diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnModeChangeRejection {
    /// `enforce` is reachable only from `shadow`, `draining` only from a pool
    /// that is actually admitting, and `off` only after a drain. Anything else
    /// is not an edge of the mode graph.
    UnsupportedTransition {
        from: ModelTurnAdmissionPhase,
        to: ModelTurnAdmissionPhase,
    },
    /// `enforce` demands a pool whose compatibility phase actually reached `d`
    /// through [`ModelTurnAdmissionRepository::request_phase_transition_in_transaction`].
    /// An uncovered or untrained pool never gets there, so it never enforces.
    CompatibilityPhaseInsufficient { phase: ModelTurnCompatibilityPhase },
    /// `enforce` demands a durably eligible identity.
    IdentityIneligible { state: ModelTurnIdentityState },
    /// A rollback step ran out of order — see [`ModelTurnRollbackPlanV1`].
    RollbackOutOfOrder {
        expected: ModelTurnRollbackStepV1,
        attempted: ModelTurnRollbackStepV1,
    },
    /// The last completed aligned window did not qualify, so there is nothing
    /// the pool has been shown to sustain. Given that Phase B never stored a
    /// capability *interval* or an authoritative usage column, this is the
    /// gate a production window is expected to fail — permanently, until that
    /// storage lands.
    WindowNotTrainable,
}

impl ModelTurnModeChangeRejection {
    /// The bounded code a caller may log or assert on. It carries no pool,
    /// credential, account, project, user, request, or lease identifier.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnsupportedTransition { .. } => "unsupported_transition",
            Self::CompatibilityPhaseInsufficient { .. } => "compatibility_phase_insufficient",
            Self::IdentityIneligible { .. } => "identity_ineligible",
            Self::RollbackOutOfOrder { .. } => "rollback_out_of_order",
            Self::WindowNotTrainable => "window_not_trainable",
        }
    }
}

/// What one mode write actually did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelTurnModeChangeOutcome {
    /// The mode moved and exactly one ledger row was appended.
    Applied {
        from: ModelTurnAdmissionPhase,
        to: ModelTurnAdmissionPhase,
        /// The instant the ledger row recorded, taken **after** the pool row
        /// was locked. Every lease admitted before this point is older than it.
        changed_at: String,
    },
    /// The pool already stands at the requested mode. No row is appended.
    Unchanged {
        mode: ModelTurnAdmissionPhase,
    },
    /// A drain that settled to `off` in the same transaction because there was
    /// nothing in flight to drain. Two ledger rows: the drain and the settle.
    DrainedAndSettled {
        changed_at: String,
    },
    Rejected(ModelTurnModeChangeRejection),
    PoolUnavailable,
}

/// The ordered teardown a Phase-D rollback must follow.
///
/// The order is the safety property, not documentation: the durable mode only
/// goes `off` once the layers that could still originate a turn are gone. The
/// enum's discriminant order *is* the required order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnRollbackStepV1 {
    /// Stop the leader-side controller so nothing re-targets the pool.
    Controller,
    /// Retire the slot wrappers that can still ask for a turn.
    SlotWrappers,
    /// Retire the provider contracts the wrappers were bound to.
    ProviderContracts,
    /// Only now may the durable mode go `off`.
    ModeOff,
}

impl ModelTurnRollbackStepV1 {
    /// The one legal sequence. A test pins this literal, so reordering the
    /// enum or this array is a visible change rather than a silent one.
    pub const ORDER: [Self; 4] = [
        Self::Controller,
        Self::SlotWrappers,
        Self::ProviderContracts,
        Self::ModeOff,
    ];
}

/// A rollback in progress. Steps may only be completed in
/// [`ModelTurnRollbackStepV1::ORDER`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelTurnRollbackPlanV1 {
    completed: usize,
}

impl ModelTurnRollbackPlanV1 {
    #[must_use]
    pub const fn new() -> Self {
        Self { completed: 0 }
    }

    /// The only step this plan will accept next, or `None` when it is done.
    #[must_use]
    pub fn next_step(&self) -> Option<ModelTurnRollbackStepV1> {
        ModelTurnRollbackStepV1::ORDER.get(self.completed).copied()
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completed == ModelTurnRollbackStepV1::ORDER.len()
    }

    /// Complete `step`, or refuse it because a prior step is still pending.
    ///
    /// # Errors
    ///
    /// Returns the expected/attempted pair when `step` is not the next step.
    pub fn complete(
        &mut self,
        step: ModelTurnRollbackStepV1,
    ) -> std::result::Result<(), ModelTurnModeChangeRejection> {
        match self.next_step() {
            Some(expected) if expected == step => {
                self.completed += 1;
                Ok(())
            }
            Some(expected) => Err(ModelTurnModeChangeRejection::RollbackOutOfOrder {
                expected,
                attempted: step,
            }),
            None => Err(ModelTurnModeChangeRejection::RollbackOutOfOrder {
                expected: ModelTurnRollbackStepV1::ModeOff,
                attempted: step,
            }),
        }
    }
}

/// One pool's slice of a leader enforcement pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnEnforcementPassInput {
    pub pool_id: i64,
    /// The live expected attempt paths for this pool: the coordinator's own
    /// workload inventory crossed with the durable dispatch topology.
    pub expected_paths: Vec<ModelTurnExpectedPathKey>,
    /// RFC 3339 instant every freshness bound is measured from.
    pub evaluated_at: String,
    /// The durable leadership fence, re-checked inside this transaction.
    pub fence: ModelTurnControllerFence,
    pub controller_generation: i64,
    /// The fail-closed qualifier's verdict for the last completed window.
    /// Only a qualifying window may advance a pool to `enforce`.
    pub window_trainable: bool,
}

/// What one pool's slice of the enforcement pass did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelTurnEnforcementOutcome {
    /// Leadership was lost, superseded, or is draining. Nothing was mutated.
    Fenced,
    /// Complete coverage was not observed, so the pool stopped admitting.
    Drained {
        from: ModelTurnAdmissionPhase,
        changed_at: String,
    },
    /// Every gate held; the pool now enforces.
    Enforced {
        changed_at: String,
    },
    /// A gate refused the advance. The pool is untouched.
    Denied(ModelTurnModeChangeRejection),
    /// Nothing to do for this pool.
    Unchanged {
        mode: ModelTurnAdmissionPhase,
    },
    PoolUnavailable,
}

/// A requested change to one pool's admission mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelTurnModeChangeInput {
    pub pool_id: i64,
    pub target_mode: ModelTurnAdmissionPhase,
    pub reason: ModelTurnModeChangeReason,
    pub controller_generation: i64,
}

/// The pool state one mode write locked and read.
struct LockedPoolModeState {
    phase: String,
    identity_state: String,
    compatibility_phase: String,
    in_flight: i64,
}

/// Every edge of the admission-mode graph. Notice the absent one: there is no
/// `enforce → off`, so an enforcing pool always passes through `draining`.
const fn mode_edge_allowed(from: ModelTurnAdmissionPhase, to: ModelTurnAdmissionPhase) -> bool {
    use ModelTurnAdmissionPhase::{Draining, Enforce, Off, Shadow};
    matches!(
        (from, to),
        (Off, Shadow)
            | (Shadow, Enforce)
            | (Shadow, Draining)
            | (Enforce, Draining)
            | (Shadow, Off)
            | (Draining, Off)
    )
}

/// Take the canonical admission locks: the pool row first, then its bucket
/// bindings ordered by `bucket_kind` — exactly `acquire_turn`'s order.
async fn lock_pool_for_mode_change(
    tx: &mut Transaction<'_, Postgres>,
    pool_id: i64,
) -> Result<Option<LockedPoolModeState>> {
    let pool: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT phase, identity_state, compatibility_phase, in_flight \
         FROM model_turn_pools WHERE id = $1 FOR UPDATE",
    )
    .bind(pool_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((phase, identity_state, compatibility_phase, in_flight)) = pool else {
        return Ok(None);
    };
    // The second lock class, in the same documented order acquisition uses.
    let _: Vec<(String,)> = sqlx::query_as(
        "SELECT bucket_kind FROM model_turn_bucket_bindings \
         WHERE pool_id = $1 ORDER BY bucket_kind FOR UPDATE",
    )
    .bind(pool_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(Some(LockedPoolModeState {
        phase,
        identity_state,
        compatibility_phase,
        in_flight,
    }))
}

/// Move the mode and append its ledger row inside an already-locked
/// transaction. Returns the instant the row recorded.
async fn apply_mode_change(
    tx: &mut Transaction<'_, Postgres>,
    pool_id: i64,
    from: ModelTurnAdmissionPhase,
    to: ModelTurnAdmissionPhase,
    reason: ModelTurnModeChangeReason,
    controller_generation: i64,
) -> Result<String> {
    let moved = sqlx::query(
        "UPDATE model_turn_pools SET phase = $2, updated_at = now() \
         WHERE id = $1 AND phase = $3",
    )
    .bind(pool_id)
    .bind(phase_name(to))
    .bind(phase_name(from))
    .execute(&mut **tx)
    .await?;
    if moved.rows_affected() != 1 {
        return Err(crate::Error::InvalidData(
            "model-turn pool mode moved under the writer".to_owned(),
        ));
    }
    sqlx::query_scalar(
        "INSERT INTO model_turn_pool_mode_transitions \
         (pool_id, from_mode, to_mode, reason, controller_generation) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING to_char(changed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .bind(pool_id)
    .bind(phase_name(from))
    .bind(phase_name(to))
    .bind(reason.code())
    .bind(controller_generation)
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}

/// Durable repository surface for the additive v1 schema.
#[derive(Clone)]
pub struct ModelTurnAdmissionRepository {
    db: Database,
}

#[derive(sqlx::FromRow)]
struct ModelTurnPoolRow {
    id: i64,
    credential_id: String,
    provider_id: String,
    model_id: String,
    phase: String,
    identity_state: String,
    capability_state: String,
    learned_concurrency: i64,
    in_flight: i64,
}

impl ModelTurnAdmissionRepository {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Report the installed schema marker. This does not activate admission.
    pub async fn schema_readiness(&self) -> Result<Option<ModelTurnAdmissionSchemaReadiness>> {
        self.db.ensure_initialized().await?;
        let version: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM model_turn_admission_schema \
             WHERE marker = 'model_turn_admission_schema'",
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(version.map(
            |model_turn_admission_schema| ModelTurnAdmissionSchemaReadiness {
                model_turn_admission_schema,
            },
        ))
    }

    /// Resolve an existing Phase A pool without creating a second ledger.
    pub async fn resolve_pool(
        &self,
        credential_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Option<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let row: Option<ModelTurnPoolRow> = sqlx::query_as(
            "SELECT id, credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight FROM model_turn_pools WHERE credential_id = $1 AND provider_id = $2 AND model_id = $3",
        ).bind(credential_id).bind(provider_id).bind(model_id).fetch_optional(self.db.pool()).await?;
        row.map(|row| {
            Ok(ModelTurnPool {
                id: row.id,
                credential_id: row.credential_id,
                provider_id: row.provider_id,
                model_id: row.model_id,
                phase: parse_phase(&row.phase)?,
                identity_state: parse_identity(&row.identity_state)?,
                capability_state: parse_capability(&row.capability_state)?,
                learned_concurrency: row.learned_concurrency,
                in_flight: row.in_flight,
            })
        })
        .transpose()
    }

    /// Resolve an existing pool through B1's opaque credential-record scope.
    /// The match is exact and unique, so callers cannot pick another credential
    /// merely because it shares the provider/model labels.
    pub async fn resolve_pool_by_credential_fingerprint(
        &self,
        credential_fingerprint: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Option<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let rows: Vec<ModelTurnPoolRow> = sqlx::query_as(
            "SELECT id, credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight FROM model_turn_pools WHERE provider_id = $1 AND model_id = $2 ORDER BY id",
        )
        .bind(provider_id)
        .bind(model_id)
        .fetch_all(self.db.pool())
        .await?;
        let mut matches = rows.into_iter().filter(|row| {
            use sha2::Digest;
            format!(
                "sha256:{:x}",
                sha2::Sha256::digest(row.credential_id.as_bytes())
            ) == credential_fingerprint
        });
        let Some(row) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(crate::Error::InvalidData(
                "ambiguous model-turn credential route".to_owned(),
            ));
        }
        Ok(Some(ModelTurnPool {
            id: row.id,
            credential_id: row.credential_id,
            provider_id: row.provider_id,
            model_id: row.model_id,
            phase: parse_phase(&row.phase)?,
            identity_state: parse_identity(&row.identity_state)?,
            capability_state: parse_capability(&row.capability_state)?,
            learned_concurrency: row.learned_concurrency,
            in_flight: row.in_flight,
        }))
    }

    /// Every pool the coordinator is durably observing for Phase C.
    ///
    /// A pool row *is* the coordinator's dispatch topology: it exists only
    /// because admission created it for one exact credential/provider/model
    /// route. `off` and `draining` pools are excluded — `off` has not been
    /// opted in, and a pool already draining has nothing left for a controller
    /// window to say. This is the topology half of the denominator; the live
    /// slot inventory is the other half, and neither is a report.
    pub async fn list_observable_pools(&self, limit: i64) -> Result<Vec<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let rows: Vec<ModelTurnPoolRow> = sqlx::query_as(
            "SELECT id, credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight \
             FROM model_turn_pools WHERE phase IN ('shadow', 'enforce') ORDER BY id LIMIT $1",
        )
        .bind(limit.clamp(1, 512))
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ModelTurnPool {
                    id: row.id,
                    credential_id: row.credential_id,
                    provider_id: row.provider_id,
                    model_id: row.model_id,
                    phase: parse_phase(&row.phase)?,
                    identity_state: parse_identity(&row.identity_state)?,
                    capability_state: parse_capability(&row.capability_state)?,
                    learned_concurrency: row.learned_concurrency,
                    in_flight: row.in_flight,
                })
            })
            .collect()
    }

    /// Persist a decision before returning a shadow send permit. Inputs contain
    /// only a one-way request fingerprint and bounded diagnostic vocabulary.
    pub async fn record_decision(&self, input: ModelTurnDecisionRecordInput) -> Result<()> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0
            || input.generation <= 0
            || !is_sha256_fingerprint(&input.request_fingerprint)
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn decision record".to_owned(),
            ));
        }
        sqlx::query("INSERT INTO model_turn_decisions (pool_id, request_fingerprint, generation, decision, diagnostic) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (pool_id, request_fingerprint, generation) DO NOTHING")
            .bind(input.pool_id).bind(input.request_fingerprint).bind(input.generation)
            .bind(decision_kind_name(input.decision))
            .bind(input.diagnostic.map(ModelTurnDecisionDiagnostic::code))
            .execute(self.db.pool()).await?;
        Ok(())
    }

    /// Persist only a live slot identity after deriving route labels from its admitted pool.
    pub async fn record_capability_heartbeat(
        &self,
        input: ModelTurnCapabilityHeartbeatInput,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        if !valid_phase_c_identity(
            input.pool_id,
            &input.slot_pod_uid,
            &input.deployment_revision,
            &input.provider_id,
            &input.model_id,
        ) {
            return invalid_phase_c();
        }
        let mut tx = self.db.pool().begin().await?;
        let result = sqlx::query("INSERT INTO model_turn_capability_heartbeats (pool_id, slot_pod_uid, deployment_revision, provider_id, model_id) SELECT id, $2, $3, provider_id, model_id FROM model_turn_pools WHERE id = $1 AND provider_id = $4 AND model_id = $5 ON CONFLICT (pool_id, slot_pod_uid, deployment_revision) DO UPDATE SET heartbeat_at = now(), provider_id = EXCLUDED.provider_id, model_id = EXCLUDED.model_id")
            .bind(input.pool_id).bind(&input.slot_pod_uid).bind(&input.deployment_revision).bind(&input.provider_id).bind(&input.model_id).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return invalid_phase_c();
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read the bounded, route-qualified heartbeat projection for coverage windows.
    pub async fn recent_capability_heartbeats(
        &self,
        pool_id: i64,
        limit: i64,
    ) -> Result<Vec<(String, String, String, String, String)>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("SELECT slot_pod_uid, deployment_revision, provider_id, model_id, heartbeat_at::text FROM model_turn_capability_heartbeats WHERE pool_id = $1 ORDER BY heartbeat_at DESC LIMIT $2")
            .bind(pool_id).bind(limit.clamp(1, 256)).fetch_all(self.db.pool()).await.map_err(Into::into)
    }

    /// Persist a closed-vocabulary attempt-chain edge only for the exact admitted route.
    pub async fn record_phase_c_evidence(&self, input: ModelTurnPhaseCEvidenceInput) -> Result<()> {
        self.db.ensure_initialized().await?;
        if !valid_phase_c_identity(
            input.pool_id,
            &input.slot_pod_uid,
            &input.deployment_revision,
            &input.provider_id,
            &input.model_id,
        ) || !is_sha256_fingerprint(&input.attempt_fingerprint)
        {
            return invalid_phase_c();
        }
        let mut tx = self.db.pool().begin().await?;
        let result = sqlx::query("INSERT INTO model_turn_phase_c_evidence (pool_id, slot_pod_uid, deployment_revision, provider_id, model_id, attempt_fingerprint, stage, outcome) SELECT id, $2, $3, provider_id, model_id, $6, $7, $8 FROM model_turn_pools WHERE id = $1 AND provider_id = $4 AND model_id = $5")
            .bind(input.pool_id).bind(&input.slot_pod_uid).bind(&input.deployment_revision).bind(&input.provider_id).bind(&input.model_id).bind(&input.attempt_fingerprint).bind(phase_c_stage_name(input.stage)).bind(phase_c_outcome_name(input.outcome)).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return invalid_phase_c();
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read recent bounded attempt evidence, including its persistence timestamp.
    pub async fn recent_phase_c_evidence(
        &self,
        pool_id: i64,
        limit: i64,
    ) -> Result<Vec<ModelTurnPhaseCEvidence>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("SELECT slot_pod_uid, deployment_revision, provider_id, model_id, attempt_fingerprint, stage, outcome, recorded_at::text AS recorded_at FROM model_turn_phase_c_evidence WHERE pool_id = $1 ORDER BY recorded_at DESC, id DESC LIMIT $2")
            .bind(pool_id).bind(limit.clamp(1, 256)).fetch_all(self.db.pool()).await.map_err(Into::into)
    }

    /// Read bounded evidence inside `[start_at, end_at)` for an aligned window.
    pub async fn phase_c_evidence_in_window(
        &self,
        pool_id: i64,
        start_at: &str,
        end_at: &str,
        limit: i64,
    ) -> Result<Vec<ModelTurnPhaseCEvidence>> {
        self.db.ensure_initialized().await?;
        if pool_id <= 0 || start_at.trim().is_empty() || end_at.trim().is_empty() {
            return invalid_phase_c();
        }
        sqlx::query_as("SELECT slot_pod_uid, deployment_revision, provider_id, model_id, attempt_fingerprint, stage, outcome, recorded_at::text AS recorded_at FROM model_turn_phase_c_evidence WHERE pool_id = $1 AND recorded_at >= $2::timestamptz AND recorded_at < $3::timestamptz AND $2::timestamptz < $3::timestamptz ORDER BY recorded_at ASC, id ASC LIMIT $4")
            .bind(pool_id).bind(start_at).bind(end_at).bind(limit.clamp(1, 256)).fetch_all(self.db.pool()).await.map_err(Into::into)
    }

    /// Typed bounded storage; production catalog qualification belongs to coordinator.
    ///
    /// The write is **fenced in the same statement** as the insert: the row
    /// materialises only if the named coordinator incarnation still exists, is
    /// not draining, and has renewed since `fence.live_since_at`. A stale
    /// generation therefore cannot commit a controller window after succession
    /// — not because it checked and lost a race, but because its INSERT selects
    /// no rows.
    pub async fn upsert_controller_window(
        &self,
        input: ModelTurnControllerWindowInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        validate_controller_window_input(&input)?;
        let summary = serde_json::to_string(&input.summary)
            .map_err(|e| crate::Error::InvalidData(e.to_string()))?;
        let changed = sqlx::query("INSERT INTO model_turn_controller_windows (pool_id, window_sequence, started_at, ended_at, admitted_turns, completed_turns, summary) SELECT p.id, $2, $3::timestamptz, $4::timestamptz, $5, $6, $7 FROM model_turn_pools p JOIN coordinator_incarnations c ON c.id = $8 WHERE p.id = $1 AND c.draining_at IS NULL AND c.last_renewed_at >= $9 ON CONFLICT (pool_id, window_sequence) DO UPDATE SET started_at = EXCLUDED.started_at, ended_at = EXCLUDED.ended_at, admitted_turns = EXCLUDED.admitted_turns, completed_turns = EXCLUDED.completed_turns, summary = EXCLUDED.summary")
            .bind(input.pool_id).bind(input.window_sequence).bind(&input.started_at).bind(&input.ended_at).bind(input.admitted_turns).bind(input.completed_turns).bind(summary)
            .bind(&input.fence.incarnation_id).bind(&input.fence.live_since_at)
            .execute(self.db.pool()).await?;
        Ok(if changed.rows_affected() == 1 {
            ModelTurnLeaseMutationOutcome::Applied
        } else {
            ModelTurnLeaseMutationOutcome::Fenced
        })
    }

    /// Commit one learned concurrency target for one pool, under the fence.
    ///
    /// This is the **production writer** of
    /// `model_turn_pools.learned_concurrency`. `acquire_turn` reads that column
    /// as the concurrency target it admits against; before this existed the
    /// only writer in the tree was
    /// [`Self::set_pool_learned_concurrency_for_test`], so the column was
    /// consumed but never produced and the subscription controller had nowhere
    /// to put its answer.
    ///
    /// The fence is applied **in the same statement** as the update, exactly as
    /// [`Self::upsert_controller_window`] does it: the `UPDATE` matches no row
    /// unless the named coordinator incarnation still exists, is not draining,
    /// and has renewed since `fence.live_since_at`. A superseded leader cannot
    /// move a pool's target after succession — not because it checked and lost
    /// a race, but because its statement updates nothing and the last committed
    /// target stands.
    ///
    /// Re-committing the same target is `Applied`, not `Fenced`: the row is
    /// matched, so the write is idempotent rather than refused.
    pub async fn apply_learned_concurrency(
        &self,
        input: ModelTurnLearnedConcurrencyInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0
            || input.learned_concurrency < 1
            || input.learned_concurrency > MODEL_TURN_LEARNED_CONCURRENCY_MAX
            || input.controller_generation < 0
            || uuid::Uuid::parse_str(&input.fence.incarnation_id).is_err()
            || chrono::DateTime::parse_from_rfc3339(&input.fence.live_since_at).is_err()
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn learned concurrency write".to_owned(),
            ));
        }
        let changed = sqlx::query(
            // `coordinator_incarnations.last_renewed_at` is a VARCHAR holding a
            // fixed-width UTC RFC 3339 rendering, so the renewal floor is the
            // same lexicographic comparison `upsert_controller_window` makes.
            "UPDATE model_turn_pools SET learned_concurrency = $2, updated_at = now() \
             WHERE id = $1 AND EXISTS ( \
               SELECT 1 FROM coordinator_incarnations c \
                WHERE c.id = $3 AND c.draining_at IS NULL \
                  AND c.last_renewed_at >= $4)",
        )
        .bind(input.pool_id)
        .bind(input.learned_concurrency)
        .bind(&input.fence.incarnation_id)
        .bind(&input.fence.live_since_at)
        .execute(self.db.pool())
        .await?;
        Ok(if changed.rows_affected() == 1 {
            ModelTurnLeaseMutationOutcome::Applied
        } else {
            ModelTurnLeaseMutationOutcome::Fenced
        })
    }

    /// Every in-flight lease observation that is stale at the 90-second
    /// boundary, read entirely from persisted `reserved_at`/`heartbeat_at`.
    ///
    /// A successor resumes from this list alone: there is no local timer, no
    /// process-local semaphore, and no in-memory record of what the previous
    /// owner had seen. Each row is the exact compare-and-swap observation
    /// [`Self::expire_lease`] requires, so a lease that heartbeats between the
    /// read and the swap is simply not expired.
    pub async fn list_stale_lease_observations(
        &self,
        boundary_at: &str,
        limit: i64,
    ) -> Result<Vec<(i64, ModelTurnLeaseExpiryInput)>> {
        self.db.ensure_initialized().await?;
        if boundary_at.trim().is_empty() {
            return invalid_phase_c();
        }
        let rows: Vec<(i64, String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT pool_id, lease_id::text, generation, request_id, lifecycle, heartbeat_at::text \
             FROM model_turn_leases \
             WHERE lifecycle IN ('reserved', 'dispatching', 'active') \
               AND COALESCE(heartbeat_at, reserved_at) <= $1::timestamptz - interval '90 seconds' \
             ORDER BY COALESCE(heartbeat_at, reserved_at) ASC, lease_id ASC \
             LIMIT $2",
        )
        .bind(boundary_at)
        .bind(limit.clamp(1, 256))
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(
                |(pool_id, lease_id, generation, request_id, lifecycle, heartbeat_at)| {
                    Ok((
                        pool_id,
                        ModelTurnLeaseExpiryInput {
                            identity: ModelTurnLeaseIdentity {
                                lease_id,
                                generation,
                                request_id,
                            },
                            observed_lifecycle: parse_lease_lifecycle(&lifecycle)?,
                            observed_heartbeat_at: heartbeat_at,
                            boundary_at: boundary_at.to_owned(),
                        },
                    ))
                },
            )
            .collect()
    }

    /// Resolve one pool by its opaque numeric id.
    ///
    /// Unlike [`Self::resolve_pool`] this needs no credential: telemetry and
    /// the reaper already hold a pool id and must not acquire a credential
    /// identity just to label a metric.
    pub async fn pool_by_id(&self, pool_id: i64) -> Result<Option<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let row: Option<ModelTurnPoolRow> = sqlx::query_as(
            "SELECT id, credential_id, provider_id, model_id, phase, identity_state, \
                    capability_state, learned_concurrency, in_flight \
             FROM model_turn_pools WHERE id = $1",
        )
        .bind(pool_id)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(|row| {
            Ok(ModelTurnPool {
                id: row.id,
                credential_id: row.credential_id,
                provider_id: row.provider_id,
                model_id: row.model_id,
                phase: parse_phase(&row.phase)?,
                identity_state: parse_identity(&row.identity_state)?,
                capability_state: parse_capability(&row.capability_state)?,
                learned_concurrency: row.learned_concurrency,
                in_flight: row.in_flight,
            })
        })
        .transpose()
    }

    /// The pool one lease belongs to, resolved from the lease id alone.
    ///
    /// The slot boundary holds a lease identity, not a pool, and telemetry must
    /// not make it acquire a credential identity to label a series.
    pub async fn pool_for_lease(&self, lease_id: &str) -> Result<Option<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let pool_id: Option<i64> =
            sqlx::query_scalar("SELECT pool_id FROM model_turn_leases WHERE lease_id = $1::uuid")
                .bind(lease_id)
                .fetch_optional(self.db.pool())
                .await?;
        match pool_id {
            Some(pool_id) => self.pool_by_id(pool_id).await,
            None => Ok(None),
        }
    }

    /// Reservations the ledger still holds open for one pool.
    ///
    /// The pool row's `in_flight` counter and this count are written by the
    /// same transactions, so a difference between them is a real accounting
    /// divergence rather than a sampling artefact.
    pub async fn open_reservation_count(&self, pool_id: i64) -> Result<i64> {
        self.db.ensure_initialized().await?;
        sqlx::query_scalar(
            "SELECT count(*) FROM model_turn_reservations \
             WHERE pool_id = $1 AND state IN ('reserved', 'dispatched')",
        )
        .bind(pool_id)
        .fetch_one(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Output units this pool's observation ledger recorded inside
    /// `[evaluated_at - window_seconds, evaluated_at)`.
    ///
    /// This is a *wall-window* total, and the caller divides it by the window
    /// to get units per wall-second. It is deliberately **not** the controller's
    /// rate formula, whose denominator is the union of active stream intervals:
    /// `model_turn_observations` stores per-pool totals with no per-attempt
    /// stream start/end, so that union cannot be reconstructed from what Phase
    /// B stored. Reporting a wall-window rate as if it were the controller's
    /// rate would be inventing the denominator.
    pub async fn observed_output_units_in_window(
        &self,
        pool_id: i64,
        evaluated_at: &str,
        window_seconds: i64,
    ) -> Result<i64> {
        self.db.ensure_initialized().await?;
        if pool_id <= 0 || evaluated_at.trim().is_empty() || window_seconds <= 0 {
            return invalid_phase_c();
        }
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT sum(output_units)::bigint FROM model_turn_observations \
             WHERE pool_id = $1 \
               AND observed_at >= $2::timestamptz - make_interval(secs => $3::double precision) \
               AND observed_at < $2::timestamptz",
        )
        .bind(pool_id)
        .bind(evaluated_at)
        .bind(window_seconds as f64)
        .fetch_one(self.db.pool())
        .await?;
        Ok(total.unwrap_or(0))
    }

    /// Atomically move the named enforcing pools to `draining`.
    ///
    /// Every pool goes through [`Self::drain_pool_in_transaction`], so the mode
    /// ledger stays the whole truth about how a pool's mode moved: there is
    /// exactly one production writer of `model_turn_pools.phase`. Breaker
    /// state, identity state, and learned concurrency are untouched. Returns
    /// the pools that actually transitioned.
    pub async fn drain_enforcing_pools(
        &self,
        pool_ids: &[i64],
        controller_generation: i64,
    ) -> Result<Vec<i64>> {
        self.db.ensure_initialized().await?;
        if pool_ids.iter().any(|pool_id| *pool_id <= 0) {
            return invalid_phase_c();
        }
        let mut drained = Vec::new();
        for pool_id in pool_ids {
            // Only an *enforcing* pool is drained here: this is the Phase-C
            // coverage-loss path, and a `shadow` pool was never admitting.
            let mode: Option<String> =
                sqlx::query_scalar("SELECT phase FROM model_turn_pools WHERE id = $1")
                    .bind(pool_id)
                    .fetch_optional(self.db.pool())
                    .await?;
            if mode.as_deref() != Some("enforce") {
                continue;
            }
            let outcome = self
                .drain_pool_in_transaction(
                    *pool_id,
                    controller_generation,
                    ModelTurnModeChangeReason::CapabilityCoverageLoss,
                )
                .await?;
            if matches!(
                outcome,
                ModelTurnModeChangeOutcome::Applied { .. }
                    | ModelTurnModeChangeOutcome::DrainedAndSettled { .. }
            ) {
                drained.push(*pool_id);
            }
        }
        Ok(drained)
    }

    /// Observe one pool's coverage and act on it, in a single transaction.
    ///
    /// This is the leader-side enforcement pass at the storage boundary. The
    /// order inside the transaction is the contract:
    ///
    /// 1. Take the canonical admission locks (pool row, then bucket bindings
    ///    ordered by `bucket_kind`).
    /// 2. Re-check the durable leadership fence. A superseded or draining
    ///    incarnation returns [`ModelTurnEnforcementOutcome::Fenced`] having
    ///    mutated nothing — a stale generation cannot commit after succession.
    /// 3. Evaluate coverage **here**, from the stored heartbeats, against the
    ///    caller's live expected-path set. The loss and the drain are therefore
    ///    the same transaction, not two.
    /// 4. Lost coverage drains this pool and only this pool. Held coverage may
    ///    advance a `shadow` pool to `enforce`, but only if the window
    ///    qualified, the compatibility phase actually reached `d`, and the
    ///    identity is eligible.
    ///
    /// Nothing outside `model_turn_*` is read or written.
    pub async fn apply_enforcement_pass_in_transaction(
        &self,
        input: ModelTurnEnforcementPassInput,
    ) -> Result<ModelTurnEnforcementOutcome> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0
            || input.controller_generation <= 0
            || input.evaluated_at.trim().is_empty()
            || input.fence.incarnation_id.trim().is_empty()
            || input.fence.live_since_at.trim().is_empty()
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn enforcement pass".to_owned(),
            ));
        }
        for attempt in 0..3 {
            match self.apply_enforcement_pass_once(&input).await {
                Err(error) if attempt < 2 && is_serialization_failure(&error) => continue,
                result => return result,
            }
        }
        unreachable!("the bounded retry loop returns on its final iteration")
    }

    async fn apply_enforcement_pass_once(
        &self,
        input: &ModelTurnEnforcementPassInput,
    ) -> Result<ModelTurnEnforcementOutcome> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let Some(locked) = lock_pool_for_mode_change(&mut tx, input.pool_id).await? else {
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::PoolUnavailable);
        };
        let leading: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM coordinator_incarnations \
             WHERE id = $1 AND draining_at IS NULL AND last_renewed_at >= $2",
        )
        .bind(&input.fence.incarnation_id)
        .bind(&input.fence.live_since_at)
        .fetch_one(&mut *tx)
        .await?;
        if leading != 1 {
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::Fenced);
        }
        let mode = parse_phase(&locked.phase)?;
        let covered = expected_path_coverage_held(
            &mut tx,
            input.pool_id,
            &input.evaluated_at,
            &input.expected_paths,
        )
        .await?;

        if !covered {
            if !matches!(
                mode,
                ModelTurnAdmissionPhase::Shadow | ModelTurnAdmissionPhase::Enforce
            ) {
                tx.commit().await?;
                return Ok(ModelTurnEnforcementOutcome::Unchanged { mode });
            }
            let changed_at = apply_mode_change(
                &mut tx,
                input.pool_id,
                mode,
                ModelTurnAdmissionPhase::Draining,
                ModelTurnModeChangeReason::CapabilityCoverageLoss,
                input.controller_generation,
            )
            .await?;
            if locked.in_flight == 0 {
                apply_mode_change(
                    &mut tx,
                    input.pool_id,
                    ModelTurnAdmissionPhase::Draining,
                    ModelTurnAdmissionPhase::Off,
                    ModelTurnModeChangeReason::DrainSettled,
                    input.controller_generation,
                )
                .await?;
            }
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::Drained {
                from: mode,
                changed_at,
            });
        }

        if mode != ModelTurnAdmissionPhase::Shadow {
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::Unchanged { mode });
        }
        if !input.window_trainable {
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::Denied(
                ModelTurnModeChangeRejection::WindowNotTrainable,
            ));
        }
        let compatibility_phase = parse_compatibility_phase(&locked.compatibility_phase)?;
        if compatibility_phase != ModelTurnCompatibilityPhase::D {
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::Denied(
                ModelTurnModeChangeRejection::CompatibilityPhaseInsufficient {
                    phase: compatibility_phase,
                },
            ));
        }
        let identity_state = parse_identity(&locked.identity_state)?;
        if identity_state != ModelTurnIdentityState::Eligible {
            tx.commit().await?;
            return Ok(ModelTurnEnforcementOutcome::Denied(
                ModelTurnModeChangeRejection::IdentityIneligible {
                    state: identity_state,
                },
            ));
        }
        let changed_at = apply_mode_change(
            &mut tx,
            input.pool_id,
            ModelTurnAdmissionPhase::Shadow,
            ModelTurnAdmissionPhase::Enforce,
            ModelTurnModeChangeReason::EnforcementAdvance,
            input.controller_generation,
        )
        .await?;
        tx.commit().await?;
        Ok(ModelTurnEnforcementOutcome::Enforced { changed_at })
    }

    /// Move one pool's admission **mode**, appending exactly one ledger row.
    ///
    /// This is the first production writer of `model_turn_pools.phase`; before
    /// Phase D only test fixtures wrote it. The canonical admission locks are
    /// taken first — the pool row, then its bucket bindings ordered by
    /// `bucket_kind`, exactly [`Self::acquire_turn`]'s order — so a concurrent
    /// acquisition serializes behind this write instead of racing it.
    ///
    /// The mode graph is deliberately narrow:
    ///
    /// * `off → shadow`
    /// * `shadow → enforce` — only for a pool whose compatibility phase
    ///   actually reached `d` and whose identity is eligible
    /// * `shadow → draining`, `enforce → draining`
    /// * `shadow → off`, `draining → off`
    ///
    /// There is no `enforce → off` edge. An enforcing pool must pass through
    /// `draining`, which is what makes "drained before the next acquisition"
    /// a property of the storage rather than of caller discipline.
    pub async fn set_pool_mode_in_transaction(
        &self,
        input: ModelTurnModeChangeInput,
    ) -> Result<ModelTurnModeChangeOutcome> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0 || input.controller_generation <= 0 {
            return Err(crate::Error::InvalidData(
                "invalid model-turn mode change".to_owned(),
            ));
        }
        for attempt in 0..3 {
            match self.set_pool_mode_once(&input).await {
                Err(error) if attempt < 2 && is_serialization_failure(&error) => continue,
                result => return result,
            }
        }
        unreachable!("the bounded retry loop returns on its final iteration")
    }

    async fn set_pool_mode_once(
        &self,
        input: &ModelTurnModeChangeInput,
    ) -> Result<ModelTurnModeChangeOutcome> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let Some(locked) = lock_pool_for_mode_change(&mut tx, input.pool_id).await? else {
            tx.commit().await?;
            return Ok(ModelTurnModeChangeOutcome::PoolUnavailable);
        };
        let from = parse_phase(&locked.phase)?;
        if from == input.target_mode {
            tx.commit().await?;
            return Ok(ModelTurnModeChangeOutcome::Unchanged { mode: from });
        }
        if !mode_edge_allowed(from, input.target_mode) {
            tx.commit().await?;
            return Ok(ModelTurnModeChangeOutcome::Rejected(
                ModelTurnModeChangeRejection::UnsupportedTransition {
                    from,
                    to: input.target_mode,
                },
            ));
        }
        if input.target_mode == ModelTurnAdmissionPhase::Enforce {
            let compatibility_phase = parse_compatibility_phase(&locked.compatibility_phase)?;
            if compatibility_phase != ModelTurnCompatibilityPhase::D {
                tx.commit().await?;
                return Ok(ModelTurnModeChangeOutcome::Rejected(
                    ModelTurnModeChangeRejection::CompatibilityPhaseInsufficient {
                        phase: compatibility_phase,
                    },
                ));
            }
            let identity_state = parse_identity(&locked.identity_state)?;
            if identity_state != ModelTurnIdentityState::Eligible {
                tx.commit().await?;
                return Ok(ModelTurnModeChangeOutcome::Rejected(
                    ModelTurnModeChangeRejection::IdentityIneligible {
                        state: identity_state,
                    },
                ));
            }
        }
        let changed_at = apply_mode_change(
            &mut tx,
            input.pool_id,
            from,
            input.target_mode,
            input.reason,
            input.controller_generation,
        )
        .await?;
        tx.commit().await?;
        Ok(ModelTurnModeChangeOutcome::Applied {
            from,
            to: input.target_mode,
            changed_at,
        })
    }

    /// Stop admitting on one pool, and settle it to `off` immediately when
    /// there is nothing in flight left to drain.
    ///
    /// A drain on an already-`off` pool is a no-op: no mode change, no ledger
    /// row. Everything else runs under the same canonical locks as
    /// [`Self::set_pool_mode_in_transaction`], so once this commits no later
    /// acquisition can commit an `Admitted` outcome against the old mode.
    pub async fn drain_pool_in_transaction(
        &self,
        pool_id: i64,
        controller_generation: i64,
        reason: ModelTurnModeChangeReason,
    ) -> Result<ModelTurnModeChangeOutcome> {
        self.db.ensure_initialized().await?;
        if pool_id <= 0 || controller_generation <= 0 {
            return Err(crate::Error::InvalidData(
                "invalid model-turn drain".to_owned(),
            ));
        }
        for attempt in 0..3 {
            match self
                .drain_pool_once(pool_id, controller_generation, reason)
                .await
            {
                Err(error) if attempt < 2 && is_serialization_failure(&error) => continue,
                result => return result,
            }
        }
        unreachable!("the bounded retry loop returns on its final iteration")
    }

    async fn drain_pool_once(
        &self,
        pool_id: i64,
        controller_generation: i64,
        reason: ModelTurnModeChangeReason,
    ) -> Result<ModelTurnModeChangeOutcome> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let Some(locked) = lock_pool_for_mode_change(&mut tx, pool_id).await? else {
            tx.commit().await?;
            return Ok(ModelTurnModeChangeOutcome::PoolUnavailable);
        };
        let from = parse_phase(&locked.phase)?;
        if matches!(
            from,
            ModelTurnAdmissionPhase::Off | ModelTurnAdmissionPhase::Draining
        ) {
            tx.commit().await?;
            return Ok(ModelTurnModeChangeOutcome::Unchanged { mode: from });
        }
        let changed_at = apply_mode_change(
            &mut tx,
            pool_id,
            from,
            ModelTurnAdmissionPhase::Draining,
            reason,
            controller_generation,
        )
        .await?;
        // Nothing in flight means nothing to drain: the pool has already
        // reached the state the drain exists to wait for.
        if locked.in_flight == 0 {
            let settled = apply_mode_change(
                &mut tx,
                pool_id,
                ModelTurnAdmissionPhase::Draining,
                ModelTurnAdmissionPhase::Off,
                ModelTurnModeChangeReason::DrainSettled,
                controller_generation,
            )
            .await?;
            tx.commit().await?;
            return Ok(ModelTurnModeChangeOutcome::DrainedAndSettled {
                changed_at: settled,
            });
        }
        tx.commit().await?;
        Ok(ModelTurnModeChangeOutcome::Applied {
            from,
            to: ModelTurnAdmissionPhase::Draining,
            changed_at,
        })
    }

    /// Take the final rollback step: move the durable mode to `off`.
    ///
    /// The plan is the gate. Attempting this while any earlier step is still
    /// pending is refused **and mutates nothing**, so the durable mode cannot
    /// go `off` while a slot wrapper or provider contract could still
    /// originate a turn against it.
    pub async fn roll_back_pool_to_off_in_transaction(
        &self,
        plan: &mut ModelTurnRollbackPlanV1,
        pool_id: i64,
        controller_generation: i64,
    ) -> Result<ModelTurnModeChangeOutcome> {
        if plan.next_step() != Some(ModelTurnRollbackStepV1::ModeOff) {
            return Ok(ModelTurnModeChangeOutcome::Rejected(
                ModelTurnModeChangeRejection::RollbackOutOfOrder {
                    expected: plan.next_step().unwrap_or(ModelTurnRollbackStepV1::ModeOff),
                    attempted: ModelTurnRollbackStepV1::ModeOff,
                },
            ));
        }
        let outcome = self
            .set_pool_mode_in_transaction(ModelTurnModeChangeInput {
                pool_id,
                target_mode: ModelTurnAdmissionPhase::Off,
                reason: ModelTurnModeChangeReason::Rollback,
                controller_generation,
            })
            .await?;
        if matches!(
            outcome,
            ModelTurnModeChangeOutcome::Applied { .. }
                | ModelTurnModeChangeOutcome::Unchanged {
                    mode: ModelTurnAdmissionPhase::Off
                }
        ) {
            plan.complete(ModelTurnRollbackStepV1::ModeOff)
                .map_err(|_| crate::Error::InvalidData("rollback step out of order".to_owned()))?;
        }
        Ok(outcome)
    }

    /// Read the durable mode-change ledger for one pool, oldest first.
    pub async fn pool_mode_transitions(
        &self,
        pool_id: i64,
        limit: i64,
    ) -> Result<
        Vec<(
            ModelTurnAdmissionPhase,
            ModelTurnAdmissionPhase,
            String,
            String,
        )>,
    > {
        self.db.ensure_initialized().await?;
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT from_mode, to_mode, reason, \
                    to_char(changed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') \
             FROM model_turn_pool_mode_transitions WHERE pool_id = $1 \
             ORDER BY changed_at ASC, id ASC LIMIT $2",
        )
        .bind(pool_id)
        .bind(limit.clamp(1, 256))
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|(from, to, reason, changed_at)| {
                Ok((parse_phase(&from)?, parse_phase(&to)?, reason, changed_at))
            })
            .collect()
    }

    /// Evaluate every A→B→C→D prerequisite and, only if all of them hold,
    /// make the requested compatibility phase effective — all in one
    /// serializable transaction.
    ///
    /// The pool row is taken `FOR UPDATE` first, the same canonical lock order
    /// [`Self::acquire_turn`] uses, so a concurrent acquisition serializes
    /// behind the decision rather than racing it.
    ///
    /// Ordering is the contract:
    ///
    /// 1. A request for a phase that is not the immediate successor of the
    ///    effective one is refused **before any predicate is evaluated and
    ///    without writing a row**. A phase cannot skip its prerequisite.
    /// 2. A request for the phase already in effect is a no-op, so an accepted
    ///    request re-issued is idempotent.
    /// 3. Otherwise all six predicates are evaluated and exactly one ledger row
    ///    records the full verdict. The effective phase advances only when
    ///    every predicate held; a denial leaves it exactly where it was.
    ///
    /// Every predicate is a storage fact read inside this transaction. None of
    /// them is a caller-supplied assertion: the only caller input the guard
    /// trusts is the live expected-path set, which is the coordinator's own
    /// inventory and is used as a *denominator* that coverage must match
    /// exactly — it can only make the guard stricter, never laxer.
    pub async fn request_phase_transition_in_transaction(
        &self,
        request: ModelTurnPhaseTransitionRequest,
    ) -> Result<ModelTurnPhaseTransitionOutcome> {
        self.db.ensure_initialized().await?;
        if request.pool_id <= 0
            || request.controller_generation <= 0
            || request.evaluated_at.trim().is_empty()
            || request.fence.incarnation_id.trim().is_empty()
            || request.fence.live_since_at.trim().is_empty()
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn phase transition request".to_owned(),
            ));
        }
        for attempt in 0..3 {
            match self.request_phase_transition_once(&request).await {
                Err(error) if attempt < 2 && is_serialization_failure(&error) => continue,
                result => return result,
            }
        }
        unreachable!("the bounded retry loop returns on its final iteration")
    }

    async fn request_phase_transition_once(
        &self,
        request: &ModelTurnPhaseTransitionRequest,
    ) -> Result<ModelTurnPhaseTransitionOutcome> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let pool: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT compatibility_phase, identity_state, provider_id, model_id \
             FROM model_turn_pools WHERE id = $1 FOR UPDATE",
        )
        .bind(request.pool_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((phase, identity_state, provider_id, model_id)) = pool else {
            tx.commit().await?;
            return Ok(ModelTurnPhaseTransitionOutcome::PoolUnavailable);
        };
        let effective_phase = parse_compatibility_phase(&phase)?;
        if effective_phase == request.requested_phase {
            tx.commit().await?;
            return Ok(ModelTurnPhaseTransitionOutcome::AlreadyEffective { effective_phase });
        }
        if effective_phase.next() != Some(request.requested_phase) {
            tx.commit().await?;
            return Ok(ModelTurnPhaseTransitionOutcome::NotAdjacent {
                effective_phase,
                requested_phase: request.requested_phase,
            });
        }

        // ── Predicate 1: the durable schema marker this binary understands ──
        let marker: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM model_turn_admission_schema \
             WHERE marker = 'model_turn_admission_schema'",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let schema_marker = marker == Some(MODEL_TURN_ADMISSION_SCHEMA_VERSION);

        // ── Predicate 2: B1 route labels agree with every fresh B2 report ──
        let (route_matched, fresh_reports): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE provider_id = $3 AND model_id = $4), count(*) \
             FROM model_turn_capability_heartbeats \
             WHERE pool_id = $1 \
               AND heartbeat_at >= $2::timestamptz - make_interval(secs => $5::double precision)",
        )
        .bind(request.pool_id)
        .bind(&request.evaluated_at)
        .bind(&provider_id)
        .bind(&model_id)
        .bind(MODEL_TURN_PHASE_PREDICATE_FRESHNESS_SECONDS as f64)
        .fetch_one(&mut *tx)
        .await?;
        let capability_reports = fresh_reports > 0 && route_matched == fresh_reports;

        // ── Predicate 3: this incarnation still holds the fenced lease ──────
        let leading: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM coordinator_incarnations \
             WHERE id = $1 AND draining_at IS NULL AND last_renewed_at >= $2",
        )
        .bind(&request.fence.incarnation_id)
        .bind(&request.fence.live_since_at)
        .fetch_one(&mut *tx)
        .await?;
        let leadership_generation = leading == 1;

        // ── Predicate 4: every observed attempt chain is complete ──────────
        //
        // Five distinct stages and no `missing` edge. An empty history is not
        // a complete history: with nothing observed there is nothing to
        // qualify, so the predicate fails closed rather than vacuously passing.
        let (attempts, complete_attempts): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE stages = 5 AND missing = 0) FROM ( \
                 SELECT attempt_fingerprint, count(DISTINCT stage) AS stages, \
                        count(*) FILTER (WHERE outcome = 'missing') AS missing \
                 FROM model_turn_phase_c_evidence \
                 WHERE pool_id = $1 \
                   AND recorded_at >= $2::timestamptz \
                                      - make_interval(secs => $3::double precision) \
                 GROUP BY attempt_fingerprint \
             ) chains",
        )
        .bind(request.pool_id)
        .bind(&request.evaluated_at)
        .bind(MODEL_TURN_PHASE_PREDICATE_FRESHNESS_SECONDS as f64)
        .fetch_one(&mut *tx)
        .await?;
        let observation_history = attempts > 0 && complete_attempts == attempts;

        // ── Predicate 5: fresh coverage equals the live expected denominator ─
        //
        // Exact set equality, not containment — see
        // [`expected_path_coverage_held`], which the leader's enforcement pass
        // evaluates from the same stored rows.
        let expected_path_coverage = expected_path_coverage_held(
            &mut tx,
            request.pool_id,
            &request.evaluated_at,
            &request.expected_paths,
        )
        .await?;

        // ── Predicate 6: durable per-pool identity eligibility ─────────────
        let identity_eligibility =
            parse_identity(&identity_state)? == ModelTurnIdentityState::Eligible;

        let verdicts = [
            (ModelTurnPhasePredicate::SchemaMarker, schema_marker),
            (
                ModelTurnPhasePredicate::CapabilityReports,
                capability_reports,
            ),
            (
                ModelTurnPhasePredicate::LeadershipGeneration,
                leadership_generation,
            ),
            (
                ModelTurnPhasePredicate::ObservationHistory,
                observation_history,
            ),
            (
                ModelTurnPhasePredicate::ExpectedPathCoverage,
                expected_path_coverage,
            ),
            (
                ModelTurnPhasePredicate::IdentityEligibility,
                identity_eligibility,
            ),
        ];
        debug_assert_eq!(verdicts.len(), ModelTurnPhasePredicate::ALL.len());
        let predicate_results: ModelTurnPhasePredicateResults = verdicts
            .iter()
            .map(|(predicate, held)| (predicate.key().to_owned(), *held))
            .collect();
        let failed: Vec<ModelTurnPhasePredicate> = verdicts
            .iter()
            .filter(|(_, held)| !*held)
            .map(|(predicate, _)| *predicate)
            .collect();
        // One decision drives both the ledger's `effective_phase` column and
        // the column update, so the row can never claim a phase the pool did
        // not actually reach.
        let advance_to = failed.is_empty().then_some(request.requested_phase);
        let effective_after = advance_to.unwrap_or(effective_phase);
        let encoded = serde_json::to_string(&predicate_results)
            .map_err(|error| crate::Error::InvalidData(error.to_string()))?;
        sqlx::query(
            "INSERT INTO model_turn_pool_phase_transitions \
             (pool_id, requested_phase, effective_phase, decided_at, predicate_results, \
              controller_generation) \
             VALUES ($1, $2, $3, $4::timestamptz, $5::jsonb, $6)",
        )
        .bind(request.pool_id)
        .bind(request.requested_phase.code())
        .bind(effective_after.code())
        .bind(&request.evaluated_at)
        .bind(&encoded)
        .bind(request.controller_generation)
        .execute(&mut *tx)
        .await?;
        if let Some(target) = advance_to {
            // Guarded by the phase this transaction actually read, so the
            // advance is exactly one step from that observation or nothing.
            let advanced = sqlx::query(
                "UPDATE model_turn_pools SET compatibility_phase = $2, updated_at = now() \
                 WHERE id = $1 AND compatibility_phase = $3",
            )
            .bind(request.pool_id)
            .bind(target.code())
            .bind(effective_phase.code())
            .execute(&mut *tx)
            .await?;
            if advanced.rows_affected() != 1 {
                return Err(crate::Error::InvalidData(
                    "model-turn compatibility phase moved under the guard".to_owned(),
                ));
            }
            tx.commit().await?;
            return Ok(ModelTurnPhaseTransitionOutcome::Advanced {
                effective_phase: target,
                predicate_results,
            });
        }
        tx.commit().await?;
        Ok(ModelTurnPhaseTransitionOutcome::Denied {
            effective_phase,
            failed,
            predicate_results,
        })
    }

    /// Read one pool's persisted compatibility phase.
    pub async fn compatibility_phase(
        &self,
        pool_id: i64,
    ) -> Result<Option<ModelTurnCompatibilityPhase>> {
        self.db.ensure_initialized().await?;
        let phase: Option<String> =
            sqlx::query_scalar("SELECT compatibility_phase FROM model_turn_pools WHERE id = $1")
                .bind(pool_id)
                .fetch_optional(self.db.pool())
                .await?;
        phase
            .map(|phase| parse_compatibility_phase(&phase))
            .transpose()
    }

    /// Read the append-only phase-decision ledger for one pool, oldest first.
    pub async fn phase_transitions(
        &self,
        pool_id: i64,
        limit: i64,
    ) -> Result<
        Vec<(
            ModelTurnCompatibilityPhase,
            ModelTurnCompatibilityPhase,
            i64,
            ModelTurnPhasePredicateResults,
        )>,
    > {
        self.db.ensure_initialized().await?;
        let rows: Vec<(String, String, i64, String)> = sqlx::query_as(
            "SELECT requested_phase, effective_phase, controller_generation, \
                    predicate_results::text \
             FROM model_turn_pool_phase_transitions WHERE pool_id = $1 \
             ORDER BY decided_at ASC, id ASC LIMIT $2",
        )
        .bind(pool_id)
        .bind(limit.clamp(1, 256))
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|(requested, effective, generation, results)| {
                Ok((
                    parse_compatibility_phase(&requested)?,
                    parse_compatibility_phase(&effective)?,
                    generation,
                    serde_json::from_str(&results)
                        .map_err(|error| crate::Error::InvalidData(error.to_string()))?,
                ))
            })
            .collect()
    }

    /// Write one ledger row verbatim so a storage-boundary regression can model
    /// what a downlevel or corrupted writer would attempt. Raw mutation stays
    /// in DB test support; the closed-shape CHECK is what must reject it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_raw_phase_transition_for_test(
        &self,
        pool_id: i64,
        requested_phase: &str,
        effective_phase: &str,
        predicate_results_json: &str,
        controller_generation: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO model_turn_pool_phase_transitions \
             (pool_id, requested_phase, effective_phase, predicate_results, controller_generation) \
             VALUES ($1, $2, $3, $4::jsonb, $5)",
        )
        .bind(pool_id)
        .bind(requested_phase)
        .bind(effective_phase)
        .bind(predicate_results_json)
        .bind(controller_generation)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Read one persisted typed summary for cross-crate storage regressions.
    ///
    /// This test-support seam keeps raw SQL inside `djinn-db`; production
    /// learners must continue through the exact-bound fail-closed projection.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn controller_window_summary_for_test(
        &self,
        pool_id: i64,
        window_sequence: i64,
    ) -> Result<Option<ModelTurnControllerWindowSummary>> {
        self.db.ensure_initialized().await?;
        let summary: Option<String> = sqlx::query_scalar(
            "SELECT summary FROM model_turn_controller_windows WHERE pool_id = $1 AND window_sequence = $2",
        )
        .bind(pool_id)
        .bind(window_sequence)
        .fetch_optional(self.db.pool())
        .await?;
        summary
            .map(|summary| {
                serde_json::from_str(&summary)
                    .map_err(|error| crate::Error::InvalidData(error.to_string()))
            })
            .transpose()
    }

    /// Seed one request-bucket binding so a scoped fixture pool can admit.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_request_bucket_binding_for_test(
        &self,
        pool_id: i64,
        capacity_units: i64,
        available_units: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO model_turn_bucket_bindings (pool_id, bucket_kind, capacity_units, available_units) \
             VALUES ($1, 'request', $2, $3) \
             ON CONFLICT (pool_id, bucket_kind) DO UPDATE SET \
               capacity_units = EXCLUDED.capacity_units, available_units = EXCLUDED.available_units",
        )
        .bind(pool_id)
        .bind(capacity_units)
        .bind(available_units)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Set one pool's learned concurrency target directly.
    ///
    /// Production writes this from the controller; a fixture that needs more
    /// than one turn in flight sets it here rather than acquiring SQL access.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pool_learned_concurrency_for_test(
        &self,
        pool_id: i64,
        target: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE model_turn_pools SET learned_concurrency = $2 WHERE id = $1")
            .bind(pool_id)
            .bind(target)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Seed one binding of any bucket kind so a scoped fixture can exercise
    /// every throttle class, not only the request bucket.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_bucket_binding_for_test(
        &self,
        pool_id: i64,
        bucket_kind: ModelTurnBucketKind,
        capacity_units: i64,
        available_units: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO model_turn_bucket_bindings \
             (pool_id, bucket_kind, capacity_units, available_units) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (pool_id, bucket_kind) DO UPDATE SET \
               capacity_units = EXCLUDED.capacity_units, available_units = EXCLUDED.available_units",
        )
        .bind(pool_id)
        .bind(bucket_kind_name(bucket_kind))
        .bind(capacity_units)
        .bind(available_units)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Record one bounded usage observation so an aggregate-rate read has
    /// something to divide. Production writes these from the provider chain.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_output_observation_for_test(
        &self,
        pool_id: i64,
        sequence: i64,
        output_units: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO model_turn_observations (pool_id, sequence, kind, output_units) \
             VALUES ($1, $2, 'usage', $3)",
        )
        .bind(pool_id)
        .bind(sequence)
        .bind(output_units)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Backdate one lease's persisted observation timestamps.
    ///
    /// This is how a fake-time reaper regression makes a lease *durably* old
    /// without a wall-clock sleep: the reaper reads exactly these columns, so
    /// moving them is moving the only clock it has.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn backdate_lease_for_test(
        &self,
        identity: &ModelTurnLeaseIdentity,
        reserved_at: &str,
        heartbeat_at: Option<&str>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let changed = sqlx::query(
            "UPDATE model_turn_leases SET reserved_at = $4::timestamptz, heartbeat_at = $5::timestamptz \
             WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3",
        )
        .bind(&identity.lease_id)
        .bind(identity.generation)
        .bind(&identity.request_id)
        .bind(reserved_at)
        .bind(heartbeat_at)
        .execute(self.db.pool())
        .await?;
        if changed.rows_affected() != 1 {
            return invalid_phase_c();
        }
        Ok(())
    }

    /// Read one pool's durable control state for cross-crate regressions.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn pool_control_state_for_test(
        &self,
        pool_id: i64,
    ) -> Result<Option<(String, String, String, i64, i64)>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as(
            "SELECT phase, identity_state, capability_state, learned_concurrency, in_flight \
             FROM model_turn_pools WHERE id = $1",
        )
        .bind(pool_id)
        .fetch_optional(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Put a pool at a given compatibility phase directly.
    ///
    /// Production reaches a phase only through
    /// [`Self::request_phase_transition_in_transaction`]; this seam exists so a
    /// dependent crate can model a pool that already got there without
    /// acquiring SQL access.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pool_compatibility_phase_for_test(
        &self,
        pool_id: i64,
        phase: ModelTurnCompatibilityPhase,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE model_turn_pools SET compatibility_phase = $2 WHERE id = $1")
            .bind(pool_id)
            .bind(phase.code())
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Overwrite the durable per-pool identity state.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pool_identity_for_test(
        &self,
        pool_id: i64,
        state: ModelTurnIdentityState,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE model_turn_pools SET identity_state = $2 WHERE id = $1")
            .bind(pool_id)
            .bind(identity_state_name(state))
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Overwrite the durable pool label pair for a fail-closed learner
    /// regression. Raw mutation stays in DB test support so dependent crates do
    /// not acquire SQL access solely to model damaged rows.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pool_labels_for_test(
        &self,
        pool_id: i64,
        provider_id: &str,
        model_id: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE model_turn_pools SET provider_id = $2, model_id = $3 WHERE id = $1")
            .bind(pool_id)
            .bind(provider_id)
            .bind(model_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Write a controller-window row verbatim, bypassing the typed write
    /// validator, so fail-closed read regressions can model rows that a
    /// corrupted or downlevel writer could leave behind. Raw mutation stays in
    /// DB test support so dependent crates do not acquire SQL access solely to
    /// model damaged rows.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_raw_controller_window_for_test(
        &self,
        pool_id: i64,
        window_sequence: i64,
        started_at: &str,
        ended_at: &str,
        admitted_turns: i64,
        completed_turns: i64,
        summary: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("INSERT INTO model_turn_controller_windows (pool_id, window_sequence, started_at, ended_at, admitted_turns, completed_turns, summary) VALUES ($1, $2, $3::timestamptz, $4::timestamptz, $5, $6, $7) ON CONFLICT (pool_id, window_sequence) DO UPDATE SET started_at = EXCLUDED.started_at, ended_at = EXCLUDED.ended_at, admitted_turns = EXCLUDED.admitted_turns, completed_turns = EXCLUDED.completed_turns, summary = EXCLUDED.summary")
            .bind(pool_id)
            .bind(window_sequence)
            .bind(started_at)
            .bind(ended_at)
            .bind(admitted_turns)
            .bind(completed_turns)
            .bind(summary)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Exact-bound fail-closed projection; the coordinator revalidates catalog membership.
    pub async fn learner_window(
        &self,
        pool_id: i64,
        window_sequence: i64,
        started_at: &str,
        ended_at: &str,
    ) -> Result<Option<ModelTurnLearnerWindow>> {
        self.db.ensure_initialized().await?;
        if pool_id <= 0 || window_sequence < 0 || !valid_aligned_minute_bounds(started_at, ended_at)
        {
            return Ok(None);
        }
        // `timestamptz::text` renders `1970-01-01 00:02:00+00`, which is not
        // RFC 3339. Render an explicitly UTC, microsecond-precision RFC 3339
        // string so the read-side alignment check sees the real stored instant
        // instead of failing every row on a formatting artifact.
        let row: Option<ControllerWindowRow> = sqlx::query_as("SELECT w.pool_id,w.window_sequence,to_char(w.started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),to_char(w.ended_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),w.admitted_turns,w.completed_turns,w.summary,p.provider_id,p.model_id FROM model_turn_controller_windows w JOIN model_turn_pools p ON p.id=w.pool_id WHERE w.pool_id=$1 AND w.window_sequence=$2 AND w.started_at=$3::timestamptz AND w.ended_at=$4::timestamptz").bind(pool_id).bind(window_sequence).bind(started_at).bind(ended_at).fetch_optional(self.db.pool()).await?;
        Ok(row.and_then(project_learner_window))
    }

    /// Commit the pre-send fence before a caller sends provider network bytes.
    pub async fn mark_dispatching(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.transition(identity, "reserved", "dispatching", true)
            .await
    }

    /// Move the same fenced lease from dispatching to active.
    pub async fn mark_active(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.transition(identity, "dispatching", "active", false)
            .await
    }

    /// Persist a heartbeat only for the identity which still owns an in-flight lease.
    pub async fn heartbeat(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        let changed = sqlx::query("UPDATE model_turn_leases SET heartbeat_at = now() WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND lifecycle IN ('dispatching', 'active')")
            .bind(&identity.lease_id).bind(identity.generation).bind(&identity.request_id).execute(self.db.pool()).await?;
        Ok(if changed.rows_affected() == 1 {
            ModelTurnLeaseMutationOutcome::Applied
        } else {
            ModelTurnLeaseMutationOutcome::Fenced
        })
    }

    /// Compare-and-swap only the watchdog's stale observation after 90 seconds.
    pub async fn expire_lease(
        &self,
        input: ModelTurnLeaseExpiryInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        // A watchdog may only terminalize an in-flight observation. Never let
        // a replay present a terminal state as its expected state: terminal
        // accounting already happened, so a second insert is neither safe nor
        // an idempotent expiry operation.
        if !is_in_flight_lifecycle(input.observed_lifecycle) {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        }
        let mut tx = self.db.pool().begin().await?;
        let lease: Option<(i64, String, String)> = sqlx::query_as("SELECT pool_id, reservation_id::text, lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND lifecycle = $4 AND lifecycle IN ('reserved', 'dispatching', 'active') AND heartbeat_at IS NOT DISTINCT FROM $5::timestamptz AND COALESCE(heartbeat_at, reserved_at) <= $6::timestamptz - interval '90 seconds' FOR UPDATE")
            .bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).bind(lease_lifecycle_name(input.observed_lifecycle)).bind(&input.observed_heartbeat_at).bind(&input.boundary_at).fetch_optional(&mut *tx).await?;
        let Some((pool_id, reservation_id, lifecycle)) = lease else {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        };
        sqlx::query("UPDATE model_turn_leases SET lifecycle = 'expired', terminal_at = $2::timestamptz WHERE lease_id = $1::uuid AND generation = $3 AND request_id = $4")
            .bind(&input.identity.lease_id).bind(&input.boundary_at).bind(input.identity.generation).bind(&input.identity.request_id).execute(&mut *tx).await?;
        let unsent = lifecycle == "reserved";
        sqlx::query("INSERT INTO model_turn_lease_terminals (lease_id, generation, request_id, outcome, accounting_state) VALUES ($1::uuid, $2, $3, 'expired', $4)")
            .bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).bind(if unsent { "refunded" } else { "quarantined" }).execute(&mut *tx).await?;
        sqlx::query("UPDATE model_turn_reservations SET state = 'expired', terminal_at = $2::timestamptz WHERE id = $1::uuid").bind(&reservation_id).bind(&input.boundary_at).execute(&mut *tx).await?;
        self.release_accounting(
            &mut tx,
            pool_id,
            &reservation_id,
            unsent,
            None,
            input.identity.generation,
        )
        .await?;
        tx.commit().await?;
        Ok(ModelTurnLeaseMutationOutcome::Applied)
    }

    /// Alias for the sole terminal reconciliation/accounting path.
    pub async fn reconcile(
        &self,
        input: ModelTurnLeaseReconciliationInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.reconcile_lease(input).await
    }

    /// Alias for callers which model expiry as the terminal action itself.
    pub async fn expire(
        &self,
        input: ModelTurnLeaseExpiryInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.expire_lease(input).await
    }

    /// Record one terminal outcome and apply reservation accounting at most once.
    pub async fn reconcile_lease(
        &self,
        input: ModelTurnLeaseReconciliationInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.reconcile_lease_with_unsent_dispatching(input, false)
            .await
    }

    /// Cancel an attempt which has not been handed to provider I/O. The slot
    /// fence may already be `dispatching`; that state alone must not quarantine
    /// a permit which was dropped before it could be sent.
    pub async fn cancel_before_send(
        &self,
        identity: ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.reconcile_lease_with_unsent_dispatching(
            ModelTurnLeaseReconciliationInput {
                identity,
                outcome: ModelTurnLeaseTerminalOutcome::Cancelled,
                authoritative_usage: None,
                detail: None,
            },
            true,
        )
        .await
    }

    async fn reconcile_lease_with_unsent_dispatching(
        &self,
        input: ModelTurnLeaseReconciliationInput,
        dispatching_is_definitely_unsent: bool,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        if input.detail.as_ref().is_some_and(|v| v.len() > 1024)
            || input
                .authoritative_usage
                .as_ref()
                .is_some_and(usage_is_negative)
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn reconciliation".to_owned(),
            ));
        }
        let mut tx = self.db.pool().begin().await?;
        let lease: Option<(i64, String, String)> = sqlx::query_as("SELECT pool_id, reservation_id::text, lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 FOR UPDATE").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).fetch_optional(&mut *tx).await?;
        let Some((pool_id, reservation_id, lifecycle)) = lease else {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        };
        let terminal: Option<(String, String)> = sqlx::query_as("SELECT outcome, accounting_state FROM model_turn_lease_terminals WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 FOR UPDATE").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).fetch_optional(&mut *tx).await?;
        if let Some((existing, accounting_state)) = terminal {
            if existing != terminal_outcome_name(input.outcome) {
                return Ok(ModelTurnLeaseMutationOutcome::Fenced);
            }
            if let Some(usage) = input.authoritative_usage.as_ref()
                && accounting_state == "quarantined"
            {
                self.resolve_quarantined_usage(&mut tx, pool_id, &reservation_id, usage)
                    .await?;
                sqlx::query("UPDATE model_turn_lease_terminals SET accounting_state = 'authoritative' WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND accounting_state = 'quarantined'")
                    .bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(ModelTurnLeaseMutationOutcome::Applied);
            }
            return Ok(ModelTurnLeaseMutationOutcome::Idempotent);
        }
        if !matches!(lifecycle.as_str(), "reserved" | "dispatching" | "active") {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        }
        let unsent = lifecycle == "reserved"
            || (dispatching_is_definitely_unsent && lifecycle == "dispatching");
        let accounting_state = if unsent {
            "refunded"
        } else if input.authoritative_usage.is_some() {
            "authoritative"
        } else {
            "quarantined"
        };
        sqlx::query("INSERT INTO model_turn_lease_terminals (lease_id, generation, request_id, outcome, detail, accounting_state) VALUES ($1::uuid, $2, $3, $4, $5, $6)").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).bind(terminal_outcome_name(input.outcome)).bind(&input.detail).bind(accounting_state).execute(&mut *tx).await?;
        sqlx::query("UPDATE model_turn_leases SET lifecycle = 'reconciled', terminal_at = now() WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).execute(&mut *tx).await?;
        let state = if input.outcome == ModelTurnLeaseTerminalOutcome::Cancelled {
            "cancelled"
        } else {
            "reconciled"
        };
        sqlx::query("UPDATE model_turn_reservations SET state = $2, terminal_at = now() WHERE id = $1::uuid").bind(&reservation_id).bind(state).execute(&mut *tx).await?;
        self.release_accounting(
            &mut tx,
            pool_id,
            &reservation_id,
            unsent,
            input.authoritative_usage.as_ref(),
            input.identity.generation,
        )
        .await?;
        tx.commit().await?;
        Ok(ModelTurnLeaseMutationOutcome::Applied)
    }

    /// Atomically acquire learned concurrency and every supplied bucket debit.
    ///
    /// Each attempt is SERIALIZABLE and locks in one documented order: the pool
    /// first, then distinct binding rows ordered by persisted `bucket_kind`.
    /// Retrying a serialization abort reruns the whole decision; no local lock
    /// or partially-applied debit is ever used as admission authority.
    pub async fn acquire_turn(
        &self,
        input: ModelTurnAcquireInput,
    ) -> Result<ModelTurnAcquireOutcome> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0
            || input.request_id.trim().is_empty()
            || input.request_id.len() > 128
            || input.generation <= 0
        {
            return Ok(ModelTurnAcquireOutcome::Rejected(
                ModelTurnAdmissionRejection::InvalidRequest,
            ));
        }
        for attempt in 0..3 {
            match self.acquire_turn_once(&input).await {
                Err(error) if attempt < 2 && is_serialization_failure(&error) => continue,
                result => return result,
            }
        }
        unreachable!("the bounded retry loop returns on its final iteration")
    }

    async fn acquire_turn_once(
        &self,
        input: &ModelTurnAcquireInput,
    ) -> Result<ModelTurnAcquireOutcome> {
        let debits = canonical_debits(&input.debits)?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let pool: Option<(String, String, String, i64, i64)> = sqlx::query_as(
            "SELECT phase, identity_state, capability_state, learned_concurrency, in_flight FROM model_turn_pools WHERE id = $1 FOR UPDATE",
        ).bind(input.pool_id).fetch_optional(&mut *tx).await?;
        let Some((phase, identity_state, capability_state, target, in_flight)) = pool else {
            return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                ModelTurnAdmissionRejection::PoolUnavailable,
            ));
        };
        let kinds: Vec<String> = debits
            .keys()
            .map(|kind| bucket_kind_name(*kind).to_owned())
            .collect();
        let rows: Vec<(String, i64, Option<String>)> = sqlx::query_as(
            "SELECT bucket_kind, available_units, reset_at::text FROM model_turn_bucket_bindings WHERE pool_id = $1 AND bucket_kind = ANY($2::text[]) ORDER BY bucket_kind FOR UPDATE",
        ).bind(input.pool_id).bind(&kinds).fetch_all(&mut *tx).await?;
        let bindings: BTreeMap<_, _> = rows
            .into_iter()
            .map(|(kind, units, reset)| (kind, (units, reset)))
            .collect();

        // Lookup follows the two admission lock classes, so an identical replay
        // cannot create a second lease or debit while a rival is in flight.
        if let Some(outcome) = existing_reservation_outcome(&mut tx, input, &debits).await? {
            return commit_outcome(outcome);
        }
        match parse_phase(&phase)? {
            ModelTurnAdmissionPhase::Off => {
                return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                    ModelTurnAdmissionRejection::Off,
                ));
            }
            ModelTurnAdmissionPhase::Shadow => {
                return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                    ModelTurnAdmissionRejection::ShadowOnly,
                ));
            }
            ModelTurnAdmissionPhase::Draining => {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::Draining,
                ));
            }
            ModelTurnAdmissionPhase::Enforce => {}
        }
        let identity_state = parse_identity(&identity_state)?;
        if identity_state != ModelTurnIdentityState::Eligible {
            return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                ModelTurnAdmissionRejection::IneligibleIdentity {
                    state: identity_state,
                },
            ));
        }
        match parse_capability(&capability_state)? {
            ModelTurnCapabilityState::Unknown => {
                // The pool row is locked above, so this insert elects one
                // durable owner across independent processes/connections.
                // Discovery completion changes `capability_state`; until then
                // all non-owners have an explicit durable retry condition.
                let owner_request_id: String = sqlx::query_scalar(
                    "INSERT INTO model_turn_capability_discoveries \
                     (pool_id, owner_request_id, owner_pod_uid) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (pool_id) DO UPDATE \
                     SET owner_request_id = model_turn_capability_discoveries.owner_request_id \
                     RETURNING owner_request_id",
                )
                .bind(input.pool_id)
                .bind(&input.request_id)
                .bind(&input.owner_pod_uid)
                .fetch_one(&mut *tx)
                .await?;
                let outcome =
                    ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::DiscoveryRequired {
                        is_owner: owner_request_id == input.request_id,
                        owner_request_id,
                    });
                // Unlike the other wait/rejection paths, electing discovery
                // ownership mutates durable cross-process coordination. Commit
                // it before reporting the owner so another connection cannot
                // subsequently elect itself after this transaction drops.
                tx.commit().await?;
                return Ok(outcome);
            }
            state
            @ (ModelTurnCapabilityState::Unsupported | ModelTurnCapabilityState::Degraded) => {
                return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                    ModelTurnAdmissionRejection::UnsupportedCapability { state },
                ));
            }
            ModelTurnCapabilityState::Supported => {}
        }
        if target <= in_flight {
            return commit_outcome(ModelTurnAcquireOutcome::Wait(
                ModelTurnAdmissionWait::Concurrency { target, in_flight },
            ));
        }
        for (kind, required) in &debits {
            let Some((available, reset_at)) = bindings.get(bucket_kind_name(*kind)) else {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::BindingUnavailable { bucket_kind: *kind },
                ));
            };
            if let Some(reset_at) = reset_at {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::ResetAt {
                        bucket_kind: *kind,
                        reset_at: reset_at.clone(),
                    },
                ));
            }
            if available < required {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::BucketUnavailable {
                        bucket_kind: *kind,
                        available_units: *available,
                        required_units: *required,
                        reset_at: reset_at.clone(),
                    },
                ));
            }
        }
        let reservation_id = uuid::Uuid::new_v4().to_string();
        let lease_identity =
            ModelTurnLeaseIdentity::new(input.generation, input.request_id.clone());
        sqlx::query("UPDATE model_turn_pools SET in_flight = in_flight + 1, updated_at = now() WHERE id = $1").bind(input.pool_id).execute(&mut *tx).await?;
        for (kind, units) in &debits {
            sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = available_units - $3, updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2")
                .bind(input.pool_id).bind(bucket_kind_name(*kind)).bind(*units).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO model_turn_reservations (id, pool_id, request_id) VALUES ($1::uuid, $2, $3)").bind(&reservation_id).bind(input.pool_id).bind(&input.request_id).execute(&mut *tx).await?;
        for (kind, units) in &debits {
            sqlx::query("INSERT INTO model_turn_reservation_buckets (reservation_id, pool_id, bucket_kind, reserved_units) VALUES ($1::uuid, $2, $3, $4)")
                .bind(&reservation_id).bind(input.pool_id).bind(bucket_kind_name(*kind)).bind(*units).execute(&mut *tx).await?;
        }
        // `clock_timestamp()`, not the `now()` default: `reserved_at` must be the
        // instant the lease row was actually created, not the instant this
        // transaction opened. A drain records its own instant the same way, so
        // "no lease was created after the drain committed" is a real comparison
        // between two real instants rather than between two transaction starts.
        sqlx::query("INSERT INTO model_turn_leases (lease_id, generation, pool_id, reservation_id, request_id, owner_pod_uid, reserved_at) VALUES ($1::uuid, $2, $3, $4::uuid, $5, $6, clock_timestamp())")
            .bind(&lease_identity.lease_id).bind(lease_identity.generation).bind(input.pool_id).bind(&reservation_id).bind(&input.request_id).bind(&input.owner_pod_uid).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ModelTurnAcquireOutcome::Admitted {
            reservation: ModelTurnReservation {
                id: reservation_id.clone(),
                pool_id: input.pool_id,
                request_id: input.request_id.clone(),
                state: ModelTurnReservationState::Reserved,
            },
            lease: ModelTurnLease {
                identity: lease_identity,
                pool_id: input.pool_id,
                reservation_id,
                owner_pod_uid: input.owner_pod_uid.clone(),
                lifecycle: ModelTurnLeaseLifecycle::Reserved,
                heartbeat_at: None,
            },
            idempotent: false,
        })
    }

    async fn transition(
        &self,
        identity: &ModelTurnLeaseIdentity,
        from: &str,
        to: &str,
        dispatch: bool,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let changed = sqlx::query("UPDATE model_turn_leases SET lifecycle = $4, dispatching_at = CASE WHEN $5 THEN now() ELSE dispatching_at END, active_at = CASE WHEN NOT $5 THEN now() ELSE active_at END WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND lifecycle = $6")
            .bind(&identity.lease_id).bind(identity.generation).bind(&identity.request_id).bind(to).bind(dispatch).bind(from).execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            if dispatch {
                sqlx::query("UPDATE model_turn_reservations SET state = 'dispatched' WHERE id = (SELECT reservation_id FROM model_turn_leases WHERE lease_id = $1::uuid)").bind(&identity.lease_id).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Ok(ModelTurnLeaseMutationOutcome::Applied);
        }
        let lifecycle: Option<String> = sqlx::query_scalar("SELECT lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3")
            .bind(&identity.lease_id).bind(identity.generation).bind(&identity.request_id).fetch_optional(&mut *tx).await?;
        Ok(if lifecycle.as_deref() == Some(to) {
            ModelTurnLeaseMutationOutcome::Idempotent
        } else {
            ModelTurnLeaseMutationOutcome::Fenced
        })
    }

    async fn release_accounting(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        pool_id: i64,
        reservation_id: &str,
        unsent: bool,
        usage: Option<&ModelTurnAuthoritativeUsage>,
        generation: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE model_turn_pools SET in_flight = GREATEST(0, in_flight - 1), updated_at = now() WHERE id = $1").bind(pool_id).execute(&mut **tx).await?;
        let buckets: Vec<(String, i64)> = sqlx::query_as("SELECT bucket_kind, reserved_units FROM model_turn_reservation_buckets WHERE reservation_id = $1::uuid ORDER BY bucket_kind FOR UPDATE").bind(reservation_id).fetch_all(&mut **tx).await?;
        for (kind, reserved) in buckets {
            if unsent {
                sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = LEAST(capacity_units, available_units + $3), updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2").bind(pool_id).bind(&kind).bind(reserved).execute(&mut **tx).await?;
            } else if let Some(usage) = usage {
                sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = LEAST(capacity_units, GREATEST(0, available_units + $3 - $4)), updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2").bind(pool_id).bind(&kind).bind(reserved).bind(usage_for_kind(usage, &kind)).execute(&mut **tx).await?;
            } else {
                sqlx::query("UPDATE model_turn_bucket_bindings SET quarantined_units = quarantined_units + $3, updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2").bind(pool_id).bind(&kind).bind(reserved).execute(&mut **tx).await?;
            }
        }
        // The pool reaches `off` as a consequence of the *last* in-flight
        // lease reaching a terminal state, in that lease's own transaction —
        // not because a later pass noticed and decided it should have.
        settle_drained_pool(tx, pool_id, generation).await?;
        Ok(())
    }

    /// Replace a previously quarantined reservation debit exactly once. The
    /// terminal row is locked by the caller, so a replay cannot credit twice.
    async fn resolve_quarantined_usage(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        pool_id: i64,
        reservation_id: &str,
        usage: &ModelTurnAuthoritativeUsage,
    ) -> Result<()> {
        let buckets: Vec<(String, i64)> = sqlx::query_as("SELECT bucket_kind, reserved_units FROM model_turn_reservation_buckets WHERE reservation_id = $1::uuid ORDER BY bucket_kind FOR UPDATE")
            .bind(reservation_id).fetch_all(&mut **tx).await?;
        for (kind, reserved) in buckets {
            sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = LEAST(capacity_units, GREATEST(0, available_units + $3 - $4)), quarantined_units = GREATEST(0, quarantined_units - $3), updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2")
                .bind(pool_id).bind(&kind).bind(reserved).bind(usage_for_kind(usage, &kind)).execute(&mut **tx).await?;
        }
        Ok(())
    }
}

fn lease_lifecycle_name(lifecycle: ModelTurnLeaseLifecycle) -> &'static str {
    match lifecycle {
        ModelTurnLeaseLifecycle::Reserved => "reserved",
        ModelTurnLeaseLifecycle::Dispatching => "dispatching",
        ModelTurnLeaseLifecycle::Active => "active",
        ModelTurnLeaseLifecycle::Reconciled => "reconciled",
        ModelTurnLeaseLifecycle::Expired => "expired",
    }
}

fn is_in_flight_lifecycle(lifecycle: ModelTurnLeaseLifecycle) -> bool {
    matches!(
        lifecycle,
        ModelTurnLeaseLifecycle::Reserved
            | ModelTurnLeaseLifecycle::Dispatching
            | ModelTurnLeaseLifecycle::Active
    )
}

fn terminal_outcome_name(outcome: ModelTurnLeaseTerminalOutcome) -> &'static str {
    match outcome {
        ModelTurnLeaseTerminalOutcome::Completed => "completed",
        ModelTurnLeaseTerminalOutcome::Cancelled => "cancelled",
        ModelTurnLeaseTerminalOutcome::Expired => "expired",
        ModelTurnLeaseTerminalOutcome::Failed => "failed",
    }
}

fn usage_is_negative(usage: &ModelTurnAuthoritativeUsage) -> bool {
    usage.request_units < 0
        || usage.input_units < 0
        || usage.output_units < 0
        || usage.combined_units < 0
}

fn usage_for_kind(usage: &ModelTurnAuthoritativeUsage, kind: &str) -> i64 {
    match kind {
        "request" => usage.request_units,
        "input" => usage.input_units,
        "output" => usage.output_units,
        "combined" => usage.combined_units,
        _ => 0,
    }
}

fn bucket_kind_name(kind: ModelTurnBucketKind) -> &'static str {
    match kind {
        ModelTurnBucketKind::Request => "request",
        ModelTurnBucketKind::Input => "input",
        ModelTurnBucketKind::Output => "output",
        ModelTurnBucketKind::Combined => "combined",
    }
}

fn decision_kind_name(kind: ModelTurnDecisionKind) -> &'static str {
    match kind {
        ModelTurnDecisionKind::ShadowPermit => "shadow_permit",
        ModelTurnDecisionKind::EnforceAdmitted => "enforce_admitted",
        ModelTurnDecisionKind::Wait => "wait",
        ModelTurnDecisionKind::Rejected => "rejected",
    }
}
fn is_sha256_fingerprint(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}
/// Widest durable summary the `model_turn_controller_windows.summary` column
/// accepts. Rejecting oversize summaries in Rust keeps a bounded-diagnostic
/// window from failing as an opaque Postgres `22001` string-truncation error.
const CONTROLLER_WINDOW_SUMMARY_MAX_BYTES: usize = 2048;

/// Raw projection tuple of the exact-bound controller-window read.
type ControllerWindowRow = (i64, i64, String, String, i64, i64, String, String, String);

/// Canonical RFC 3339 rendering of an exact aligned 60-second half-open window,
/// or `None` when the pair is not such a window.
fn canonical_aligned_minute_bounds(started_at: &str, ended_at: &str) -> Option<(String, String)> {
    let start = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(ended_at).ok()?;
    if start.timestamp_subsec_nanos() != 0
        || end.timestamp_subsec_nanos() != 0
        || start.timestamp().rem_euclid(60) != 0
        || end.timestamp() != start.timestamp() + 60
    {
        return None;
    }
    Some((
        start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ))
}
fn valid_aligned_minute_bounds(started_at: &str, ended_at: &str) -> bool {
    canonical_aligned_minute_bounds(started_at, ended_at).is_some()
}

/// Fail-closed read-side projection. Every rejection path returns `None`, so a
/// damaged, diagnostic, or downlevel durable row is simply invisible to the
/// learner rather than surfacing as a partially trusted window.
fn project_learner_window(row: ControllerWindowRow) -> Option<ModelTurnLearnerWindow> {
    let (
        pool_id,
        window_sequence,
        started_at,
        ended_at,
        admitted_turns,
        completed_turns,
        summary,
        pool_provider,
        pool_model,
    ) = row;
    if pool_id <= 0 || window_sequence < 0 || admitted_turns < 0 || completed_turns < 0 {
        return None;
    }
    let (started_at, ended_at) = canonical_aligned_minute_bounds(&started_at, &ended_at)?;
    if !chrono::DateTime::parse_from_rfc3339(&started_at)
        .is_ok_and(|start| start.timestamp().div_euclid(60) == window_sequence)
    {
        return None;
    }
    if summary.len() > CONTROLLER_WINDOW_SUMMARY_MAX_BYTES {
        return None;
    }
    let summary = serde_json::from_str::<ModelTurnControllerWindowSummary>(&summary).ok()?;
    if !summary.trainable
        || !summary.diagnostics.is_empty()
        || summary.provider_id != pool_provider
        || summary.model_id != pool_model
    {
        return None;
    }
    Some(ModelTurnLearnerWindow {
        pool_id,
        window_sequence,
        started_at,
        ended_at,
        admitted_turns,
        completed_turns,
        provider_id: summary.provider_id,
        model_id: summary.model_id,
    })
}
fn validate_controller_window_input(input: &ModelTurnControllerWindowInput) -> Result<()> {
    let oversize = serde_json::to_string(&input.summary)
        .map(|summary| summary.len() > CONTROLLER_WINDOW_SUMMARY_MAX_BYTES)
        .unwrap_or(true);
    if input.pool_id <= 0
        || input.window_sequence < 0
        || input.admitted_turns < 0
        || input.completed_turns < 0
        || !valid_aligned_minute_bounds(&input.started_at, &input.ended_at)
        || !chrono::DateTime::parse_from_rfc3339(&input.started_at)
            .is_ok_and(|s| s.timestamp().div_euclid(60) == input.window_sequence)
        || input.summary.provider_id.trim().is_empty()
        || input.summary.model_id.trim().is_empty()
        || input.summary.provider_id.len() > 191
        || input.summary.model_id.len() > 191
        || (input.summary.trainable && !input.summary.diagnostics.is_empty())
        || input.summary.diagnostics.len() > 64
        || input
            .summary
            .diagnostics
            .iter()
            .any(|d| d.pool_id != 0 && d.pool_id != input.pool_id)
        || oversize
        || uuid::Uuid::parse_str(&input.fence.incarnation_id).is_err()
        || chrono::DateTime::parse_from_rfc3339(&input.fence.live_since_at).is_err()
    {
        return Err(crate::Error::InvalidData(
            "invalid model-turn controller window".to_owned(),
        ));
    }
    Ok(())
}
fn valid_phase_c_identity(
    pool_id: i64,
    slot: &str,
    revision: &str,
    provider: &str,
    model: &str,
) -> bool {
    pool_id > 0
        && !slot.trim().is_empty()
        && slot.len() <= 255
        && !revision.trim().is_empty()
        && revision.len() <= 255
        && !provider.trim().is_empty()
        && provider.len() <= 191
        && !model.trim().is_empty()
        && model.len() <= 191
}
fn invalid_phase_c<T>() -> Result<T> {
    Err(crate::Error::InvalidData(
        "invalid Phase-C evidence".to_owned(),
    ))
}
fn phase_c_stage_name(stage: ModelTurnPhaseCEvidenceStage) -> &'static str {
    match stage {
        ModelTurnPhaseCEvidenceStage::Decision => "decision",
        ModelTurnPhaseCEvidenceStage::Dispatch => "dispatch",
        ModelTurnPhaseCEvidenceStage::Heartbeat => "heartbeat",
        ModelTurnPhaseCEvidenceStage::ProviderOutcome => "provider_outcome",
        ModelTurnPhaseCEvidenceStage::Reconcile => "reconcile",
    }
}
fn phase_c_outcome_name(outcome: ModelTurnPhaseCEvidenceOutcome) -> &'static str {
    match outcome {
        ModelTurnPhaseCEvidenceOutcome::Recorded => "recorded",
        ModelTurnPhaseCEvidenceOutcome::Succeeded => "succeeded",
        ModelTurnPhaseCEvidenceOutcome::Failed => "failed",
        ModelTurnPhaseCEvidenceOutcome::Missing => "missing",
    }
}
fn canonical_debits(debits: &[ModelTurnBucketDebit]) -> Result<BTreeMap<ModelTurnBucketKind, i64>> {
    let mut result = BTreeMap::new();
    for debit in debits {
        if debit.units < 0 {
            return Err(crate::Error::InvalidData(
                "model-turn bucket debit is negative".to_owned(),
            ));
        }
        let units = result.entry(debit.bucket_kind).or_insert(0_i64);
        *units = units.checked_add(debit.units).ok_or_else(|| {
            crate::Error::InvalidData("model-turn bucket debit overflows".to_owned())
        })?;
    }
    Ok(result)
}

fn commit_outcome(outcome: ModelTurnAcquireOutcome) -> Result<ModelTurnAcquireOutcome> {
    Ok(outcome)
}

/// Does fresh coverage equal the live expected denominator exactly?
///
/// Set equality, not containment: a path the coordinator expects but nothing
/// covers is a loss, and coverage reported by a path the coordinator does not
/// expect is a loss too. An empty denominator is a loss, never a vacuous pass.
///
/// A heartbeat row records that a path reported *covered at one instant*. It
/// does not record a coverage interval, so this predicate deliberately claims
/// only recency. Establishing that coverage held for a whole aligned window is
/// not derivable from what Phase B stored, and the coordinator's fail-closed
/// window qualifier — not this function — is what refuses to train on it.
async fn expected_path_coverage_held(
    tx: &mut Transaction<'_, Postgres>,
    pool_id: i64,
    evaluated_at: &str,
    expected_paths: &[ModelTurnExpectedPathKey],
) -> Result<bool> {
    let expected: std::collections::BTreeSet<&ModelTurnExpectedPathKey> =
        expected_paths.iter().collect();
    if expected.is_empty() {
        return Ok(false);
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT slot_pod_uid, deployment_revision FROM model_turn_capability_heartbeats \
         WHERE pool_id = $1 \
           AND heartbeat_at >= $2::timestamptz - make_interval(secs => $3::double precision)",
    )
    .bind(pool_id)
    .bind(evaluated_at)
    .bind(MODEL_TURN_PHASE_PREDICATE_FRESHNESS_SECONDS as f64)
    .fetch_all(&mut **tx)
    .await?;
    let covered: std::collections::BTreeSet<ModelTurnExpectedPathKey> = rows
        .into_iter()
        .map(
            |(slot_pod_uid, deployment_revision)| ModelTurnExpectedPathKey {
                slot_pod_uid,
                deployment_revision,
            },
        )
        .collect();
    Ok(covered.iter().collect::<std::collections::BTreeSet<_>>() == expected)
}

/// Settle a draining pool to `off` the moment its last lease terminalizes.
///
/// The guard is the durable counter, not a caller's belief: the update only
/// matches a pool that is still `draining` and now has `in_flight = 0`.
async fn settle_drained_pool(
    tx: &mut Transaction<'_, Postgres>,
    pool_id: i64,
    generation: i64,
) -> Result<()> {
    let settled = sqlx::query(
        "UPDATE model_turn_pools SET phase = 'off', updated_at = now() \
         WHERE id = $1 AND phase = 'draining' AND in_flight = 0",
    )
    .bind(pool_id)
    .execute(&mut **tx)
    .await?;
    if settled.rows_affected() == 1 {
        sqlx::query(
            "INSERT INTO model_turn_pool_mode_transitions \
             (pool_id, from_mode, to_mode, reason, controller_generation) \
             VALUES ($1, 'draining', 'off', 'drain_settled', $2)",
        )
        .bind(pool_id)
        .bind(generation.max(1))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The persisted spelling of one durable identity state.
#[cfg(any(test, feature = "test-support"))]
const fn identity_state_name(state: ModelTurnIdentityState) -> &'static str {
    match state {
        ModelTurnIdentityState::Eligible => "eligible",
        ModelTurnIdentityState::Revoked => "revoked",
        ModelTurnIdentityState::Ambiguous => "ambiguous",
        ModelTurnIdentityState::Colliding => "colliding",
    }
}

/// The persisted spelling of one admission mode.
const fn phase_name(phase: ModelTurnAdmissionPhase) -> &'static str {
    match phase {
        ModelTurnAdmissionPhase::Off => "off",
        ModelTurnAdmissionPhase::Shadow => "shadow",
        ModelTurnAdmissionPhase::Draining => "draining",
        ModelTurnAdmissionPhase::Enforce => "enforce",
    }
}

fn parse_phase(value: &str) -> Result<ModelTurnAdmissionPhase> {
    match value {
        "off" => Ok(ModelTurnAdmissionPhase::Off),
        "shadow" => Ok(ModelTurnAdmissionPhase::Shadow),
        "draining" => Ok(ModelTurnAdmissionPhase::Draining),
        "enforce" => Ok(ModelTurnAdmissionPhase::Enforce),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid model-turn phase: {value}"
        ))),
    }
}
fn parse_identity(value: &str) -> Result<ModelTurnIdentityState> {
    match value {
        "eligible" => Ok(ModelTurnIdentityState::Eligible),
        "revoked" => Ok(ModelTurnIdentityState::Revoked),
        "ambiguous" => Ok(ModelTurnIdentityState::Ambiguous),
        "colliding" => Ok(ModelTurnIdentityState::Colliding),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid model-turn identity state: {value}"
        ))),
    }
}
fn parse_capability(value: &str) -> Result<ModelTurnCapabilityState> {
    match value {
        "unknown" => Ok(ModelTurnCapabilityState::Unknown),
        "supported" => Ok(ModelTurnCapabilityState::Supported),
        "unsupported" => Ok(ModelTurnCapabilityState::Unsupported),
        "degraded" => Ok(ModelTurnCapabilityState::Degraded),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid model-turn capability state: {value}"
        ))),
    }
}
fn parse_reservation_state(value: &str) -> Result<ModelTurnReservationState> {
    match value {
        "reserved" => Ok(ModelTurnReservationState::Reserved),
        "dispatched" => Ok(ModelTurnReservationState::Dispatched),
        "reconciled" => Ok(ModelTurnReservationState::Reconciled),
        "expired" => Ok(ModelTurnReservationState::Expired),
        "cancelled" => Ok(ModelTurnReservationState::Cancelled),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid reservation state: {value}"
        ))),
    }
}
fn parse_lease_lifecycle(value: &str) -> Result<ModelTurnLeaseLifecycle> {
    match value {
        "reserved" => Ok(ModelTurnLeaseLifecycle::Reserved),
        "dispatching" => Ok(ModelTurnLeaseLifecycle::Dispatching),
        "active" => Ok(ModelTurnLeaseLifecycle::Active),
        "reconciled" => Ok(ModelTurnLeaseLifecycle::Reconciled),
        "expired" => Ok(ModelTurnLeaseLifecycle::Expired),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid lease lifecycle: {value}"
        ))),
    }
}

async fn existing_reservation_outcome(
    tx: &mut Transaction<'_, Postgres>,
    input: &ModelTurnAcquireInput,
    debits: &BTreeMap<ModelTurnBucketKind, i64>,
) -> Result<Option<ModelTurnAcquireOutcome>> {
    let existing: Option<(String, String, String, i64, Option<String>, String)> = sqlx::query_as("SELECT r.id::text, r.state, l.lease_id::text, l.generation, l.owner_pod_uid, l.lifecycle FROM model_turn_reservations r JOIN model_turn_leases l ON l.reservation_id = r.id WHERE r.pool_id = $1 AND r.request_id = $2 FOR UPDATE OF r, l").bind(input.pool_id).bind(&input.request_id).fetch_optional(&mut **tx).await?;
    let Some((reservation_id, reservation_state, lease_id, generation, owner_pod_uid, lifecycle)) =
        existing
    else {
        return Ok(None);
    };
    let persisted: Vec<(String, i64)> = sqlx::query_as("SELECT bucket_kind, reserved_units FROM model_turn_reservation_buckets WHERE reservation_id = $1::uuid ORDER BY bucket_kind FOR UPDATE").bind(&reservation_id).fetch_all(&mut **tx).await?;
    let expected: Vec<_> = debits
        .iter()
        .map(|(kind, units)| (bucket_kind_name(*kind).to_owned(), *units))
        .collect();
    if persisted != expected
        || generation != input.generation
        || owner_pod_uid != input.owner_pod_uid
    {
        return Ok(Some(ModelTurnAcquireOutcome::Rejected(
            ModelTurnAdmissionRejection::RequestConflict,
        )));
    }
    Ok(Some(ModelTurnAcquireOutcome::Admitted {
        reservation: ModelTurnReservation {
            id: reservation_id.clone(),
            pool_id: input.pool_id,
            request_id: input.request_id.clone(),
            state: parse_reservation_state(&reservation_state)?,
        },
        lease: ModelTurnLease {
            identity: ModelTurnLeaseIdentity {
                lease_id,
                generation,
                request_id: input.request_id.clone(),
            },
            pool_id: input.pool_id,
            reservation_id,
            owner_pod_uid,
            lifecycle: parse_lease_lifecycle(&lifecycle)?,
            heartbeat_at: None,
        },
        idempotent: true,
    }))
}

fn is_serialization_failure(error: &crate::Error) -> bool {
    matches!(error, crate::Error::Sqlx(sqlx::Error::Database(database_error)) if matches!(database_error.code().as_deref(), Some("40001" | "40P01")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_identity_uses_a_random_id_and_retains_inputs() {
        let lease = ModelTurnLeaseIdentity::new(7, "request-1");
        assert!(uuid::Uuid::parse_str(&lease.lease_id).is_ok());
        assert_eq!(lease.generation, 7);
        assert_eq!(lease.request_id, "request-1");
    }

    #[test]
    fn schema_version_is_v1() {
        assert_eq!(MODEL_TURN_ADMISSION_SCHEMA_VERSION, 1);
    }

    #[test]
    fn fingerprints_match_the_lowercase_sql_check() {
        let valid = format!("sha256:{}", "a".repeat(64));
        assert!(is_sha256_fingerprint(&valid));
        assert!(!is_sha256_fingerprint(&valid.to_uppercase()));
        assert!(!is_sha256_fingerprint("sha256:abc"));
    }

    #[test]
    fn controller_window_summary_serialization_is_closed_and_pool_local() {
        let summary = ModelTurnControllerWindowSummary {
            provider_id: "provider".into(),
            model_id: "namespace/model".into(),
            trainable: false,
            diagnostics: vec![ModelTurnControllerWindowDiagnostic {
                pool_id: 7,
                code: ModelTurnControllerWindowDiagnosticCode::MissingCapability,
            }],
        };
        assert_eq!(
            serde_json::to_value(&summary).expect("serialize closed summary"),
            serde_json::json!({
                "provider_id": "provider", "model_id": "namespace/model",
                "trainable": false,
                "diagnostics": [{"pool_id": 7, "code": "missing_capability"}],
            })
        );
        assert!(
            serde_json::from_value::<ModelTurnControllerWindowSummary>(serde_json::json!({
                "provider_id": "provider", "model_id": "model", "trainable": false,
                "diagnostics": [], "reporter_text": "forbidden"
            }))
            .is_err()
        );
        let input = ModelTurnControllerWindowInput {
            pool_id: 7,
            window_sequence: 2,
            started_at: "1970-01-01T00:02:00Z".into(),
            ended_at: "1970-01-01T00:03:00Z".into(),
            admitted_turns: 0,
            completed_turns: 0,
            summary,
            fence: ModelTurnControllerFence {
                incarnation_id: "00000000-0000-7000-8000-000000000001".into(),
                live_since_at: "1970-01-01T00:00:00Z".into(),
            },
        };
        assert!(validate_controller_window_input(&input).is_ok());
        let unrelated = ModelTurnControllerWindowInput {
            summary: ModelTurnControllerWindowSummary {
                diagnostics: vec![ModelTurnControllerWindowDiagnostic {
                    pool_id: 8,
                    code: ModelTurnControllerWindowDiagnosticCode::MissingCapability,
                }],
                ..input.summary.clone()
            },
            ..input
        };
        assert!(validate_controller_window_input(&unrelated).is_err());
    }

    #[test]
    fn controller_window_write_rejects_every_out_of_contract_dimension() {
        let valid = ModelTurnControllerWindowInput {
            pool_id: 7,
            window_sequence: 2,
            started_at: "1970-01-01T00:02:00Z".into(),
            ended_at: "1970-01-01T00:03:00Z".into(),
            admitted_turns: 3,
            completed_turns: 3,
            summary: ModelTurnControllerWindowSummary {
                provider_id: "provider".into(),
                model_id: "model".into(),
                trainable: true,
                diagnostics: Vec::new(),
            },
            fence: ModelTurnControllerFence {
                incarnation_id: "00000000-0000-7000-8000-000000000001".into(),
                live_since_at: "1970-01-01T00:00:00Z".into(),
            },
        };
        assert!(validate_controller_window_input(&valid).is_ok());

        let mutate = |f: &dyn Fn(&mut ModelTurnControllerWindowInput)| {
            let mut input = valid.clone();
            f(&mut input);
            input
        };
        let rejected: Vec<(&str, ModelTurnControllerWindowInput)> = vec![
            ("nonpositive pool", mutate(&|i| i.pool_id = 0)),
            ("negative pool", mutate(&|i| i.pool_id = -1)),
            (
                "negative sequence",
                mutate(&|i| {
                    i.window_sequence = -1;
                }),
            ),
            (
                "negative admitted count",
                mutate(&|i| i.admitted_turns = -1),
            ),
            (
                "negative completed count",
                mutate(&|i| i.completed_turns = -1),
            ),
            (
                "subsecond start",
                mutate(&|i| {
                    i.started_at = "1970-01-01T00:02:00.5Z".into();
                }),
            ),
            (
                "unaligned start",
                mutate(&|i| {
                    i.started_at = "1970-01-01T00:02:30Z".into();
                    i.ended_at = "1970-01-01T00:03:30Z".into();
                }),
            ),
            (
                "ninety second span",
                mutate(&|i| i.ended_at = "1970-01-01T00:03:30Z".into()),
            ),
            (
                "thirty second span",
                mutate(&|i| i.ended_at = "1970-01-01T00:02:30Z".into()),
            ),
            (
                "reversed bounds",
                mutate(&|i| i.ended_at = "1970-01-01T00:01:00Z".into()),
            ),
            (
                "unparsable bounds",
                mutate(&|i| i.started_at = "not-a-time".into()),
            ),
            (
                "sequence disagrees with start",
                mutate(&|i| i.window_sequence = 3),
            ),
            (
                "blank provider",
                mutate(&|i| i.summary.provider_id = "   ".into()),
            ),
            (
                "blank model",
                mutate(&|i| i.summary.model_id = String::new()),
            ),
            (
                "overlong provider",
                mutate(&|i| i.summary.provider_id = "p".repeat(192)),
            ),
            (
                "overlong model",
                mutate(&|i| i.summary.model_id = "m".repeat(192)),
            ),
            (
                "trainable with diagnostics",
                mutate(&|i| {
                    i.summary.diagnostics = vec![ModelTurnControllerWindowDiagnostic {
                        pool_id: 7,
                        code: ModelTurnControllerWindowDiagnosticCode::MissingUsage,
                    }];
                }),
            ),
            (
                "unbounded diagnostics",
                mutate(&|i| {
                    i.summary.trainable = false;
                    i.summary.diagnostics = (0..65)
                        .map(|_| ModelTurnControllerWindowDiagnostic {
                            pool_id: 7,
                            code: ModelTurnControllerWindowDiagnosticCode::MissingUsage,
                        })
                        .collect();
                }),
            ),
            (
                "foreign pool diagnostic",
                mutate(&|i| {
                    i.summary.trainable = false;
                    i.summary.diagnostics = vec![ModelTurnControllerWindowDiagnostic {
                        pool_id: 8,
                        code: ModelTurnControllerWindowDiagnosticCode::MissingUsage,
                    }];
                }),
            ),
            (
                "summary wider than the durable column",
                mutate(&|i| {
                    i.summary.trainable = false;
                    i.summary.diagnostics = (0..64)
                        .map(|_| ModelTurnControllerWindowDiagnostic {
                            pool_id: 7,
                            code:
                                ModelTurnControllerWindowDiagnosticCode::PartialCapabilityCoverage,
                        })
                        .collect();
                }),
            ),
        ];
        for (label, input) in rejected {
            assert!(
                validate_controller_window_input(&input).is_err(),
                "{label} must be rejected by the typed storage boundary"
            );
        }
        // Every accepted summary fits the durable column, so a bounded
        // diagnostic window can never fail as a Postgres truncation error.
        let bounded = mutate(&|i| {
            i.summary.trainable = false;
            i.summary.diagnostics = (0..30)
                .map(|_| ModelTurnControllerWindowDiagnostic {
                    pool_id: 7,
                    code: ModelTurnControllerWindowDiagnosticCode::PartialCapabilityCoverage,
                })
                .collect();
        });
        assert!(validate_controller_window_input(&bounded).is_ok());
        assert!(
            serde_json::to_string(&bounded.summary)
                .expect("serialize")
                .len()
                <= CONTROLLER_WINDOW_SUMMARY_MAX_BYTES
        );
    }

    #[test]
    fn learner_projection_fails_closed_on_every_damaged_durable_row() {
        let trainable = serde_json::to_string(&ModelTurnControllerWindowSummary {
            provider_id: "provider".into(),
            model_id: "model".into(),
            trainable: true,
            diagnostics: Vec::new(),
        })
        .expect("serialize trainable summary");
        let row = |summary: &str| -> ControllerWindowRow {
            (
                7,
                2,
                "1970-01-01T00:02:00.000000Z".into(),
                "1970-01-01T00:03:00.000000Z".into(),
                3,
                3,
                summary.to_owned(),
                "provider".into(),
                "model".into(),
            )
        };
        let accepted = project_learner_window(row(&trainable)).expect("canonical row projects");
        assert_eq!(accepted.started_at, "1970-01-01T00:02:00Z");
        assert_eq!(accepted.ended_at, "1970-01-01T00:03:00Z");
        assert_eq!(accepted.admitted_turns, 3);

        let mutate = |f: &dyn Fn(&mut ControllerWindowRow)| {
            let mut row = row(&trainable);
            f(&mut row);
            row
        };
        let rejected: Vec<(&str, ControllerWindowRow)> = vec![
            ("nonpositive pool", mutate(&|r| r.0 = 0)),
            ("negative sequence", mutate(&|r| r.1 = -1)),
            ("negative admitted count", mutate(&|r| r.4 = -1)),
            ("negative completed count", mutate(&|r| r.5 = -1)),
            (
                "subsecond start",
                mutate(&|r| r.2 = "1970-01-01T00:02:00.000001Z".into()),
            ),
            (
                "unaligned start",
                mutate(&|r| {
                    r.2 = "1970-01-01T00:02:30.000000Z".into();
                    r.3 = "1970-01-01T00:03:30.000000Z".into();
                }),
            ),
            (
                "ninety second span",
                mutate(&|r| r.3 = "1970-01-01T00:03:30.000000Z".into()),
            ),
            (
                "sequence disagrees with start",
                mutate(&|r| {
                    r.2 = "1970-01-01T00:03:00.000000Z".into();
                    r.3 = "1970-01-01T00:04:00.000000Z".into();
                }),
            ),
            ("unparsable bounds", mutate(&|r| r.2 = "not-a-time".into())),
            ("malformed summary json", mutate(&|r| r.6 = "{".into())),
            (
                "summary with an extra key",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":true,"diagnostics":[],"reporter_text":"leak"}"#.into();
                }),
            ),
            (
                "summary with an unknown reason code",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":false,"diagnostics":[{"pool_id":7,"code":"free_text"}]}"#.into();
                }),
            ),
            (
                "summary wider than the durable column",
                mutate(&|r| r.6 = "x".repeat(CONTROLLER_WINDOW_SUMMARY_MAX_BYTES + 1)),
            ),
            (
                "not trainable",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":false,"diagnostics":[]}"#.into();
                }),
            ),
            (
                "diagnostic window",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":true,"diagnostics":[{"pool_id":7,"code":"missing_usage"}]}"#.into();
                }),
            ),
            ("provider label mismatch", mutate(&|r| r.7 = "other".into())),
            ("model label mismatch", mutate(&|r| r.8 = "other".into())),
        ];
        for (label, row) in rejected {
            assert!(
                project_learner_window(row).is_none(),
                "{label} must yield no learner window"
            );
        }
    }

    #[test]
    fn decision_diagnostics_have_only_fixed_non_identifying_codes() {
        let codes = [
            ModelTurnDecisionDiagnostic::CapabilityUnknown.code(),
            ModelTurnDecisionDiagnostic::CapabilityUnsupported.code(),
            ModelTurnDecisionDiagnostic::PoolUnavailable.code(),
            ModelTurnDecisionDiagnostic::PolicyDraining.code(),
            ModelTurnDecisionDiagnostic::RequestInvalid.code(),
        ];
        assert!(codes.iter().all(|code| {
            code.len() <= 32
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }));
    }
}

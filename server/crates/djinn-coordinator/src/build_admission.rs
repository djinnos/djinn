// djinn:allow-oversize — durable controller, restart recovery, readiness gates, and focused tests share lifecycle invariants.
//! Coordinator-owned durable admission policy for build-producing workloads.
//!
//! The journal supplies serialization and lifecycle fencing; this module fixes
//! workload classification before dispatch and translates controller facts into
//! the data-only graph-warmer protocol.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering},
};
use std::time::Instant;

use async_trait::async_trait;
use djinn_core::clock::{Clock, SystemClock};
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionJournalRepository, AdmissionJournalRow,
    AdmissionRecoveryResult, AdmissionState, AdmissionWorkloadKind, CreateStartedInput,
    ReserveAdmissionInput, TerminalAdmissionInput, UidFencedAdmissionInput, V0Mode, V1Mode,
};
use djinn_k8s::{
    WarmAdmission, WarmAdmissionError, WarmAdmissionPermit, WarmAdmissionRequest,
    WarmAdmissionTransition,
};
use djinn_runtime::RoleResourceClass;
use tokio::sync::{Mutex, Notify};

/// Policy applied at the coordinator admission boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildAdmissionMode {
    /// Deliberately bypass durable admission during rollout.
    Off,
    /// Record reservations but never deny at the configured reference cap.
    Observe,
    /// Atomically enforce the configured cap.
    #[default]
    Enforce,
}

impl BuildAdmissionMode {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Observe => 1,
            Self::Enforce => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Off,
            1 => Self::Observe,
            2 => Self::Enforce,
            _ => Self::Enforce,
        }
    }
}

/// Smallest legal reference cap. A cap of zero would deny all admission.
pub const MIN_ADMISSION_CAP: i64 = 1;

/// Largest legal reference cap. A sane upper bound that rejects an obviously
/// mistyped configuration up front rather than letting it reach the durable row.
pub const MAX_ADMISSION_CAP: i64 = 4096;

/// Validate an admission-epoch configuration before it is written durably.
///
/// Two rules are enforced up front so a bad configuration never reaches the
/// durable handoff row:
///
/// - The illegal mode combination in which neither authority enforces
///   (`v0 ∈ {observe, disabled} ∧ v1 ∈ {off, shadow}`) is rejected.
///
///   The rule is retained but its MEANING changed when capacity accounting was
///   unified onto the v1 lease. `V0Mode::Enforce` no longer means "enforces the
///   cap" -- v0 has no cap -- it means the durable lifecycle LEDGER is
///   authoritative and fails closed. So this now rejects an epoch in which
///   neither the ledger nor the capacity authority is authoritative, which is
///   still a fail-closed misconfiguration and still the thing worth refusing.
///   Only `V1Mode::Enforce` arms the actual build-slot cap.
/// - The reference cap must be within `[MIN_ADMISSION_CAP, MAX_ADMISSION_CAP]`.
pub fn validate_admission_config(v0: V0Mode, v1: V1Mode, cap: i64) -> Result<(), String> {
    if !v0.is_enforcing() && !v1.is_enforcing() {
        return Err(format!(
            "illegal admission mode combination: neither authority enforces \
             (v0={v0:?}, v1={v1:?}); at least one of v0 or v1 must enforce the cap"
        ));
    }
    if !(MIN_ADMISSION_CAP..=MAX_ADMISSION_CAP).contains(&cap) {
        return Err(format!(
            "admission cap {cap} is out of range [{MIN_ADMISSION_CAP}, {MAX_ADMISSION_CAP}]"
        ));
    }
    Ok(())
}

/// Typed classification captured before dispatch. Two classes weigh zero: the
/// explicitly audited [`BuildWorkloadKind::NonBuild`] bypass, and a task-run
/// whose role is [`RoleResourceClass::Light`] (see [`TaskRunRole`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildWorkloadKind {
    TaskRun {
        role: TaskRunRole,
    },
    GraphWarmJob,
    /// Explicit, auditable non-build work.
    NonBuild {
        audit_reason: &'static str,
    },
}

/// Audit reason recorded when a Light task-run is admitted without reserving a
/// build slot.
///
/// Distinct and greppable on purpose: it is the single string that explains why
/// an admitted task-run left no journal row behind.
///
/// It states a dispatch-admission prior, NOT a capability boundary. Light roles
/// are *unlikely* to run the project's compile/test toolchain — measured at 5.5%
/// of light sessions on 2026-07-25, 8.1% for reviewers alone — which is why
/// pre-charging them a scarce slot is the wrong trade, not because they cannot
/// compile. The ones that do compile are governed by the measured, role-agnostic
/// invocation lease. See [`djinn_runtime::RoleResourceClass`], whose earlier
/// claim that these roles "never run the project's compile/test toolchain" was
/// false when written and took 34 days to be caught.
pub const LIGHT_ROLE_AUDIT_REASON: &str = "light role: not pre-charged a build slot at dispatch (unlikely to compile); \
     any compile it does run is governed by the invocation lease";

/// Every task-run role the coordinator can dispatch.
///
/// These roles are NOT uniformly build-producing. Only Worker and Architect
/// (and Verifier, which is an in-pod stage — see [`TaskRunRole::parse`]) run the
/// project's compile/test toolchain; Planner, Reviewer, Lead and the refinement
/// tribunal (Advocate/Adversary/Judge) are orchestration-only. The distinction
/// is owned by [`djinn_runtime::RoleResourceClass`] — the single classifier
/// shared with `djinn-k8s` pod sizing — and reached here through
/// [`TaskRunRole::resource_class`]. Admission consumes a build slot only for
/// [`RoleResourceClass::BuildCapable`]: with a production cap of 3 on a 12-vCPU
/// node, charging a Planner or a tribunal round a slot would queue it behind
/// builds it never competes with and collapse throughput.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunRole {
    Worker,
    Reviewer,
    Lead,
    Planner,
    Architect,
    Advocate,
    Adversary,
    Judge,
}

impl TaskRunRole {
    /// Classify a known coordinator role. Unknown and missing values fail closed.
    ///
    /// There is deliberately no `"verifier"` arm. `djinn_runtime::RoleKind`
    /// carries a `Verifier`, but it is an IN-POD supervisor stage, not a
    /// coordinator dispatch role: `djinn_roles::AgentType` has no `Verifier`
    /// variant, `RoleRegistry::new` registers none, and the agent maps
    /// `RoleKind::Verifier` back onto `AgentType::Worker`
    /// (`djinn-agent/src/actors/slot/lifecycle/role_overrides.rs`,
    /// `djinn-agent/src/supervisor_impl/stage.rs`). Every production caller of
    /// `admit_task_run` passes either a `RoleRegistry` dispatch role, the literal
    /// `"planner"` (`dispatch/retry.rs`), or a refinement `agent_type`
    /// (`advocate`/`adversary`/`judge`) — never `"verifier"`. A verifier's
    /// compile therefore runs inside a Worker task-run that already holds a slot.
    /// If a verifier ever becomes separately dispatchable it must be added here
    /// as build-capable; until then adding it would be dead classification.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            Some("worker") => Some(Self::Worker),
            Some("reviewer") => Some(Self::Reviewer),
            Some("lead") => Some(Self::Lead),
            Some("planner") => Some(Self::Planner),
            Some("architect") => Some(Self::Architect),
            Some("advocate") => Some(Self::Advocate),
            Some("adversary") => Some(Self::Adversary),
            Some("judge") => Some(Self::Judge),
            _ => None,
        }
    }

    /// Canonical lowercase dispatch-role string; the exact inverse of
    /// [`Self::parse`], which the round-trip test locks.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Lead => "lead",
            Self::Planner => "planner",
            Self::Architect => "architect",
            Self::Advocate => "advocate",
            Self::Adversary => "adversary",
            Self::Judge => "judge",
        }
    }

    /// Whether this role's task-run may run the project's compile/test toolchain.
    ///
    /// Delegates to [`djinn_runtime::RoleResourceClass`] rather than keeping a
    /// second table here: pod sizing and build admission must never disagree
    /// about what "light" means.
    #[must_use]
    pub fn resource_class(self) -> RoleResourceClass {
        RoleResourceClass::for_role_name(self.as_str())
    }
}

/// Where a request's build capacity comes from.
///
/// This field exists because the previous design had no way to say it, and the
/// resulting ambiguity was the defect: the v0 journal and the v1 lease each
/// assumed they were the authority, each enforced `DJINN_MAX_BUILD_TASKRUNS`,
/// and because they covered disjoint populations they together admitted twice
/// the operator's intent. Worse, they were structurally blind to each other --
/// a leased warm Job wrote no journal row at all, so neither could observe the
/// other's occupancy even in principle.
///
/// Making the capacity holder an explicit, exhaustively-matched part of the
/// request means a new admission caller cannot compile without stating where
/// its capacity came from. There is no default and no inference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapacitySource {
    /// The controller must acquire a layer-1 dispatch slot before proceeding.
    /// Used by task dispatch for build-capable roles.
    AcquireDispatchSlot,
    /// The caller already occupies a build-lease row and is presenting it. The
    /// journal write is ledger-only and cannot deny. Used by the graph warmer,
    /// which holds a `graph_warm` lease before it ever reaches admission.
    HeldByLease,
    /// This work occupies no capacity, for an explicitly audited reason.
    /// Light-role task-runs and the `NonBuild` bypass.
    ZeroWeight { audit_reason: &'static str },
}

/// Immutable identity fixed before capacity is reserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionRequest {
    pub domain: AdmissionDomain,
    pub work_id: String,
    pub generation: i64,
    pub object_name: String,
    pub kind: BuildWorkloadKind,
    /// Which authority owns this request's capacity. See [`CapacitySource`].
    pub capacity: CapacitySource,
}

/// Outcome of one layer-1 dispatch-slot acquisition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchSlotOutcome {
    /// Capacity was acquired (or was already held by this exact identity).
    Granted,
    /// The unified pool is full. The task stays queued.
    AtCapacity { occupancy: i64, cap: i64 },
    /// The authority is not enforcing yet. Nothing was acquired and nothing is
    /// denied; `would_defer` records what enforcement WOULD have done.
    Observed { would_defer: bool },
    /// The authority did not return a capacity answer. Fails closed for
    /// Enforce.
    ///
    /// `detail` is the authority's own words -- typically the lease outcome or
    /// `terminal_reason` that produced this. It exists because the previous
    /// unit-variant form was indistinguishable, at the operator's log, from a
    /// pool that was genuinely full: a lease tombstone arrived here and was
    /// printed as `occupancy=0 cap=3`, a denial no capacity rule can justify.
    Unavailable { detail: String },
}

/// The single capacity authority, as the admission controller sees it.
///
/// Implemented over the v1 build lease, which is the only place occupancy is
/// counted. It is a trait rather than a concrete type for two reasons: it keeps
/// `djinn-coordinator`'s admission module free of a dependency on the lease's
/// internals, and -- more importantly -- it is the seam where a cap COMPUTED
/// from the node's allocatable CPU replaces the hand-set
/// `DJINN_MAX_BUILD_TASKRUNS` without any change to the grant path. Nothing
/// below composition reads an environment variable to learn the cap.
#[async_trait]
pub trait BuildSlotAuthority: Send + Sync {
    /// Acquire (or idempotently re-acquire) a dispatch slot for one attempt.
    async fn acquire_dispatch_slot(&self, task_id: &str, generation: i64) -> DispatchSlotOutcome;

    /// Release a dispatch slot once its task-run reaches a terminal state.
    /// Idempotent: a slot that was never acquired, or already released, is a
    /// no-op rather than an error.
    async fn release_dispatch_slot(&self, task_id: &str, generation: i64);

    /// Drop any still-QUEUED dispatch reservation for a closed task.
    ///
    /// A queued row occupies nothing, so it is tempting to leave it. But
    /// `grant_next` selects the oldest queued row, so an orphan whose task is
    /// gone is eventually granted, occupies a slot, and is released by nobody.
    /// A closed task must therefore surrender its queue position explicitly.
    async fn abandon_queued_dispatch(&self, task_id: &str);

    /// Currently occupied capacity, in build slots, across EVERY population.
    async fn occupancy(&self) -> Option<i64>;

    /// The cap being enforced. Resolved by the authority, never by the caller.
    fn cap(&self) -> i64;
}

/// Admission decision returned to task dispatch callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildAdmissionDecision {
    Permitted {
        permit: WarmAdmissionPermit,
        idempotent: bool,
    },
    Denied {
        /// Occupancy as READ from the capacity authority, or `None` when this
        /// denial never consulted occupancy at all.
        ///
        /// `Option` rather than `i64` on purpose. The old shape forced every
        /// non-capacity denial to invent a number, and the invented number was
        /// `0` -- which the only operator-facing log then printed verbatim,
        /// asserting a capacity figure arithmetically incapable of justifying
        /// the denial it accompanied. A denial that did not measure occupancy
        /// must now say so.
        occupancy: Option<i64>,
        cap: i64,
        /// Why. Printed alongside the numbers so a tombstoned lease can never
        /// again be mistaken for a full pool.
        cause: DenialCause,
    },
    /// Classification was absent or unrecognized. The observation counter is bounded.
    Unclassified,
}

/// Why one build-admission request was denied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenialCause {
    /// The genuine one: weighted occupancy plus this request's weight exceeds
    /// the cap. Only this variant carries a measured occupancy.
    AtCapacity,
    /// The controller itself is not admitting -- not yet recovered, or
    /// draining. Nothing was measured and nothing was acquired.
    ///
    /// `readiness` is which gate is closed. Without it this variant is the
    /// least actionable string in the system: on 2026-07-29 five hours of logs
    /// read `cause: "controller_not_admitting"`, and the field that said WHICH
    /// gate (`readiness=create_unknown_health`) was on a different, adjacent
    /// log line that nobody had a reason to correlate. The two travel together
    /// now, and the pair is what gets persisted.
    ///
    /// `Display` is unchanged (`controller_not_admitting`) on purpose: it is
    /// the string operators and runbooks already grep for. The readiness rides
    /// alongside as structured data instead of mutating that identity.
    ControllerNotAdmitting { readiness: BuildAdmissionReadiness },
    /// The build-slot authority answered with something that is not a capacity
    /// answer. `detail` is its own words.
    AuthorityUnavailable { detail: String },
}

impl std::fmt::Display for DenialCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCapacity => f.write_str("at_capacity"),
            Self::ControllerNotAdmitting { .. } => f.write_str("controller_not_admitting"),
            Self::AuthorityUnavailable { detail } => {
                write!(f, "authority_unavailable: {detail}")
            }
        }
    }
}

impl DenialCause {
    /// The bare cause name, without the `AuthorityUnavailable` detail suffix.
    ///
    /// This is what goes in `build_admission_denials.cause`, which is
    /// CHECK-constrained to the three names; `Display` is the log form.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AtCapacity => "at_capacity",
            Self::ControllerNotAdmitting { .. } => "controller_not_admitting",
            Self::AuthorityUnavailable { .. } => "authority_unavailable",
        }
    }

    /// The closed readiness gate, when the controller is the one refusing.
    #[must_use]
    pub fn readiness(&self) -> Option<BuildAdmissionReadiness> {
        match self {
            Self::ControllerNotAdmitting { readiness } => Some(*readiness),
            _ => None,
        }
    }

    /// The capacity authority's own words, when it is the one that answered
    /// with something that is not a capacity answer.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::AuthorityUnavailable { detail } => Some(detail),
            _ => None,
        }
    }
}

/// Observe-only disk-capacity adapter for build admission (proposal nquz,
/// phase 1 — OBSERVE).
///
/// A source, when installed, returns what disk admission WOULD do for a build
/// request — never denying and never allocating. `observe` is consulted only
/// AFTER a permit has been issued, so an installed source can add telemetry but
/// can never turn a permit into a denial. The production source is composed by
/// `crate::run_dir_observe`; the concrete decision logic lives in
/// [`crate::disk_admission`].
#[async_trait]
pub trait DiskCapacitySource: Send + Sync {
    /// Return the observed disk decision for a build request, or `None` when no
    /// capacity sample is available (which the caller treats as no-op).
    async fn observe(
        &self,
        request: &BuildAdmissionRequest,
    ) -> Option<crate::disk_admission::DiskObservation>;
}

/// Bounded, deterministic readiness reason for Enforce admission gating.
///
/// Enforce admission fails closed until every required gate is healthy. Observe
/// records degradation but remains non-denying. Off has no readiness coupling.
/// Variants are exhaustive and intentionally bounded so telemetry and tests can
/// rely on a stable, enumerated set. The default is fail-closed
/// ([`BuildAdmissionReadiness::JournalRecoveryIncomplete`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BuildAdmissionReadiness {
    /// The journal has not been recovered yet; Enforce starts in this state.
    #[default]
    JournalRecoveryIncomplete,
    /// The journal itself is unhealthy (a recovery/seed query failed).
    JournalUnhealthy,
    /// At least one recovered row is in CreateUnknown state.
    CreateUnknownHealth,
    /// Seeded occupancy exceeded the configured cap after recovery.
    SeededOccupancyAboveCap,
    /// Kubernetes inventory has not completed yet.
    InventoryPending,
    /// Single-active topology check has not succeeded yet.
    TopologyPending,
    /// Graceful shutdown is draining; new reservations are blocked.
    ShutdownDraining,
    /// Every required gate is healthy; admission may proceed.
    Healthy,
}

impl BuildAdmissionReadiness {
    #[must_use]
    pub fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// Stable snake_case label. Bounded and closed, so it is safe as a metric
    /// label, a doctor-finding field, and an operator-facing string all at
    /// once — an operator reading a finding sees the SAME token the code
    /// branches on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JournalRecoveryIncomplete => "journal_recovery_incomplete",
            Self::JournalUnhealthy => "journal_unhealthy",
            Self::CreateUnknownHealth => "create_unknown_health",
            Self::SeededOccupancyAboveCap => "seeded_occupancy_above_cap",
            Self::InventoryPending => "inventory_pending",
            Self::TopologyPending => "topology_pending",
            Self::ShutdownDraining => "shutdown_draining",
            Self::Healthy => "healthy",
        }
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::JournalRecoveryIncomplete => 0,
            Self::JournalUnhealthy => 1,
            Self::CreateUnknownHealth => 2,
            Self::SeededOccupancyAboveCap => 3,
            Self::InventoryPending => 4,
            Self::TopologyPending => 5,
            Self::ShutdownDraining => 6,
            Self::Healthy => 7,
        }
    }
}

impl std::fmt::Display for BuildAdmissionReadiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snake_case label for an admission domain, stable enough to appear in an
/// operator-facing identity string.
const fn domain_label(domain: AdmissionDomain) -> &'static str {
    match domain {
        AdmissionDomain::TaskObservation => "task_observation",
        AdmissionDomain::WarmBuild => "warm_build",
        AdmissionDomain::InvocationBuild => "invocation_build",
    }
}

/// The operator-facing identity of one admission-journal row.
///
/// Mirrors the `identity=dispatch:{task_id}:{generation}` convention the
/// sibling `build_lease_reclaim` path already logs, extended with the object
/// name so the row can be looked up in Kubernetes without a database at all.
#[must_use]
pub fn blocking_identity(key: &AdmissionJournalKey, object_name: &str) -> String {
    format!(
        "{}:{}:{}@{}",
        domain_label(key.domain),
        key.work_id,
        key.generation,
        object_name
    )
}

/// Sentinel for "no readiness has been reported yet", distinct from every
/// [`BuildAdmissionReadiness`] discriminant so the very first report always
/// fires an edge.
const READINESS_NEVER_REPORTED: u8 = u8::MAX;

/// How many blocking row identities a single report or snapshot names before it
/// switches to a count. Bounded for the same reason `named_reclaim_failures`
/// is: a log line and a doctor finding must stay a fixed size no matter how
/// large the wedge is.
pub const MAX_NAMED_BLOCKING_IDENTITIES: usize = 16;

/// Operator-facing description of why build admission is (not) open.
///
/// Assembled from process-local state only, and cheap enough to build on the
/// synchronous `DoctorCheck::run` seam: readiness is derived from atomics and
/// the identity set is a bounded in-memory set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionHealthReport {
    /// The current bounded readiness reason.
    pub readiness: BuildAdmissionReadiness,
    /// The configured/effective admission mode.
    pub mode: BuildAdmissionMode,
    /// Whether the shutdown-draining latch is set.
    pub draining: bool,
    /// How many recovered rows still occupy as CreateUnknown.
    pub create_unknown_pending: u64,
    /// Bounded identities of those rows — WHICH rows are wedging the board.
    pub blocking_identities: Vec<String>,
    /// Identities elided by [`MAX_NAMED_BLOCKING_IDENTITIES`].
    pub blocking_identities_elided: usize,
    /// Seconds since the last blocker-free reconciliation pass, or `None` if no
    /// pass has ever completed in this process.
    pub seconds_since_last_reconcile: Option<i64>,
}

/// [`BuildAdmissionHealthReport`] plus the capacity facts an HTTP surface can
/// afford to read.
///
/// # Why this exists on top of the health report
///
/// On 2026-07-29 the controller latched `CreateUnknownHealth` and denied every
/// dispatch on the board for five hours. `/debug/dispatch-state` — the endpoint
/// whose entire purpose is answering "why is nothing dispatching" — omitted
/// build admission completely, and every field it DID report was healthy.
///
/// The health report is the shared answer and is deliberately synchronous, so
/// the doctor check keeps working when the database is the problem. Occupancy
/// is not free: it consults the capacity authority. Rather than duplicate the
/// report's fields, this type EMBEDS it, so the doctor and the debug endpoint
/// can never disagree about readiness, mode or which rows are blocking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionDebugSnapshot {
    /// The same report the `build_admission_health` doctor check consumes.
    pub health: BuildAdmissionHealthReport,
    /// Every currently-failing readiness gate, in fail-closed priority order.
    /// Empty exactly when `health.readiness` is `Healthy`. The report carries
    /// only the highest-priority one, because that is what `admit()` acts on.
    pub unsatisfied_gates: Vec<&'static str>,
    /// The cap actually in force, resolved from the capacity authority.
    pub effective_cap: i64,
    /// The constructor's fallback cap, used only when no authority is
    /// installed. Reported so a disagreement with `effective_cap` is visible.
    pub configured_cap: i64,
    /// Build slots the capacity authority reports in use, or `None` when no
    /// authority is installed or it could not be read. `None` is NOT zero.
    pub occupancy: Option<i64>,
    /// This process's admission epoch, for comparison against
    /// `admission_journal.creator_server_epoch`.
    pub server_epoch: String,
    /// Requests currently parked in the queued-lifecycle map.
    pub queued: usize,
}

#[derive(Clone, Debug)]
struct PermitState {
    key: AdmissionJournalKey,
    creator_server_epoch: String,
    object_name: String,
    durable: bool,
    released: bool,
    /// This permit was seeded from a recovered CreateUnknown row and has not
    /// yet been adopted into Live. Tracked so the startup CreateUnknown gate
    /// is decremented exactly once when the row resolves.
    create_unknown_outstanding: bool,
    /// Where this permit's capacity came from, retained so the terminal
    /// transition hands back exactly what admission took -- and nothing else.
    /// Releasing a slot this permit never acquired would free another
    /// task-run's capacity.
    capacity: CapacitySource,
}

trait QueueClock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemQueueClock;

impl QueueClock for SystemQueueClock {
    fn now(&self) -> Instant {
        SystemClock::new().now_instant()
    }
}

/// Outcome of durable predecessor-epoch recovery and controller seeding.
///
/// The controller seeds in-memory permit bookkeeping from the durable active
/// rows returned by [`AdmissionJournalRepository::recover_predecessor_epoch`]
/// without duplicating occupancy or relying on a separate in-memory permit
/// count: occupancy is always derived from the journal itself.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AdmissionSeedReport {
    /// Number of predecessor Reserved rows atomically retired to Terminal.
    pub retired_reserved: u64,
    /// Number of predecessor CreateInFlight rows converted to CreateUnknown.
    pub marked_create_unknown: u64,
    /// Number of recovered rows the controller seeded as occupying permits.
    pub seeded_rows: u64,
    /// Final readiness reason applied after seeding completed.
    pub readiness: BuildAdmissionReadiness,
}

/// A single controller shared by task-run dispatch and graph warming.
pub struct BuildAdmissionController {
    journal: Arc<AdmissionJournalRepository>,
    mode: AtomicU8,
    cap: i64,
    creator_server_epoch: String,
    permits: Mutex<HashMap<WarmAdmissionPermit, PermitState>>,
    permits_by_key: Mutex<HashMap<String, WarmAdmissionPermit>>,
    /// The permit for each work item's CURRENT admission generation.
    ///
    /// A journal generation is one object lifecycle, so a second dispatch
    /// attempt for the same work reserves a new generation whose number the
    /// caller does not know. Lifecycle callbacks that only carry the work
    /// identity — a session start that has just learned its runtime UID —
    /// resolve their permit here rather than recomputing a generation from a
    /// caller-side counter that no longer identifies the attempt.
    permits_by_work: Mutex<HashMap<String, WarmAdmissionPermit>>,
    /// Runtime task-run IDs are learned when a session starts. This binding
    /// prevents a delayed terminal callback from selecting a later generation.
    permits_by_task_run: Mutex<HashMap<String, WarmAdmissionPermit>>,
    /// Durable lifecycle transitions the journal accepted / rejected. Exposed
    /// as bounded telemetry so a fleet-wide rejection rate is observable
    /// without log scraping, and retained in-process so the readiness surface
    /// can report the last rejection reason verbatim.
    accepted_transitions: AtomicU64,
    rejected_transitions: AtomicU64,
    last_transition_rejection: Mutex<Option<String>>,
    unclassified_observations: Mutex<u64>,
    would_defer_observations: Mutex<u64>,
    /// Bounded observe-only disk would-defer signal (proposal nquz, phase 1).
    ///
    /// Advances only when a [`DiskCapacitySource`] has been installed. The disk
    /// dimension NEVER changes an admission decision — it records what disk
    /// admission WOULD do.
    disk_would_defer_observations: Mutex<u64>,
    disk_capacity_source: std::sync::Mutex<Option<Arc<dyn DiskCapacitySource>>>,
    /// Readiness gate flags. The bounded [`BuildAdmissionReadiness`] reason is
    /// DERIVED from these flags in fail-closed priority order, so no caller can
    /// mark Enforce healthy without every real startup check completing:
    /// journal recovery, journal health, CreateUnknown resolution, cap
    /// seeding, Kubernetes inventory, and single-active topology.
    ///
    /// The durable journal has been loaded and recovered for this process.
    journal_recovered: AtomicBool,
    /// The recovery/seed queries themselves succeeded.
    journal_healthy: AtomicBool,
    /// Recovered rows still occupying as CreateUnknown. Seeding sets this from
    /// the durable journal; adopting a seeded CreateUnknown row into Live
    /// decrements it exactly once. Enforce stays closed while it is non-zero.
    create_unknown_pending: AtomicU64,
    /// WHICH rows are holding [`BuildAdmissionReadiness::CreateUnknownHealth`]
    /// closed, as bounded `{domain}:{work_id}:{generation}@{object_name}`
    /// identities.
    ///
    /// A count alone is what made the 2026-07-29 outage cost five hours: every
    /// build-admission log line reported `stale`/`reclaimed`/`create_unknown`
    /// as NUMBERS, so an operator could see that exactly one row was denying
    /// every dispatch and had no non-SQL way to learn which one. The sibling
    /// `build_lease_reclaim` path already logs `identity=dispatch:…`; this is
    /// the same convention for the admission journal.
    ///
    /// Maintained on exactly the edges that maintain `create_unknown_pending`,
    /// so the two never disagree about whether the gate is held.
    create_unknown_identities: std::sync::Mutex<BTreeSet<String>>,
    /// Unix seconds at which a reconciliation pass last completed WITHOUT
    /// blockers. Zero means "never in this process".
    ///
    /// The single most valuable signal added after the outage: nothing anywhere
    /// asserted that a reconciliation pass had completed within the last N
    /// seconds, so a reconciler that had silently died looked exactly like one
    /// that was working. Read by
    /// [`Self::seconds_since_last_reconcile`], exported as a gauge, and
    /// surfaced to operators by the `build_admission_health` doctor check.
    last_reconcile_success_unix: AtomicI64,
    /// The readiness reason last reported by [`Self::report_readiness_edge`],
    /// so the loud gate report is edge-triggered rather than once per publish.
    last_reported_readiness: AtomicU8,
    /// Seeded durable occupancy exceeded the configured cap at recovery.
    /// Cleared when a terminal release brings occupancy back within the cap.
    over_cap: AtomicBool,
    /// Whether the loud over-cap alarm has already been logged for the current
    /// episode. Occupancy is republished on every admission and every
    /// lifecycle transition, so the alarm is edge-triggered: exactly one
    /// `ERROR` when durable occupancy crosses the cap and exactly one `INFO`
    /// when it comes back under, instead of one line per publication.
    over_cap_alarm_active: AtomicBool,
    /// The broad Kubernetes inventory LIST completed successfully.
    inventory_ready: AtomicBool,
    /// The single-active topology gate (coordinator leadership) is held by
    /// this process.
    topology_ready: AtomicBool,
    /// Graceful shutdown begins draining before permit release. New Enforce
    /// reservations are blocked while this is set; Observe/Off are unaffected.
    draining: AtomicBool,
    released: Notify,
    queued_lifecycle: std::sync::Mutex<HashMap<String, Instant>>,
    queue_clock: Arc<dyn QueueClock>,
    /// Whether this controller writes the process-global admission metrics.
    ///
    /// Always set in production. Test builds default it OFF because the
    /// Prometheus recorder is a process-wide singleton while cargo runs every
    /// test in the binary as a thread of one process: the occupancy gauges are
    /// unlabelled and written with `set`, so any controller publishing
    /// concurrently makes another test's reading arbitrary. Only the tests that
    /// actually assert on these series opt in, via
    /// `enable_process_metrics_for_test`, and they serialize against each other
    /// on [`telemetry_guard`]. Gating emission at the writer means a
    /// test that never opts in cannot corrupt the reading no matter what
    /// admission path it exercises.
    emit_process_metrics: AtomicBool,
    /// The single build-slot capacity authority (the v1 lease).
    ///
    /// `None` means this controller is not capacity-gated -- the Off shape and
    /// the many focused tests that exercise lifecycle without a pool. It is an
    /// Option rather than a required field so that "no capacity authority" is a
    /// visible, matched state instead of a silently permissive default.
    slot_authority: Option<Arc<dyn BuildSlotAuthority>>,
}

impl BuildAdmissionController {
    #[must_use]
    pub fn new(
        journal: Arc<AdmissionJournalRepository>,
        mode: BuildAdmissionMode,
        cap: i64,
        creator_server_epoch: impl Into<String>,
    ) -> Self {
        Self {
            journal,
            mode: AtomicU8::new(mode.as_u8()),
            cap,
            creator_server_epoch: creator_server_epoch.into(),
            permits: Mutex::new(HashMap::new()),
            permits_by_key: Mutex::new(HashMap::new()),
            permits_by_work: Mutex::new(HashMap::new()),
            permits_by_task_run: Mutex::new(HashMap::new()),
            accepted_transitions: AtomicU64::new(0),
            rejected_transitions: AtomicU64::new(0),
            last_transition_rejection: Mutex::new(None),
            unclassified_observations: Mutex::new(0),
            would_defer_observations: Mutex::new(0),
            disk_would_defer_observations: Mutex::new(0),
            disk_capacity_source: std::sync::Mutex::new(None),
            journal_recovered: AtomicBool::new(true),
            journal_healthy: AtomicBool::new(true),
            create_unknown_pending: AtomicU64::new(0),
            create_unknown_identities: std::sync::Mutex::new(BTreeSet::new()),
            last_reconcile_success_unix: AtomicI64::new(0),
            last_reported_readiness: AtomicU8::new(READINESS_NEVER_REPORTED),
            over_cap: AtomicBool::new(false),
            over_cap_alarm_active: AtomicBool::new(false),
            inventory_ready: AtomicBool::new(true),
            topology_ready: AtomicBool::new(true),
            draining: AtomicBool::new(false),
            released: Notify::new(),
            queued_lifecycle: std::sync::Mutex::new(HashMap::new()),
            queue_clock: Arc::new(SystemQueueClock),
            emit_process_metrics: AtomicBool::new(!cfg!(test)),
            slot_authority: None,
        }
    }

    /// Install the single build-slot capacity authority.
    ///
    /// Composition passes the v1 `BuildLeaseService` adapter. This is the ONLY
    /// place capacity enters the controller; there is no environment read and
    /// no second cap below this point, which is what lets a node-derived cap
    /// replace the configured one without touching admission.
    #[must_use]
    pub fn with_slot_authority(mut self, authority: Arc<dyn BuildSlotAuthority>) -> Self {
        self.slot_authority = Some(authority);
        self
    }

    /// Whether this controller may write the process-global admission metrics.
    fn process_metrics_enabled(&self) -> bool {
        self.emit_process_metrics.load(Ordering::Acquire)
    }

    /// Opt this controller into publishing the process-global admission
    /// metrics. Only for tests that assert on those series, and only while
    /// holding [`telemetry_guard`].
    #[cfg(test)]
    pub(crate) fn enable_process_metrics_for_test(&self) {
        self.emit_process_metrics.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn new_with_queue_clock(
        journal: Arc<AdmissionJournalRepository>,
        mode: BuildAdmissionMode,
        cap: i64,
        creator_server_epoch: impl Into<String>,
        queue_clock: Arc<dyn QueueClock>,
    ) -> Self {
        let mut controller = Self::new(journal, mode, cap, creator_server_epoch);
        controller.queue_clock = queue_clock;
        controller
    }

    /// Construct an Enforce controller which cannot admit work until every
    /// startup gate completes.
    ///
    /// The controller starts fail-closed with all startup gates unsatisfied:
    /// journal recovery, Kubernetes inventory, and the single-active topology
    /// check must each complete before admission opens. Observe and Off never
    /// gate admission and are constructed via [`Self::new`].
    #[must_use]
    pub fn new_closed(
        journal: Arc<AdmissionJournalRepository>,
        cap: i64,
        creator_server_epoch: impl Into<String>,
    ) -> Self {
        let controller = Self::new(
            journal,
            BuildAdmissionMode::Enforce,
            cap,
            creator_server_epoch,
        );
        controller.journal_recovered.store(false, Ordering::Release);
        controller.inventory_ready.store(false, Ordering::Release);
        controller.topology_ready.store(false, Ordering::Release);
        controller
    }

    /// Open the controller after every startup gate has completed.
    ///
    /// This satisfies all readiness gates at once. Production startup uses the
    /// granular `mark_*` methods as each real check completes (journal
    /// recovery first, then inventory, then topology); this helper is for
    /// tests that need an open Enforce controller without walking startup.
    pub fn mark_ready(&self) {
        self.journal_recovered.store(true, Ordering::Release);
        self.journal_healthy.store(true, Ordering::Release);
        self.create_unknown_pending.store(0, Ordering::Release);
        self.clear_blocking_identities();
        self.over_cap.store(false, Ordering::Release);
        self.inventory_ready.store(true, Ordering::Release);
        self.topology_ready.store(true, Ordering::Release);
        // `readiness()` checks the draining latch FIRST, ahead of every gate
        // this method satisfies. Leaving the latch set here would make
        // "mark ready" a method that provably does not make the controller
        // ready — the exact class of silent contradiction that produced the
        // 2026-07-19 `occupancy 0 reached cap 3` wedge.
        self.draining.store(false, Ordering::Release);
    }

    /// Promote this controller to emergency Enforce and reset every startup
    /// gate. The handoff reader invokes this before recovery so a configured
    /// Off/Observe process cannot weaken a durable emergency-primary epoch.
    pub fn require_enforcement(&self) {
        self.mode
            .store(BuildAdmissionMode::Enforce.as_u8(), Ordering::Release);
        self.journal_recovered.store(false, Ordering::Release);
        self.journal_healthy.store(true, Ordering::Release);
        self.create_unknown_pending.store(0, Ordering::Release);
        self.clear_blocking_identities();
        self.over_cap.store(false, Ordering::Release);
        self.inventory_ready.store(false, Ordering::Release);
        self.topology_ready.store(false, Ordering::Release);
        // The draining latch is deliberately NOT cleared here. This path is
        // emergency promotion of a possibly-live process; a process that is
        // genuinely shutting down must not be talked back into admitting work
        // by a handoff tick that happens to land during teardown.
    }

    /// Release emergency authority only after a committed invocation-primary
    /// epoch is observed by the handoff policy.
    pub fn disable(&self) {
        self.mode
            .store(BuildAdmissionMode::Off.as_u8(), Ordering::Release);
    }

    /// Record that journal recovery failed. Enforce stays fail-closed with
    /// [`BuildAdmissionReadiness::JournalUnhealthy`]; Observe records the same
    /// degradation but never denies.
    pub fn mark_journal_unhealthy(&self) {
        self.journal_recovered.store(true, Ordering::Release);
        self.journal_healthy.store(false, Ordering::Release);
    }

    /// The broad Kubernetes inventory LIST completed successfully.
    pub fn mark_inventory_ready(&self) {
        self.inventory_ready.store(true, Ordering::Release);
    }

    /// The Kubernetes inventory has not completed (or failed); Enforce stays
    /// fail-closed with [`BuildAdmissionReadiness::InventoryPending`].
    pub fn mark_inventory_pending(&self) {
        self.inventory_ready.store(false, Ordering::Release);
    }

    /// The single-active topology gate is held: this process won the
    /// coordinator leadership race, so it is the only active admission writer.
    pub fn mark_topology_ready(&self) {
        self.topology_ready.store(true, Ordering::Release);
    }

    /// Inspect the current bounded readiness reason, derived from the startup
    /// gates in fail-closed priority order.
    #[must_use]
    pub fn readiness(&self) -> BuildAdmissionReadiness {
        if self.draining.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::ShutdownDraining;
        }
        if !self.journal_recovered.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::JournalRecoveryIncomplete;
        }
        if !self.journal_healthy.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::JournalUnhealthy;
        }
        if self.create_unknown_pending.load(Ordering::Acquire) > 0 {
            return BuildAdmissionReadiness::CreateUnknownHealth;
        }
        if self.over_cap.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::SeededOccupancyAboveCap;
        }
        if !self.inventory_ready.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::InventoryPending;
        }
        if !self.topology_ready.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::TopologyPending;
        }
        BuildAdmissionReadiness::Healthy
    }

    /// Bounded, sync, allocation-light description of why admission is (not)
    /// open, including WHICH rows are responsible.
    ///
    /// Deliberately synchronous and journal-free: the `DoctorCheck::run` seam
    /// is synchronous, and a health report that needs a database round-trip is
    /// a health report that stops working exactly when the database is the
    /// problem.
    #[must_use]
    pub fn health_report(&self) -> BuildAdmissionHealthReport {
        let (blocking_identities, blocking_identities_elided) = self.named_blocking_identities();
        BuildAdmissionHealthReport {
            readiness: self.readiness(),
            mode: self.mode(),
            draining: self.draining.load(Ordering::Acquire),
            create_unknown_pending: self.create_unknown_pending.load(Ordering::Acquire),
            blocking_identities,
            blocking_identities_elided,
            seconds_since_last_reconcile: self.seconds_since_last_reconcile(),
        }
    }

    /// [`Self::health_report`] plus the capacity facts an HTTP operator
    /// surface can afford to read.
    ///
    /// It EXTENDS the health report rather than restating it: the doctor check
    /// and `/debug/dispatch-state` must never be able to disagree about
    /// readiness, mode or which rows are blocking. The extra fields are the
    /// ones `health_report` deliberately excludes because it has to stay
    /// synchronous for the `DoctorCheck::run` seam — reading occupancy
    /// consults the capacity authority and is therefore async.
    pub async fn debug_snapshot(&self) -> BuildAdmissionDebugSnapshot {
        BuildAdmissionDebugSnapshot {
            health: self.health_report(),
            unsatisfied_gates: self.unsatisfied_readiness_gates(),
            effective_cap: self.effective_cap(),
            configured_cap: self.cap,
            // `None` for "no authority installed" and for "the authority could
            // not be read" alike. Both are honestly unmeasured, and neither may
            // render as `0`: a fabricated zero occupancy is exactly what made a
            // tombstoned lease indistinguishable from a full pool for forty
            // minutes (#2661).
            occupancy: match self.slot_authority.as_ref() {
                None => None,
                Some(authority) => authority.occupancy().await,
            },
            server_epoch: self.creator_server_epoch.clone(),
            queued: self
                .queued_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
        }
    }

    /// Every currently-failing readiness gate, not just the first.
    ///
    /// [`Self::readiness`] answers with the highest-priority failure because
    /// that is what `admit()` acts on, and [`Self::health_report`] carries
    /// that one answer. An operator needs the whole set: clearing one gate,
    /// finding the board still wedged, and having no way to see the second is
    /// how a five-hour outage becomes a ten-hour one.
    #[must_use]
    pub fn unsatisfied_readiness_gates(&self) -> Vec<&'static str> {
        let mut gates = Vec::new();
        if self.draining.load(Ordering::Acquire) {
            gates.push(BuildAdmissionReadiness::ShutdownDraining.as_str());
        }
        if !self.journal_recovered.load(Ordering::Acquire) {
            gates.push(BuildAdmissionReadiness::JournalRecoveryIncomplete.as_str());
        }
        if !self.journal_healthy.load(Ordering::Acquire) {
            gates.push(BuildAdmissionReadiness::JournalUnhealthy.as_str());
        }
        if self.create_unknown_pending.load(Ordering::Acquire) > 0 {
            gates.push(BuildAdmissionReadiness::CreateUnknownHealth.as_str());
        }
        if self.over_cap.load(Ordering::Acquire) {
            gates.push(BuildAdmissionReadiness::SeededOccupancyAboveCap.as_str());
        }
        if !self.inventory_ready.load(Ordering::Acquire) {
            gates.push(BuildAdmissionReadiness::InventoryPending.as_str());
        }
        if !self.topology_ready.load(Ordering::Acquire) {
            gates.push(BuildAdmissionReadiness::TopologyPending.as_str());
        }
        gates
    }

    /// Replace the set of identities holding the CreateUnknown gate closed.
    /// Called on exactly the edge that stores `create_unknown_pending`.
    fn set_blocking_identities(&self, identities: BTreeSet<String>) {
        *self
            .create_unknown_identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = identities;
    }

    /// Drop one identity as its row is adopted into Live. Called on exactly the
    /// edge that decrements `create_unknown_pending`, so the count and the
    /// named set never disagree about whether the gate is still held.
    fn clear_blocking_identity(&self, identity: &str) {
        self.create_unknown_identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(identity);
    }

    fn clear_blocking_identities(&self) {
        self.create_unknown_identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Report a readiness change exactly once per episode, NAMING the rows
    /// responsible when the reason is one that has named rows.
    ///
    /// Before this, every build-admission line carried counts only. During the
    /// 2026-07-29 outage an operator could see `create_unknown=1` — one row
    /// denying every dispatch on the board — and had no way short of raw SQL on
    /// the production database to learn which row it was. Five hours.
    fn report_readiness_edge(&self) {
        let readiness = self.readiness();
        let previous = self
            .last_reported_readiness
            .swap(readiness.as_u8(), Ordering::AcqRel);
        if previous == readiness.as_u8() {
            return;
        }
        if readiness.is_healthy() {
            tracing::info!(
                readiness = readiness.as_str(),
                mode = ?self.mode(),
                "build_admission: readiness is healthy; admission is open"
            );
            return;
        }
        let (identities, elided) = self.named_blocking_identities();
        tracing::error!(
            readiness = readiness.as_str(),
            mode = ?self.mode(),
            create_unknown_pending = self.create_unknown_pending.load(Ordering::Acquire),
            blocking_identities = ?identities,
            blocking_identities_elided = elided,
            seconds_since_last_reconcile = ?self.seconds_since_last_reconcile(),
            "build_admission: readiness is NOT healthy; every Enforce admission is \
             denied until it clears. `blocking_identities` names the admission-journal \
             rows responsible as {{domain}}:{{work_id}}:{{generation}}@{{object_name}}."
        );
    }

    /// The bounded head of the blocking-identity set plus how many were elided.
    fn named_blocking_identities(&self) -> (Vec<String>, usize) {
        let guard = self
            .create_unknown_identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let named: Vec<String> = guard
            .iter()
            .take(MAX_NAMED_BLOCKING_IDENTITIES)
            .cloned()
            .collect();
        let elided = guard.len().saturating_sub(named.len());
        (named, elided)
    }

    /// Record that a reconciliation pass completed with no blockers.
    ///
    /// The caller is the composition root that knows whether the pass actually
    /// proved anything (`server::AppState::initialize_build_admission_inventory`),
    /// because a pass that RAN and a pass that SUCCEEDED are different facts and
    /// only the second one means occupancy is being reclaimed.
    pub fn note_reconcile_success(&self) {
        self.note_reconcile_success_at(::time::OffsetDateTime::now_utc().unix_timestamp());
    }

    /// [`Self::note_reconcile_success`] with an injected wall clock, so a test
    /// can assert on the AGE rather than on the fact that a setter was called.
    pub fn note_reconcile_success_at(&self, unix_seconds: i64) {
        self.last_reconcile_success_unix
            .store(unix_seconds, Ordering::Release);
    }

    /// Unix seconds of the last blocker-free reconciliation pass, or `None` if
    /// none has completed in this process.
    #[must_use]
    pub fn last_reconcile_success_unix(&self) -> Option<i64> {
        match self.last_reconcile_success_unix.load(Ordering::Acquire) {
            0 => None,
            stamp => Some(stamp),
        }
    }

    /// How long ago the last blocker-free reconciliation pass completed.
    ///
    /// `None` means no pass has EVER completed in this process, which is a
    /// louder condition than a large age, not a quieter one — the caller must
    /// not silently treat it as healthy.
    #[must_use]
    pub fn seconds_since_last_reconcile(&self) -> Option<i64> {
        self.last_reconcile_success_unix()
            .map(|stamp| (::time::OffsetDateTime::now_utc().unix_timestamp() - stamp).max(0))
    }

    /// The configured admission mode.
    #[must_use]
    pub fn mode(&self) -> BuildAdmissionMode {
        BuildAdmissionMode::from_u8(self.mode.load(Ordering::Acquire))
    }

    /// The unique server epoch allocated for this controller's process.
    #[must_use]
    pub fn server_epoch(&self) -> &str {
        &self.creator_server_epoch
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.readiness().is_healthy()
    }

    /// Begin graceful shutdown draining. New Enforce reservations are blocked
    /// while draining; in-flight permits may still transition to terminal.
    pub fn begin_draining(&self) {
        self.draining.store(true, Ordering::Release);
        // finish_all_queued_waits drains the single lifecycle map atomically,
        // clearing both membership and timestamps in one critical section.
        self.finish_all_queued_waits(djinn_telemetry::build_slot_queue::OUTCOME_SHUTDOWN);
        if self.process_metrics_enabled() {
            djinn_telemetry::build_slot_occupancy::set_slots_queued(0);
        }
    }

    /// Clear the shutdown-draining latch.
    ///
    /// `begin_draining` used to be an ABSOLUTE latch: nothing anywhere stored
    /// `false`, and neither [`Self::mark_ready`] nor [`Self::require_enforcement`]
    /// cleared it. That was safe only by convention — one production caller, on
    /// the shutdown path — while the method stayed reachable on `AppState`. A
    /// single mistaken call would have denied every admission for the life of
    /// the process with no in-process recovery of any kind, which is precisely
    /// the failure shape this whole hardening pass exists to remove.
    ///
    /// It is deliberately loud: on the real shutdown path nothing calls this,
    /// so a line here always means a drain was entered that should not have
    /// been.
    pub fn end_draining(&self) {
        if self.draining.swap(false, Ordering::AcqRel) {
            tracing::warn!(
                mode = ?self.mode(),
                readiness = self.readiness().as_str(),
                "build_admission: shutdown-draining latch CLEARED; new Enforce \
                 reservations are unblocked. On a real shutdown nothing clears this, \
                 so reaching here means the drain was entered outside teardown."
            );
        }
    }

    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Queue consumers may wait here after a terminal release instead of polling.
    #[must_use]
    pub fn release_notifier(&self) -> &Notify {
        &self.released
    }

    /// Durable inspection seam used by coordinator integration tests.
    pub(crate) fn journal(&self) -> &Arc<AdmissionJournalRepository> {
        &self.journal
    }

    /// Bounded count suitable for a telemetry exporter; values saturate at 1024.
    pub async fn unclassified_observation_count(&self) -> u64 {
        *self.unclassified_observations.lock().await
    }

    /// Bounded Observe-mode signal that the reference cap would have deferred work.
    pub async fn would_defer_observation_count(&self) -> u64 {
        *self.would_defer_observations.lock().await
    }

    /// Install the observe-only disk-capacity source (proposal nquz).
    ///
    /// Called by the coordinator's startup composition in
    /// `crate::run_dir_observe`. Installing a source can only add telemetry — it
    /// can never turn a permit into a denial.
    pub fn set_disk_capacity_source(&self, source: Arc<dyn DiskCapacitySource>) {
        *self
            .disk_capacity_source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    }

    fn disk_capacity_source(&self) -> Option<Arc<dyn DiskCapacitySource>> {
        self.disk_capacity_source
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Bounded observe-only signal that disk admission WOULD have deferred a
    /// build; values saturate at 1024. Zero unless a source is installed.
    pub async fn disk_would_defer_observation_count(&self) -> u64 {
        *self.disk_would_defer_observations.lock().await
    }

    /// Record what disk admission WOULD do for a granted build. Never denies and
    /// never allocates: it only advances a bounded counter and emits a typed
    /// queue-reason metric when a source reports a would-defer.
    async fn observe_disk_admission(&self, request: &BuildAdmissionRequest) {
        let Some(source) = self.disk_capacity_source() else {
            return;
        };
        let Some(observation) = source.observe(request).await else {
            return;
        };
        if let Some(reason) = observation.would_defer {
            let mut count = self.disk_would_defer_observations.lock().await;
            *count = count.saturating_add(1).min(1024);
            drop(count);
            if self.process_metrics_enabled() {
                djinn_telemetry::run_dir::increment_queue_reason(reason.as_metric());
            }
            tracing::debug!(
                reason = reason.as_metric(),
                work_id = %request.work_id,
                projected_reservation_bytes = observation.projected_reservation_bytes,
                "disk admission would defer this build (observe-only; dispatch unaffected)"
            );
        }
    }

    /// Export bounded admission metrics from the durable journal snapshot.
    /// InvocationBuild rows are intentionally excluded from all v0 views.
    pub async fn publish_metrics(&self) {
        // Refresh the over-cap GATE before anything about telemetry is decided.
        //
        // This used to live further down, inside the metrics body, where its
        // value was computed correctly on every pass and then thrown away into
        // a gauge. That left `over_cap` with only ONE downward edge: a durable
        // terminal transition. A total denial starves itself of exactly that
        // input — nothing is admitted, so nothing goes terminal, so the gate
        // that caused the denial can never fall. Re-deriving it here gives the
        // latch a way down that does not depend on the traffic it is blocking.
        //
        // It is deliberately ABOVE the `process_metrics_enabled` early return:
        // this is admission state, not telemetry, and it must not be silently
        // disabled by a flag whose entire purpose is to keep test binaries out
        // of a process-global Prometheus registry.
        self.refresh_over_cap_gate().await;
        // Loud, edge-triggered, row-naming readiness report. Also above the
        // early return: an operator must be able to learn WHY admission is shut
        // from the log stream alone, in every build.
        self.report_readiness_edge();
        // Every emission below targets the process-global registry; a
        // controller that has not opted in stays out of it entirely. The body
        // is pure export — a journal read plus gauge writes — so skipping it
        // changes no admission state.
        if !self.process_metrics_enabled() {
            return;
        }
        let mode = match self.mode() {
            BuildAdmissionMode::Off => "off",
            BuildAdmissionMode::Observe => "observe",
            BuildAdmissionMode::Enforce => "enforce",
        };
        if self.mode() == BuildAdmissionMode::Off {
            djinn_telemetry::build_slot_occupancy::set_slots_in_use(0);
            djinn_telemetry::build_slot_occupancy::set_slots_queued(0);
            djinn_telemetry::build_admission::set_health(
                mode, self.cap, false, false, false, false,
            );
            djinn_telemetry::build_admission::set_stale_rows(mode, self.cap, 0);
            return;
        }
        // Keep individual health gauges independent. `readiness()` is a
        // priority-ordered denial reason, whereas telemetry must show every
        // simultaneous underlying degradation.
        let (occupied, journal_snapshot_degraded) = match self.journal.list_active_rows().await {
            Ok(rows) => (
                rows.into_iter()
                    .filter(|row| {
                        matches!(
                            row.key.domain,
                            AdmissionDomain::TaskObservation | AdmissionDomain::WarmBuild
                        )
                    })
                    .map(|row| {
                        format!(
                            "{:?}:{}:{}",
                            row.key.domain, row.key.work_id, row.key.generation
                        )
                    })
                    .collect::<HashSet<_>>()
                    .len(),
                false,
            ),
            Err(error) => {
                tracing::warn!(%error, "build admission metrics journal snapshot unavailable");
                (0, true)
            }
        };
        let queued = if self.mode() == BuildAdmissionMode::Enforce {
            self.queued_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        } else {
            0
        };
        // Occupancy above the cap is the exact shape that denies every
        // admission, and it is invisible in a log stream that only reports
        // per-transition warnings. Export it as a bounded gauge and alarm on the
        // edge so it is never something a human has to discover by reading
        // thousands of warn lines.
        //
        // Read from the ONE capacity authority, in build slots. This used to
        // compare the journal row count above against `self.cap`, which was
        // wrong in both directions once capacity moved: stale LIFECYCLE rows
        // raised an alarm that says "every enforced admission will be denied"
        // while denying nothing (the v0.7.5 false alarm), and genuinely
        // exhausted build slots raised no alarm at all whenever the journal
        // happened to sit within the cap. The gauge must track the thing that
        // actually runs out.
        //
        // No authority installed means this controller is not capacity gated,
        // so there is no cap to exceed. An authority that IS installed but
        // unreadable reports false rather than inventing an alarm: the reachable
        // degradation is already surfaced by the journal/inventory gauges.
        // Already re-derived and stored at the top of this method; read the
        // gate rather than recomputing it, so the gauge and the gate can never
        // disagree about the same pass.
        let over_cap = self.over_cap.load(Ordering::Acquire);
        self.report_over_cap_edge(over_cap, occupied);
        djinn_telemetry::build_slot_occupancy::set_slots_in_use(occupied);
        djinn_telemetry::build_admission::set_seconds_since_reconcile(
            mode,
            self.seconds_since_last_reconcile(),
        );
        djinn_telemetry::build_slot_occupancy::set_slots_queued(queued);
        djinn_telemetry::build_admission::set_health(
            mode,
            self.cap,
            !self.inventory_ready.load(Ordering::Acquire),
            journal_snapshot_degraded
                || !self.journal_recovered.load(Ordering::Acquire)
                || !self.journal_healthy.load(Ordering::Acquire),
            self.create_unknown_pending.load(Ordering::Acquire) > 0,
            over_cap,
        );
    }

    /// Re-derive the over-cap gate from the ONE capacity authority.
    ///
    /// Returns the value now in force. Only a READABLE authority moves the
    /// gate: an installed-but-unreadable authority leaves it exactly as it was
    /// (conservative in both directions), and no authority at all means this
    /// controller is not capacity-gated, so there is no cap to exceed.
    async fn refresh_over_cap_gate(&self) -> bool {
        match self.slot_authority.as_ref() {
            None => {
                self.over_cap.store(false, Ordering::Release);
                false
            }
            Some(authority) => match authority.occupancy().await {
                Some(slots) => {
                    let over_cap = slots > authority.cap();
                    self.over_cap.store(over_cap, Ordering::Release);
                    over_cap
                }
                None => self.over_cap.load(Ordering::Acquire),
            },
        }
    }

    /// Log the over-cap alarm exactly once per episode, in both directions.
    fn report_over_cap_edge(&self, over_cap: bool, occupancy: usize) {
        if self
            .over_cap_alarm_active
            .compare_exchange(!over_cap, over_cap, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // `occupancy` is the LIFECYCLE row count, logged as context. The
        // decision above is build slots against the authority's cap; the two
        // are different units and the message says which is which, so an
        // operator reading this line is never left comparing them.
        if over_cap {
            tracing::error!(
                ledger_rows = occupancy,
                cap = self.effective_cap(),
                mode = ?self.mode(),
                "build_admission: occupied build slots exceed the cap; every enforced \
                 admission will be denied until the excess is released. Run the \
                 stale-admission-occupancy runbook."
            );
        } else {
            tracing::info!(
                ledger_rows = occupancy,
                cap = self.effective_cap(),
                "build_admission: occupied build slots are back within the cap"
            );
        }
    }

    /// Publish the outcome of one reconciliation pass over durable occupancy.
    ///
    /// `stale_rows` is the number of occupying rows whose Kubernetes object the
    /// pass proved absent, counted BEFORE reclamation, so the gauge reports the
    /// size of the problem rather than the size of the fix.
    pub fn publish_reconciliation(&self, stale_rows: usize, reclaimed: usize, fenced: usize) {
        if stale_rows > 0 || reclaimed > 0 {
            // Compared against the cap actually in force, not the configured
            // fallback: an epoch `set-cap` changes what "more stale rows than
            // the cap" means the moment it is adopted.
            let effective_cap = self.effective_cap();
            let level_is_alarm = i64::try_from(stale_rows).unwrap_or(i64::MAX) > effective_cap;
            if level_is_alarm {
                tracing::error!(
                    stale_rows,
                    reclaimed,
                    fenced,
                    cap = effective_cap,
                    mode = ?self.mode(),
                    "build_admission: reconciliation found more occupying rows with absent \
                     Kubernetes objects than the cap in force; this is the population that \
                     wedges the board when the cap is armed"
                );
            } else {
                tracing::info!(
                    stale_rows,
                    reclaimed,
                    fenced,
                    cap = effective_cap,
                    "build_admission: reconciliation released stale durable occupancy"
                );
            }
        }
        if !self.process_metrics_enabled() {
            return;
        }
        let mode = match self.mode() {
            BuildAdmissionMode::Off => "off",
            BuildAdmissionMode::Observe => "observe",
            BuildAdmissionMode::Enforce => "enforce",
        };
        djinn_telemetry::build_admission::set_stale_rows(mode, self.cap, stale_rows as u64);
        for _ in 0..reclaimed {
            djinn_telemetry::build_admission::record_transition_outcome(
                mode,
                self.cap,
                djinn_telemetry::build_admission::OUTCOME_RECLAIMED,
            );
        }
        for _ in 0..fenced {
            djinn_telemetry::build_admission::record_transition_outcome(
                mode,
                self.cap,
                djinn_telemetry::build_admission::OUTCOME_RECLAIM_FENCED,
            );
        }
    }

    pub async fn admit(
        &self,
        request: BuildAdmissionRequest,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let readiness = self.readiness();
        if self.mode() == BuildAdmissionMode::Enforce && (!readiness.is_healthy() || self.is_draining())
        {
            // The cap reported with a denial is the one that WOULD have been
            // enforced, resolved from the single capacity authority. Reporting
            // the constructor's configured value here made v0 and v1 disagree
            // in the operator's log about which number was in force.
            //
            // The readiness travels WITH the cause. `controller_not_admitting`
            // on its own is the least actionable string in the system: it says
            // the controller refused without saying which of seven gates is
            // closed, which is why 2026-07-29 needed node access to diagnose.
            return Ok(BuildAdmissionDecision::Denied {
                occupancy: None,
                cap: self.effective_cap(),
                cause: DenialCause::ControllerNotAdmitting { readiness },
            });
        }
        // Capture an observe-only copy before any field of `request` is moved.
        // Only cloned when the disk dimension is armed, so a controller with
        // no source installed pays nothing.
        let disk_observe_request = self
            .disk_capacity_source()
            .is_some()
            .then(|| request.clone());
        let kind = request.kind;
        let capacity = request.capacity.clone();
        let workload_kind = match kind {
            // A Light task-run is orchestration-only: it never runs the
            // project's compile/test toolchain, so it weighs zero slots. It is
            // admitted the same way the audited NonBuild bypass is — a permit
            // with no journal reservation, via `permit_without_reservation` —
            // which means it can never be Denied at the cap and its terminal
            // transition is a no-op that cannot touch occupancy (the permit is
            // recorded `durable: false`, and occupancy is always derived from
            // the journal, never from in-memory permits). Warm builds and every
            // build-capable role fall through and reserve normally; an unknown
            // role never reaches here at all (`admit_task_run` rejects it as
            // Unclassified before constructing the request).
            BuildWorkloadKind::TaskRun { role } if !role.resource_class().gated_at_dispatch() => {
                let key = AdmissionJournalKey {
                    domain: request.domain,
                    work_id: request.work_id,
                    generation: request.generation,
                };
                let permit_key = permit_key(&key);
                if let Some(permit) = self.permits_by_key.lock().await.get(&permit_key).cloned() {
                    return Ok(BuildAdmissionDecision::Permitted {
                        permit,
                        idempotent: true,
                    });
                }
                tracing::debug!(
                    role = role.as_str(),
                    resource_class = role.resource_class().as_str(),
                    audit_reason = LIGHT_ROLE_AUDIT_REASON,
                    "build admission: zero-slot task-run permitted without reservation"
                );
                return self
                    .permit_without_reservation(key, permit_key, request.object_name)
                    .await;
            }
            BuildWorkloadKind::TaskRun { .. } => match request.domain {
                AdmissionDomain::TaskObservation => AdmissionWorkloadKind::Task,
                AdmissionDomain::InvocationBuild => AdmissionWorkloadKind::Invocation,
                AdmissionDomain::WarmBuild => AdmissionWorkloadKind::Warm,
            },
            BuildWorkloadKind::GraphWarmJob => AdmissionWorkloadKind::Warm,
            BuildWorkloadKind::NonBuild { audit_reason } if !audit_reason.is_empty() => {
                return Ok(BuildAdmissionDecision::Permitted {
                    permit: WarmAdmissionPermit::new(),
                    idempotent: false,
                });
            }
            BuildWorkloadKind::NonBuild { .. } => {
                self.observe_unclassified().await;
                return Ok(BuildAdmissionDecision::Unclassified);
            }
        };
        let mut key = AdmissionJournalKey {
            domain: request.domain,
            work_id: request.work_id,
            generation: request.generation,
        };
        let durable = self.mode() != BuildAdmissionMode::Off;
        // The caller's generation is a floor, not the identity. A second
        // dispatch attempt for the same work is a second object with its own
        // Kubernetes UID and terminal release, so the journal — not the
        // caller's counter — decides which generation this attempt reserves.
        // Resolving BEFORE the in-memory permit lookup is what stops a retired
        // generation's permit from being replayed as an idempotent hit and
        // then colliding with that generation's recorded UID.
        if durable {
            match self
                .journal
                .resolve_dispatch_generation(key.domain, &key.work_id, key.generation)
                .await
            {
                Ok(generation) => key.generation = generation,
                Err(error) => {
                    if self.mode() != BuildAdmissionMode::Observe {
                        return Err(unavailable(error));
                    }
                    tracing::warn!(%error, "build admission generation resolution unavailable; permitting without journal telemetry");
                    self.mark_journal_unhealthy();
                    self.publish_metrics().await;
                    let permit_key = permit_key(&key);
                    return self
                        .permit_without_reservation(key, permit_key, request.object_name)
                        .await;
                }
            }
        }
        let permit_key = permit_key(&key);
        let idempotent_permit = self.permits_by_key.lock().await.get(&permit_key).cloned();
        if let Some(permit) = idempotent_permit {
            return Ok(BuildAdmissionDecision::Permitted {
                permit,
                idempotent: true,
            });
        }
        // ── Capacity, decided exactly once, by exactly one authority ────────
        //
        // This is the whole of the fix. Capacity is acquired HERE, from the
        // single build-slot authority, and the journal write below is a pure
        // ledger append that cannot deny. Previously the journal ran its own
        // cap check at this point while the v1 lease ran another over a
        // different population, so the two together admitted 2x the cap.
        if let CapacitySource::AcquireDispatchSlot = capacity {
            match self.acquire_dispatch_capacity(&key).await {
                DispatchSlotOutcome::Granted => {}
                DispatchSlotOutcome::Observed { would_defer } => {
                    // Shadow: the authority is not enforcing yet. Nothing was
                    // acquired and nothing is denied -- this records only what
                    // enforcement WOULD have done, which is the signal the
                    // operator reads before arming the epoch.
                    if would_defer {
                        let mut count = self.would_defer_observations.lock().await;
                        *count = count.saturating_add(1).min(1024);
                        if self.process_metrics_enabled() {
                            djinn_telemetry::build_admission::increment_would_defer(
                                "observe", self.cap,
                            );
                        }
                    }
                }
                DispatchSlotOutcome::AtCapacity { occupancy, cap } => {
                    // Atomically install one queued lifecycle record containing
                    // the monotonic start time. Membership and timestamp are
                    // installed under a single lock, so a concurrent
                    // cancellation can never observe membership before the
                    // timestamp exists. A retry (Occupied entry) reuses the
                    // original start time, preserving first-denial timing.
                    {
                        let mut lifecycle = self
                            .queued_lifecycle
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        match lifecycle.entry(permit_key.clone()) {
                            std::collections::hash_map::Entry::Vacant(slot) => {
                                slot.insert(self.queue_clock.now());
                            }
                            std::collections::hash_map::Entry::Occupied(_) => {
                                // Retry: keep the original first-denial start time.
                            }
                        }
                    }
                    self.publish_metrics().await;
                    return Ok(BuildAdmissionDecision::Denied {
                        occupancy: Some(occupancy),
                        cap,
                        cause: DenialCause::AtCapacity,
                    });
                }
                DispatchSlotOutcome::Unavailable { detail } => {
                    // A capacity authority we cannot reach is not permission.
                    // Observe stays non-denying; anything else fails closed.
                    //
                    // No occupancy is reported here because none was read. The
                    // previous `occupancy: 0` was fabricated at this exact
                    // line, and it is what made a permanently tombstoned lease
                    // indistinguishable in the log from a full pool.
                    if self.mode() != BuildAdmissionMode::Observe {
                        return Ok(BuildAdmissionDecision::Denied {
                            occupancy: None,
                            cap: self.effective_cap(),
                            cause: DenialCause::AuthorityUnavailable { detail },
                        });
                    }
                    tracing::warn!(
                        %detail,
                        "build admission: build-slot authority unavailable; \
                         Observe continues without capacity accounting"
                    );
                }
            }
        }

        let mut idempotent = false;
        if durable {
            // Ledger append. This NEVER denies -- capacity was settled above.
            // It runs for every population including leased warm Jobs, which
            // previously wrote no row and were invisible to reclamation.
            let reservation = self
                .journal
                .reserve(&ReserveAdmissionInput {
                    key: key.clone(),
                    workload_kind,
                    creator_server_epoch: self.creator_server_epoch.clone(),
                    object_name: request.object_name.clone(),
                })
                .await;
            let reservation = match reservation {
                Ok(reservation) => reservation,
                Err(error) => {
                    if self.mode() != BuildAdmissionMode::Observe {
                        // The ledger is how this capacity is later released and
                        // reclaimed. Losing the row while holding a slot would
                        // leak that slot, so hand the capacity back before
                        // failing.
                        self.release_dispatch_capacity(&key, &capacity).await;
                        return Err(unavailable(error));
                    }
                    // Observe is telemetry-only: a journal outage must not become a dispatch denial.
                    tracing::warn!(%error, "build admission observation unavailable; permitting without journal telemetry");
                    // Surface the live journal failure as a degraded health
                    // signal even though Observe continues to permit dispatch.
                    self.mark_journal_unhealthy();
                    self.publish_metrics().await;
                    return self
                        .permit_without_reservation(key, permit_key, request.object_name)
                        .await;
                }
            };
            idempotent = reservation.idempotent;
            // This exact identity has successfully left deferred state.
            // Atomically remove membership and extract the start time
            // under one lock; emit exactly one admitted observation.
            // Other waiters remain queued until their own retry succeeds.
            self.finish_queued_wait(
                &permit_key,
                djinn_telemetry::build_slot_queue::OUTCOME_ADMITTED,
            );
        }
        let permit = WarmAdmissionPermit::new();
        let work_key = work_key(&key);
        let state = PermitState {
            key: key.clone(),
            creator_server_epoch: self.creator_server_epoch.clone(),
            object_name: request.object_name,
            durable,
            released: false,
            create_unknown_outstanding: false,
            capacity,
        };
        self.permits.lock().await.insert(permit.clone(), state);
        self.permits_by_key
            .lock()
            .await
            .insert(permit_key, permit.clone());
        self.permits_by_work
            .lock()
            .await
            .insert(work_key, permit.clone());
        // A durable Reserved row occupies immediately; do not wait for a
        // later cap denial or terminal release to refresh the gauge.
        self.publish_metrics().await;
        // Observe-only disk dimension: records what disk admission WOULD do for
        // this granted build without ever changing the decision above.
        if let Some(observe_request) = disk_observe_request.as_ref() {
            self.observe_disk_admission(observe_request).await;
        }
        Ok(BuildAdmissionDecision::Permitted { permit, idempotent })
    }

    /// Occupied build slots across EVERY population, from the one authority.
    ///
    /// `None` means the answer is unknown (no authority installed, or it could
    /// not be reached) and is never treated as "zero" -- an unknown occupancy
    /// must not clear a fail-closed gate.
    async fn unified_occupancy(&self) -> Option<i64> {
        match self.slot_authority.as_ref() {
            Some(authority) => authority.occupancy().await,
            None => None,
        }
    }

    /// The cap actually in force. The authority resolves it (from the durable
    /// epoch, and later from measured node capacity); the constructor value is
    /// only the fallback for a controller with no authority installed.
    fn effective_cap(&self) -> i64 {
        match self.slot_authority.as_ref() {
            Some(authority) => authority.cap(),
            None => self.cap,
        }
    }

    /// Acquire layer-1 dispatch capacity for one attempt from the single
    /// authority. Absent an authority, admission is not capacity-gated at all
    /// (the Off / local-dev shape), which is reported as a non-denying
    /// observation rather than silently granting.
    async fn acquire_dispatch_capacity(&self, key: &AdmissionJournalKey) -> DispatchSlotOutcome {
        let Some(authority) = self.slot_authority.as_ref() else {
            return DispatchSlotOutcome::Observed { would_defer: false };
        };
        authority
            .acquire_dispatch_slot(&key.work_id, key.generation)
            .await
    }

    /// Hand back exactly the capacity this permit took, if any.
    ///
    /// Matched on the retained [`CapacitySource`] rather than on the domain or
    /// the role, so a permit that never acquired a slot can never release one.
    /// `HeldByLease` is deliberately a no-op: the graph warmer owns that lease's
    /// lifecycle and releases it on its own terms, and releasing it from here
    /// would free capacity a live warm Job is still using.
    async fn release_dispatch_capacity(
        &self,
        key: &AdmissionJournalKey,
        capacity: &CapacitySource,
    ) {
        if !matches!(capacity, CapacitySource::AcquireDispatchSlot) {
            return;
        }
        let Some(authority) = self.slot_authority.as_ref() else {
            return;
        };
        authority
            .release_dispatch_slot(&key.work_id, key.generation)
            .await;
    }

    async fn permit_without_reservation(
        &self,
        key: AdmissionJournalKey,
        permit_key: String,
        object_name: String,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let permit = WarmAdmissionPermit::new();
        let work_key = work_key(&key);
        self.permits.lock().await.insert(
            permit.clone(),
            PermitState {
                key,
                creator_server_epoch: self.creator_server_epoch.clone(),
                object_name,
                durable: false,
                released: false,
                create_unknown_outstanding: false,
                // Reached only by paths that took no capacity: Light roles,
                // the audited NonBuild bypass, and Observe-mode journal
                // outages. Recording it explicitly means the terminal path
                // cannot release a slot that was never acquired.
                capacity: CapacitySource::ZeroWeight {
                    audit_reason: "permit issued without a capacity reservation",
                },
            },
        );
        self.permits_by_key
            .lock()
            .await
            .insert(permit_key, permit.clone());
        self.permits_by_work
            .lock()
            .await
            .insert(work_key, permit.clone());
        Ok(BuildAdmissionDecision::Permitted {
            permit,
            idempotent: false,
        })
    }

    /// The permit for a work item's current admission generation, in any state.
    ///
    /// This is the lookup for lifecycle callbacks that carry only the work
    /// identity. It is deliberately generation-free: after a retry the current
    /// generation is a journal fact, not a caller-side counter.
    pub async fn current_permit_for_work(
        &self,
        domain: AdmissionDomain,
        work_id: &str,
    ) -> Option<WarmAdmissionPermit> {
        self.permits_by_work
            .lock()
            .await
            .get(&work_key(&AdmissionJournalKey {
                domain,
                work_id: work_id.to_owned(),
                generation: 0,
            }))
            .cloned()
    }

    /// The permit for this task's current admission generation.
    pub async fn current_task_run_permit(&self, task_id: &str) -> Option<WarmAdmissionPermit> {
        self.current_permit_for_work(AdmissionDomain::TaskObservation, task_id)
            .await
    }

    /// Durable lifecycle transitions the journal accepted since process start.
    #[must_use]
    pub fn accepted_transition_count(&self) -> u64 {
        self.accepted_transitions.load(Ordering::Acquire)
    }

    /// Durable lifecycle transitions the journal rejected since process start.
    /// A rate approaching the accepted count means the journal is refusing the
    /// observations it would have to trust when the mode is armed to Enforce.
    #[must_use]
    pub fn rejected_transition_count(&self) -> u64 {
        self.rejected_transitions.load(Ordering::Acquire)
    }

    /// The most recent rejection diagnostic, verbatim.
    pub async fn last_transition_rejection(&self) -> Option<String> {
        self.last_transition_rejection.lock().await.clone()
    }

    /// A missing or unknown task role is a fail-closed classification result.
    pub async fn admit_task_run(
        &self,
        role: Option<&str>,
        domain: AdmissionDomain,
        work_id: String,
        generation: i64,
        object_name: String,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let Some(role) = TaskRunRole::parse(role) else {
            self.observe_unclassified().await;
            return Ok(BuildAdmissionDecision::Unclassified);
        };
        // Capacity depends on the DOMAIN first, then the role. Getting this
        // ordering wrong is how an invocation ends up charged a dispatch slot.
        let capacity = match domain {
            // Layer 1. A role certain enough to compile pre-charges a dispatch
            // slot; a Light role does not, and contends later (if at all)
            // through the measured invocation lease.
            AdmissionDomain::TaskObservation => {
                if role.resource_class().gated_at_dispatch() {
                    CapacitySource::AcquireDispatchSlot
                } else {
                    CapacitySource::ZeroWeight {
                        audit_reason: LIGHT_ROLE_AUDIT_REASON,
                    }
                }
            }
            // Layer 2, and warm. Neither takes a DISPATCH slot: an invocation
            // is governed by its own invocation lease -- weight 0 when its
            // task-run already holds a dispatch slot, full weight when it does
            // not -- and a warm Job is governed by the graph-warm lease its
            // warmer acquired before ever reaching admission. Charging either a
            // dispatch slot here would double-charge one physical compile,
            // which is the whole defect this design removes. The role is
            // deliberately not consulted: below the dispatch boundary,
            // capacity is measured, not predicted.
            AdmissionDomain::InvocationBuild | AdmissionDomain::WarmBuild => {
                CapacitySource::HeldByLease
            }
        };
        self.admit(BuildAdmissionRequest {
            domain,
            work_id,
            generation,
            object_name,
            kind: BuildWorkloadKind::TaskRun { role },
            capacity,
        })
        .await
    }

    /// Return the retained permit for this exact admission key, in any domain.
    ///
    /// This is the domain-appropriate recovered-permit lookup: seeded and
    /// admitted permits are keyed by the full journal key, so a warm-build row
    /// is addressable with [`AdmissionDomain::WarmBuild`] while a task-run row
    /// uses [`AdmissionDomain::TaskObservation`]. Recovery and adoption use
    /// this to reach the permit seeded from a recovered row.
    pub async fn permit_for_key(
        &self,
        domain: AdmissionDomain,
        work_id: &str,
        generation: i64,
    ) -> Option<WarmAdmissionPermit> {
        let key = AdmissionJournalKey {
            domain,
            work_id: work_id.to_owned(),
            generation,
        };
        self.permits_by_key
            .lock()
            .await
            .get(&permit_key(&key))
            .cloned()
    }

    /// Return the retained permit for this exact task generation.
    pub async fn task_run_permit(
        &self,
        task_id: &str,
        generation: i64,
    ) -> Option<WarmAdmissionPermit> {
        self.permit_for_key(AdmissionDomain::TaskObservation, task_id, generation)
            .await
    }

    /// Bind a UID-bearing runtime task-run to a permit already made Live.
    pub async fn bind_task_run(&self, task_run_id: String, permit: WarmAdmissionPermit) {
        self.permits_by_task_run
            .lock()
            .await
            .insert(task_run_id, permit);
    }

    /// Return only the permit bound to this runtime task-run UID. There is no
    /// task-ID fallback because that could release a newer reopened generation.
    pub async fn task_run_permit_for_runtime_id(
        &self,
        task_run_id: &str,
    ) -> Option<WarmAdmissionPermit> {
        self.permits_by_task_run
            .lock()
            .await
            .get(task_run_id)
            .cloned()
    }

    async fn observe_unclassified(&self) {
        let mut count = self.unclassified_observations.lock().await;
        *count = count.saturating_add(1).min(1024);
        let mode = match self.mode() {
            BuildAdmissionMode::Off => "off",
            BuildAdmissionMode::Observe => "observe",
            BuildAdmissionMode::Enforce => "enforce",
        };
        if self.process_metrics_enabled() {
            djinn_telemetry::build_admission::increment_unknown_classification(mode, self.cap);
        }
        tracing::warn!(
            observations = *count,
            "build admission classification missing or unknown; denying dispatch"
        );
    }

    /// Cancel every deferred generation for a task that has become terminal.
    /// Each matching key is terminated atomically by [`Self::finish_queued_wait`],
    /// which removes membership and extracts the timestamp under one lock, so no
    /// cancelled observation is lost and no timestamp is orphaned.
    pub async fn cancel_deferred_task(&self, work_id: &str) {
        // A closed task must also surrender any queue position it still holds
        // in the capacity authority. Without this the queued row survives its
        // task, is eventually granted, and leaks the slot permanently.
        if let Some(authority) = self.slot_authority.as_ref() {
            authority.abandon_queued_dispatch(work_id).await;
        }
        let prefix = format!("{:?}:{work_id}:", AdmissionDomain::TaskObservation);
        let matching: Vec<String> = {
            let lifecycle = self
                .queued_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lifecycle
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect()
        };
        for key in matching {
            self.finish_queued_wait(&key, djinn_telemetry::build_slot_queue::OUTCOME_CANCELLED);
        }
        self.publish_metrics().await;
    }

    /// Rebuild deferred identities from the durable coordinator ready queue.
    pub async fn reconcile_deferred_tasks<I>(&self, task_ids: I)
    where
        I: IntoIterator<Item = (String, i64)>,
    {
        if self.mode() != BuildAdmissionMode::Enforce {
            return;
        }
        let mut deferred: HashSet<_> = task_ids
            .into_iter()
            .map(|(work_id, generation)| {
                permit_key(&AdmissionJournalKey {
                    domain: AdmissionDomain::TaskObservation,
                    work_id,
                    generation,
                })
            })
            .collect();
        // The ready queue is durable but a task can have been claimed between
        // its ready-queue snapshot and this recovery pass. Journal rows remain
        // authoritative for in-use identities, so never restore the same task
        // generation as deferred as well.
        let active = match self.journal.list_active_rows().await {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| row.key.domain == AdmissionDomain::TaskObservation)
                .map(|row| permit_key(&row.key))
                .collect::<HashSet<_>>(),
            Err(error) => {
                tracing::warn!(%error, "build admission deferred recovery journal snapshot unavailable");
                self.publish_metrics().await;
                return;
            }
        };
        deferred.retain(|key| !active.contains(key));
        {
            let mut queued = self
                .queued_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Reconcile task-queue membership to the durable ready source while
            // preserving warm-build waiters, which have no task ready-queue
            // representation. This also removes a previously reconstructed
            // task identity if it has since become active.
            let task_prefix = format!("{:?}:", AdmissionDomain::TaskObservation);
            queued.retain(|key, _| !key.starts_with(&task_prefix) || deferred.contains(key));
            for key in deferred {
                // Recovery rebuilds membership; a fresh monotonic start time is
                // installed so the lifecycle record is complete. The original
                // first-denial time is not recoverable across a process restart.
                queued.entry(key).or_insert_with(|| self.queue_clock.now());
            }
        }
        self.publish_metrics().await;
    }

    /// Atomically terminate one queued lifecycle record. Removes membership
    /// and extracts the monotonic start time under a single lock, then emits
    /// exactly one terminal observation. If the key is absent (duplicate signal,
    /// or cancellation raced ahead of admission), no observation is emitted.
    fn finish_queued_wait(&self, key: &str, outcome: &'static str) {
        let queued_at = self
            .queued_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key);
        if let Some(queued_at) = queued_at
            && self.process_metrics_enabled()
        {
            djinn_telemetry::build_slot_queue::record_wait_seconds(
                outcome,
                self.queue_clock.now().saturating_duration_since(queued_at),
            );
        }
    }

    /// Atomically drain every queued lifecycle record (membership + timestamp)
    /// under one lock and emit one terminal observation per record. Used by the
    /// graceful-shutdown drain path so no queued identity loses its observation.
    fn finish_all_queued_waits(&self, outcome: &'static str) {
        let waiters = {
            let mut lifecycle = self
                .queued_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *lifecycle)
        };
        if !self.process_metrics_enabled() {
            return;
        }
        for queued_at in waiters.into_values() {
            djinn_telemetry::build_slot_queue::record_wait_seconds(
                outcome,
                self.queue_clock.now().saturating_duration_since(queued_at),
            );
        }
    }

    /// Count one durable lifecycle transition by outcome.
    ///
    /// The WARN stays exactly as loud as it was; this makes the SAME fact
    /// countable. A process whose rejected series equals its total is a
    /// journal that would mis-account every grant if the mode were armed, and
    /// that is now visible as a ratio rather than only as log volume.
    async fn record_transition_outcome(&self, accepted: bool, error: Option<&WarmAdmissionError>) {
        if accepted {
            self.accepted_transitions.fetch_add(1, Ordering::AcqRel);
        } else {
            self.rejected_transitions.fetch_add(1, Ordering::AcqRel);
            if let Some(error) = error {
                *self.last_transition_rejection.lock().await = Some(error.to_string());
            }
        }
        if self.process_metrics_enabled() {
            let mode = match self.mode() {
                BuildAdmissionMode::Off => "off",
                BuildAdmissionMode::Observe => "observe",
                BuildAdmissionMode::Enforce => "enforce",
            };
            djinn_telemetry::build_admission::record_transition(mode, self.cap, accepted);
        }
    }

    async fn transition_permit(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        let Some(state) = self.permits.lock().await.get(permit).cloned() else {
            return Err(WarmAdmissionError::UnknownPermit);
        };
        if !state.durable {
            return Ok(());
        }
        let terminal = matches!(
            transition,
            WarmAdmissionTransition::DefinitiveFailure { .. }
                | WarmAdmissionTransition::Terminal { .. }
        );
        let adopts_into_live = matches!(transition, WarmAdmissionTransition::Live { .. });
        let state_permit_key = permit_key(&state.key);
        let state_key = state.key.clone();
        let state_capacity = state.capacity.clone();
        let result = match transition {
            WarmAdmissionTransition::CreateStarted => self
                .journal
                .mark_create_started(&CreateStartedInput {
                    key: state.key.clone(),
                    creator_server_epoch: state.creator_server_epoch,
                    object_name: state.object_name,
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::Live { uid } => self
                .journal
                .mark_live(&UidFencedAdmissionInput {
                    key: state.key.clone(),
                    object_uid: uid,
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::CreateUnknown { .. } => self
                .journal
                .mark_create_unknown(&state.key)
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::DefinitiveFailure { .. } => self
                .journal
                .mark_definitive_create_failure(&state.key)
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::Terminal { uid } => self
                .journal
                .mark_terminal(&TerminalAdmissionInput {
                    key: state.key.clone(),
                    object_uid: Some(uid),
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
        };
        let transition_durable = result.is_ok();
        self.record_transition_outcome(transition_durable, result.as_ref().err())
            .await;
        if let Err(error) = result {
            if self.mode() != BuildAdmissionMode::Observe {
                return Err(error);
            }
            tracing::warn!(%error, "build admission observation transition unavailable; continuing without journal telemetry");
            // Observe remains non-denying, but every lifecycle journal outage
            // immediately raises the bounded degradation signal.
            self.mark_journal_unhealthy();
            self.publish_metrics().await;
        }
        // A recovered CreateUnknown row stops occupying as unknown once it is
        // adopted into Live with the authoritative UID: clear its startup-gate
        // contribution exactly once so readiness can advance past
        // `CreateUnknownHealth`.
        if transition_durable && adopts_into_live {
            let cleared = {
                let mut permits = self.permits.lock().await;
                match permits.get_mut(permit) {
                    Some(state) if state.create_unknown_outstanding => {
                        state.create_unknown_outstanding = false;
                        // Carry the identity out so the named set is dropped on
                        // exactly the edge that decrements the count.
                        Some(blocking_identity(&state.key, &state.object_name))
                    }
                    Some(_) | None => None,
                }
            };
            if let Some(identity) = cleared {
                self.create_unknown_pending
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_sub(1))
                    })
                    .ok();
                self.clear_blocking_identity(&identity);
                // CreateUnknown resolution changes a bounded health signal;
                // publish immediately so the gauge reflects the new state
                // rather than waiting for an unrelated terminal event.
                self.publish_metrics().await;
            }
        }
        if terminal {
            let newly_released = {
                let mut permits = self.permits.lock().await;
                match permits.get_mut(permit) {
                    Some(state) if !state.released => {
                        state.released = true;
                        true
                    }
                    Some(_) | None => false,
                }
            };
            if newly_released {
                // Hand back the layer-1 dispatch slot exactly once, on the same
                // edge that marks the permit released. Doing it here rather
                // than on every terminal callback is what keeps a duplicate
                // terminal signal from releasing capacity twice and letting an
                // extra build in.
                self.release_dispatch_capacity(&state_key, &state_capacity)
                    .await;
                // Retain one wakeup when the actor is currently handling the event
                // that performed this release and therefore has no `notified()`
                // future registered in its select loop.
                self.released.notify_one();
            }
            // Waking one waiter cannot prove unrelated identities have left
            // deferred state. Only remove this terminal identity, if present.
            // Using finish_queued_wait atomically removes membership + timestamp
            // so no timestamp is orphaned after a permit goes terminal. If the
            // identity was already admitted (which removed the record), this is
            // an idempotent no-op.
            self.finish_queued_wait(
                &state_permit_key,
                djinn_telemetry::build_slot_queue::OUTCOME_ADMITTED,
            );
            self.publish_metrics().await;
            // A terminal release can bring seeded occupancy back within the
            // cap; refresh the over-cap gate from the durable journal rather
            // than trusting in-memory bookkeeping.
            // Refresh the over-cap gate from the UNIFIED capacity authority,
            // not from the journal. The journal no longer counts capacity, so
            // reading occupancy from it here would compare a lifecycle row
            // count against a build-slot cap -- two different units.
            if transition_durable && self.over_cap.load(Ordering::Acquire) {
                match self.unified_occupancy().await {
                    Some(occupancy) if occupancy <= self.effective_cap() => {
                        self.over_cap.store(false, Ordering::Release);
                    }
                    Some(_) => {}
                    None => {
                        tracing::warn!(
                            "build admission: failed to refresh over-cap gate after release; \
                             retaining it conservatively"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Recover the durable predecessor epoch and seed this controller from the
    /// active recovered rows.
    ///
    /// This is the single startup recovery primitive. It must run before any
    /// Kubernetes inventory or task/warm create can proceed under Enforce. It
    /// uses [`AdmissionJournalRepository::recover_predecessor_epoch`] to
    /// atomically retire predecessor Reserved rows, convert predecessor
    /// CreateInFlight rows to occupying CreateUnknown, and retain predecessor
    /// CreateUnknown/Live rows — then seeds in-memory permit bookkeeping from
    /// all active recovered rows without duplicating occupancy.
    ///
    /// Occupancy is never tracked by an in-memory permit count: the journal is
    /// the single source of truth. Seeds record one permit per recovered active
    /// row so that idempotent re-admission and lifecycle transitions remain
    /// consistent across the restart boundary.
    ///
    /// After seeding, the readiness gates are updated deterministically from
    /// the durable journal: `CreateUnknownHealth` while any recovered
    /// CreateUnknown row still occupies, `SeededOccupancyAboveCap` while
    /// task/warm occupancy exceeds the cap. Journal recovery alone NEVER marks
    /// Enforce healthy: the inventory and topology gates stay pending until
    /// their own production checks complete, so a recovered controller with no
    /// other degradation reports `InventoryPending`. Observe/Off ignore the
    /// gates for admission but still receive the report for telemetry.
    pub async fn recover_and_seed(
        &self,
        predecessor_epoch: &str,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        self.recover_and_seed_with_filter(predecessor_epoch, |_| true)
            .await
    }

    /// Variant of [`Self::recover_and_seed`] that lets a caller restrict which
    /// recovered active rows become in-memory seeded permits. The durable
    /// journal recovery still processes every predecessor row; only the
    /// in-memory seeding bookkeeping is filtered. This is used by tests that
    /// need to simulate a replacement process whose initial Kubernetes
    /// inventory is empty (all rows recovered from the journal, none from
    /// inventory) while still validating the durable occupancy accounting.
    pub async fn recover_and_seed_with_filter(
        &self,
        predecessor_epoch: &str,
        mut seed_filter: impl FnMut(&AdmissionJournalRow) -> bool,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        let recovery = self
            .journal
            .recover_predecessor_epoch(predecessor_epoch)
            .await
            .map_err(unavailable)?;
        self.seed_from_recovery(&recovery, &mut seed_filter).await
    }

    /// Recover every predecessor epoch and seed this controller from all active
    /// recovered rows.
    ///
    /// This is the cold-restart recovery entry point: a replacement process does
    /// not know the exact predecessor epoch string(s), so it recovers every row
    /// whose `creator_server_epoch` differs from this process's epoch. See
    /// [`AdmissionJournalRepository::recover_all_predecessors`].
    pub async fn recover_all_predecessors_and_seed(
        &self,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        let recovery = self
            .journal
            .recover_all_predecessors(&self.creator_server_epoch)
            .await
            .map_err(unavailable)?;
        self.seed_from_recovery(&recovery, &mut |_| true).await
    }

    /// Seed in-memory permit bookkeeping from a pre-fetched recovery result.
    ///
    /// Exposed so callers that have already recovered (for example via a
    /// shared journal repository in tests) can seed without a second recovery
    /// call. The journal remains the authoritative occupancy source; this only
    /// populates the permit/key maps used for idempotent re-admission and
    /// lifecycle transitions.
    pub async fn seed_from_recovery(
        &self,
        recovery: &AdmissionRecoveryResult,
        seed_filter: &mut impl FnMut(&AdmissionJournalRow) -> bool,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        let mut seeded = 0u64;
        // The CreateUnknown startup gate counts RECOVERED unknowns — rows whose
        // creator process is gone — and deliberately nothing else.
        //
        // `CreateUnknown` means two entirely different things depending on who
        // wrote the row:
        //
        // * A PREDECESSOR's row is a genuine unknown. The process that POSTed
        //   the create died without learning the outcome, nothing in this
        //   process is waiting on it, and only reconciliation against the API
        //   server can resolve it. Enforce must fail closed until it does.
        // * THIS process's own row is the ordinary intermediate state of a
        //   healthy dispatch. `finish_task_run_build_admission` writes
        //   `CreateUnknown` ("slot-pool accepted create without object UID")
        //   for *every* task-run the moment the pool accepts it, and it stays
        //   that way until the `("session","started")` callback supplies the
        //   UID. A warm Job is the same between its POST and `job_uid()`.
        //
        // Counting the second kind is what halted the board for five hours on
        // 2026-07-29. This seed runs at the tail of EVERY periodic
        // reconciliation pass (see `build_admission_inventory::reconcile`,
        // 120s since #2711), so a tick that lands inside one normal dispatch's
        // POST→session window armed `CreateUnknownHealth` against the live
        // process's own healthy in-flight work — denying every admission
        // before any capacity was measured. It normally cleared on the next
        // tick; that day the worker never registered a session, so `mark_live`
        // never ran and the row stayed. And `is_reclaimable` refuses to retire
        // a row under this process's own epoch (correctly — see
        // `BuildAdmissionReconciler::is_reclaimable`), so the gate had no
        // reachable clearing path at all. Only a restart, which reclassifies
        // the row as a predecessor's, could clear it.
        //
        // Arming and reclamation must therefore agree on the same population:
        // the gate is armed by exactly the rows the reclaimer is allowed to
        // retire. A row this process is mid-creating is not a *recovered*
        // unknown, and it is not evidence that this process cannot be trusted
        // to admit.
        let is_recovered_unknown = |row: &AdmissionJournalRow| {
            row.state == AdmissionState::CreateUnknown
                && row.creator_server_epoch != self.creator_server_epoch
        };
        // Every active recovered row counts, not only the ones seeded into
        // memory: an unseeded recovered CreateUnknown row still occupies
        // durable capacity and still gates Enforce readiness.
        let create_unknown_rows = recovery
            .active_rows
            .iter()
            .filter(|row| is_recovered_unknown(row))
            .count() as u64;
        // Capture WHICH rows those are, from the same iteration that counts
        // them, so the count and the named set can never come from different
        // views of the journal.
        let create_unknown_identities: BTreeSet<String> = recovery
            .active_rows
            .iter()
            .filter(|row| row.state == AdmissionState::CreateUnknown)
            .map(|row| blocking_identity(&row.key, &row.object_name))
            .collect();
        {
            let mut permits = self.permits.lock().await;
            let mut by_key = self.permits_by_key.lock().await;
            let mut by_work = self.permits_by_work.lock().await;
            for row in &recovery.active_rows {
                if !seed_filter(row) {
                    continue;
                }
                // Re-seeding the same key reuses the existing permit so a
                // repeated recovery never duplicates in-memory bookkeeping.
                let key = permit_key(&row.key);
                let permit = match by_key.get(&key) {
                    Some(existing) => existing.clone(),
                    None => {
                        let permit = WarmAdmissionPermit::new();
                        by_key.insert(key, permit.clone());
                        permit
                    }
                };
                // A recovered row is the work item's current generation, so a
                // lifecycle callback that arrives after restart resolves to it
                // exactly as it would have before the restart.
                by_work.insert(work_key(&row.key), permit.clone());
                permits.insert(
                    permit,
                    PermitState {
                        key: row.key.clone(),
                        creator_server_epoch: row.creator_server_epoch.clone(),
                        object_name: row.object_name.clone(),
                        durable: true,
                        released: false,
                        // Exactly the rows that armed the gate above. This flag
                        // is what `transition` decrements the gate on when the
                        // row is adopted into Live, so seeding it for a row
                        // that never contributed to the count would let one
                        // healthy own-epoch dispatch clear a gate a
                        // predecessor's row is still holding — fail-open.
                        create_unknown_outstanding: is_recovered_unknown(row),
                        // A recovered row's capacity was acquired by the
                        // predecessor process, but the lease row it acquired
                        // survives the restart in `build_leases` and is
                        // recovered alongside. Attributing capacity by DOMAIN
                        // reconstructs who owns the release: a task-run holds a
                        // dispatch slot this controller must hand back when it
                        // terminalizes, while warm/invocation rows are owned by
                        // consumers that release their own leases.
                        capacity: match row.key.domain {
                            AdmissionDomain::TaskObservation => CapacitySource::AcquireDispatchSlot,
                            AdmissionDomain::WarmBuild | AdmissionDomain::InvocationBuild => {
                                CapacitySource::HeldByLease
                            }
                        },
                    },
                );
                seeded = seeded.saturating_add(1);
            }
        }
        // There is deliberately no journal occupancy read here any more.
        //
        // Recovery used to count occupying journal rows and compare that count
        // against the build-slot cap. Capacity is not derived from the journal:
        // it is the weighted sum of occupying `build_leases` rows, read below
        // from the one authority. Keeping a journal count here would be both a
        // pointless startup round-trip and a standing invitation to compare it
        // to a cap again.
        if self.mode() != BuildAdmissionMode::Off {
            // Journal recovery succeeded. Only the journal-derived gates are
            // updated here: the inventory and topology gates are deliberately
            // NOT touched, so Enforce remains fail-closed
            // (`InventoryPending`/`TopologyPending`) until the real Kubernetes
            // inventory LIST and the single-active topology check complete.
            self.journal_recovered.store(true, Ordering::Release);
            self.journal_healthy.store(true, Ordering::Release);
            self.create_unknown_pending
                .store(create_unknown_rows, Ordering::Release);
            self.set_blocking_identities(create_unknown_identities);
            // Compare BUILD SLOTS to a build-slot cap.
            //
            // This used to compare `count_task_or_warm_occupancy()` -- a count
            // of occupying JOURNAL rows -- against `self.cap`, which is a count
            // of build slots. Those were already two different units, and once
            // the journal became the lifecycle ledger rather than the capacity
            // authority the comparison stopped being meaningful entirely: a
            // journal row records an object lifecycle, it does not reserve CPU.
            //
            // The distinction is not academic. It is exactly the production
            // wedge observed on v0.7.5: 59 occupying journal rows (58 of them
            // stale, left by a predecessor whose objects are long gone) against
            // a cap of 3 latched `over_cap`, which `readiness()` reports as
            // `SeededOccupancyAboveCap`, which fails Enforce closed for every
            // admission. Stale LIFECYCLE rows must be retired by reconciliation
            // (#2597) -- they must never have been able to deny capacity that
            // they were not holding.
            //
            // Only a KNOWN occupancy above the cap latches this gate. Unknown
            // occupancy is not evidence of over-cap, and treating it as such
            // wedges the node.
            //
            // `initialize_build_admission_recovery` runs BEFORE
            // `initialize_graph_warmer`, and the lease service recovers inside
            // the latter -- so at this point the authority has never recovered
            // on ANY startup and reports its occupancy as unknown. Latching on
            // unknown therefore set `over_cap` on every boot, and `over_cap` is
            // sticky: it is re-read only when a durable terminal transition
            // releases a permit, which cannot happen while readiness reports
            // `SeededOccupancyAboveCap` and denies every admission. Enforce
            // would have failed closed permanently, for a reason that was not
            // even true.
            //
            // Nothing is lost by not latching. Capacity is already fail-closed
            // where it matters and without stickiness: an unreachable authority
            // yields `DispatchSlotOutcome::Unavailable`, which `admit` turns
            // into a denial for every mode but Observe, and which clears by
            // itself the moment the authority becomes readable. Enforce also
            // stays shut behind `InventoryPending`/`TopologyPending` until the
            // real startup checks pass.
            //
            // No authority installed means this controller is not capacity
            // gated at all (the Off shape), so there is no cap to exceed.
            let over_cap = match self.slot_authority.as_ref() {
                None => false,
                Some(authority) => authority
                    .occupancy()
                    .await
                    .is_some_and(|slots| slots > authority.cap()),
            };
            self.over_cap.store(over_cap, Ordering::Release);
        }
        let readiness = self.readiness();
        // Recovery changes active durable rows, including predecessor
        // CreateInFlight rows converted to CreateUnknown, so export now.
        self.publish_metrics().await;
        Ok(AdmissionSeedReport {
            retired_reserved: recovery.retired_reserved,
            marked_create_unknown: recovery.marked_create_unknown,
            seeded_rows: seeded,
            readiness,
        })
    }
}

/// Allocate a fresh, unique server epoch for this process.
///
/// The epoch is a time-ordered UUIDv7 string so a replacement process always
/// sorts after its predecessor and recovery can distinguish rows by creator.
#[must_use]
pub fn allocate_server_epoch() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn unavailable(error: impl std::fmt::Display) -> WarmAdmissionError {
    WarmAdmissionError::Unavailable {
        diagnostic: error.to_string(),
    }
}

fn permit_key(key: &AdmissionJournalKey) -> String {
    admission_generation_key(key)
}

/// Generation-free identity for one work item. Shares the `{domain:?}:{work_id}:`
/// prefix with [`admission_generation_key`] so the deferred-queue prefix scans
/// and this map agree on what "the same work" means.
fn work_key(key: &AdmissionJournalKey) -> String {
    format!("{:?}:{}", key.domain, key.work_id)
}

/// Canonical string identity for one admission generation.
///
/// This is the single source of truth for the `generation_key` used by the
/// durable admission-handoff per-generation acknowledgements
/// ([`djinn_db::AdmissionHandoffRepository::record_generation_ack`]) and the
/// `required_generations` set on the invocation-primary edge. Both the producer
/// of that required set and every live generation that acknowledges an epoch
/// MUST format their key through this function so the two byte-match. It is the
/// same `{domain:?}:{work_id}:{generation}` form used for in-memory permit
/// bookkeeping.
///
/// The generation component is a JOURNAL fact. A producer that formats a key
/// from a caller-side counter (a task's `reopen_count`) only byte-matches while
/// that task has had one dispatch attempt per reopen; a retried attempt reserves
/// its own generation. Any producer of the required set must read the generation
/// from the journal row rather than recompute it.
#[must_use]
pub fn admission_generation_key(key: &AdmissionJournalKey) -> String {
    format!("{:?}:{}:{}", key.domain, key.work_id, key.generation)
}

/// Convenience [`admission_generation_key`] for a task-run generation, whose
/// admission domain is always [`AdmissionDomain::TaskObservation`] and whose
/// generation counter is `task.reopen_count`.
#[must_use]
pub fn task_run_generation_key(task_id: &str, generation: i64) -> String {
    admission_generation_key(&AdmissionJournalKey {
        domain: AdmissionDomain::TaskObservation,
        work_id: task_id.to_owned(),
        generation,
    })
}

#[async_trait]
impl WarmAdmission for BuildAdmissionController {
    async fn admit(
        &self,
        request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError> {
        let decision = self
            .admit(BuildAdmissionRequest {
                domain: AdmissionDomain::WarmBuild,
                work_id: request.work_id,
                generation: request.generation,
                object_name: request.object_name,
                kind: BuildWorkloadKind::GraphWarmJob,
                // Warm capacity is ALWAYS the graph-warm lease, acquired by the
                // warmer before it reaches admission. This call is therefore a
                // ledger append, never a second capacity decision -- which is
                // exactly the duplication being removed. When no lease service
                // is composed, `initialize_graph_warmer` leaves warming ungated
                // and says so; it does not fall back to a second cap here.
                capacity: CapacitySource::HeldByLease,
            })
            .await?;
        match decision {
            BuildAdmissionDecision::Permitted { permit, .. } => Ok(permit),
            BuildAdmissionDecision::Denied {
                occupancy,
                cap,
                cause,
            } => Err(WarmAdmissionError::Denied {
                diagnostic: match occupancy {
                    Some(occupancy) => format!("occupancy {occupancy} reached cap {cap} ({cause})"),
                    None => {
                        format!("denied without measuring occupancy against cap {cap} ({cause})")
                    }
                },
            }),
            BuildAdmissionDecision::Unclassified => Err(WarmAdmissionError::Denied {
                diagnostic: "unclassified build workload".into(),
            }),
        }
    }

    async fn transition(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        self.transition_permit(permit, transition).await
    }
}

/// Serializes every test that asserts on the process-global build-admission
/// metrics, across all modules in this crate.
///
/// The occupancy gauges are unlabelled and the health gauges are keyed only by
/// mode and cap, so two tests publishing at once make each other's reading
/// arbitrary. `enable_process_metrics_for_test` decides *which* controllers may
/// write at all; this lock decides that only one of them writes at a time. Both
/// are needed — the flag alone would still let two opted-in tests collide.
///
/// Poisoning is recovered rather than propagated so one failing test reports
/// its own assertion instead of turning its peers into unwrap panics.
#[cfg(test)]
pub(crate) fn telemetry_guard() -> std::sync::MutexGuard<'static, ()> {
    static TELEMETRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TELEMETRY_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::{
        AdmissionState, Database, ImageRepository, ProjectRepository,
        test_support::reject_admission_create_started_for_test,
    };
    use djinn_k8s::{
        K8sGraphWarmer, KubernetesConfig, WarmJobDispatcher, WarmJobManifest, WarmJobWatcher,
        WarmTerminalOutcome,
    };
    use djinn_runtime::GraphWarmerService;
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    use crate::build_admission_capacity_support::CapacityHarness;

    #[test]
    fn validate_admission_config_rejects_illegal_combo_and_out_of_range_cap() {
        // At least one enforcing authority is legal across the cap range.
        assert!(validate_admission_config(V0Mode::Enforce, V1Mode::Off, 1).is_ok());
        assert!(validate_admission_config(V0Mode::Enforce, V1Mode::Shadow, 8).is_ok());
        assert!(validate_admission_config(V0Mode::Observe, V1Mode::Enforce, 3).is_ok());
        assert!(
            validate_admission_config(V0Mode::Disabled, V1Mode::Enforce, MAX_ADMISSION_CAP).is_ok()
        );

        // Neither authority enforcing is illegal for every non-enforcing pairing.
        for (v0, v1) in [
            (V0Mode::Observe, V1Mode::Off),
            (V0Mode::Observe, V1Mode::Shadow),
            (V0Mode::Disabled, V1Mode::Off),
            (V0Mode::Disabled, V1Mode::Shadow),
        ] {
            assert!(
                validate_admission_config(v0, v1, 4).is_err(),
                "{v0:?}/{v1:?} must be rejected"
            );
        }

        // Cap out of range is rejected even with an enforcing authority.
        assert!(validate_admission_config(V0Mode::Enforce, V1Mode::Off, 0).is_err());
        assert!(validate_admission_config(V0Mode::Enforce, V1Mode::Off, -1).is_err());
        assert!(
            validate_admission_config(V0Mode::Enforce, V1Mode::Off, MAX_ADMISSION_CAP + 1).is_err()
        );
    }

    /// A controller with NO capacity authority attached.
    ///
    /// This is a real production shape — the Off / local-dev composition, where
    /// nothing is capacity gated — and it is the right fixture for the lifecycle,
    /// fencing, readiness and telemetry properties below, none of which are
    /// about a cap. It must never be used to assert that something is DENIED at
    /// a cap: with no authority there is nothing to deny with, and the
    /// assertion would pass for the wrong reason. Those tests use
    /// [`capacity_harness`].
    fn ungated_controller(mode: BuildAdmissionMode, cap: i64) -> BuildAdmissionController {
        BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(
                Database::open_in_memory().unwrap(),
            )),
            mode,
            cap,
            "epoch",
        )
    }

    /// The production composition: one controller reaching capacity through the
    /// ONE armed lease authority, over one database.
    async fn capacity_harness(mode: BuildAdmissionMode, cap: i64) -> CapacityHarness {
        crate::build_admission_capacity_support::controller_with_capacity(mode, cap, "epoch").await
    }

    /// The deferred population is TASK dispatch.
    ///
    /// It used to be graph warming, because the journal capped both. It no
    /// longer caps either: a warm Job's capacity is its graph-warm lease, taken
    /// before it reaches admission, so a warm admission is a ledger append that
    /// cannot be denied and therefore never enters the deferred queue. Task
    /// dispatch is the population that IS refused at layer 1, so it is the one
    /// whose queue lifecycle these gauges describe. The properties asserted —
    /// first-denial timing, unique-waiter cardinality, exactly-once terminal
    /// observations — are unchanged; only the population that exhibits them is.
    async fn dispatch_permit(
        controller: &BuildAdmissionController,
        work_id: &str,
    ) -> WarmAdmissionPermit {
        match controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                work_id.to_owned(),
                0,
                format!("task-run-{work_id}"),
            )
            .await
            .unwrap()
        {
            BuildAdmissionDecision::Permitted { permit, .. } => permit,
            other => panic!("{work_id} must be permitted, got {other:?}"),
        }
    }

    async fn dispatch_denied(controller: &BuildAdmissionController, work_id: &str) -> bool {
        matches!(
            controller
                .admit_task_run(
                    Some("worker"),
                    AdmissionDomain::TaskObservation,
                    work_id.to_owned(),
                    0,
                    format!("task-run-{work_id}"),
                )
                .await
                .unwrap(),
            BuildAdmissionDecision::Denied { .. }
        )
    }
    fn warm(id: &str) -> WarmAdmissionRequest {
        WarmAdmissionRequest {
            domain: "ignored".into(),
            work_id: id.into(),
            generation: 0,
            object_name: format!("job-{id}"),
        }
    }

    async fn seed_project_with_ready_image(db: &Database, name: &str) -> String {
        let projects = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = projects.create(name, "test", name).await.unwrap();
        let images = ImageRepository::new(db.clone());
        let image_id = format!("img-{name}");
        images.create(&image_id, name, None, "{}").await.unwrap();
        images
            .mark_ready(
                &image_id,
                &format!("reg.example:5000/djinn-project-{}:abc123", project.id),
                None,
            )
            .await
            .unwrap();
        images
            .set_project_image(&project.id, Some(&image_id))
            .await
            .unwrap();
        project.id
    }

    struct AdmissionStateRecordingDispatcher {
        journal: Arc<AdmissionJournalRepository>,
        work_id: String,
        posts: Arc<AtomicUsize>,
        posted: Arc<Notify>,
    }

    #[async_trait]
    impl WarmJobDispatcher for AdmissionStateRecordingDispatcher {
        async fn dispatch(
            &self,
            _namespace: &str,
            _job: WarmJobManifest,
        ) -> Result<String, String> {
            let history = self
                .journal
                .list_history(AdmissionDomain::WarmBuild, &self.work_id)
                .await
                .unwrap();
            assert_eq!(
                history[0].state,
                AdmissionState::CreateInFlight,
                "the concrete controller must durably record CreateStarted before POST"
            );
            self.posts.fetch_add(1, Ordering::SeqCst);
            self.posted.notify_one();
            Ok("warm-job".into())
        }
    }

    struct FencedTerminalWatcher;

    #[async_trait]
    impl WarmJobWatcher for FencedTerminalWatcher {
        async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
            WarmTerminalOutcome::Succeeded
        }

        async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
            Some("warm-uid".into())
        }
    }

    /// Kubernetes assigns a fresh UID to every Job object it creates, even when
    /// the deterministic object name is identical. This watcher reproduces that:
    /// each observed warm lifecycle reports its own immutable UID.
    struct FreshUidPerJobWatcher {
        observed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WarmJobWatcher for FreshUidPerJobWatcher {
        async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
            WarmTerminalOutcome::Succeeded
        }

        async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
            let n = self.observed.fetch_add(1, Ordering::SeqCst) + 1;
            Some(format!("warm-uid-{n}"))
        }
    }

    struct CountingWarmDispatcher {
        posts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WarmJobDispatcher for CountingWarmDispatcher {
        async fn dispatch(
            &self,
            _namespace: &str,
            _job: WarmJobManifest,
        ) -> Result<String, String> {
            self.posts.fetch_add(1, Ordering::SeqCst);
            Ok("warm-job".into())
        }
    }

    async fn await_terminal_generations(
        journal: &AdmissionJournalRepository,
        work_id: &str,
        expected: usize,
    ) -> Vec<AdmissionJournalRow> {
        for _ in 0..600 {
            let history = journal
                .list_history(AdmissionDomain::WarmBuild, work_id)
                .await
                .unwrap();
            if history.len() >= expected
                && history
                    .iter()
                    .all(|row| row.state == AdmissionState::Terminal)
            {
                return history;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        journal
            .list_history(AdmissionDomain::WarmBuild, work_id)
            .await
            .unwrap()
    }

    /// Regression (ymx9): two real graph-warm Job lifecycles for the same
    /// project revision must each own a journal generation carrying that Job's
    /// own Kubernetes UID.
    ///
    /// This drives the production `K8sGraphWarmer` create → observe → terminal
    /// sequence through the production `BuildAdmissionController` and the
    /// production `AdmissionJournalRepository` in the mode production actually
    /// runs (`Observe`). Before the fix, the second lifecycle reused the first
    /// lifecycle's retired row, so every observation transition was rejected —
    /// `cannot mark create started from Terminal` followed by two
    /// `Kubernetes UID does not match admission row` rejections.
    #[tokio::test]
    async fn repeated_warm_lifecycles_record_one_generation_per_kubernetes_uid() {
        let db = Database::open_in_memory().unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Observe,
            4,
            "epoch",
        ));
        let project_id = seed_project_with_ready_image(&db, "warm-uid-regression").await;
        let work_id = djinn_k8s::warm_work_id(&project_id, "unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db,
            Arc::new(CountingWarmDispatcher {
                posts: Arc::clone(&posts),
            }),
            Arc::new(FreshUidPerJobWatcher {
                observed: Arc::clone(&observed),
            }),
        )
        .with_warm_admission(controller.clone());

        warmer.trigger(&project_id).await;
        let history = await_terminal_generations(&journal, &work_id, 1).await;
        assert_eq!(history.len(), 1, "the first warm records one generation");

        warmer.trigger(&project_id).await;
        let history = await_terminal_generations(&journal, &work_id, 2).await;

        assert_eq!(posts.load(Ordering::SeqCst), 2, "both warm lifecycles POST");
        assert_eq!(
            history.len(),
            2,
            "a second warm Job is a second object lifecycle and must own its own \
             admission generation instead of reusing the retired row"
        );
        let uids: Vec<Option<&str>> = history
            .iter()
            .map(|row| row.object_uid.as_deref())
            .collect();
        assert_eq!(
            uids,
            vec![Some("warm-uid-1"), Some("warm-uid-2")],
            "every generation persists the exact Kubernetes UID its own Live and \
             Terminal observations supplied"
        );
        assert!(
            history
                .iter()
                .all(|row| row.state == AdmissionState::Terminal),
            "both generations reach Terminal through the observed UID"
        );
        assert_eq!(
            controller.rejected_transition_count(),
            0,
            "Observe mode must emit successful journal observations, not a 100% \
             rejection rate; last rejection: {:?}",
            controller.last_transition_rejection().await
        );
        assert!(
            controller.accepted_transition_count() >= 6,
            "each warm lifecycle records CreateStarted, Live and Terminal"
        );
        assert_eq!(controller.last_transition_rejection().await, None);
    }

    /// Regression (ymx9): the create observation and the terminal observation
    /// are independent messages, so a create report can arrive after the
    /// generation is already retired. That has a defined idempotent outcome —
    /// the terminal row is retained, occupancy is not resurrected — while a
    /// transition addressed to a superseded generation is still rejected.
    #[tokio::test]
    async fn late_create_observation_is_idempotent_while_stale_generations_stay_rejected() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 4);
        controller.mark_ready();
        let first = WarmAdmission::admit(&controller, warm("late-create"))
            .await
            .unwrap();
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Live {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();

        // The create outcome finally lands, after the terminal one.
        controller
            .transition(
                &first,
                WarmAdmissionTransition::CreateUnknown {
                    diagnostic: "create response arrived after the job terminated".into(),
                },
            )
            .await
            .expect("a late create observation resolves idempotently");
        let history = controller
            .journal()
            .list_history(AdmissionDomain::WarmBuild, "late-create")
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].state,
            AdmissionState::Terminal,
            "a late create observation must not resurrect a retired generation"
        );
        assert_eq!(
            controller
                .journal()
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            0,
            "a late create observation must not re-occupy capacity"
        );

        // A second attempt supersedes the retired generation. Every transition
        // still addressed to the old one is rejected, loudly.
        let second = WarmAdmission::admit(&controller, warm("late-create"))
            .await
            .unwrap();
        assert_ne!(first, second, "a new attempt gets a new permit");
        controller
            .transition(&second, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        let stale = controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .expect_err("a superseded generation cannot transition");
        assert!(
            stale.to_string().contains("stale admission generation"),
            "unexpected diagnostic: {stale}"
        );
        assert!(controller.rejected_transition_count() >= 1);
        assert!(
            controller
                .last_transition_rejection()
                .await
                .is_some_and(|reason| reason.contains("stale admission generation"))
        );
    }

    /// The concrete warmer draws from the SAME pool as task dispatch, and a
    /// warm refused by a full pool retries when capacity is handed back.
    ///
    /// Composed the way production composes it, which is the point: the warmer
    /// holds a graph-warm lease from the one `BuildLeaseService`, and that lease
    /// — not the admission call — is where its capacity is decided. Without the
    /// lease adapter, `initialize_graph_warmer` leaves warming ungated and says
    /// so; admission deliberately does not fall back to a second cap.
    #[tokio::test]
    async fn concrete_k8s_warmer_shares_task_cap_and_retries_after_fenced_release() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let h = crate::build_admission_capacity_support::attach_capacity(
            &db,
            BuildAdmissionController::new(
                Arc::clone(&journal),
                BuildAdmissionMode::Enforce,
                1,
                "epoch",
            ),
            1,
        )
        .await;
        let controller = Arc::clone(&h.controller);
        let project_id = seed_project_with_ready_image(&db, "shared-cap").await;
        let work_id = djinn_k8s::warm_work_id(&project_id, "unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let posted = Arc::new(Notify::new());
        // A leased warm Job waits for its Kubernetes candidate up to
        // `warm_job_timeout_seconds`; this harness stubs the dispatcher, not the
        // candidate inventory, so the 3600s default would park the retry task
        // forever. Every ledger write this test asserts happens BEFORE the POST.
        let mut config = KubernetesConfig::for_testing();
        config.warm_job_timeout_seconds = 1;
        let warmer = K8sGraphWarmer::with_dispatcher(
            config,
            db.clone(),
            Arc::new(AdmissionStateRecordingDispatcher {
                journal: Arc::clone(&journal),
                work_id: work_id.clone(),
                posts: Arc::clone(&posts),
                posted: Arc::clone(&posted),
            }),
            Arc::new(FencedTerminalWatcher),
        )
        .with_warm_admission(controller.clone())
        .with_graph_warm_lease(Arc::new(
            crate::graph_warm_lease::BuildLeaseGraphWarmAdapter::new(Arc::clone(&h.lease)),
        ));

        let task = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                1,
                "task-job".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: task, .. } = task else {
            panic!("the task must win the cap-one reservation");
        };

        assert_eq!(h.occupancy().await, 1, "the task holds the only build slot");

        warmer.trigger(&project_id).await;
        assert_eq!(posts.load(Ordering::SeqCst), 0, "denied warm must not POST");
        assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 1);
        assert!(
            journal
                .list_history(AdmissionDomain::WarmBuild, &work_id)
                .await
                .unwrap()
                .is_empty(),
            "the denied warm does not become completed or failed"
        );

        controller
            .transition(&task, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &task,
                WarmAdmissionTransition::Live {
                    uid: "task-uid".into(),
                },
            )
            .await
            .unwrap();
        let released = controller.release_notifier().notified();
        controller
            .transition(
                &task,
                WarmAdmissionTransition::Terminal {
                    uid: "task-uid".into(),
                },
            )
            .await
            .unwrap();
        released.await;

        let post = posted.notified();
        tokio::pin!(post);
        tokio::time::timeout(std::time::Duration::from_secs(3), post)
            .await
            .expect("pending warm should retry after the admission backoff");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "released capacity retries the pending warm"
        );
    }

    #[tokio::test]
    async fn concrete_k8s_warmer_keeps_failed_create_started_pending_without_posting() {
        let db = Database::open_in_memory().unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Enforce,
            1,
            "epoch",
        ));
        let project_id = seed_project_with_ready_image(&db, "create-started-failure").await;
        let work_id = djinn_k8s::warm_work_id(&project_id, "unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let posted = Arc::new(Notify::new());
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db.clone(),
            Arc::new(AdmissionStateRecordingDispatcher {
                journal: Arc::clone(&journal),
                work_id: work_id.clone(),
                posts: Arc::clone(&posts),
                posted: Arc::clone(&posted),
            }),
            Arc::new(FencedTerminalWatcher),
        )
        .with_warm_admission(controller);

        // Fail the real controller durable state transition after it reserves
        // the warm row, rather than substituting a fake WarmAdmission.
        reject_admission_create_started_for_test(&db, true).await;

        warmer.trigger(&project_id).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "a real-controller CreateStarted failure must perform zero POSTs"
        );
        assert_eq!(
            journal
                .list_history(AdmissionDomain::WarmBuild, &work_id)
                .await
                .unwrap()[0]
                .state,
            AdmissionState::Reserved,
            "the failed transition retains the coalesced warm reservation"
        );
        warmer.trigger(&project_id).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "an immediate retrigger coalesces onto the pending warm"
        );

        reject_admission_create_started_for_test(&db, false).await;
        let post = posted.notified();
        tokio::pin!(post);
        tokio::time::timeout(std::time::Duration::from_secs(3), post)
            .await
            .expect("pending warm should retry after the admission backoff");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "the retained warm retries after the journal transition becomes durable"
        );
    }

    #[test]
    fn classification_covers_every_dispatch_role_and_rejects_unknown() {
        for role in [
            "worker",
            "reviewer",
            "lead",
            "planner",
            "architect",
            "advocate",
            "adversary",
            "judge",
        ] {
            assert!(TaskRunRole::parse(Some(role)).is_some());
        }
        assert_eq!(TaskRunRole::parse(None), None);
        assert_eq!(TaskRunRole::parse(Some("mystery")), None);
    }

    #[tokio::test]
    async fn off_is_noop_and_unknown_is_bounded() {
        let controller = ungated_controller(BuildAdmissionMode::Off, 0);
        let permit = WarmAdmission::admit(&controller, warm("off"))
            .await
            .unwrap();
        controller
            .transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            0
        );
        for _ in 0..1025 {
            let _ = controller
                .admit_task_run(
                    None,
                    AdmissionDomain::TaskObservation,
                    "x".into(),
                    0,
                    "x".into(),
                )
                .await;
        }
        assert_eq!(controller.unclassified_observation_count().await, 1024);
    }

    #[tokio::test]
    async fn observe_permits_when_journal_reservation_is_unavailable() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        db.pool().close().await;
        let controller = BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(db)),
            BuildAdmissionMode::Observe,
            1,
            "epoch",
        );

        assert!(
            WarmAdmission::admit(&controller, warm("journal-down"))
                .await
                .is_ok(),
            "Observe journal failures are telemetry-only and must not defer dispatch"
        );
    }

    /// A shadow authority records what enforcement WOULD have done, per probe,
    /// and denies nothing; an armed one draws both domains from one pool.
    ///
    /// The would-defer signal is the operator's evidence before arming the
    /// epoch, so it is emitted by the shadow path and only there. A shadow probe
    /// deliberately inserts NO row -- a shadow reservation would occupy real
    /// capacity and start denying graph warming, which is the silent behaviour
    /// change the rollout exists to avoid. That is why the observation is one
    /// per probe rather than "the loser of a race": with nothing inserted, there
    /// is no race to lose, and occupancy comes entirely from the warm Job that
    /// really is holding the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shadow_records_would_defer_without_denial_and_enforce_combines_domains() {
        let shadow = capacity_harness(BuildAdmissionMode::Observe, 1).await;
        shadow.controller.mark_ready();
        let _held = shadow
            .hold_warm_lease("shadow-occupant")
            .await
            .expect("the warm Job takes the only slot");
        shadow.lease.set_dispatch_enforcing_for_test(false);

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut probes = Vec::new();
        for name in ["a", "b"] {
            let controller = Arc::clone(&shadow.controller);
            let barrier = Arc::clone(&barrier);
            probes.push(tokio::spawn(async move {
                barrier.wait().await;
                controller
                    .admit_task_run(
                        Some("worker"),
                        AdmissionDomain::TaskObservation,
                        name.to_owned(),
                        0,
                        format!("task-job-{name}"),
                    )
                    .await
            }));
        }
        for probe in probes {
            assert!(
                matches!(
                    probe.await.unwrap().unwrap(),
                    BuildAdmissionDecision::Permitted { .. }
                ),
                "a shadow authority never denies"
            );
        }
        assert_eq!(shadow.controller.would_defer_observation_count().await, 2);
        assert_eq!(
            shadow.occupancy().await,
            1,
            "and never acquires: the warm Job is still the only occupant"
        );

        // Armed, the same pool serves both domains: the task-run's dispatch slot
        // is the warm Job's slot.
        let enforced = capacity_harness(BuildAdmissionMode::Enforce, 1).await;
        enforced.controller.mark_ready();
        let _ = enforced
            .controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                0,
                "task-job".into(),
            )
            .await
            .unwrap();
        assert!(
            enforced.hold_warm_lease("warm").await.is_none(),
            "the task-run's slot denies the warm Job"
        );
    }

    #[tokio::test]
    async fn permits_are_idempotent_and_terminal_notifies_and_is_uid_fenced() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 2);
        let first = WarmAdmission::admit(&controller, warm("same"))
            .await
            .unwrap();
        let second = WarmAdmission::admit(&controller, warm("same"))
            .await
            .unwrap();
        assert_eq!(first, second);
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(&first, WarmAdmissionTransition::Live { uid: "uid".into() })
            .await
            .unwrap();
        assert!(
            controller
                .transition(
                    &first,
                    WarmAdmissionTransition::Terminal {
                        uid: "wrong".into()
                    }
                )
                .await
                .is_err()
        );
        let notified = controller.release_notifier().notified();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal { uid: "uid".into() },
            )
            .await
            .unwrap();
        notified.await;
        assert_eq!(
            controller
                .journal
                .list_history(AdmissionDomain::WarmBuild, "same")
                .await
                .unwrap()[0]
                .state,
            AdmissionState::Terminal
        );
    }

    #[tokio::test]
    async fn task_generations_and_runtime_uids_fence_terminal_release() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 3);
        let first = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                1,
                "task-run-task-1".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: first, .. } = first else {
            panic!("task generation one must be admitted");
        };
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Live {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_some(),
            "generation one release must retain exactly one wakeup"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "generation one release must not retain a second wakeup"
        );

        // Repeating the matching terminal callback while generation one is
        // still current is idempotent and does not emit another wakeup.
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "duplicate generation-one terminal must not wake dispatch again"
        );

        let second = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                2,
                "task-run-task-2".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: second, .. } = second else {
            panic!("task generation two must be admitted");
        };
        controller
            .transition(&second, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &second,
                WarmAdmissionTransition::Live {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();

        // Once generation two exists, a delayed callback for the old
        // generation is stale and cannot release the newer row.
        let error = controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .expect_err("generation-one callback must be rejected as stale");
        assert_eq!(
            error,
            WarmAdmissionError::Unavailable {
                diagnostic: "invalid transition: stale admission generation 1 for task".into(),
            }
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "delayed old-generation callback must not wake dispatch"
        );
        let history = controller
            .journal
            .list_history(AdmissionDomain::TaskObservation, "task")
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .find(|row| row.key.generation == 2)
                .unwrap()
                .state,
            AdmissionState::Live
        );
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1,
            "delayed old-generation duplicate must leave generation two occupied"
        );

        // A wrong UID and an unbound (UID-less) callback retain occupancy.
        assert!(
            controller
                .transition(
                    &second,
                    WarmAdmissionTransition::Terminal {
                        uid: "uid-one".into(),
                    },
                )
                .await
                .is_err()
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "wrong generation-two UID must not wake dispatch"
        );
        assert!(
            controller
                .task_run_permit_for_runtime_id("missing-uid")
                .await
                .is_none()
        );
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1,
            "UID-less terminal handling must retain generation-two occupancy"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "UID-less terminal handling must not wake dispatch"
        );

        controller
            .transition(
                &second,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_some(),
            "matching generation-two terminal must retain one wakeup"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "matching generation-two terminal must retain only one wakeup"
        );

        // A duplicate matching terminal callback is idempotent.
        controller
            .transition(
                &second,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "duplicate generation-two terminal must not wake dispatch again"
        );
    }
    #[tokio::test]
    async fn closed_enforce_controller_denies_until_recovery_marks_ready() {
        let controller = BuildAdmissionController::new_closed(
            Arc::new(AdmissionJournalRepository::new(
                Database::open_in_memory().unwrap(),
            )),
            1,
            "epoch",
        );
        assert!(!controller.is_ready());
        assert!(matches!(
            WarmAdmission::admit(&controller, warm("closed")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
        controller.mark_ready();
        assert!(
            WarmAdmission::admit(&controller, warm("open"))
                .await
                .is_ok()
        );
    }

    fn predecessor_input(
        work_id: &str,
        generation: i64,
        epoch: &str,
    ) -> djinn_db::ReserveAdmissionInput {
        djinn_db::ReserveAdmissionInput {
            key: djinn_db::AdmissionJournalKey {
                domain: AdmissionDomain::WarmBuild,
                work_id: work_id.into(),
                generation,
            },
            workload_kind: djinn_db::AdmissionWorkloadKind::Warm,
            creator_server_epoch: epoch.into(),
            object_name: format!("warm-{work_id}-{generation}"),
        }
    }

    /// **The 2026-07-29 visibility gap.** The controller's readiness lives in
    /// process-local atomics on the leader, so no durable query can reach it.
    /// For five hours the only way to learn that `CreateUnknownHealth` was
    /// denying every dispatch on the board was to ssh the node and grep
    /// container logs for the `readiness=` field.
    ///
    /// `debug_snapshot` is what `/debug/dispatch-state` reports instead.
    #[tokio::test]
    async fn debug_snapshot_names_the_gate_that_is_denying_every_dispatch() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 3, "this-epoch");

        let snapshot = controller.debug_snapshot().await;
        assert_eq!(
            snapshot.health.readiness.as_str(),
            "journal_recovery_incomplete"
        );
        assert!(!snapshot.health.readiness.is_healthy());
        assert_eq!(snapshot.health.mode, BuildAdmissionMode::Enforce);
        assert_eq!(snapshot.configured_cap, 3);
        assert_eq!(snapshot.server_epoch, "this-epoch");
        assert!(
            snapshot.occupancy.is_none(),
            "no capacity authority is installed; `None` must never render as 0 — \
             a fabricated zero is what made a wedged controller look like a full pool"
        );

        // Latch the gate that actually wedged the board.
        controller.journal_recovered.store(true, Ordering::Release);
        controller.mark_inventory_ready();
        controller.mark_topology_ready();
        controller
            .create_unknown_pending
            .store(1, Ordering::Release);

        let snapshot = controller.debug_snapshot().await;
        assert_eq!(snapshot.health.readiness.as_str(), "create_unknown_health");
        assert_eq!(snapshot.health.create_unknown_pending, 1);
        assert_eq!(snapshot.unsatisfied_gates, vec!["create_unknown_health"]);
        assert!(!snapshot.health.readiness.is_healthy());

        controller
            .create_unknown_pending
            .store(0, Ordering::Release);
        let snapshot = controller.debug_snapshot().await;
        assert_eq!(snapshot.health.readiness.as_str(), "healthy");
        assert!(snapshot.health.readiness.is_healthy());
        assert!(snapshot.unsatisfied_gates.is_empty());
    }

    /// `readiness()` answers with the highest-priority failing gate because
    /// that is what `admit()` acts on. `unsatisfied_gates` must report the
    /// whole set: clearing one gate, finding the board still wedged, and
    /// having no way to see the second is how a five-hour outage becomes a
    /// ten-hour one.
    ///
    /// This also guards the two from drifting apart — they are separate
    /// hand-written ladders over the same atomics.
    #[tokio::test]
    async fn unsatisfied_gates_reports_every_failing_gate_and_agrees_with_readiness() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        let controller = BuildAdmissionController::new_closed(journal, 3, "this-epoch");

        // A freshly closed controller fails recovery, inventory and topology
        // at once. `readiness()` names only the first.
        let snapshot = controller.debug_snapshot().await;
        assert_eq!(
            snapshot.health.readiness.as_str(),
            "journal_recovery_incomplete"
        );
        assert_eq!(
            snapshot.unsatisfied_gates,
            vec![
                "journal_recovery_incomplete",
                "inventory_pending",
                "topology_pending"
            ],
            "three gates are closed; reporting only the first hides two of them"
        );
        assert_eq!(
            snapshot.unsatisfied_gates.first().copied(),
            Some(snapshot.health.readiness.as_str()),
            "the ladders must not drift: the first unsatisfied gate IS the readiness"
        );

        // Clearing the first leaves the other two, and the readiness advances.
        controller.journal_recovered.store(true, Ordering::Release);
        let snapshot = controller.debug_snapshot().await;
        assert_eq!(snapshot.health.readiness.as_str(), "inventory_pending");
        assert_eq!(
            snapshot.unsatisfied_gates,
            vec!["inventory_pending", "topology_pending"]
        );
        assert_eq!(
            snapshot.unsatisfied_gates.first().copied(),
            Some(snapshot.health.readiness.as_str())
        );
    }

    #[tokio::test]
    async fn enforce_recovery_alone_stays_closed_until_inventory_and_topology_complete() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 1, "replacement-epoch");
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::JournalRecoveryIncomplete,
            "Enforce starts fail-closed with the journal-recovery-incomplete gate"
        );
        assert!(matches!(
            WarmAdmission::admit(&controller, warm("denied-before-recovery")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(report.retired_reserved, 0);
        assert_eq!(report.marked_create_unknown, 0);
        assert_eq!(report.seeded_rows, 0);
        // Journal recovery alone must NOT mark Enforce healthy: even with an
        // empty journal the inventory gate keeps admission fail-closed until
        // the real Kubernetes inventory completes.
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::InventoryPending,
            "journal recovery advances to inventory-pending, never straight to healthy"
        );
        assert!(!controller.is_ready());
        assert!(
            matches!(
                WarmAdmission::admit(&controller, warm("denied-before-inventory")).await,
                Err(WarmAdmissionError::Denied { .. })
            ),
            "admission stays fail-closed while the inventory gate is pending"
        );

        controller.mark_inventory_ready();
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::TopologyPending,
            "completed inventory advances the gate to topology-pending"
        );
        assert!(
            matches!(
                WarmAdmission::admit(&controller, warm("denied-before-topology")).await,
                Err(WarmAdmissionError::Denied { .. })
            ),
            "admission stays fail-closed while the topology gate is pending"
        );

        controller.mark_topology_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
        assert!(controller.is_ready());
        assert!(
            WarmAdmission::admit(&controller, warm("after-all-gates"))
                .await
                .is_ok(),
            "admission opens only after journal + inventory + topology all complete"
        );
    }

    #[tokio::test]
    async fn recovery_retires_predecessor_reserved_and_seeds_occupancy_without_duplicates() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        // Predecessor rows from the old epoch.
        journal
            .reserve(&predecessor_input("reserved", 0, "old-epoch"))
            .await
            .unwrap();
        journal
            .reserve(&predecessor_input("in-flight", 0, "old-epoch"))
            .await
            .unwrap();
        journal
            .reserve(&predecessor_input("unknown", 0, "old-epoch"))
            .await
            .unwrap();
        journal
            .reserve(&predecessor_input("live", 0, "old-epoch"))
            .await
            .unwrap();
        // Mark in-flight and advance the others.
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "in-flight".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-in-flight-0".into(),
            })
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "unknown".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-unknown-0".into(),
            })
            .await
            .unwrap();
        journal
            .mark_create_unknown(&djinn_db::AdmissionJournalKey {
                domain: AdmissionDomain::WarmBuild,
                work_id: "unknown".into(),
                generation: 0,
            })
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "live".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-live-0".into(),
            })
            .await
            .unwrap();
        journal
            .mark_live(&djinn_db::UidFencedAdmissionInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "live".into(),
                    generation: 0,
                },
                object_uid: "uid-live".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            journal.count_task_or_warm_occupancy().await.unwrap(),
            4,
            "four predecessor rows occupy before recovery"
        );

        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 64, "replacement-epoch");
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(report.retired_reserved, 1, "predecessor Reserved retired");
        assert_eq!(
            report.marked_create_unknown, 1,
            "predecessor CreateInFlight converted to CreateUnknown"
        );
        assert_eq!(
            report.seeded_rows, 3,
            "in-flight(now unknown), unknown, and live seeded"
        );
        // The predecessor Reserved row no longer occupies; the converted
        // in-flight row now occupies as CreateUnknown.
        assert_eq!(
            journal.count_task_or_warm_occupancy().await.unwrap(),
            3,
            "retired Reserved releases one slot; CreateUnknown still occupies"
        );
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::CreateUnknownHealth,
            "CreateUnknown rows gate readiness"
        );
        assert!(!controller.is_ready());

        // The seeded permits are addressable through the domain-appropriate
        // recovered-permit lookup: these are WarmBuild rows, so the lookup
        // must key on `AdmissionDomain::WarmBuild` (the task-run accessor
        // deliberately filters to `AdmissionDomain::TaskObservation`).
        let live_permit = controller
            .permit_for_key(AdmissionDomain::WarmBuild, "live", 0)
            .await
            .expect("seeded live warm permit is addressable");
        let mut unknown_permits = Vec::new();
        for work in ["in-flight", "unknown"] {
            unknown_permits.push(
                controller
                    .permit_for_key(AdmissionDomain::WarmBuild, work, 0)
                    .await
                    .expect("seeded CreateUnknown warm permit is addressable"),
            );
        }
        assert!(
            controller.task_run_permit("live", 0).await.is_none(),
            "the task-run accessor must not return warm-build rows"
        );

        // Adopting each recovered CreateUnknown row into Live (authoritative
        // GET/UID proof) clears the CreateUnknown startup gate; readiness then
        // falls through to the still-pending inventory gate.
        for (index, permit) in unknown_permits.iter().enumerate() {
            controller
                .transition(
                    permit,
                    WarmAdmissionTransition::Live {
                        uid: format!("adopted-uid-{index}"),
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::InventoryPending,
            "adopting every CreateUnknown row advances the gate to inventory-pending"
        );
        assert!(!controller.is_ready());
        controller.mark_inventory_ready();
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::TopologyPending
        );
        controller.mark_topology_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);

        // The seeded permits are idempotent: re-admitting the same key returns
        // the seeded permit without consuming a new slot.
        let retry = WarmAdmission::admit(&controller, warm("live"))
            .await
            .unwrap();
        assert_eq!(
            retry, live_permit,
            "re-admission returns the seeded permit without duplicating occupancy"
        );
        assert_eq!(
            journal.count_task_or_warm_occupancy().await.unwrap(),
            3,
            "idempotent re-admission does not add occupancy"
        );
    }

    /// A recovered process that finds MORE capacity occupied than its cap
    /// allows must fail closed until the excess is handed back.
    ///
    /// The gate is fed BUILD SLOTS. It used to be fed a count of occupying
    /// journal rows compared against a build-slot cap — two different units,
    /// and the exact shape of the v0.7.5 wedge where 58 stale lifecycle rows
    /// latched `over_cap` while holding no CPU at all. The assertions below are
    /// unchanged; only the measurement is now real capacity.
    #[tokio::test]
    async fn seeded_occupancy_above_cap_gates_readiness_fail_closed() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        // Two predecessor Live rows under a cap of one.
        for work in ["over-a", "over-b"] {
            journal
                .reserve(&predecessor_input(work, 0, "old-epoch"))
                .await
                .unwrap();
            journal
                .mark_create_started(&djinn_db::CreateStartedInput {
                    key: djinn_db::AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: work.into(),
                        generation: 0,
                    },
                    creator_server_epoch: "old-epoch".into(),
                    object_name: format!("warm-{work}-0"),
                })
                .await
                .unwrap();
            journal
                .mark_live(&djinn_db::UidFencedAdmissionInput {
                    key: djinn_db::AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: work.into(),
                        generation: 0,
                    },
                    object_uid: format!("uid-{work}"),
                })
                .await
                .unwrap();
        }
        let h = crate::build_admission_capacity_support::attach_capacity(
            &db,
            BuildAdmissionController::new_closed(Arc::clone(&journal), 1, "replacement-epoch"),
            1,
        )
        .await;
        let controller = Arc::clone(&h.controller);
        // The predecessor's two warm Jobs each held a graph-warm lease. Those
        // rows live in `build_leases` and survive the restart, which is what
        // makes recovered occupancy exceed the cap of one.
        let predecessor_slots = h.occupy_slots_beyond_cap(2).await;
        assert_eq!(h.occupancy().await, 2);
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(report.seeded_rows, 2);
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::SeededOccupancyAboveCap,
            "seeded occupancy above cap must gate readiness"
        );
        assert!(!controller.is_ready());
        assert!(matches!(
            WarmAdmission::admit(controller.as_ref(), warm("denied-over-cap")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));

        // The predecessor's Jobs end. Slot and ledger row are retired by their
        // own owners: the warmer hands back the lease, the terminal transition
        // closes the journal row. Handing back the capacity is what brings
        // occupancy within the cap; the terminal transition is what re-reads the
        // gate.
        for slot in predecessor_slots {
            h.release_warm_lease(slot).await;
        }
        for (work, uid) in [("over-a", "uid-over-a"), ("over-b", "uid-over-b")] {
            let permit = controller
                .permit_for_key(AdmissionDomain::WarmBuild, work, 0)
                .await
                .expect("seeded over-cap permit is addressable");
            controller
                .transition(
                    &permit,
                    WarmAdmissionTransition::Terminal { uid: uid.into() },
                )
                .await
                .unwrap();
        }
        assert_eq!(h.occupancy().await, 0);
        assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 0);
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::InventoryPending,
            "clearing the over-cap gate does not skip the inventory gate"
        );
        controller.mark_inventory_ready();
        controller.mark_topology_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
        assert!(
            WarmAdmission::admit(controller.as_ref(), warm("after-drain"))
                .await
                .is_ok(),
            "admission opens once occupancy is within the cap and all gates complete"
        );
    }

    #[tokio::test]
    async fn observe_and_off_do_not_gate_admission_on_readiness() {
        // Observe records degradation but never denies; the readiness value is
        // inspectable for telemetry.
        let observe = ungated_controller(BuildAdmissionMode::Observe, 1);
        observe.mark_journal_unhealthy();
        assert_eq!(
            observe.readiness(),
            BuildAdmissionReadiness::JournalUnhealthy
        );
        assert!(
            WarmAdmission::admit(&observe, warm("observe-degraded"))
                .await
                .is_ok(),
            "Observe must not deny even when readiness is degraded"
        );

        // Off has no readiness coupling and never touches the journal.
        let off = ungated_controller(BuildAdmissionMode::Off, 0);
        off.mark_inventory_pending();
        assert!(
            WarmAdmission::admit(&off, warm("off-uncoupled"))
                .await
                .is_ok(),
            "Off has no readiness coupling"
        );
    }

    #[tokio::test]
    async fn shutdown_draining_blocks_new_enforce_reservations() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 1);
        // A ready controller that begins draining must block every new
        // reservation, regardless of prior occupancy. The drain gate is checked
        // before any journal reservation, so this is independent of DB state.
        controller.mark_ready();
        assert!(controller.is_ready());
        controller.begin_draining();
        assert!(controller.is_draining());
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::ShutdownDraining
        );
        assert!(
            matches!(
                WarmAdmission::admit(&controller, warm("during-drain")).await,
                Err(WarmAdmissionError::Denied { .. })
            ),
            "draining blocks new Enforce reservations"
        );
    }

    #[test]
    fn allocate_server_epoch_is_unique() {
        let a = allocate_server_epoch();
        let b = allocate_server_epoch();
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        assert_ne!(a, b, "each allocated epoch is unique");
    }

    // ── Build-admission telemetry regression tests ──────────────────────
    //
    // Each of these holds `telemetry_guard()` for its whole assertion window
    // and opts its controller into publishing via
    // `enable_process_metrics_for_test`, so exactly one controller is writing
    // the process-global admission series at a time.

    fn sample_value(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
        if matches!(
            metric,
            "djinn_build_slots_in_use" | "djinn_build_slots_queued"
        ) {
            let line = rendered
                .lines()
                .find(|line| line.starts_with(metric) && !line.contains("{"))
                .unwrap_or_else(|| panic!("missing unlabelled sample {metric} in:\n{rendered}"));
            return line
                .rsplit_once(' ')
                .and_then(|(_, value)| value.parse::<f64>().ok())
                .unwrap_or_else(|| panic!("sample should end with a number: {line}"));
        }
        let line = rendered
            .lines()
            .find(|l| {
                l.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(k, v)| l.contains(&format!("{k}=\"{v}\"")))
            })
            .unwrap_or_else(|| panic!("missing sample {metric}{labels:?} in:\n{rendered}"));
        line.rsplit_once(' ')
            .and_then(|(_, v)| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("sample should end with a number: {line}"))
    }

    fn assert_no_identity_labels(rendered: &str, metric: &str) {
        for forbidden in ["work_id=", "uid=", "epoch=", "task_id=", "session_id="] {
            for line in rendered.lines() {
                if line.starts_with(metric) {
                    assert!(
                        !line.contains(forbidden),
                        "{metric} must not carry identity label {forbidden}: {line}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn telemetry_occupied_gauge_refreshes_on_normal_lifecycle() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        // Starting from zero: the first successfully reserved task must
        // immediately refresh the occupied gauge — it must not wait for a
        // later cap denial or terminal release.
        let c = ungated_controller(BuildAdmissionMode::Enforce, 3);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        c.admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            "task-occ".into(),
            1,
            "task-job".into(),
        )
        .await
        .unwrap();

        let rendered = djinn_telemetry::render().unwrap();
        let occupied = sample_value(
            &rendered,
            "djinn_build_slots_in_use",
            &[("effective_mode", "enforce"), ("effective_cap", "3")],
        );
        assert_eq!(
            occupied, 1.0,
            "occupied gauge must reflect the newly reserved row immediately"
        );
        assert_no_identity_labels(&rendered, "djinn_build_slots_in_use");
    }

    #[tokio::test]
    async fn telemetry_occupied_gauge_deduplicates_recovered_and_adopted_rows() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        // Seed one predecessor Live row, then adopt that same durable identity.
        for work in ["rec-a"] {
            journal
                .reserve(&predecessor_input(work, 0, "old-epoch"))
                .await
                .unwrap();
            journal
                .mark_create_started(&djinn_db::CreateStartedInput {
                    key: djinn_db::AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: work.into(),
                        generation: 0,
                    },
                    creator_server_epoch: "old-epoch".into(),
                    object_name: format!("warm-{work}-0"),
                })
                .await
                .unwrap();
            journal
                .mark_live(&djinn_db::UidFencedAdmissionInput {
                    key: djinn_db::AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: work.into(),
                        generation: 0,
                    },
                    object_uid: format!("uid-{work}"),
                })
                .await
                .unwrap();
        }
        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 64, "replacement-epoch");
        controller.enable_process_metrics_for_test();
        controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        journal
            .adopt_live(&djinn_db::AdoptLiveAdmissionInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "rec-a".into(),
                    generation: 0,
                },
                workload_kind: AdmissionWorkloadKind::Warm,
                creator_server_epoch: "replacement-epoch".into(),
                object_name: "warm-rec-a-0".into(),
                object_uid: "uid-rec-a".into(),
            })
            .await
            .unwrap();
        controller.publish_metrics().await;

        let rendered = djinn_telemetry::render().unwrap();
        let occupied = sample_value(
            &rendered,
            "djinn_build_slots_in_use",
            &[("effective_mode", "enforce"), ("effective_cap", "64")],
        );
        assert_eq!(
            occupied, 1.0,
            "recovery and adoption of one identity must export one occupied slot"
        );
    }

    #[tokio::test]
    async fn telemetry_off_mode_reports_no_occupancy() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let c = ungated_controller(BuildAdmissionMode::Off, 0);
        c.enable_process_metrics_for_test();
        c.publish_metrics().await;

        let rendered = djinn_telemetry::render().unwrap();
        let occupied = sample_value(
            &rendered,
            "djinn_build_slots_in_use",
            &[("effective_mode", "off"), ("effective_cap", "0")],
        );
        assert_eq!(occupied, 0.0, "Off must report zero occupancy");
        let queued = sample_value(
            &rendered,
            "djinn_build_slots_queued",
            &[("effective_mode", "off"), ("effective_cap", "0")],
        );
        assert_eq!(queued, 0.0, "Off must report zero queued");
    }

    #[tokio::test]
    async fn telemetry_unknown_classification_counter_increments() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let c = ungated_controller(BuildAdmissionMode::Enforce, 3);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        // A NonBuild request with an empty audit reason triggers unknown
        // classification.
        let decision = c
            .admit(BuildAdmissionRequest {
                domain: AdmissionDomain::TaskObservation,
                work_id: "unknown".into(),
                generation: 0,
                object_name: "obj".into(),
                kind: BuildWorkloadKind::NonBuild { audit_reason: "" },
                capacity: CapacitySource::ZeroWeight {
                    audit_reason: "unclassified probe",
                },
            })
            .await
            .unwrap();
        assert_eq!(decision, BuildAdmissionDecision::Unclassified);

        let rendered = djinn_telemetry::render().unwrap();
        let value = sample_value(
            &rendered,
            "djinn_build_admission_unknown_classification_total",
            &[("effective_mode", "enforce"), ("effective_cap", "3")],
        );
        assert!(
            value >= 1.0,
            "unknown-classification counter must increment on unclassified admission"
        );
        assert_no_identity_labels(
            &rendered,
            "djinn_build_admission_unknown_classification_total",
        );
    }

    /// AC4 (ymx9): accepted and rejected journal transitions are separate
    /// bounded series, so a 100% rejection rate is a ratio an alert can read
    /// instead of a log volume an operator has to notice.
    #[tokio::test]
    async fn telemetry_transition_counter_separates_accepted_from_rejected() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let c = ungated_controller(BuildAdmissionMode::Observe, 4);
        c.enable_process_metrics_for_test();
        let permit = WarmAdmission::admit(&c, warm("counted")).await.unwrap();
        c.transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        c.transition(&permit, WarmAdmissionTransition::Live { uid: "uid".into() })
            .await
            .unwrap();
        // Observe never denies dispatch, so this rejection is only visible as
        // a counter and a WARN — never as a changed decision.
        c.transition(
            &permit,
            WarmAdmissionTransition::Terminal {
                uid: "other-uid".into(),
            },
        )
        .await
        .expect("Observe does not turn a rejected transition into a denial");

        let rendered = djinn_telemetry::render().unwrap();
        let labels = |outcome| {
            [
                ("effective_mode", "observe"),
                ("effective_cap", "4"),
                ("outcome", outcome),
            ]
        };
        assert!(
            sample_value(
                &rendered,
                "djinn_build_admission_transition_total",
                &labels("accepted")
            ) >= 2.0
        );
        assert!(
            sample_value(
                &rendered,
                "djinn_build_admission_transition_total",
                &labels("rejected")
            ) >= 1.0
        );
        assert_no_identity_labels(&rendered, "djinn_build_admission_transition_total");
        assert_eq!(c.rejected_transition_count(), 1);
        assert!(
            c.last_transition_rejection()
                .await
                .is_some_and(|reason| reason.contains("Kubernetes UID does not match"))
        );
    }

    #[tokio::test]
    async fn telemetry_would_defer_counter_increments_in_observe() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let h = capacity_harness(BuildAdmissionMode::Observe, 1).await;
        h.controller.mark_ready();
        h.controller.enable_process_metrics_for_test();
        // A warm Job holds the only slot, and layer-1 dispatch is still in
        // shadow: the pool is genuinely full, so the probe reports what
        // enforcement WOULD have done while permitting anyway.
        let _held = h.hold_warm_lease("observe-occupant").await.unwrap();
        h.lease.set_dispatch_enforcing_for_test(false);
        let _ = h
            .controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "would-defer".into(),
                0,
                "would-defer-job".into(),
            )
            .await
            .unwrap();

        let rendered = djinn_telemetry::render().unwrap();
        let value = sample_value(
            &rendered,
            "djinn_build_admission_would_defer_total",
            &[("effective_mode", "observe"), ("effective_cap", "1")],
        );
        assert!(
            value >= 1.0,
            "would-defer counter must increment when Observe sees a would-defer"
        );
    }

    struct StubDiskSource {
        observation: Option<crate::disk_admission::DiskObservation>,
    }

    #[async_trait]
    impl DiskCapacitySource for StubDiskSource {
        async fn observe(
            &self,
            _request: &BuildAdmissionRequest,
        ) -> Option<crate::disk_admission::DiskObservation> {
            self.observation
        }
    }

    #[tokio::test]
    async fn disk_dimension_records_would_defer_without_denial() {
        use crate::disk_admission::{DiskObservation, DiskQueueReason};
        let c = ungated_controller(BuildAdmissionMode::Observe, 4);
        c.set_disk_capacity_source(Arc::new(StubDiskSource {
            observation: Some(DiskObservation {
                would_defer: Some(DiskQueueReason::DiskPressure),
                projected_reservation_bytes: 8_589_934_592,
            }),
        }));
        // The build is still permitted — the disk dimension never denies.
        let permit = WarmAdmission::admit(&c, warm("disk-a")).await;
        assert!(permit.is_ok(), "observe-only disk dimension must not deny");
        assert_eq!(c.disk_would_defer_observation_count().await, 1);
    }

    #[tokio::test]
    async fn disk_dimension_silent_when_source_grants_or_has_no_sample() {
        use crate::disk_admission::DiskObservation;
        let granting = ungated_controller(BuildAdmissionMode::Observe, 4);
        granting.set_disk_capacity_source(Arc::new(StubDiskSource {
            observation: Some(DiskObservation {
                would_defer: None,
                projected_reservation_bytes: 0,
            }),
        }));
        WarmAdmission::admit(&granting, warm("disk-grant"))
            .await
            .unwrap();
        assert_eq!(granting.disk_would_defer_observation_count().await, 0);

        let no_sample = ungated_controller(BuildAdmissionMode::Observe, 4);
        no_sample.set_disk_capacity_source(Arc::new(StubDiskSource { observation: None }));
        WarmAdmission::admit(&no_sample, warm("disk-none"))
            .await
            .unwrap();
        assert_eq!(no_sample.disk_would_defer_observation_count().await, 0);
    }

    #[tokio::test]
    async fn disk_dimension_is_dark_without_a_source() {
        let c = ungated_controller(BuildAdmissionMode::Observe, 4);
        WarmAdmission::admit(&c, warm("dark")).await.unwrap();
        assert_eq!(c.disk_would_defer_observation_count().await, 0);
    }

    #[tokio::test]
    async fn telemetry_queued_gauge_increments_on_enforce_deny() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        // A denial now comes from the ONE capacity authority, so this drives a
        // real recovered lease behind the controller. Denying through the
        // journal is no longer possible -- it is the lifecycle ledger and has
        // no cap -- and a controller with no authority is simply not capacity
        // gated, which is why this test must compose one to observe a denial.
        let db = Database::open_in_memory().unwrap();
        let leases = Arc::new(djinn_db::BuildLeaseRepository::new(db.clone()));
        let lease = Arc::new(crate::build_lease::BuildLeaseService::new(
            Arc::clone(&leases),
            1,
        ));
        assert!(matches!(
            lease.recover().await,
            djinn_supervisor::services::LeaseResult::Status(_)
        ));
        assert!(matches!(
            lease.set_cap(1).await,
            djinn_supervisor::services::LeaseResult::Status(_)
        ));
        lease.set_dispatch_enforcing_for_test(true);
        let authority: Arc<dyn BuildSlotAuthority> = Arc::new(
            crate::build_slot_authority::BuildLeaseDispatchAuthority::new(Arc::clone(&lease)),
        );
        let c = BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(db)),
            BuildAdmissionMode::Enforce,
            1,
            "epoch",
        )
        .with_slot_authority(authority);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        // Fill the single slot with a real dispatch reservation.
        let filled = c
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "queued-a".into(),
                0,
                "djinn-taskrun-queued-a".into(),
            )
            .await
            .unwrap();
        assert!(matches!(filled, BuildAdmissionDecision::Permitted { .. }));
        // The second is denied — it becomes a deferred Enforce identity.
        let denied = c
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "queued-b".into(),
                0,
                "djinn-taskrun-queued-b".into(),
            )
            .await
            .unwrap();
        assert!(
            matches!(denied, BuildAdmissionDecision::Denied { .. }),
            "the second build-capable dispatch must be denied at cap 1"
        );

        let rendered = djinn_telemetry::render().unwrap();
        let queued = sample_value(
            &rendered,
            "djinn_build_slots_queued",
            &[("effective_mode", "enforce"), ("effective_cap", "1")],
        );
        assert_eq!(
            queued, 1.0,
            "queued gauge must reflect the Enforce-deferred identity"
        );
    }

    #[tokio::test]
    async fn telemetry_queue_tracks_unique_waiters_until_each_reenters() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();
        // Use a fake clock pinned at t=0 so the admitted observation this test
        // emits contributes a deterministic 0.0s to the shared histogram rather
        // than a non-deterministic real-time gap that pollutes sibling tests
        // asserting exact histogram sums.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let h = crate::build_admission_capacity_support::attach_capacity(
            &db,
            BuildAdmissionController::new_with_queue_clock(
                Arc::new(AdmissionJournalRepository::new(db.clone())),
                BuildAdmissionMode::Enforce,
                1,
                "queue-tracks",
                Arc::new(FakeQueueClock {
                    base: Instant::now(),
                    elapsed_seconds: AtomicU64::new(0),
                }),
            ),
            1,
        )
        .await;
        let c = Arc::clone(&h.controller);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        let first = dispatch_permit(&c, "release-a").await;
        assert!(dispatch_denied(&c, "release-b").await);
        assert!(dispatch_denied(&c, "release-b").await);
        assert!(dispatch_denied(&c, "release-c").await);
        assert_eq!(
            sample_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_build_slots_queued",
                &[]
            ),
            2.0
        );
        c.transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        c.transition(&first, WarmAdmissionTransition::Live { uid: "uid".into() })
            .await
            .unwrap();
        c.transition(
            &first,
            WarmAdmissionTransition::Terminal { uid: "uid".into() },
        )
        .await
        .unwrap();
        assert_eq!(
            sample_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_build_slots_queued",
                &[]
            ),
            2.0
        );
        // The released slot went to the FIFO head, which is `release-b`; its
        // retry observes the grant it already holds and leaves the queue.
        let _ = dispatch_permit(&c, "release-b").await;
        assert_eq!(
            sample_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_build_slots_queued",
                &[]
            ),
            1.0
        );
    }

    #[tokio::test]
    async fn telemetry_create_unknown_health_resolves_on_adoption() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        // Seed a predecessor CreateInFlight row (will become CreateUnknown).
        journal
            .reserve(&predecessor_input("cu", 0, "old-epoch"))
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "cu".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-cu-0".into(),
            })
            .await
            .unwrap();

        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 64, "replacement-epoch");
        controller.enable_process_metrics_for_test();
        controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();

        // After recovery the CreateUnknown health signal must be elevated.
        let rendered = djinn_telemetry::render().unwrap();
        let cu_health = sample_value(
            &rendered,
            "djinn_build_admission_create_unknown_health",
            &[("effective_mode", "enforce"), ("effective_cap", "64")],
        );
        assert_eq!(
            cu_health, 1.0,
            "CreateUnknown health must be elevated after recovery"
        );

        // Adopting the CreateUnknown row into Live must immediately refresh
        // the health signal — without waiting for a terminal event.
        let permit = controller
            .permit_for_key(AdmissionDomain::WarmBuild, "cu", 0)
            .await
            .expect("seeded CreateUnknown permit is addressable");
        controller
            .transition(
                &permit,
                WarmAdmissionTransition::Live {
                    uid: "adopted".into(),
                },
            )
            .await
            .unwrap();

        let rendered = djinn_telemetry::render().unwrap();
        let cu_health = sample_value(
            &rendered,
            "djinn_build_admission_create_unknown_health",
            &[("effective_mode", "enforce"), ("effective_cap", "64")],
        );
        assert_eq!(
            cu_health, 0.0,
            "CreateUnknown health must clear immediately after adoption into Live"
        );
    }

    #[tokio::test]
    async fn telemetry_inventory_degraded_surfaces_on_pending_inventory() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        // Use a non-closed controller (journal is healthy by default) and
        // simulate the post-recovery state where inventory is still pending.
        let c = ungated_controller(BuildAdmissionMode::Enforce, 3);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        c.mark_inventory_pending();
        c.publish_metrics().await;

        let rendered = djinn_telemetry::render().unwrap();
        let inv_degraded = sample_value(
            &rendered,
            "djinn_build_admission_inventory_degraded",
            &[("effective_mode", "enforce"), ("effective_cap", "3")],
        );
        assert_eq!(
            inv_degraded, 1.0,
            "inventory_degraded must be elevated while inventory gate is pending"
        );

        // Completing inventory must clear the degraded signal.
        c.mark_inventory_ready();
        c.publish_metrics().await;
        let rendered = djinn_telemetry::render().unwrap();
        let inv_degraded = sample_value(
            &rendered,
            "djinn_build_admission_inventory_degraded",
            &[("effective_mode", "enforce"), ("effective_cap", "3")],
        );
        assert_eq!(
            inv_degraded, 0.0,
            "inventory_degraded must clear after inventory completes"
        );
    }

    #[tokio::test]
    async fn telemetry_journal_degraded_surfaces_on_unhealthy_journal() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let c = ungated_controller(BuildAdmissionMode::Enforce, 3);
        c.enable_process_metrics_for_test();
        c.mark_journal_unhealthy();
        c.publish_metrics().await;

        let rendered = djinn_telemetry::render().unwrap();
        let journal_degraded = sample_value(
            &rendered,
            "djinn_build_admission_journal_degraded",
            &[("effective_mode", "enforce"), ("effective_cap", "3")],
        );
        assert_eq!(
            journal_degraded, 1.0,
            "journal_degraded must be elevated when journal health is marked unhealthy"
        );
        assert_no_identity_labels(&rendered, "djinn_build_admission_journal_degraded");
    }

    #[tokio::test]
    async fn telemetry_invocation_build_excluded_from_occupied() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let c = ungated_controller(BuildAdmissionMode::Enforce, 3);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        // Reserve an InvocationBuild row — it must not appear in occupied.
        let _ = c
            .admit(BuildAdmissionRequest {
                domain: AdmissionDomain::InvocationBuild,
                work_id: "inv".into(),
                generation: 0,
                object_name: "inv-job".into(),
                kind: BuildWorkloadKind::TaskRun {
                    role: TaskRunRole::Worker,
                },
                capacity: CapacitySource::HeldByLease,
            })
            .await
            .unwrap();
        c.publish_metrics().await;

        let rendered = djinn_telemetry::render().unwrap();
        let occupied = sample_value(
            &rendered,
            "djinn_build_slots_in_use",
            &[("effective_mode", "enforce"), ("effective_cap", "3")],
        );
        assert_eq!(
            occupied, 0.0,
            "InvocationBuild rows must be excluded from v0 occupied"
        );
    }

    #[tokio::test]
    async fn telemetry_task_and_warm_share_combined_cap() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let h = capacity_harness(BuildAdmissionMode::Enforce, 1).await;
        let c = Arc::clone(&h.controller);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        // Reserve a task — occupies 1.
        let task = c
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task-cap".into(),
                1,
                "task-job".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: task, .. } = task else {
            panic!("task must win the cap-one reservation");
        };
        // Warm must be refused — the combined cap is exhausted by the task. The
        // refusal is at the graph-warm lease, which is where a warm Job's
        // capacity is decided; its admission call is a ledger append.
        assert!(
            h.hold_warm_lease("warm-cap").await.is_none(),
            "warm must be refused when the task consumes the combined cap"
        );

        let rendered = djinn_telemetry::render().unwrap();
        let occupied = sample_value(
            &rendered,
            "djinn_build_slots_in_use",
            &[("effective_mode", "enforce"), ("effective_cap", "1")],
        );
        assert_eq!(
            occupied, 1.0,
            "combined cap occupied reflects the task reservation"
        );

        // Releasing the task frees the combined cap.
        c.transition(&task, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        c.transition(
            &task,
            WarmAdmissionTransition::Live {
                uid: "t-uid".into(),
            },
        )
        .await
        .unwrap();
        c.transition(
            &task,
            WarmAdmissionTransition::Terminal {
                uid: "t-uid".into(),
            },
        )
        .await
        .unwrap();
        let rendered = djinn_telemetry::render().unwrap();
        let occupied = sample_value(
            &rendered,
            "djinn_build_slots_in_use",
            &[("effective_mode", "enforce"), ("effective_cap", "1")],
        );
        assert_eq!(
            occupied, 0.0,
            "combined cap must be zero after task terminal release"
        );
    }

    #[tokio::test]
    async fn telemetry_observe_journal_outage_surfaces_journal_degraded() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        db.pool().close().await;
        let c = BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(db)),
            BuildAdmissionMode::Observe,
            1,
            "epoch",
        );
        c.enable_process_metrics_for_test();

        // An Observe journal failure must permit dispatch (telemetry-only)
        // but must surface the degradation.
        assert!(
            WarmAdmission::admit(&c, warm("journal-down")).await.is_ok(),
            "Observe journal failures are telemetry-only and must not defer dispatch"
        );

        let rendered = djinn_telemetry::render().unwrap();
        let journal_degraded = sample_value(
            &rendered,
            "djinn_build_admission_journal_degraded",
            &[("effective_mode", "observe"), ("effective_cap", "1")],
        );
        assert_eq!(
            journal_degraded, 1.0,
            "Observe must surface journal degradation on a live journal outage"
        );
    }

    #[tokio::test]
    async fn telemetry_observe_transition_failure_is_non_denying_and_degraded() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();
        let db = Database::open_in_memory().unwrap();
        let c = BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(db.clone())),
            BuildAdmissionMode::Observe,
            1,
            "transition-failure",
        );
        c.enable_process_metrics_for_test();
        let permit = WarmAdmission::admit(&c, warm("transition-down"))
            .await
            .unwrap();
        reject_admission_create_started_for_test(&db, true).await;
        assert!(
            c.transition(&permit, WarmAdmissionTransition::CreateStarted)
                .await
                .is_ok()
        );
        assert_eq!(
            sample_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_build_admission_journal_degraded",
                &[("effective_mode", "observe"), ("effective_cap", "1")],
            ),
            1.0
        );
        reject_admission_create_started_for_test(&db, false).await;
    }

    #[tokio::test]
    async fn telemetry_all_health_labels_are_bounded() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let c = ungated_controller(BuildAdmissionMode::Enforce, 5);
        c.enable_process_metrics_for_test();
        c.mark_ready();
        c.publish_metrics().await;

        let rendered = djinn_telemetry::render().unwrap();
        for metric in [
            "djinn_build_admission_inventory_degraded",
            "djinn_build_admission_journal_degraded",
            "djinn_build_admission_create_unknown_health",
            "djinn_build_admission_occupancy_over_cap",
            "djinn_build_admission_stale_rows",
            "djinn_build_slots_in_use",
            "djinn_build_slots_queued",
        ] {
            assert_no_identity_labels(&rendered, metric);
        }
    }

    /// Occupancy above the cap must be readable as a bounded gauge, and
    /// reclamation must report through the same `outcome` family the lifecycle
    /// transitions report through — not a parallel metric nobody queries.
    ///
    /// The gauge tracks BUILD SLOTS, from the one authority. It used to track
    /// occupying journal rows against the cap, which was wrong in both
    /// directions once capacity moved: stale lifecycle rows raised an alarm
    /// claiming every admission would be denied while denying nothing, and
    /// exhausted slots raised no alarm whenever the journal sat within the cap.
    #[tokio::test]
    async fn telemetry_reports_over_cap_occupancy_and_reclamation_outcomes() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();

        let h = capacity_harness(BuildAdmissionMode::Enforce, 2).await;
        let controller = Arc::clone(&h.controller);
        controller.enable_process_metrics_for_test();
        controller.mark_ready();
        let labels = [("effective_mode", "enforce"), ("effective_cap", "2")];

        controller.publish_metrics().await;
        assert_eq!(
            sample_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_build_admission_occupancy_over_cap",
                &labels
            ),
            0.0
        );

        // A stale lifecycle population alone is NOT over cap: it holds no CPU.
        // This is the v0.7.5 false alarm, and it must stay silent.
        for index in 0..3 {
            controller
                .journal()
                .adopt_live(&djinn_db::AdoptLiveAdmissionInput {
                    key: AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: format!("stale-{index}"),
                        generation: 0,
                    },
                    workload_kind: AdmissionWorkloadKind::Warm,
                    creator_server_epoch: "predecessor".into(),
                    object_name: format!("stale-{index}-job"),
                    object_uid: format!("stale-uid-{index}"),
                })
                .await
                .unwrap();
        }
        controller.publish_metrics().await;
        assert_eq!(
            sample_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_build_admission_occupancy_over_cap",
                &labels
            ),
            0.0,
            "stale lifecycle rows deny nothing and must not raise the alarm"
        );

        // Slots a predecessor really took, surviving in `build_leases` past a
        // cap the operator has since lowered. THIS denies every admission.
        let _predecessor_slots = h.occupy_slots_beyond_cap(3).await;
        controller.publish_metrics().await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(
                &rendered,
                "djinn_build_admission_occupancy_over_cap",
                &labels
            ),
            1.0,
            "occupancy above the cap must be visible without reading a log"
        );

        controller.publish_reconciliation(7, 5, 2);
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_admission_stale_rows", &labels),
            7.0
        );
        assert_eq!(
            sample_value(
                &rendered,
                "djinn_build_admission_transition_total",
                &[
                    ("effective_mode", "enforce"),
                    ("effective_cap", "2"),
                    ("outcome", "reclaimed")
                ]
            ),
            5.0
        );
        assert_eq!(
            sample_value(
                &rendered,
                "djinn_build_admission_transition_total",
                &[
                    ("effective_mode", "enforce"),
                    ("effective_cap", "2"),
                    ("outcome", "reclaim_fenced")
                ]
            ),
            2.0
        );
    }

    struct FakeQueueClock {
        base: Instant,
        elapsed_seconds: AtomicU64,
    }
    impl FakeQueueClock {
        fn advance(&self, seconds: u64) {
            self.elapsed_seconds.store(seconds, Ordering::Release);
        }
    }
    impl QueueClock for FakeQueueClock {
        fn now(&self) -> Instant {
            self.base
                .checked_add(std::time::Duration::from_secs(
                    self.elapsed_seconds.load(Ordering::Acquire),
                ))
                .unwrap()
        }
    }
    fn queue_histogram_value(rendered: &str, outcome: &str, suffix: &str) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(&format!("djinn_build_slot_queue_wait_seconds_{suffix}"))
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .and_then(|line| line.rsplit_once(' '))
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0.0)
    }
    #[tokio::test]
    async fn queue_wait_uses_first_denial_and_observes_each_terminal_once() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();
        let clock = Arc::new(FakeQueueClock {
            base: Instant::now(),
            elapsed_seconds: AtomicU64::new(0),
        });
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let h = crate::build_admission_capacity_support::attach_capacity(
            &db,
            BuildAdmissionController::new_with_queue_clock(
                Arc::new(AdmissionJournalRepository::new(db.clone())),
                BuildAdmissionMode::Enforce,
                1,
                "fake-clock",
                clock.clone(),
            ),
            1,
        )
        .await;
        let controller = Arc::clone(&h.controller);
        controller.enable_process_metrics_for_test();
        controller.mark_ready();
        let occupied = dispatch_permit(&controller, "occupied").await;
        let before_admitted_count =
            queue_histogram_value(&djinn_telemetry::render().unwrap(), "admitted", "count");
        let before_admitted_sum =
            queue_histogram_value(&djinn_telemetry::render().unwrap(), "admitted", "sum");
        assert!(dispatch_denied(&controller, "admitted").await);
        clock.advance(7);
        assert!(dispatch_denied(&controller, "admitted").await);
        controller
            .transition(
                &occupied,
                WarmAdmissionTransition::DefinitiveFailure {
                    diagnostic: "free".into(),
                },
            )
            .await
            .unwrap();
        let _replacement = dispatch_permit(&controller, "admitted").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            queue_histogram_value(&rendered, "admitted", "count") - before_admitted_count,
            1.0
        );
        assert_eq!(
            queue_histogram_value(&rendered, "admitted", "sum") - before_admitted_sum,
            7.0
        );
        let before_cancelled_count = queue_histogram_value(&rendered, "cancelled", "count");
        let before_cancelled_sum = queue_histogram_value(&rendered, "cancelled", "sum");
        assert!(dispatch_denied(&controller, "cancelled").await);
        clock.advance(18);
        controller.cancel_deferred_task("cancelled").await;
        controller.cancel_deferred_task("cancelled").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            queue_histogram_value(&rendered, "cancelled", "count") - before_cancelled_count,
            1.0
        );
        assert_eq!(
            queue_histogram_value(&rendered, "cancelled", "sum") - before_cancelled_sum,
            11.0
        );
        let before_shutdown_count = queue_histogram_value(&rendered, "shutdown", "count");
        let before_shutdown_sum = queue_histogram_value(&rendered, "shutdown", "sum");
        assert!(dispatch_denied(&controller, "shutdown").await);
        clock.advance(31);
        controller.begin_draining();
        controller.begin_draining();
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            queue_histogram_value(&rendered, "shutdown", "count") - before_shutdown_count,
            1.0
        );
        assert_eq!(
            queue_histogram_value(&rendered, "shutdown", "sum") - before_shutdown_sum,
            13.0
        );
    }

    /// Gauges equal disjoint unique state cardinalities after every transition.
    ///
    /// After each lifecycle event we assert both the histogram count AND the
    /// `djinn_build_slots_queued` gauge. Duplicate terminal signals must be
    /// no-ops on the gauge (no double-decrement), and the queued gauge must
    /// always equal the number of unique queued identities.
    #[tokio::test]
    async fn queue_gauges_match_cardinality_after_every_transition() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();
        let clock = Arc::new(FakeQueueClock {
            base: Instant::now(),
            elapsed_seconds: AtomicU64::new(0),
        });
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let h = crate::build_admission_capacity_support::attach_capacity(
            &db,
            BuildAdmissionController::new_with_queue_clock(
                Arc::new(AdmissionJournalRepository::new(db.clone())),
                BuildAdmissionMode::Enforce,
                1,
                "gauge-test",
                clock.clone(),
            ),
            1,
        )
        .await;
        let controller = Arc::clone(&h.controller);
        controller.enable_process_metrics_for_test();
        controller.mark_ready();

        // One slot occupied → in_use=1, queued=0.
        let occupied = dispatch_permit(&controller, "occupied").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_in_use", &[]),
            1.0,
            "in_use gauge must reflect the one admitted identity"
        );
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            0.0,
            "queued gauge must be zero before any denial"
        );

        // Deny "queued-a": one identity enters deferred state.
        assert!(dispatch_denied(&controller, "queued-a").await);
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            1.0,
            "queued gauge must be 1 after first denial"
        );

        // Deny "queued-b": second unique identity enters deferred state.
        assert!(dispatch_denied(&controller, "queued-b").await);
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            2.0,
            "queued gauge must be 2 after two distinct denials"
        );

        // Retry "queued-a" (still denied): gauge stays 2 (reuses the record).
        assert!(dispatch_denied(&controller, "queued-a").await);
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            2.0,
            "retry must not double-count a queued identity"
        );

        // Cancel "queued-a": gauge drops to 1. This is the production hook, and
        // it also surrenders the durable FIFO position — without that, freeing
        // the occupied slot below would GRANT the cancelled identity and leak
        // the slot with nobody left to release it.
        controller.cancel_deferred_task("queued-a").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            1.0,
            "cancel must decrement the queued gauge"
        );

        // Duplicate cancel of "queued-a": gauge stays 1 (idempotent no-op).
        controller.cancel_deferred_task("queued-a").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            1.0,
            "duplicate cancel must be a no-op on the gauge"
        );

        // Free the occupied slot → admit "queued-b": queued drops to 0, in_use stays 1.
        controller
            .transition(
                &occupied,
                WarmAdmissionTransition::DefinitiveFailure {
                    diagnostic: "free".into(),
                },
            )
            .await
            .unwrap();
        let _admitted = dispatch_permit(&controller, "queued-b").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            0.0,
            "admitting the last queued identity must drain the queued gauge"
        );
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_in_use", &[]),
            1.0,
            "in_use must remain 1 after the replacement admit"
        );

        // Shutdown drain with no remaining queued identities: gauge stays 0.
        controller.begin_draining();
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            0.0,
            "shutdown drain with no queued identities must leave gauge at 0"
        );
    }

    /// A task-close cancellation via `cancel_deferred_task` emits exactly one
    /// cancelled observation per queued generation and never orphans state.
    #[tokio::test]
    async fn cancel_deferred_task_terminates_every_queued_generation_once() {
        let _guard = telemetry_guard();
        djinn_telemetry::init().unwrap();
        let clock = Arc::new(FakeQueueClock {
            base: Instant::now(),
            elapsed_seconds: AtomicU64::new(0),
        });
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let h = crate::build_admission_capacity_support::attach_capacity(
            &db,
            BuildAdmissionController::new_with_queue_clock(
                Arc::new(AdmissionJournalRepository::new(db.clone())),
                BuildAdmissionMode::Enforce,
                1,
                "cancel-task-test",
                clock.clone(),
            ),
            1,
        )
        .await;
        let controller = Arc::clone(&h.controller);
        controller.enable_process_metrics_for_test();
        controller.mark_ready();
        let _occupied = dispatch_permit(&controller, "occupied").await;

        // Queue two generations of the same task (task-id "gen-task").
        let gen0 = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "gen-task".into(),
                0,
                "run-0".into(),
            )
            .await
            .unwrap();
        assert!(
            matches!(gen0, BuildAdmissionDecision::Denied { .. }),
            "gen-0 must be denied (cap full): {gen0:?}"
        );
        let gen1 = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "gen-task".into(),
                1,
                "run-1".into(),
            )
            .await
            .unwrap();
        assert!(
            matches!(gen1, BuildAdmissionDecision::Denied { .. }),
            "gen-1 must be denied (cap full): {gen1:?}"
        );
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            2.0,
            "two distinct generations must both be queued"
        );

        let before_cancelled =
            queue_histogram_value(&djinn_telemetry::render().unwrap(), "cancelled", "count");
        clock.advance(5);

        // Task closes: cancel_deferred_task cancels ALL generations of "gen-task".
        controller.cancel_deferred_task("gen-task").await;

        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            0.0,
            "all queued generations must be removed after task-close cancel"
        );
        assert_eq!(
            queue_histogram_value(&rendered, "cancelled", "count") - before_cancelled,
            2.0,
            "both generations must emit exactly one cancelled observation each"
        );

        // Duplicate task-close cancel: no-op.
        controller.cancel_deferred_task("gen-task").await;
        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            sample_value(&rendered, "djinn_build_slots_queued", &[]),
            0.0,
            "duplicate cancel must not change the gauge"
        );
        assert_eq!(
            queue_histogram_value(&rendered, "cancelled", "count") - before_cancelled,
            2.0,
            "duplicate cancel must not emit additional observations"
        );
    }

    // ─── Recovery-machinery hardening (2026-07-29 five-hour board outage) ───

    /// Fake capacity authority whose occupancy is settable, so the over-cap
    /// gate can be driven in both directions without a lease service.
    struct SettableAuthority {
        occupancy: std::sync::Mutex<Option<i64>>,
        cap: i64,
    }

    impl SettableAuthority {
        fn new(occupancy: Option<i64>, cap: i64) -> Self {
            Self {
                occupancy: std::sync::Mutex::new(occupancy),
                cap,
            }
        }

        fn set(&self, occupancy: Option<i64>) {
            *self.occupancy.lock().unwrap() = occupancy;
        }
    }

    #[async_trait]
    impl BuildSlotAuthority for SettableAuthority {
        async fn acquire_dispatch_slot(
            &self,
            _task_id: &str,
            _generation: i64,
        ) -> DispatchSlotOutcome {
            DispatchSlotOutcome::Granted
        }
        async fn release_dispatch_slot(&self, _task_id: &str, _generation: i64) {}
        async fn abandon_queued_dispatch(&self, _task_id: &str) {}
        async fn occupancy(&self) -> Option<i64> {
            *self.occupancy.lock().unwrap()
        }
        fn cap(&self) -> i64 {
            self.cap
        }
    }

    fn over_cap_controller(authority: Arc<SettableAuthority>) -> BuildAdmissionController {
        BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(
                Database::open_in_memory().unwrap(),
            )),
            BuildAdmissionMode::Enforce,
            3,
            "epoch",
        )
        .with_slot_authority(authority)
    }

    /// L7. The over-cap latch's only downward edge was a durable terminal
    /// transition — and a total denial admits nothing, so nothing ever goes
    /// terminal, so the gate that caused the denial could never fall. Meanwhile
    /// `publish_metrics` computed the correct current value on every single
    /// pass and threw it away into a gauge.
    #[tokio::test]
    async fn the_over_cap_gate_falls_on_its_own_once_occupancy_is_back_within_the_cap() {
        let authority = Arc::new(SettableAuthority::new(Some(9), 3));
        let controller = over_cap_controller(Arc::clone(&authority));
        controller.mark_ready();

        // Latch the gate the way recovery does: a known occupancy above the cap.
        controller.publish_metrics().await;
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::SeededOccupancyAboveCap,
            "a known occupancy above the cap must shut admission"
        );

        // Occupancy comes back within the cap. No terminal transition happens,
        // because a denied board produces none.
        authority.set(Some(1));
        controller.publish_metrics().await;
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::Healthy,
            "the over-cap gate must be able to fall without the terminal traffic it is blocking"
        );
    }

    /// An authority that cannot be read is not evidence of anything, in either
    /// direction: it must not clear a latched gate and it must not raise one.
    #[tokio::test]
    async fn an_unreadable_authority_leaves_the_over_cap_gate_exactly_as_it_was() {
        let authority = Arc::new(SettableAuthority::new(Some(9), 3));
        let controller = over_cap_controller(Arc::clone(&authority));
        controller.mark_ready();
        controller.publish_metrics().await;
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::SeededOccupancyAboveCap
        );

        authority.set(None);
        controller.publish_metrics().await;
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::SeededOccupancyAboveCap,
            "an unreadable authority must not clear a latched over-cap gate"
        );

        let healthy_authority = Arc::new(SettableAuthority::new(None, 3));
        let healthy = over_cap_controller(Arc::clone(&healthy_authority));
        healthy.mark_ready();
        healthy.publish_metrics().await;
        assert_eq!(
            healthy.readiness(),
            BuildAdmissionReadiness::Healthy,
            "an unreadable authority must not invent an over-cap denial either"
        );
    }

    /// L6. `begin_draining` stored `true` and NOTHING anywhere stored `false`.
    #[tokio::test]
    async fn the_shutdown_draining_latch_can_be_cleared() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 4);
        controller.mark_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);

        controller.begin_draining();
        assert!(controller.is_draining());
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::ShutdownDraining
        );

        controller.end_draining();
        assert!(!controller.is_draining());
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::Healthy,
            "a drain entered outside teardown must be recoverable in-process"
        );
        // Idempotent.
        controller.end_draining();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
    }

    /// `readiness()` checks the draining latch ahead of every gate `mark_ready`
    /// satisfies, so leaving it set made `mark_ready` a method that provably did
    /// not make the controller ready.
    #[tokio::test]
    async fn mark_ready_clears_the_draining_latch() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 4);
        controller.begin_draining();
        controller.mark_ready();
        assert!(!controller.is_draining());
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
    }

    /// Emergency promotion of a possibly-live process must NOT talk a genuinely
    /// terminating process back into admitting work.
    #[tokio::test]
    async fn require_enforcement_does_not_clear_the_draining_latch() {
        let controller = ungated_controller(BuildAdmissionMode::Observe, 4);
        controller.begin_draining();
        controller.require_enforcement();
        assert!(
            controller.is_draining(),
            "a handoff tick landing during teardown must not reopen admission"
        );
    }

    /// L4. Nothing anywhere asserted "a reconcile pass completed within the last
    /// N seconds", and that missing assertion is what turned a five-minute event
    /// into a five-hour one.
    #[tokio::test]
    async fn the_last_successful_reconcile_age_is_readable() {
        let controller = ungated_controller(BuildAdmissionMode::Enforce, 4);
        assert_eq!(
            controller.seconds_since_last_reconcile(),
            None,
            "a process that has never reconciled must say so, not report age zero"
        );
        assert_eq!(controller.last_reconcile_success_unix(), None);

        let now = ::time::OffsetDateTime::now_utc().unix_timestamp();
        controller.note_reconcile_success_at(now - 900);
        assert_eq!(controller.last_reconcile_success_unix(), Some(now - 900));
        let age = controller
            .seconds_since_last_reconcile()
            .expect("a completed pass has an age");
        assert!(
            (890..=910).contains(&age),
            "the reported age must be real elapsed wall time, got {age}"
        );

        controller.note_reconcile_success();
        let fresh = controller
            .seconds_since_last_reconcile()
            .expect("a completed pass has an age");
        assert!(
            fresh <= 2,
            "a just-completed pass must report ~0s, got {fresh}"
        );
    }

    /// Counts alone are what cost five hours: an operator could see that ONE row
    /// was denying every dispatch and had no non-SQL way to learn which row.
    #[tokio::test]
    async fn a_wedging_create_unknown_row_is_named_not_just_counted() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        journal
            .reserve(&predecessor_input("wedger", 7, "old-epoch"))
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "wedger".into(),
                    generation: 7,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-wedger-7".into(),
            })
            .await
            .unwrap();

        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 4, "replacement-epoch");
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::CreateUnknownHealth,
            "an orphaned CreateUnknown row shuts admission for the whole board"
        );

        let health = controller.health_report();
        assert_eq!(
            health.readiness,
            BuildAdmissionReadiness::CreateUnknownHealth
        );
        assert_eq!(health.create_unknown_pending, 1);
        assert_eq!(
            health.blocking_identities,
            vec!["warm_build:wedger:7@warm-wedger-7".to_owned()],
            "the report must name the row, not only count it"
        );
        assert_eq!(health.blocking_identities_elided, 0);
    }

    /// The named set and the count must never disagree about whether the gate is
    /// still held, or the report would keep accusing an already-adopted row.
    #[tokio::test]
    async fn adopting_the_row_into_live_drops_its_name_and_its_count_together() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        journal
            .reserve(&predecessor_input("wedger", 0, "old-epoch"))
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "wedger".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-wedger-0".into(),
            })
            .await
            .unwrap();

        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 4, "replacement-epoch");
        controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(controller.health_report().blocking_identities.len(), 1);

        let permit = controller
            .permit_for_key(AdmissionDomain::WarmBuild, "wedger", 0)
            .await
            .expect("the recovered row seeded an addressable permit");
        controller
            .transition(
                &permit,
                WarmAdmissionTransition::Live {
                    uid: "uid-wedger".into(),
                },
            )
            .await
            .unwrap();

        let health = controller.health_report();
        assert_eq!(health.create_unknown_pending, 0);
        assert!(
            health.blocking_identities.is_empty(),
            "an adopted row must stop being named the moment it stops being counted"
        );
    }
}

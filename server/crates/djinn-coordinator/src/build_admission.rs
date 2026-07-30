//! Coordinator-owned classification for build-producing workloads.
//!
//! This module used to host the durable pre-create admission ledger — the
//! `task_dispatch` reservation, its journal/inventory/stale-reclaim
//! compensations and the `BuildAdmissionReadiness` gate ladder. Kueue owns
//! build capacity now (proposal 9oga): a build Job is created suspended and
//! admitted against a ClusterQueue quota, so nothing reserves capacity before
//! the object that consumes it exists.
//!
//! What survives is the part Kueue does NOT decide: which LocalQueue a workload
//! belongs to, and which task-run roles are build-capable at all. The classes
//! that weigh zero still weigh zero — they simply never reach a queue.

use djinn_db::{V0Mode, V1Mode};
use djinn_runtime::RoleResourceClass;

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
///   (`v0 ∈ {observe, disabled} ∧ v1 ∈ {off, shadow}`) is rejected. Note the
///   meaning of `V0Mode::Enforce` changed when capacity accounting was unified
///   onto the v1 lease: v0 has no cap of its own. Only `V1Mode::Enforce` arms
///   the actual build-slot cap, and only `V1Mode::Enforce` lifts the
///   per-invocation cgroup quota.
/// - The reference cap must be within `[MIN_ADMISSION_CAP, MAX_ADMISSION_CAP]`.
///
/// Retained by o53p alongside [`crate::build_admission_transition`], which is
/// its only caller: both belong to the `admission_handoff` epoch, not to the
/// deleted pods-quota reservation.
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

/// Audit reason recorded when a Light task-run is dispatched without being
/// charged build capacity.
///
/// Distinct and greppable on purpose: it is the single string that explains why
/// an admitted task-run left no capacity accounting behind.
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
/// [`TaskRunRole::resource_class`]. Only [`RoleResourceClass::BuildCapable`]
/// task-runs are build workloads: with a production quota of 3 on a 12-vCPU
/// node, queueing a Planner or a tribunal round behind builds it never competes
/// with would collapse throughput.
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
    /// `djinn-agent/src/supervisor_impl/stage.rs`). Every production caller
    /// passes either a `RoleRegistry` dispatch role, the literal `"planner"`
    /// (`dispatch/retry.rs`), or a refinement `agent_type`
    /// (`advocate`/`adversary`/`judge`) — never `"verifier"`. A verifier's
    /// compile therefore runs inside a Worker task-run.
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

/// The canonical durable key for one task-run admission generation.
///
/// # This is a retained STUB of a deleted producer, and S3b owns removing it
///
/// The Kueue cutover (o53p) deleted `admission_journal` and with it
/// `admission_generation_key`, which formatted this string from a real journal
/// key. The one surviving consumer is `SupervisorServices::record_generation_ack`
/// (`djinn-agent`'s `supervisor_impl/stage.rs`), which writes the durable
/// `admission_handoff_generation_ack` rows the invocation-primary edge reads.
/// That relation and that trait method are BOTH in sibling task `ubne` (S3b),
/// which removes the method rather than stubbing it — so this function must
/// keep producing byte-identical keys until S3b lands, and disappears with it.
///
/// The byte form is deliberately unchanged: `TaskObservation:{task_id}:{generation}`,
/// exactly what `format!("{:?}:{}:{}", AdmissionDomain::TaskObservation, ..)`
/// produced. Existing `admission_handoff_generation_ack` rows in production were
/// written with that spelling, and an ack that no longer byte-matches its
/// required-generation set is an ack that silently never counts.
#[must_use]
pub fn task_run_generation_key(task_id: &str, generation: i64) -> String {
    format!("TaskObservation:{task_id}:{generation}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ROLES: [TaskRunRole; 8] = [
        TaskRunRole::Worker,
        TaskRunRole::Reviewer,
        TaskRunRole::Lead,
        TaskRunRole::Planner,
        TaskRunRole::Architect,
        TaskRunRole::Advocate,
        TaskRunRole::Adversary,
        TaskRunRole::Judge,
    ];

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

    /// `as_str` is documented as the exact inverse of `parse`. Lock it, or a
    /// renamed variant silently reclassifies a role's resource class — which is
    /// what `resource_class` looks up by name.
    #[test]
    fn as_str_round_trips_through_parse_for_every_role() {
        for role in ALL_ROLES {
            assert_eq!(TaskRunRole::parse(Some(role.as_str())), Some(role));
        }
    }

    /// The generation key is a DURABLE byte format, not an internal detail:
    /// `admission_handoff_generation_ack` rows written before the Kueue cutover
    /// carry exactly this spelling, and the invocation-primary edge matches its
    /// required-generation set against them byte-for-byte. Changing the format
    /// silently stops every existing ack from counting. Lock it.
    #[test]
    fn task_run_generation_key_keeps_its_durable_byte_format() {
        assert_eq!(
            task_run_generation_key("019fb35b-105f-7a93-aa52-af9758de06d5", 3),
            "TaskObservation:019fb35b-105f-7a93-aa52-af9758de06d5:3"
        );
        assert_eq!(task_run_generation_key("t", 0), "TaskObservation:t:0");
    }

    /// The build-capable set is the whole reason this module still exists: it
    /// decides which task-runs are build workloads at all. Assert the exact
    /// partition, not merely that the call returns.
    #[test]
    fn only_worker_and_architect_are_build_capable() {
        for role in ALL_ROLES {
            let build_capable = role.resource_class() == RoleResourceClass::BuildCapable;
            let expected = matches!(role, TaskRunRole::Worker | TaskRunRole::Architect);
            assert_eq!(
                build_capable,
                expected,
                "role {} build-capable classification changed",
                role.as_str()
            );
        }
    }
}

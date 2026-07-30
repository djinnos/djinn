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

use djinn_runtime::RoleResourceClass;

/// Smallest legal reference cap. A cap of zero would deny all admission.
pub const MIN_ADMISSION_CAP: i64 = 1;

/// Largest legal reference cap. A sane upper bound that rejects an obviously
/// mistyped configuration up front rather than letting it reach the durable row.
pub const MAX_ADMISSION_CAP: i64 = 4096;

/// Validate a reference cap before it is written durably.
///
/// The durable row's CHECK constraint already refuses a non-positive cap; this
/// exists so an operator typo is rejected at the CLI with a range in the message
/// rather than as a constraint violation, and so an absurd upper value never
/// reaches the row at all.
///
/// This used to also reject the "neither authority enforces" mode combination.
/// That rule belonged to the two-authority handoff: it existed so a rollout step
/// could never commit a state in which both v0 and v1 were released at once.
/// With one authority left, "not enforcing" is not an illegal combination — it
/// is the kill switch, and refusing to express it would remove the operator's
/// only way to disarm.
///
/// Retained by o53p alongside [`crate::invocation_lease_control`], which is its
/// only caller: both belong to the invocation-lease authority, not to the
/// deleted pods-quota reservation.
pub fn validate_reference_cap(cap: i64) -> Result<(), String> {
    if !(MIN_ADMISSION_CAP..=MAX_ADMISSION_CAP).contains(&cap) {
        return Err(format!(
            "reference cap {cap} is out of range [{MIN_ADMISSION_CAP}, {MAX_ADMISSION_CAP}]"
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

// `task_run_generation_key` lived here until S3b and is now gone.
//
// It formatted the key for an `admission_handoff_generation_ack` row, whose only
// purpose was to hold the v0→v1 invocation-primary handoff edge closed until
// every live task-run generation had confirmed the new authority. That edge, the
// phase ring it belonged to, and the required-generation set the keys were
// matched against were all retired with the v0 authority. Nothing reads those
// rows, so nothing needs to be able to spell their keys.

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

    /// The cap range is the operator-facing guard, and "disarmed" must stay
    /// expressible: rejecting it here would remove the kill switch.
    #[test]
    fn the_reference_cap_range_is_enforced_at_both_ends() {
        assert!(validate_reference_cap(MIN_ADMISSION_CAP).is_ok());
        assert!(validate_reference_cap(MAX_ADMISSION_CAP).is_ok());
        assert!(validate_reference_cap(0).is_err());
        assert!(validate_reference_cap(-1).is_err());
        assert!(validate_reference_cap(MAX_ADMISSION_CAP + 1).is_err());
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

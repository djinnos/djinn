//! Rendered CPU facts the build-slot weight policy is derived from.
//!
//! Split verbatim out of `build_lease.rs`, which the resize-authorization hook
//! left 43 bytes under the 51200-byte source-size guard. The contract is
//! self-contained — it names no lease, no repository and no clock, only the
//! projection from rendered millicores onto slot weight — and `build_lease`
//! re-exports it, so every existing `crate::build_lease::BuildSlotWeights` path
//! still resolves.

use djinn_runtime::BuildSlotWeight;

/// Rendered CPU facts the weight policy is derived from, in millicores.
///
/// Supplied by composition from the SAME `KubernetesConfig` that renders the
/// manifests, so the weight a workload is charged and the CPU it is actually
/// given can never drift. Deliberately not read from the environment here: the
/// grant path must have no opinion about where capacity facts come from, which
/// is what lets a node-derived cap replace the configured one later without
/// touching this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildSlotWeights {
    /// One build slot, in millicores: the quota a granted lease actually runs
    /// under (`launcher_leased_millicores`, the task-run pod's `cpu_limit`).
    pub slot_millicores: u32,
    /// The graph-warm Job's rendered CPU request.
    pub warm_millicores: u32,
}

impl Default for BuildSlotWeights {
    /// The default render: a warm Job and a leased task invocation both request
    /// 4000m, so both weigh exactly one slot. See [`BuildSlotWeight`] for why
    /// equal weight is the measured answer rather than an approximation.
    fn default() -> Self {
        Self {
            slot_millicores: 4_000,
            warm_millicores: 4_000,
        }
    }
}

impl BuildSlotWeights {
    /// Weight of a graph-warm Job.
    #[must_use]
    pub fn warm(&self) -> BuildSlotWeight {
        BuildSlotWeight::for_millicores(self.warm_millicores, self.slot_millicores)
    }

    /// Weight of a layer-1 dispatch reservation for a build-capable task-run.
    /// Light task-runs never reach here: dispatch does not acquire for them.
    #[must_use]
    pub fn dispatch(&self) -> BuildSlotWeight {
        BuildSlotWeight::for_millicores(self.slot_millicores, self.slot_millicores)
    }

    /// Weight of a layer-2 invocation escalation.
    ///
    /// `holds_dispatch_slot` is the durable answer from
    /// [`djinn_db::BuildLeaseRepository::has_occupying_dispatch`], never
    /// anything the invocation itself asserted. That matters: weight is the
    /// difference between occupying capacity and not, so a value the sandboxed
    /// pod could supply would be a way to escape the cap.
    #[must_use]
    pub fn invocation(&self, holds_dispatch_slot: bool) -> BuildSlotWeight {
        if holds_dispatch_slot {
            BuildSlotWeight::REENTRANT
        } else {
            BuildSlotWeight::for_millicores(self.slot_millicores, self.slot_millicores)
        }
    }
}

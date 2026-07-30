//! The lease authority an invocation leaf is BORN under, and the `cpu.max`
//! line that expresses "no quota at this level".
//!
//! Split out of `lib.rs` (goxi launcher blocker 11), which was within a few
//! hundred bytes of the 51200-byte size guard. The contract is self-contained:
//! it names no leaf, no child and no filesystem, only the projection from
//! "can the durable authority ever lift this?" onto a birth quota.

use crate::{DEFAULT_PERIOD_US, Error};

/// The quota-authority protocol is **not defined here**.
///
/// It is the one contract `djinn-cgroup-launcher`, `djinn-db` (migration 164's
/// `CHECK ... IN ('leaf-v1', 'resize-v2')`) and the admission plane must all
/// agree on, so it lives in the zero-`djinn` leaf crate
/// [`djinn_launcher_protocol`] and is re-exported here. This crate keeps the
/// re-export — not a copy — because a second definition is exactly how #2800
/// shipped a wire form nothing could compare against.
///
/// The launcher stays a static sidecar: the protocol crate depends on `serde`
/// (without `derive`) and on nothing else.
pub use djinn_launcher_protocol::LauncherAuthorityProtocol;

/// `cpu.max` line for a leaf with NO quota of its own.
///
/// `max` is cgroup-v2's "no limit at this level"; the enclosing Pod cgroup still
/// caps the child, so an unrestricted leaf runs at exactly the throughput the
/// same command would get with no launcher at all.
pub fn unrestricted_cpu_max() -> String {
    format!("max {DEFAULT_PERIOD_US}")
}

/// Whether the durable lease authority is in a state that can ever lift THIS
/// invocation.
///
/// # Why the birth quota needs this, and why it cannot be decided later
///
/// A leaf's `cpu.max` is written once, before the child is allowed to `execve`,
/// and the lift is deliberately one-way and fenced. So the question "will a
/// lease ever be able to raise this quota?" has to be answered *before the leaf
/// exists* — there is no safe unfenced path back down from a clamp.
///
/// Getting that wrong cost four production rollouts. The launcher was armed
/// while the durable `admission_handoff` row was ABSENT, which
/// `evaluate_invocation_lift` maps to `Unleased` — a branch the runner
/// implemented as a literal no-op. But the leaf had already been born at
/// [`UnleasedQuota::DEFAULT_MILLICORES`], so "no-op" actually meant *every*
/// build ran clamped to 250m for its whole life while the pod held 4 CPUs. A
/// measured leaf reached 21.1 CPU-seconds — 84x the escalation threshold — and
/// `cpu.max` never moved off `25000 100000`. Arming was a ~16x slowdown, i.e.
/// strictly worse than leaving the feature off, which is why production sat on
/// `mode: disabled`.
///
/// [`Self::Unarmed`] exists so that state is expressible at leaf creation:
/// containment without a reachable lift is pure loss, so an invocation that
/// provably cannot be lifted is not clamped in the first place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseAuthority {
    /// v1 can act on this invocation: the leaf is born at the unleased quota
    /// and a matching fenced grant lifts it to the leased quota. Shadow
    /// observation also arms — it deliberately clamps without lifting.
    Armed,
    /// v1 cannot lift this invocation at all (the handoff row is absent,
    /// unreadable, at the `emergency_primary` baseline, at a stale epoch, or
    /// `v1_mode` is off).
    ///
    /// The leaf is still created, still named by invocation id, still killable
    /// via `cgroup.kill` and still measurable on `cpu.stat` — every containment
    /// property is retained. Only the quota is omitted, so the child inherits
    /// the Pod's own budget instead of being pinned to a quota nothing can ever
    /// raise.
    Unarmed,
}

impl LeaseAuthority {
    /// Can a fenced lift be applied to a leaf born under this authority?
    pub fn can_lift(self) -> bool {
        matches!(self, Self::Armed)
    }

    pub(crate) fn wire(self) -> u8 {
        match self {
            Self::Armed => 1,
            Self::Unarmed => 0,
        }
    }

    pub(crate) fn from_wire(value: u8) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Unarmed),
            1 => Ok(Self::Armed),
            _ => Err(Error::InvalidTransportFrame),
        }
    }
}

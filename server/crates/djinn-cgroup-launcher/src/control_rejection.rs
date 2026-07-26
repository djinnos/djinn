//! Why the privileged broker refused a control, as a bounded wire category.
//!
//! # Why this exists (goxi launcher blocker 14)
//!
//! [`crate::transport::reply`] used to answer every server-side failure with a
//! single byte, `1`, and the client turned that byte into
//! [`crate::Error::InvalidControl`] regardless of what had actually happened.
//! So a production worker whose `LIFT` was rejected for a **fence mismatch**
//! reported:
//!
//! ```text
//! lease invocation failed: Launcher(Custom { kind: Other, error: InvalidControl })
//! ```
//!
//! `InvalidControl` is a real broker error in its own right (see
//! [`crate::Error::InvalidControl`]), so the message named a cause that had not
//! occurred, and it named it identically for a stale nonce, an unarmed leaf, an
//! already-applied lift and a terminal invocation. The whole diagnosis had to be
//! done by reading `cpu.max`/`cpu.stat` off the node by hand — for the second
//! time in this feature.
//!
//! # What is and is not disclosed
//!
//! The peer is already authenticated at this point: the broker checked
//! `SO_PEERCRED` pid/uid/gid against the configured worker and compared the
//! worker-private pod credential, both before any control is dispatched. Telling
//! *that* peer which category its own control fell into leaks nothing it could
//! not learn by probing, and it is deliberately a CATEGORY: no path, no errno,
//! no leaf name, no command text, no length. The coarse-on-purpose refusals
//! (`InvalidCommand` above all) stay coarse — [`ControlRejection::Command`]
//! still does not say whether the program, the cwd or one environment entry was
//! at fault, which is why the worker keeps its own copy of that check.

/// The category of a refused broker control, as carried on the wire.
///
/// Codes are stable: they are a protocol surface between the worker and a
/// privileged process that may be a different build during a rolling update, so
/// an unknown code decodes to [`Self::Unspecified`] rather than failing the
/// frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlRejection {
    /// The presented fencing value is not the one this invocation was begun
    /// with. This is what a `LIFT` returns when the birth fence and the lift
    /// fence disagree.
    Fence,
    /// The one-way lift has already been applied to this leaf.
    AlreadyLifted,
    /// The leaf was born under an unarmed lease authority, so it has no quota to
    /// lift and writing one would LOWER its ceiling.
    Unarmed,
    /// Terminal intent has been recorded for this invocation; no lift may follow.
    Terminal,
    /// The control does not name the invocation bound to this connection.
    Binding,
    /// The control nonce was stale, forged, or replayed.
    Nonce,
    /// The connection has not presented a valid worker readiness assertion.
    Worker,
    /// Peer credentials or the worker-private pod credential did not match.
    Credential,
    /// The command request is malformed, over-budget, or outside the allow-list.
    /// Deliberately says no more than that.
    Command,
    /// The control is legal but does not match the invocation's current state
    /// (no leaf yet, a duplicate create, a still-populated cgroup, …).
    State,
    /// The frame itself did not parse: wrong length, an unknown opcode, a
    /// trailing descriptor, an oversized payload. Distinct from
    /// [`Self::Command`] on purpose — a malformed frame is rejected before the
    /// allow-list ever sees a command, so reporting the two alike would let a
    /// wire-format mistake masquerade as a policy refusal (which is exactly what
    /// the launcher's own allow-list tests were doing).
    Malformed,
    /// A privileged-side filesystem, cgroup or child-process operation failed.
    Kernel,
    /// A category this build does not know. Never produced locally; only decoded
    /// from a peer that is a newer build.
    Unspecified,
}

impl ControlRejection {
    /// Classify a broker-side error into its wire category.
    pub fn of(error: &crate::Error) -> Self {
        use crate::Error as E;
        match error {
            E::FenceMismatch => Self::Fence,
            E::LiftAlreadyApplied => Self::AlreadyLifted,
            E::LiftWithoutAuthority => Self::Unarmed,
            E::TerminalIntent => Self::Terminal,
            E::InvalidInvocationBinding => Self::Binding,
            E::InvalidNonce => Self::Nonce,
            E::InvalidWorker => Self::Worker,
            E::UnauthenticatedPeer | E::InvalidCredential => Self::Credential,
            E::InvalidCommand => Self::Command,
            E::InvalidControl | E::StillPopulated | E::UnsafeLeafName => Self::State,
            E::InvalidTransportFrame => Self::Malformed,
            _ => Self::Kernel,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Fence => 1,
            Self::AlreadyLifted => 2,
            Self::Unarmed => 3,
            Self::Terminal => 4,
            Self::Binding => 5,
            Self::Nonce => 6,
            Self::Worker => 7,
            Self::Credential => 8,
            Self::Command => 9,
            Self::State => 10,
            Self::Kernel => 11,
            Self::Malformed => 12,
            Self::Unspecified => 0,
        }
    }

    /// Decode a wire code. An unrecognised code is [`Self::Unspecified`]: a
    /// worker paired with a newer launcher must still see "the broker refused
    /// this", never a transport-frame error that hides it.
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Fence,
            2 => Self::AlreadyLifted,
            3 => Self::Unarmed,
            4 => Self::Terminal,
            5 => Self::Binding,
            6 => Self::Nonce,
            7 => Self::Worker,
            8 => Self::Credential,
            9 => Self::Command,
            10 => Self::State,
            11 => Self::Kernel,
            12 => Self::Malformed,
            _ => Self::Unspecified,
        }
    }

    /// One-line operator-facing explanation. This is what reaches a task-run
    /// Pod's logs, so it names the contract that was violated rather than the
    /// enum variant.
    pub fn explain(self) -> &'static str {
        match self {
            Self::Fence => {
                "the fencing value presented with this control is not the one the invocation was \
                 begun with; the birth quota and the lift must name one fence"
            }
            Self::AlreadyLifted => "the one-way lift has already been applied to this leaf",
            Self::Unarmed => {
                "the leaf was born under an unarmed lease authority and has no quota to lift"
            }
            Self::Terminal => "terminal intent forbids a lift on this invocation",
            Self::Binding => "the control is not bound to the active launcher invocation",
            Self::Nonce => "the control nonce was stale, forged, or replayed",
            Self::Worker => "the connection has not presented a valid worker readiness assertion",
            Self::Credential => "peer or worker-private credential authentication failed",
            Self::Command => {
                "the command request is malformed, over-budget, or outside the broker allow-list"
            }
            Self::State => "the control does not match the invocation's current state",
            Self::Malformed => "the control frame did not parse: bad length, opcode, or payload",
            Self::Kernel => "a privileged cgroup, filesystem, or child-process operation failed",
            Self::Unspecified => "the broker refused the control for an unrecognised reason",
        }
    }
}

impl std::fmt::Display for ControlRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}: {}", self.explain())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    /// Every category must survive the wire, and an unknown code must decode to
    /// a refusal rather than corrupting the frame.
    #[test]
    fn every_category_round_trips_and_unknown_codes_stay_refusals() {
        for category in [
            ControlRejection::Fence,
            ControlRejection::AlreadyLifted,
            ControlRejection::Unarmed,
            ControlRejection::Terminal,
            ControlRejection::Binding,
            ControlRejection::Nonce,
            ControlRejection::Worker,
            ControlRejection::Credential,
            ControlRejection::Command,
            ControlRejection::State,
            ControlRejection::Kernel,
            ControlRejection::Malformed,
            ControlRejection::Unspecified,
        ] {
            assert_eq!(ControlRejection::from_code(category.code()), category);
        }
        assert_eq!(
            ControlRejection::from_code(200),
            ControlRejection::Unspecified
        );
    }

    /// The classification that matters: a fence mismatch must NOT be reported as
    /// `InvalidControl`. Those two were the same byte on the wire, which is why
    /// production said "InvalidControl" for a fence the runner had chosen wrong.
    #[test]
    fn a_fence_mismatch_is_distinguishable_from_an_invalid_control() {
        assert_eq!(
            ControlRejection::of(&Error::FenceMismatch),
            ControlRejection::Fence
        );
        assert_eq!(
            ControlRejection::of(&Error::InvalidControl),
            ControlRejection::State
        );
        assert_ne!(
            ControlRejection::of(&Error::FenceMismatch),
            ControlRejection::of(&Error::InvalidControl),
            "the two errors production could not tell apart must not share a wire code"
        );
        // And the lift-specific refusals stay distinct from each other, because
        // the operator action differs: a fence mismatch is a runner defect, an
        // unarmed leaf is a durable-epoch state.
        assert_ne!(
            ControlRejection::of(&Error::LiftWithoutAuthority),
            ControlRejection::of(&Error::FenceMismatch)
        );
        assert_ne!(
            ControlRejection::of(&Error::LiftAlreadyApplied),
            ControlRejection::of(&Error::FenceMismatch)
        );
    }
}

//! Origin tagging for run cancellation.
//!
//! A [`tokio_util::sync::CancellationToken`] carries no payload: once it is
//! cancelled, every observer learns *that* the run was cancelled and nothing
//! about *who* cancelled it. A worker Pod fires exactly one token from many
//! distinct places (SIGTERM, the in-pod soft deadline, a host `Cancel` control
//! frame, RPC transport death, orderly teardown), so every one of those causes
//! collapsed into the same terminal reason and production could not tell them
//! apart.
//!
//! [`CancelOriginTag`] is the missing side channel: a cheap, cloneable,
//! lock-free slot written immediately *before* `.cancel()` at each trigger site
//! and read wherever the cancellation is observed.
//!
//! # This is observability, never a gate
//!
//! An unrecorded cancellation reads back as [`CancelOrigin::Unknown`]. That is
//! a legitimate, expected value — it means "nobody tagged this trigger", not
//! "something is wrong". Nothing here returns an error, fails closed, or
//! changes control flow, and no caller may treat `Unknown` as a fault.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Who fired a run's cancellation token.
///
/// The discriminants are part of the on-wire-ish contract only in the sense
/// that [`CancelOriginTag`] packs them into an `AtomicU8`; they are never
/// persisted numerically. [`CancelOrigin::as_str`] is the stable, greppable
/// spelling that reaches durable terminal reasons and log fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum CancelOrigin {
    /// No trigger site recorded an origin before cancelling. Always a valid
    /// outcome — read it as "not attributed", never as an error.
    #[default]
    Unknown,
    /// A session-scoped cancel: this session was stopped on its own, without
    /// the surrounding supervisor or Pod winding down.
    Session,
    /// The supervisor tore the run down (server shutdown, operator kill).
    SupervisorShutdown,
    /// The worker Pod received `SIGTERM` (kubelet eviction, graceful drain,
    /// `activeDeadlineSeconds` grace, `helm upgrade` roll).
    Sigterm,
    /// The worker Pod received `SIGINT`.
    Sigint,
    /// The in-pod soft deadline fired ahead of the kubelet's hard
    /// `activeDeadlineSeconds` backstop.
    SoftDeadline,
    /// The host sent an explicit `Control(Cancel)` frame over RPC.
    HostCancelControl,
    /// The host sent a `Control(Shutdown)` frame over RPC.
    HostShutdownControl,
    /// The RPC transport died (socket closed, frame write failed). There is no
    /// reconnect, so the worker winds down through the same graceful path.
    RpcTransportClosed,
    /// Orderly end-of-run teardown after the supervisor already returned.
    WorkerTeardown,
}

impl CancelOrigin {
    /// Stable, lowercase, snake_case spelling used in terminal reasons, log
    /// fields, and metric labels. Never change an existing spelling — dashboards
    /// and incident greps key off these tokens.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Session => "session",
            Self::SupervisorShutdown => "supervisor_shutdown",
            Self::Sigterm => "sigterm",
            Self::Sigint => "sigint",
            Self::SoftDeadline => "soft_deadline",
            Self::HostCancelControl => "host_cancel_control",
            Self::HostShutdownControl => "host_shutdown_control",
            Self::RpcTransportClosed => "rpc_transport_closed",
            Self::WorkerTeardown => "worker_teardown",
        }
    }

    /// Whether an origin was actually attributed.
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Session => 1,
            Self::SupervisorShutdown => 2,
            Self::Sigterm => 3,
            Self::Sigint => 4,
            Self::SoftDeadline => 5,
            Self::HostCancelControl => 6,
            Self::HostShutdownControl => 7,
            Self::RpcTransportClosed => 8,
            Self::WorkerTeardown => 9,
        }
    }

    /// Total: any unrecognised byte degrades to [`CancelOrigin::Unknown`]
    /// rather than panicking. A torn or future-versioned value must never be
    /// able to take a process down over a diagnostic.
    const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Session,
            2 => Self::SupervisorShutdown,
            3 => Self::Sigterm,
            4 => Self::Sigint,
            5 => Self::SoftDeadline,
            6 => Self::HostCancelControl,
            7 => Self::HostShutdownControl,
            8 => Self::RpcTransportClosed,
            9 => Self::WorkerTeardown,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for CancelOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Shared, cloneable slot recording which trigger fired a cancellation token.
///
/// Clone it wherever the matching `CancellationToken` is cloned; every clone
/// reads and writes the same slot.
///
/// # First writer wins
///
/// Cancellation is racy by nature: SIGTERM and the RPC reader can observe the
/// same Pod teardown microseconds apart. The *first* recorded origin is the one
/// that caused the cancellation; later ones are consequences of it, so
/// [`Self::record`] never overwrites an already-attributed origin.
#[derive(Clone, Debug, Default)]
pub struct CancelOriginTag {
    slot: Arc<AtomicU8>,
}

impl CancelOriginTag {
    /// A fresh, unattributed tag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribute this cancellation, if it is not already attributed.
    ///
    /// Call this immediately *before* `token.cancel()` so any observer that
    /// wakes on the token can already read the origin. Recording
    /// [`CancelOrigin::Unknown`] is a no-op, and recording a second origin
    /// leaves the first in place (see the type docs).
    ///
    /// Infallible by construction — a diagnostic must never be able to fail a
    /// teardown path.
    pub fn record(&self, origin: CancelOrigin) {
        if !origin.is_known() {
            return;
        }
        // Only claim the slot when it is still unattributed. `Relaxed` on
        // failure is sufficient: we discard the observed value either way.
        let _ = self.slot.compare_exchange(
            CancelOrigin::Unknown.as_u8(),
            origin.as_u8(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Read the recorded origin, or [`CancelOrigin::Unknown`] when no trigger
    /// site attributed one.
    pub fn get(&self) -> CancelOrigin {
        CancelOrigin::from_u8(self.slot.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrecorded_tag_reads_unknown_and_never_errors() {
        let tag = CancelOriginTag::new();
        assert_eq!(tag.get(), CancelOrigin::Unknown);
        assert!(!tag.get().is_known());
        // Recording `Unknown` is a no-op, not a state change.
        tag.record(CancelOrigin::Unknown);
        assert_eq!(tag.get(), CancelOrigin::Unknown);
    }

    #[test]
    fn record_attributes_the_origin_to_every_clone() {
        let tag = CancelOriginTag::new();
        let observer = tag.clone();
        tag.record(CancelOrigin::Sigterm);
        assert_eq!(observer.get(), CancelOrigin::Sigterm);
        assert!(observer.get().is_known());
    }

    #[test]
    fn first_writer_wins_so_a_consequence_cannot_overwrite_the_cause() {
        let tag = CancelOriginTag::new();
        // SIGTERM is the cause; the RPC transport dying afterwards is a
        // consequence of the same teardown and must not relabel the row.
        tag.record(CancelOrigin::Sigterm);
        tag.record(CancelOrigin::RpcTransportClosed);
        tag.record(CancelOrigin::WorkerTeardown);
        assert_eq!(tag.get(), CancelOrigin::Sigterm);
    }

    #[test]
    fn every_origin_round_trips_through_the_packed_byte() {
        for origin in [
            CancelOrigin::Unknown,
            CancelOrigin::Session,
            CancelOrigin::SupervisorShutdown,
            CancelOrigin::Sigterm,
            CancelOrigin::Sigint,
            CancelOrigin::SoftDeadline,
            CancelOrigin::HostCancelControl,
            CancelOrigin::HostShutdownControl,
            CancelOrigin::RpcTransportClosed,
            CancelOrigin::WorkerTeardown,
        ] {
            assert_eq!(CancelOrigin::from_u8(origin.as_u8()), origin);
            let tag = CancelOriginTag::new();
            tag.record(origin);
            assert_eq!(tag.get(), origin);
        }
    }

    #[test]
    fn an_unrecognised_byte_degrades_to_unknown_instead_of_panicking() {
        assert_eq!(CancelOrigin::from_u8(200), CancelOrigin::Unknown);
        assert_eq!(CancelOrigin::from_u8(u8::MAX), CancelOrigin::Unknown);
    }

    #[test]
    fn origin_spellings_are_distinct_and_stable() {
        let all = [
            CancelOrigin::Unknown,
            CancelOrigin::Session,
            CancelOrigin::SupervisorShutdown,
            CancelOrigin::Sigterm,
            CancelOrigin::Sigint,
            CancelOrigin::SoftDeadline,
            CancelOrigin::HostCancelControl,
            CancelOrigin::HostShutdownControl,
            CancelOrigin::RpcTransportClosed,
            CancelOrigin::WorkerTeardown,
        ];
        let mut seen = std::collections::HashSet::new();
        for origin in all {
            assert!(
                seen.insert(origin.as_str()),
                "duplicate origin spelling: {origin}"
            );
            assert_eq!(origin.to_string(), origin.as_str());
        }
        assert_eq!(CancelOrigin::Sigterm.as_str(), "sigterm");
        assert_eq!(CancelOrigin::Unknown.as_str(), "unknown");
    }

    #[test]
    fn concurrent_recorders_settle_on_exactly_one_known_origin() {
        let tag = CancelOriginTag::new();
        let origins = [
            CancelOrigin::Sigterm,
            CancelOrigin::SoftDeadline,
            CancelOrigin::RpcTransportClosed,
            CancelOrigin::WorkerTeardown,
        ];
        std::thread::scope(|scope| {
            for origin in origins {
                let tag = tag.clone();
                scope.spawn(move || tag.record(origin));
            }
        });
        let settled = tag.get();
        assert!(settled.is_known(), "a raced record must still attribute");
        assert!(origins.contains(&settled));
        // Stable afterwards: reads never mutate, later writes never win.
        assert_eq!(tag.get(), settled);
        tag.record(CancelOrigin::Session);
        assert_eq!(tag.get(), settled);
    }
}

//! The terminal seam's view of proposal `3i92`'s fail-safe drop.
//!
//! Sibling of [`crate::task_run_resize_admission`], and for the same reason: the
//! machinery is composed in `djinn_server::task_run_resize_drop`, `djinn-agent`
//! cannot depend on the server crate (ADR-047), and the only place that owns the
//! per-invocation terminal signal is [`crate::direct_services::DirectServices`]'s
//! `release_lease` handler.
//!
//! # Two ledgers
//!
//! `BuildLeaseService::release` writes `build_leases`. The resize lifecycle it
//! must wait for lives in `build_pod_permits`. Those are DIFFERENT STORES, and
//! before this gate existed nothing connected them: `release_lease` returned
//! capacity the moment the worker asked, whether or not the launcher sidecar had
//! come back down to 250m. A Pod left holding a lifted ceiling with its lease
//! already returned is capacity the cluster has sold twice.
//!
//! So the ordering this trait imposes is: the `build_pod_permits` row must have
//! left `drop_applying` for `birth_confirmed` on a **confirmed** 250m, or the
//! exact Pod UID must be confirmed absent, BEFORE `build_leases` moves at all.

use std::sync::Arc;

use async_trait::async_trait;

/// One invocation asking to be released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeDropGateRequest {
    /// The task run whose permit row carries the resize lifecycle.
    pub task_run_id: String,
    /// The invocation this terminal belongs to. It is the invocation half of
    /// the `(task_run_id, pod_uid, resize_invocation_id)` fence — a task run has
    /// exactly one permit row and many invocations, so without it a terminal
    /// could retire a lift it never performed.
    pub invocation_id: String,
}

/// Whether the durable CPU lease may be released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResizeDropVerdict {
    /// The launcher is confirmed back at its birth limit, its Pod UID is
    /// confirmed absent, or the run never held a resize-governed ceiling.
    /// `build_leases` may move.
    Releasable,
    /// The drop is not settled. The lease MUST stay held and terminal MUST NOT
    /// be acknowledged. The durable permit row records why, and survives a
    /// restart of whichever process was driving the drop.
    Held {
        /// Rendered for the operator log.
        detail: String,
    },
}

/// The server-side drop, as [`crate::context::AgentContext`] holds it.
pub type ResizeDropBridge = Arc<dyn TaskRunResizeDropGate>;

/// The server-side drop, as the terminal seam consumes it.
#[async_trait]
pub trait TaskRunResizeDropGate: Send + Sync {
    /// Drive the fail-safe drop for one invocation's terminal and report whether
    /// the durable CPU lease may now be released.
    ///
    /// Implementations must fail CLOSED: an unreadable database, an unreachable
    /// apiserver, or a Pod that is still quarantined are all
    /// [`ResizeDropVerdict::Held`], never `Releasable`.
    async fn confirm_drop(&self, request: &ResizeDropGateRequest) -> ResizeDropVerdict;
}

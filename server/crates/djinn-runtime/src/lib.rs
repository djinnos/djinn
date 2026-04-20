//! `djinn-runtime` — narrow crate for the runtime boundary that the
//! supervisor speaks to.
//!
//! Phase 2 PR 1 — this crate currently holds the wire-capable spec types
//! (`TaskRunSpec`, `TaskRunOutcome`, `TaskRunReport`, `RoleKind`,
//! `SupervisorFlow`) plus placeholder shapes for the future session-runtime
//! boundary:
//!
//! - [`SessionRuntime`] trait — object-safe `prepare` / `attach_stdio` /
//!   `cancel` / `teardown` surface that the coordinator will call.
//! - [`RunHandle`] — opaque per-run handle holding container/pod refs.
//! - [`BiStream`] — duplex event/request channel between coordinator and the
//!   in-container supervisor.
//! - [`TestRuntime`] (behind `cfg(test)` / `feature = "test-runtime"`) — an
//!   in-process stub; wired up for real in a later PR.
//!
//! Nothing in this crate may depend on `djinn-agent` or any crate that drags
//! in `AppState`. Keeping the dependency tree narrow is what allows
//! `djinn-agent-worker` (PR 5) to link against `djinn-runtime` +
//! `djinn-supervisor` without pulling in the coordinator's actor framework.

pub mod handle;
pub mod session_runtime;
pub mod spec;
pub mod stream;
pub mod warmer;
pub mod wire;

#[cfg(any(test, feature = "test-runtime"))]
pub mod test_runtime;

pub use handle::RunHandle;
pub use session_runtime::{RuntimeError, SessionRuntime};
pub use warmer::{GraphWarmerService, WarmerError};
pub use spec::{
    RoleKind, SupervisorFlow, TaskRunOutcome, TaskRunReport, TaskRunSpec, role_sequence,
};
pub use stream::{BiStream, StreamEvent, StreamFrame};
pub use wire::{ControlMsg, MAX_FRAME_BYTES, WorkerEvent, WorkspaceRef, read_frame, write_frame};

#[cfg(any(test, feature = "test-runtime"))]
pub use test_runtime::TestRuntime;

//! [`SessionRuntime`] — the object-safe async trait that hides "how a
//! task-run actually executes" from the coordinator.
//!
//! Phase 2 PR 1 — trait definition plus a [`RuntimeError`] enum.  Impls
//! arrive in later PRs:
//!
//! - [`crate::TestRuntime`] (PR 1, stub in this PR) — in-process, for tests.
//! - `LocalDockerRuntime` (PR 6) — spawns a Docker container per run.
//! - `RemoteKubernetesRuntime` (Phase 3+) — dispatches to a pod via the
//!   cluster API.
//!
//! The trait is deliberately narrow: four verbs (`prepare`, `attach_stdio`,
//! `cancel`, `teardown`) bounded by `async_trait` to keep it object-safe.
//! Any richer contract (progress callbacks, tracing hooks) sits on top of
//! these via the [`crate::BiStream`] the runtime hands back.

use async_trait::async_trait;
use thiserror::Error;

use crate::handle::RunHandle;
use crate::spec::{TaskRunReport, TaskRunSpec};
use crate::stream::BiStream;

/// Failure modes the runtime surface can return.
///
/// Variants are intentionally coarse — callers route on the category, not on
/// the specific cause (that travels in the wrapped message).  Additional
/// variants will be added as the backends land (`Docker(bollard::Error)`,
/// `Kubernetes(...)`).
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// `prepare` could not materialise the run environment (container
    /// failed to start, workspace clone failed, socket bind failed, …).
    #[error("prepare failed: {0}")]
    Prepare(String),
    /// `attach_stdio` could not wire the duplex stream — usually an IPC
    /// handshake timeout or a container that died before accept.
    #[error("attach_stdio failed: {0}")]
    Attach(String),
    /// `cancel` could not deliver the termination signal to the run.
    #[error("cancel failed: {0}")]
    Cancel(String),
    /// `teardown` failed to collect the terminal report or clean the
    /// per-run resources (container removal, socket unlink, tempdir drop).
    #[error("teardown failed: {0}")]
    Teardown(String),
    /// Catch-all for internal invariant violations that are not one of the
    /// lifecycle-stage failures above.
    #[error("runtime internal: {0}")]
    Internal(String),
}

/// Object-safe lifecycle interface every runtime backend implements.
///
/// Implementations own any per-run state (container ids, socket paths,
/// tempdirs) behind [`RunHandle`].  The coordinator never inspects that
/// state — it just threads the handle back into the next method.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Materialise the run environment — clone the workspace, start the
    /// container, open the IPC socket — and return a handle the caller
    /// threads into the remaining methods.
    async fn prepare(&self, spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError>;

    /// Attach to the duplex stream created by `prepare`.  Called exactly
    /// once per handle after `prepare` returns.
    async fn attach_stdio(&self, handle: &RunHandle) -> Result<BiStream, RuntimeError>;

    /// Request graceful cancellation.  Implementations should deliver SIGTERM
    /// (or the backend-equivalent), wait a bounded grace period, then escalate
    /// to SIGKILL.  Returns once the cancellation signal has been *delivered*
    /// — waiting for the process to exit is `teardown`'s job.
    async fn cancel(&self, handle: &RunHandle) -> Result<(), RuntimeError>;

    /// Collect the terminal [`TaskRunReport`] and clean up per-run resources
    /// (container removal, socket unlink, tempdir drop).  Consumes the
    /// handle so no further calls can be made against it.
    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError>;
}

/// Compile-time assertion that [`SessionRuntime`] is object-safe — any
/// change to the trait that breaks `dyn SessionRuntime` will fail this
/// function's type check.
#[allow(dead_code)]
fn _obj_safe(_: &dyn SessionRuntime) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_runtime_is_object_safe() {
        // Compile-only: if this file compiles, `dyn SessionRuntime` is
        // valid, which is all we need to guarantee for PR 1.
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn SessionRuntime>();
    }
}

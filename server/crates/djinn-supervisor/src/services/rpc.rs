//! Placeholder [`SupervisorServices`] impl that will host the real
//! bincode-over-unix-socket client in PR 4/5.
//!
//! Today [`StubRpcServices`] only pins the trait layout: every method
//! `unimplemented!()`s so that `TaskRunSupervisor::new(Arc::new(StubRpcServices::default()))`
//! typechecks but panics the moment the supervisor tries to drive a run.
//! That's intentional — it keeps the trait object-safety and the
//! `Arc<dyn SupervisorServices>` dispatch plumbing honest without shipping
//! any fake RPC behaviour.

use async_trait::async_trait;
use djinn_core::models::Task;
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use super::SupervisorServices;
use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

/// RPC-backed `SupervisorServices` placeholder.
///
/// PR 4/5 replaces every `unimplemented!()` with a typed bincode frame over
/// the Unix-domain-socket client established in
/// `djinn-agent-worker::rpc_client`.
pub struct StubRpcServices {
    /// Supervisor-wide cancellation.  Kept on the stub so `cancel()` has
    /// something to return without panicking — useful for tests that
    /// construct the stub purely to assert object-safety.
    cancel: CancellationToken,
}

impl StubRpcServices {
    /// Construct a stub with a fresh, un-cancelled token.
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }

    /// Construct a stub bound to an externally-owned cancellation token.
    pub fn with_cancel(cancel: CancellationToken) -> Self {
        Self { cancel }
    }
}

impl Default for StubRpcServices {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SupervisorServices for StubRpcServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn load_task(&self, _task_id: String) -> Result<Task, String> {
        unimplemented!("StubRpcServices::load_task — filled in PR 4/5")
    }

    async fn execute_stage(
        &self,
        _task: &Task,
        _workspace: &Workspace,
        _role_kind: RoleKind,
        _task_run_id: &str,
        _spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        unimplemented!("StubRpcServices::execute_stage — filled in PR 4/5")
    }

    async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
        unimplemented!("StubRpcServices::open_pr — filled in PR 4/5")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The stub satisfies the trait (compile-time) and can be stored behind
    /// `Arc<dyn SupervisorServices>` (the supervisor's dispatch shape).
    #[test]
    fn stub_is_object_safe() {
        let svc: Arc<dyn SupervisorServices> = Arc::new(StubRpcServices::new());
        assert!(!svc.cancel().is_cancelled());
    }

    /// Every RPC method is `unimplemented!()` today.  Calling one must panic
    /// with the PR-4/5 hand-off message — proves the stub is a genuine
    /// placeholder and nothing is silently stubbed out.
    #[tokio::test]
    #[should_panic(expected = "filled in PR 4/5")]
    async fn stub_load_task_panics() {
        let svc = StubRpcServices::new();
        let _ = svc.load_task("t".into()).await;
    }
}

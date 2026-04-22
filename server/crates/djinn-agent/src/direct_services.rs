//! `DirectServices` — in-process [`SupervisorServices`] impl.
//!
//! Phase 2 PR 3 replaced `djinn-supervisor`'s struct-with-callbacks
//! `SupervisorServices` with a trait.  `DirectServices` is the production
//! (and integration-test) impl: it wraps an [`AgentContext`], a
//! supervisor-wide [`CancellationToken`], and an optional test-only
//! [`LlmProvider`] override, and delegates every trait method straight into
//! the in-tree lifecycle helpers.  Behaviour is verbatim with PR 2 — this
//! file just reshapes the closure bodies that used to live on
//! `SupervisorServices` into trait-method bodies.
//!
//! The worker-side sibling impl (`djinn_supervisor::services::rpc::StubRpcServices`
//! → real RPC client in PR 4/5) lives on the other side of the crate split
//! so this code never links the bincode/Unix-socket plumbing.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_core::models::Task;
use djinn_supervisor::{
    RoleKind, StageError, StageOutcome, SupervisorServices, TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use djinn_provider::provider::LlmProvider;
use crate::supervisor_impl::{SupervisorCallbackContext, execute_stage, supervisor_pr_open};

/// In-process `SupervisorServices` impl that delegates straight to the
/// lifecycle helpers inside `djinn-agent`.
pub struct DirectServices {
    callbacks: SupervisorCallbackContext,
}

impl DirectServices {
    /// Construct a `DirectServices` bound to the given [`AgentContext`] and
    /// cancellation token.  Production path.
    pub fn new(agent_context: AgentContext, cancel: CancellationToken) -> Self {
        Self::with_provider_override(agent_context, cancel, None)
    }

    /// Same as [`DirectServices::new`] but installs a test-only
    /// [`LlmProvider`] override on the stage executor, bypassing the catalog
    /// / vault credential lookup inside `execute_stage`.  Used by
    /// `tests/phase1_supervisor.rs`.
    pub fn with_provider_override(
        agent_context: AgentContext,
        cancel: CancellationToken,
        provider_override: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self {
            callbacks: SupervisorCallbackContext {
                agent_context,
                cancel,
                provider_override,
            },
        }
    }
}

#[async_trait]
impl SupervisorServices for DirectServices {
    fn cancel(&self) -> &CancellationToken {
        &self.callbacks.cancel
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        crate::actors::slot::helpers::load_task(&task_id, &self.callbacks.agent_context)
            .await
            .map_err(|e| e.to_string())
    }

    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        execute_stage(task, workspace, role_kind, task_run_id, spec, &self.callbacks).await
    }

    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome {
        supervisor_pr_open(spec, task, &self.callbacks).await
    }
}

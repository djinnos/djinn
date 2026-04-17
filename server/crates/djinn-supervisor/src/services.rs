//! The `SupervisorServices` trait — the object-safe surface the supervisor
//! orchestration loop calls into.
//!
//! See the crate docs for the PR 3 context. Two impls exist today:
//!
//! - `djinn_agent::direct_services::DirectServices` — in-process, wraps
//!   `AgentContext`. Production path.
//! - [`rpc::StubRpcServices`] — `unimplemented!()` placeholder that pins the
//!   trait layout ahead of PR 4/5.

use async_trait::async_trait;
use djinn_core::models::Task;
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

pub mod rpc;
pub mod server;
pub mod wire;

/// Dependencies shared across every stage in a task-run.
///
/// Object-safe by construction: no generic method parameters, no
/// `Self`-by-value receivers. `async_trait` handles the `Pin<Box<dyn
/// Future + Send>>` boxing so the trait can be used behind
/// `Arc<dyn SupervisorServices>`.
#[async_trait]
pub trait SupervisorServices: Send + Sync + 'static {
    /// Supervisor-wide cancellation token.  Flagged when the task-run is torn
    /// down (server shutdown, user kill).
    fn cancel(&self) -> &CancellationToken;

    /// Load the [`Task`] row backing this task-run.  Called once, before the
    /// first stage executes.
    async fn load_task(&self, task_id: String) -> Result<Task, String>;

    /// Execute one role stage against the shared workspace.  Called once per
    /// entry in `spec.flow.role_sequence()`.
    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError>;

    /// Open (or adopt) a GitHub PR for the completed task-run.  Called at
    /// most once per run, only for `NewTask` / `ReviewResponse` /
    /// `ConflictRetry` flows that reached the end of their role sequence
    /// cleanly.
    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome;
}

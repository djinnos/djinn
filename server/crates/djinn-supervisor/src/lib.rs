//! `djinn-supervisor` — task-run orchestration body extracted from
//! `djinn-agent::supervisor` during Phase 2 PR 2 of
//! `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! This crate owns the orchestration skeleton (`TaskRunSupervisor`,
//! `SupervisorServices`, `StageOutcome`, `StageError`, `SupervisorError`) but
//! does **not** depend on `djinn-agent` — that would be a cycle because
//! `djinn-agent` now re-exports this crate under `djinn_agent::supervisor::*`.
//!
//! ## Option A: callbacks on `SupervisorServices`
//!
//! The per-stage body (`execute_stage`) and the PR-open body
//! (`supervisor_pr_open`) still live in `djinn-agent` because they reach
//! deeply into `AgentContext`, role impls, the reply loop, the message/
//! provider types, the MCP/prompt/setup/teardown helpers, and
//! `task_merge`. Moving those helpers into yet-another sub-crate is Phase 2
//! PR 3's job (convert `SupervisorServices` into a trait with
//! `DirectServices` + `RpcServices` impls).
//!
//! For PR 2 we extracted the *shape* cleanly: the supervisor orchestration
//! loop lives here, and `djinn-agent` injects closures that bind the heavy
//! lifecycle machinery. The closures are stored on `SupervisorServices` and
//! invoked by `TaskRunSupervisor::run_sequence` when stepping through the
//! role sequence.
//!
//! See `djinn-agent/src/actors/slot/supervisor_runner.rs` for the
//! construction site.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use djinn_core::models::{Task, TaskRunStatus, TaskRunTrigger};
use djinn_db::TaskRunRepository;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_workspace::{MirrorError, MirrorManager, Workspace, WorkspaceError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

// Re-export runtime spec types at the crate root so the thin
// `djinn_agent::supervisor` shim preserves every existing import path.
pub use djinn_runtime::spec::{
    RoleKind, SupervisorFlow, TaskRunOutcome, TaskRunReport, TaskRunSpec, role_sequence,
};

// ── Error types ──────────────────────────────────────────────────────────────

/// Failure from [`TaskRunSupervisor::run`] *before* a stage can return a
/// typed [`StageOutcome`]. Errors that occur inside a stage and are
/// recoverable at the supervisor level arrive as a [`StageOutcome::Failed`]
/// variant instead.
#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("mirror: {0}")]
    Mirror(#[from] MirrorError),

    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceError),

    #[error("db: {0}")]
    Db(#[from] djinn_db::Error),

    #[error("load task: {0}")]
    LoadTask(String),

    #[error("stage: {0}")]
    Stage(#[from] StageError),
}

/// Pre-reply-loop failure surfaced by the per-stage executor injected via
/// [`SupervisorServices::execute_stage_fn`]. Always fatal for the task-run.
#[derive(Debug, Error)]
pub enum StageError {
    #[error("model resolution: {0}")]
    ModelResolution(String),

    #[error("setup/verification: {0}")]
    Setup(String),

    #[error("session create: {0}")]
    SessionCreate(String),
}

/// Outcome of executing one role stage.
#[derive(Clone, Debug)]
pub enum StageOutcome {
    WorkerDone,
    PlannerExecute,
    PlannerClose { reason: String },
    ReviewerApproved,
    ReviewerRejected { feedback: String },
    VerifierPassed,
    VerifierFailed { reason: String },
    ArchitectDone,
    Escalate { reason: String },
    Failed { reason: String },
}

impl StageOutcome {
    /// Whether this outcome should short-circuit the role sequence.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StageOutcome::PlannerClose { .. }
                | StageOutcome::Escalate { .. }
                | StageOutcome::Failed { .. }
                | StageOutcome::ReviewerRejected { .. }
                | StageOutcome::VerifierFailed { .. }
        )
    }
}

// ── Callback type aliases ────────────────────────────────────────────────────
//
// Every supervisor hook into `djinn-agent` lifecycle code travels through one
// of these async closures. Keeping the set small (three) lets us grow the
// supervisor body without paging in the full lifecycle surface — the body only
// sees `Task` (from `djinn-core`) and `Workspace` (from `djinn-workspace`).
//
// Each closure returns a `Pin<Box<dyn Future + Send>>` because the supervisor
// holds them as `dyn Fn`; trait-object-returning `async fn` isn't stable.

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type LoadTaskFn = Arc<dyn Fn(String) -> BoxFut<'static, Result<Task, String>> + Send + Sync>;

pub type ExecuteStageFn = Arc<
    dyn for<'a> Fn(
            &'a Task,
            &'a Workspace,
            RoleKind,
            &'a str,
            &'a TaskRunSpec,
            &'a SupervisorServices,
        ) -> BoxFut<'a, Result<StageOutcome, StageError>>
        + Send
        + Sync,
>;

pub type OpenPrFn = Arc<
    dyn for<'a> Fn(&'a TaskRunSpec, &'a Task, &'a SupervisorServices) -> BoxFut<'a, TaskRunOutcome>
        + Send
        + Sync,
>;

// ── SupervisorServices ───────────────────────────────────────────────────────

/// Dependencies shared across every stage in a task-run.
///
/// This is the concrete struct; PR 3 converts it into a trait with
/// `DirectServices` + `RpcServices` impls (see
/// `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md` §6).
///
/// The callbacks (`load_task_fn`, `execute_stage_fn`, `open_pr_fn`) are the
/// PR-2 seam that lets `djinn-supervisor` avoid depending on `djinn-agent`:
/// `djinn-agent`'s `supervisor_runner.rs` wires them to in-process bodies
/// that continue to live next to the lifecycle helpers they compose.
#[derive(Clone)]
pub struct SupervisorServices {
    /// Supervisor-wide cancellation.  Flagged when the task-run is torn down
    /// (server shutdown, user kill).
    pub cancel: CancellationToken,

    /// Injected loader for `djinn_core::models::Task` — implemented in
    /// `djinn-agent` via `TaskRepository`.
    pub load_task_fn: LoadTaskFn,

    /// Per-stage executor — bound by `djinn-agent` to its `execute_stage`
    /// body. Returns a `StageOutcome` that the supervisor matches on to
    /// decide the next step, or a `StageError` that short-circuits the run.
    pub execute_stage_fn: ExecuteStageFn,

    /// PR-open body — bound by `djinn-agent` to its `supervisor_pr_open`
    /// body. Only invoked when the flow's role sequence completes cleanly
    /// and the flow is one of `NewTask` / `ReviewResponse` / `ConflictRetry`.
    pub open_pr_fn: OpenPrFn,
}

// ── TaskRunSupervisor ────────────────────────────────────────────────────────

pub struct TaskRunSupervisor {
    task_runs: Arc<TaskRunRepository>,
    mirror: Arc<MirrorManager>,
    services: SupervisorServices,
}

impl TaskRunSupervisor {
    /// Construct a supervisor bound to the given services.
    pub fn new(
        task_runs: Arc<TaskRunRepository>,
        mirror: Arc<MirrorManager>,
        services: SupervisorServices,
    ) -> Self {
        Self {
            task_runs,
            mirror,
            services,
        }
    }

    /// Drive a task-run from start to terminal state.
    pub async fn run(&self, spec: TaskRunSpec) -> Result<TaskRunReport, SupervisorError> {
        let run_id = uuid::Uuid::now_v7().to_string();
        let trigger_str = spec.trigger.as_str().to_string();

        info!(
            task_run_id = %run_id,
            task_id = %spec.task_id,
            flow = ?spec.flow,
            "task-run starting"
        );

        self.task_runs
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &spec.project_id,
                task_id: &spec.task_id,
                trigger_type: &trigger_str,
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await?;

        let workspace = self
            .mirror
            .clone_ephemeral(&spec.project_id, &spec.base_branch)
            .await?;
        debug!(task_run_id = %run_id, path = ?workspace.path(), "ephemeral workspace ready");

        let task = (self.services.load_task_fn)(spec.task_id.clone())
            .await
            .map_err(SupervisorError::LoadTask)?;

        let sequence = spec.flow.role_sequence();
        let mut completed: Vec<RoleKind> = Vec::new();
        let outcome = self
            .run_sequence(&spec, &task, &workspace, &run_id, sequence, &mut completed)
            .await?;

        let terminal_status = match &outcome {
            TaskRunOutcome::PrOpened { .. } | TaskRunOutcome::Closed { .. } => {
                TaskRunStatus::Completed
            }
            TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
            TaskRunOutcome::Failed { .. } => TaskRunStatus::Failed,
            TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
        };
        self.task_runs
            .update_status(&run_id, terminal_status)
            .await?;

        info!(task_run_id = %run_id, ?outcome, "task-run finished");
        Ok(TaskRunReport {
            task_run_id: run_id,
            outcome,
            stages_completed: completed,
        })
    }

    /// Execute the role sequence against the shared workspace, invoking the
    /// injected `execute_stage_fn` for each role.
    async fn run_sequence(
        &self,
        spec: &TaskRunSpec,
        task: &Task,
        workspace: &Workspace,
        task_run_id: &str,
        sequence: &[RoleKind],
        completed: &mut Vec<RoleKind>,
    ) -> Result<TaskRunOutcome, SupervisorError> {
        let mut last_stage_role: Option<RoleKind> = None;
        for &role_kind in sequence {
            if self.services.cancel.is_cancelled() {
                return Ok(TaskRunOutcome::Interrupted);
            }

            let outcome = (self.services.execute_stage_fn)(
                task,
                workspace,
                role_kind,
                task_run_id,
                spec,
                &self.services,
            )
            .await?;

            last_stage_role = Some(role_kind);
            completed.push(role_kind);

            match outcome {
                StageOutcome::WorkerDone
                | StageOutcome::PlannerExecute
                | StageOutcome::ReviewerApproved
                | StageOutcome::VerifierPassed
                | StageOutcome::ArchitectDone => {
                    // Advance to the next stage.
                }
                StageOutcome::PlannerClose { reason } => {
                    return Ok(TaskRunOutcome::Closed { reason });
                }
                StageOutcome::Escalate { reason } => {
                    return Ok(TaskRunOutcome::Escalated { reason });
                }
                StageOutcome::ReviewerRejected { feedback } => {
                    return Ok(TaskRunOutcome::Failed {
                        stage: "reviewer".into(),
                        reason: format!("review rejected: {feedback}"),
                    });
                }
                StageOutcome::VerifierFailed { reason } => {
                    return Ok(TaskRunOutcome::Failed {
                        stage: "verifier".into(),
                        reason,
                    });
                }
                StageOutcome::Failed { reason } => {
                    return Ok(TaskRunOutcome::Failed {
                        stage: role_kind.as_str().into(),
                        reason,
                    });
                }
            }
        }

        // All stages completed successfully.  Spike / Planning have no PR
        // semantics; the merge-landing flows go through `open_pr_fn`.
        match spec.flow {
            SupervisorFlow::Planning | SupervisorFlow::Spike => Ok(TaskRunOutcome::Closed {
                reason: format!(
                    "{} flow completed (last stage: {:?})",
                    spec.flow.as_str(),
                    last_stage_role
                ),
            }),
            SupervisorFlow::NewTask
            | SupervisorFlow::ReviewResponse
            | SupervisorFlow::ConflictRetry => {
                Ok((self.services.open_pr_fn)(spec, task, &self.services).await)
            }
        }
    }
}

/// Convenience helper so the supervisor's trigger vocabulary travels cleanly
/// to the `TaskRunRecord` column.
#[inline]
pub fn trigger_as_str(t: TaskRunTrigger) -> &'static str {
    t.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Minimal struct-construction test proving the crate links and the
    // `SupervisorServices` closures compose with the supervisor's pub API.
    // A fully-working `TaskRunSupervisor::run` requires a live DB +
    // `MirrorManager`, exercised by the `phase1_supervisor` integration test
    // in `djinn-agent`.
    #[test]
    fn services_struct_constructs_with_noop_callbacks() {
        let load_task: LoadTaskFn = Arc::new(|_id: String| {
            Box::pin(async move { Err("noop".to_string()) })
                as BoxFut<'static, Result<Task, String>>
        });
        let execute_stage: ExecuteStageFn = Arc::new(|_t, _w, _r, _id, _s, _svc| {
            Box::pin(async move {
                Ok(StageOutcome::Failed {
                    reason: "noop".into(),
                })
            }) as BoxFut<'_, Result<StageOutcome, StageError>>
        });
        let open_pr: OpenPrFn = Arc::new(|_s, _t, _svc| {
            Box::pin(async move { TaskRunOutcome::Interrupted }) as BoxFut<'_, TaskRunOutcome>
        });

        let _services = SupervisorServices {
            cancel: CancellationToken::new(),
            load_task_fn: load_task,
            execute_stage_fn: execute_stage,
            open_pr_fn: open_pr,
        };

        // Sanity-check the StageOutcome terminal classifier so the test
        // actually executes code from this crate (not just type-level
        // plumbing).
        assert!(StageOutcome::Failed { reason: "x".into() }.is_terminal());
        assert!(!StageOutcome::WorkerDone.is_terminal());

        // Round-trip a TaskRunSpec through the re-exports to confirm they
        // point at the runtime types.
        let spec = TaskRunSpec {
            task_id: "t".into(),
            project_id: "p".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/t".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
        };
        assert_eq!(spec.flow.as_str(), "new_task");
    }
}

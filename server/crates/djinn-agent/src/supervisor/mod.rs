//! Task-run supervisor: orchestrates one full execution of a task across its
//! role sequence (planner → worker → reviewer → verifier → open PR, or
//! variants for spikes / conflict retries / review responses).
//!
//! This replaces per-agent-dispatch lifecycle: today's `run_task_lifecycle`
//! runs once per role; a `TaskRunSupervisor` runs once per *task-run* and
//! internally sequences the role phases, reusing the same workspace across
//! them so warm build caches compound naturally between stages.
//!
//! Phase 1 status: the sequencing is wired end-to-end through
//! [`stage::execute_stage`], but the path is additive — consumers must
//! explicitly call `TaskRunSupervisor::run` to use it.  Production traffic
//! still travels `run_task_lifecycle` under the old coordinator dispatch; the
//! coordinator rewrite (task #7) will swap the default path.

use std::sync::Arc;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_db::{SessionRepository, TaskRepository, TaskRunRepository};
use djinn_workspace::{MirrorError, MirrorManager, Workspace, WorkspaceError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub use self::flow::{RoleKind, SupervisorFlow};
pub use self::spec::{TaskRunOutcome, TaskRunReport, TaskRunSpec};
pub use self::stage::{StageError, StageOutcome};

use crate::actors::slot::helpers::load_task;
use crate::context::AgentContext;
use crate::roles::RoleRegistry;

mod flow;
mod pr;
mod spec;
mod stage;

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

/// Dependencies shared across every stage in a task-run.
///
/// The supervisor owns this bundle for the lifetime of a `run()` call and
/// passes it by reference into each [`stage::execute_stage`] invocation.
/// Anything that is role-scoped or stage-scoped (provider, session record,
/// conversation) is built inside `execute_stage` from these services.
#[derive(Clone)]
pub struct SupervisorServices {
    pub agent_context: AgentContext,
    pub role_registry: Arc<RoleRegistry>,
    pub session_repo: Arc<SessionRepository>,
    pub task_repo: Arc<TaskRepository>,
    /// Supervisor-wide cancellation.  Flagged when the task-run is torn down
    /// (server shutdown, user kill).  The per-stage reply loop treats this
    /// as both `cancel` and `global_cancel` in the Phase 1 path.
    pub cancel: CancellationToken,
}

impl SupervisorServices {
    pub fn new(agent_context: AgentContext, cancel: CancellationToken) -> Self {
        let session_repo = Arc::new(SessionRepository::new(
            agent_context.db.clone(),
            agent_context.event_bus.clone(),
        ));
        let task_repo = Arc::new(TaskRepository::new(
            agent_context.db.clone(),
            agent_context.event_bus.clone(),
        ));
        let role_registry = agent_context.role_registry.clone();
        Self {
            agent_context,
            role_registry,
            session_repo,
            task_repo,
            cancel,
        }
    }
}

pub struct TaskRunSupervisor {
    task_runs: Arc<TaskRunRepository>,
    mirror: Arc<MirrorManager>,
    services: SupervisorServices,
}

impl TaskRunSupervisor {
    /// Construct a supervisor bound to the given services.
    ///
    /// `task_runs` and `mirror` stay separate arguments because they are
    /// infrastructure the supervisor owns directly (create the task-run row,
    /// clone the workspace) rather than role-scoped services.
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

        let task = load_task(&spec.task_id, &self.services.agent_context)
            .await
            .map_err(|e| SupervisorError::LoadTask(e.to_string()))?;

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

    /// Execute the role sequence against the shared workspace, calling
    /// [`stage::execute_stage`] for each role.
    ///
    /// Short-circuits on terminal outcomes (close / escalate / rejection /
    /// failure).  On a clean traversal, flows that land a PR (`NewTask`,
    /// `ReviewResponse`, `ConflictRetry`) invoke [`pr::supervisor_pr_open`]
    /// to squash-merge and open a GitHub PR; `Spike` / `Planning` synthesize
    /// a `Closed` outcome directly because they don't touch the remote.
    async fn run_sequence(
        &self,
        spec: &TaskRunSpec,
        task: &djinn_core::models::Task,
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

            let outcome = stage::execute_stage(
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
        // semantics; the merge-landing flows go through `supervisor_pr_open`.
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
                Ok(pr::supervisor_pr_open(spec, task, &self.services).await)
            }
        }
    }
}

/// Convenience helper so the supervisor's trigger vocabulary travels cleanly
/// to the `TaskRunRecord` column. Mirrors `TaskRunTrigger::as_str` but takes
/// ownership-free.
#[inline]
pub fn trigger_as_str(t: TaskRunTrigger) -> &'static str {
    t.as_str()
}

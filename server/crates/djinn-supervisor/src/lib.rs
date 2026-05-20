//! `djinn-supervisor` — task-run orchestration body extracted from
//! `djinn-agent::supervisor` during Phase 2 PR 2 of
//! `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! This crate owns the orchestration skeleton (`TaskRunSupervisor`,
//! `SupervisorServices`, `StageOutcome`, `StageError`, `SupervisorError`) but
//! does **not** depend on `djinn-agent` — that would be a cycle because
//! `djinn-agent` now re-exports this crate under `djinn_agent::supervisor::*`.
//!
//! ## Phase 2 PR 3: SupervisorServices is a trait
//!
//! PR 2 left `SupervisorServices` as a struct-with-callbacks (`Arc<dyn Fn …>`
//! fields for `load_task_fn` / `execute_stage_fn` / `open_pr_fn`). PR 3 swaps
//! that shape for an object-safe trait (see [`services::SupervisorServices`])
//! with two impls:
//!
//! - `djinn_agent::direct_services::DirectServices` — wraps `AgentContext`,
//!   delegates straight into the in-tree lifecycle helpers. Production path
//!   and the `phase1_supervisor` integration test.
//! - [`services::rpc::StubRpcServices`] — a placeholder that pins the trait
//!   layout ahead of PR 4/5's real bincode-over-unix-socket worker wiring.
//!   Every method `unimplemented!()`s today.
//!
//! The supervisor holds the services behind `Arc<dyn SupervisorServices>`
//! (rather than a generic `S: SupervisorServices`) because PR 4/5's dispatch
//! story reuses the same `Arc` plumbing on the host side to hand the
//! supervisor to a `SessionRuntime`.

use std::sync::Arc;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_workspace::{EphemeralWorkspaceError, GitIdentity, MirrorError, MirrorManager};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

pub mod services;

pub use services::SupervisorServices;
pub use services::rpc::{
    ConnectTcpError, RpcBackgroundTasks, RpcServices, StubRpcServices, UnimplementedRpcServices,
};
pub use services::server::{
    AllowAllValidator, ConnectionRegistry, DenyAllValidator, ExpectedTokenValidator,
    PendingConnection, PendingConnectionParts, ServeHandle, TokenValidation, TokenValidator,
    serve_on_tcp, serve_on_unix_socket,
};
pub use services::wire::{
    AuthHelloMsg, AuthResultMsg, Frame, FramePayload, SerializableCreateTaskRunParams,
    ServiceRpcRequest, ServiceRpcResponse,
};

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
    Workspace(#[from] EphemeralWorkspaceError),

    #[error("db: {0}")]
    Db(#[from] djinn_db::Error),

    #[error("load task: {0}")]
    LoadTask(String),

    #[error("create task_run: {0}")]
    CreateTaskRun(String),

    #[error("update task_run status: {0}")]
    UpdateTaskRunStatus(String),

    #[error("stage: {0}")]
    Stage(#[from] StageError),
}

/// Pre-reply-loop failure surfaced by [`SupervisorServices::execute_stage`].
/// Always fatal for the task-run.
///
/// `Serialize + Deserialize` are derived (PR 5) so the variant can ride the
/// bincode RPC envelope between worker and launcher.  The carried strings
/// are all plain `String`s — no non-serializable fields hide here today, so
/// a `#[serde(untagged)]` wrapper is not required.
#[derive(Clone, Debug, Error, Serialize, Deserialize)]
pub enum StageError {
    #[error("model resolution: {0}")]
    ModelResolution(String),

    #[error("setup/verification: {0}")]
    Setup(String),

    #[error("session create: {0}")]
    SessionCreate(String),
}

/// Outcome of executing one role stage.
///
/// `Serialize + Deserialize` are derived (PR 5) so the variant can ride the
/// bincode RPC envelope between worker and launcher.
#[derive(Clone, Debug, Serialize, Deserialize)]
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

// ── TaskRunSupervisor ────────────────────────────────────────────────────────

pub struct TaskRunSupervisor {
    mirror: Arc<MirrorManager>,
    services: Arc<dyn SupervisorServices>,
}

impl TaskRunSupervisor {
    /// Construct a supervisor bound to the given services.
    ///
    /// Phase 4 of `~/.claude/plans/phase2-worker-execution-architecture.md`:
    /// the supervisor no longer holds an `Arc<TaskRunRepository>` directly.
    /// `task_run` row writes are routed through
    /// [`SupervisorServices::create_task_run`] /
    /// [`SupervisorServices::update_task_run_status`] so the worker pod —
    /// which has no DB connection — can construct a supervisor and ship
    /// those writes back through the RPC channel.
    pub fn new(mirror: Arc<MirrorManager>, services: Arc<dyn SupervisorServices>) -> Self {
        Self { mirror, services }
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

        // RPC-failure-during-cancellation policy: when any host-bound RPC
        // fails *and* the shared cancel token is already set, treat the
        // failure as the user-initiated cancel path (build an Interrupted
        // report) instead of bubbling a SupervisorError that would make
        // the worker exit non-zero.  When cancel is NOT set, the failure
        // stays fatal — that's a genuine RPC malfunction worth surfacing.
        //
        // The early-cancel branches still hand off through
        // `finalize_interrupted`, which always attempts the terminal
        // `update_task_run_status` RPC.  Without that, a cancel arriving
        // during `create_task_run` or `load_task` would skip the status
        // write entirely and leave the host's `task_runs` row stuck at
        // `running`.
        if let Err(e) = self
            .services
            .create_task_run(SerializableCreateTaskRunParams {
                id: run_id.clone(),
                project_id: spec.project_id.clone(),
                task_id: spec.task_id.clone(),
                trigger_type: trigger_str.clone(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
        {
            if self.services.cancel().is_cancelled() {
                debug!(
                    task_run_id = %run_id,
                    error = %e,
                    "create_task_run failed during cancellation"
                );
                return self.finalize_interrupted(run_id, vec![]).await;
            }
            return Err(SupervisorError::CreateTaskRun(e));
        }

        let workspace = self
            .mirror
            .clone_ephemeral(&spec.project_id, &spec.base_branch)
            .await?;
        debug!(task_run_id = %run_id, path = ?workspace.path(), "ephemeral workspace ready");

        let task = match self.services.load_task(spec.task_id.clone()).await {
            Ok(task) => task,
            Err(e) => {
                if self.services.cancel().is_cancelled() {
                    debug!(
                        task_run_id = %run_id,
                        error = %e,
                        "load_task failed during cancellation"
                    );
                    return self.finalize_interrupted(run_id, vec![]).await;
                }
                return Err(SupervisorError::LoadTask(e));
            }
        };

        // The ephemeral workspace is cloned on spec.base_branch; create the
        // task_branch as a local ref now so commits in this run land on it
        // and `push_to_origin(task_branch)` has something to push. Without
        // this, all worker stages commit to the base branch and the eventual
        // push fails with `src refspec task/<short_id> does not match any`.
        if let Err(e) = workspace.ensure_branch(&spec.task_branch).await {
            tracing::warn!(
                task_run_id = %run_id,
                task_id = %spec.task_id,
                branch = %spec.task_branch,
                error = %e,
                "supervisor: ensure_branch failed (push will likely fail later)"
            );
        }

        let sequence = spec.flow.role_sequence();
        let mut completed: Vec<RoleKind> = Vec::new();
        let outcome = {
            let mut last_stage_role: Option<RoleKind> = None;
            let mut result: Option<TaskRunOutcome> = None;
            for &role_kind in sequence {
                if self.services.cancel().is_cancelled() {
                    result = Some(TaskRunOutcome::Interrupted);
                    break;
                }

                let stage_outcome = match self
                    .services
                    .execute_stage(&task, &workspace, role_kind, &run_id, &spec)
                    .await
                {
                    Ok(o) => o,
                    Err(e) => {
                        // Stage failure during an in-flight cancellation
                        // is the expected shape: `execute_stage` saw the
                        // CancellationToken flip and tore its provider /
                        // RPC dependencies down with an error.  Surface
                        // an Interrupted outcome rather than a fatal
                        // SupervisorError so the worker exits cleanly.
                        if self.services.cancel().is_cancelled() {
                            debug!(
                                task_run_id = %run_id,
                                error = %e,
                                role = %role_kind.as_str(),
                                "execute_stage failed during cancellation; \
                                 returning Interrupted outcome"
                            );
                            result = Some(TaskRunOutcome::Interrupted);
                            break;
                        }
                        return Err(SupervisorError::from(e));
                    }
                };

                last_stage_role = Some(role_kind);
                completed.push(role_kind);

                match stage_outcome {
                    StageOutcome::WorkerDone | StageOutcome::ArchitectDone => {
                        // The worker/architect just wrote files. Auto-commit
                        // before advancing so the verifier sees real changes
                        // and `push_to_origin` has something to push. Empty
                        // diffs are a no-op (workspace.commit returns false).
                        let identity = GitIdentity {
                            name: "djinn-bot",
                            email: "bot@djinn.local",
                        };
                        let message = format!(
                            "{}: {}",
                            task.short_id, task.title
                        );
                        match workspace.commit(&message, identity).await {
                            Ok(true) => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    task_run_id = %run_id,
                                    role = %role_kind.as_str(),
                                    "supervisor: committed worker/architect changes"
                                );
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    task_id = %task.short_id,
                                    task_run_id = %run_id,
                                    role = %role_kind.as_str(),
                                    "supervisor: no changes to commit after stage"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    task_run_id = %run_id,
                                    role = %role_kind.as_str(),
                                    error = %e,
                                    "supervisor: workspace.commit failed (continuing to next stage)"
                                );
                            }
                        }
                    }
                    StageOutcome::PlannerExecute
                    | StageOutcome::ReviewerApproved
                    | StageOutcome::VerifierPassed => {
                        // Advance to the next stage. No file changes expected
                        // from planner/reviewer/verifier — they're read-only.
                    }
                    StageOutcome::PlannerClose { reason } => {
                        result = Some(TaskRunOutcome::Closed { reason });
                        break;
                    }
                    StageOutcome::Escalate { reason } => {
                        result = Some(TaskRunOutcome::Escalated { reason });
                        break;
                    }
                    StageOutcome::ReviewerRejected { feedback } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: "reviewer".into(),
                            reason: format!("review rejected: {feedback}"),
                        });
                        break;
                    }
                    StageOutcome::VerifierFailed { reason } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: "verifier".into(),
                            reason,
                        });
                        break;
                    }
                    StageOutcome::Failed { reason } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: role_kind.as_str().into(),
                            reason,
                        });
                        break;
                    }
                }
            }

            info!(
                task_run_id = %run_id,
                task_id = %spec.task_id,
                flow = ?spec.flow,
                last_stage_role = ?last_stage_role,
                result_is_some = result.is_some(),
                "supervisor: stage loop exited; computing final outcome"
            );
            match result {
                Some(r) => {
                    info!(
                        task_run_id = %run_id,
                        outcome = ?r,
                        "supervisor: early-exit outcome from stage loop"
                    );
                    r
                }
                None => {
                    // All stages completed successfully.  Spike / Planning
                    // have no PR semantics; the merge-landing flows go
                    // through `open_pr`.
                    match spec.flow {
                        SupervisorFlow::Planning | SupervisorFlow::Spike => {
                            TaskRunOutcome::Closed {
                                reason: format!(
                                    "{} flow completed (last stage: {:?})",
                                    spec.flow.as_str(),
                                    last_stage_role
                                ),
                            }
                        }
                        SupervisorFlow::NewTask
                        | SupervisorFlow::ReviewResponse
                        | SupervisorFlow::ConflictRetry => {
                            info!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                flow = ?spec.flow,
                                "supervisor: invoking services.open_pr"
                            );
                            let outcome = self.services.open_pr(&spec, &task).await;
                            info!(
                                task_run_id = %run_id,
                                outcome = ?outcome,
                                "supervisor: services.open_pr returned"
                            );
                            outcome
                        }
                    }
                }
            }
        };

        let terminal_status = match &outcome {
            TaskRunOutcome::PrOpened { .. } | TaskRunOutcome::Closed { .. } => {
                TaskRunStatus::Completed
            }
            TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
            TaskRunOutcome::Failed { .. } => TaskRunStatus::Failed,
            TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
        };
        // On the cancellation path the host-bound RPC channel may already
        // be torn down (the reader loop saw `Control(Cancel)` and the
        // writer's `cancelled()` branch shut the write half).  In that
        // case `update_task_run_status` returns a transport-level error
        // and we must still produce an `Interrupted` `TaskRunReport` so
        // the worker exits cleanly and the host's per-task-run dispatch
        // can pair it with the `KubernetesRuntime::teardown` path.  When
        // cancel is NOT set, an update_task_run_status failure stays
        // fatal — that's a genuine RPC malfunction worth surfacing.
        if let Err(e) = self
            .services
            .update_task_run_status(run_id.clone(), terminal_status)
            .await
        {
            if self.services.cancel().is_cancelled() {
                debug!(
                    task_run_id = %run_id,
                    error = %e,
                    "update_task_run_status failed during cancellation; \
                     proceeding with Interrupted report"
                );
            } else {
                return Err(SupervisorError::UpdateTaskRunStatus(e));
            }
        }

        info!(task_run_id = %run_id, ?outcome, "task-run finished");
        Ok(TaskRunReport {
            task_run_id: run_id,
            outcome,
            stages_completed: completed,
        })
    }

    /// Best-effort terminal status write for an early-cancelled run.
    ///
    /// Called from `run` when a host-bound RPC fails *during* an active
    /// cancellation, before the supervisor would otherwise reach the
    /// stage for-loop's natural cancel-check (and therefore before the
    /// trailing `update_task_run_status` at the bottom of `run`).  The
    /// helper always attempts the terminal RPC so the host's
    /// `task_runs.status` row flips to `interrupted` regardless of which
    /// stage tripped the cancel.  A failure on this last RPC is
    /// swallowed — the cancellation IS the success, and a transport
    /// error here just means the host's per-task-run dispatch will fall
    /// back to its Job-status polling path.
    async fn finalize_interrupted(
        &self,
        run_id: String,
        stages_completed: Vec<RoleKind>,
    ) -> Result<TaskRunReport, SupervisorError> {
        if let Err(e) = self
            .services
            .update_task_run_status(run_id.clone(), TaskRunStatus::Interrupted)
            .await
        {
            debug!(
                task_run_id = %run_id,
                error = %e,
                "finalize_interrupted: update_task_run_status failed; \
                 host will fall back to Job-status polling"
            );
        }
        info!(task_run_id = %run_id, "task-run interrupted (early-cancel path)");
        Ok(TaskRunReport {
            task_run_id: run_id,
            outcome: TaskRunOutcome::Interrupted,
            stages_completed,
        })
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

    /// Compile-time assertion: `SupervisorServices` is object-safe.
    ///
    /// PR 3 dispatches the supervisor through `Arc<dyn SupervisorServices>`,
    /// so the trait must stay object-safe forever. If a new method sneaks in
    /// with a generic parameter or a `Self`-by-value receiver, this function
    /// stops compiling.
    #[allow(dead_code)]
    fn _obj_safe(_: &dyn SupervisorServices) {}

    #[test]
    fn stage_outcome_terminal_classifier() {
        assert!(StageOutcome::Failed { reason: "x".into() }.is_terminal());
        assert!(StageOutcome::PlannerClose { reason: "x".into() }.is_terminal());
        assert!(StageOutcome::Escalate { reason: "x".into() }.is_terminal());
        assert!(StageOutcome::ReviewerRejected { feedback: "x".into() }.is_terminal());
        assert!(StageOutcome::VerifierFailed { reason: "x".into() }.is_terminal());
        assert!(!StageOutcome::WorkerDone.is_terminal());
        assert!(!StageOutcome::PlannerExecute.is_terminal());
        assert!(!StageOutcome::ReviewerApproved.is_terminal());
        assert!(!StageOutcome::VerifierPassed.is_terminal());
        assert!(!StageOutcome::ArchitectDone.is_terminal());
    }
}

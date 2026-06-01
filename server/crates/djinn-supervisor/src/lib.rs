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
use djinn_workspace::{EphemeralWorkspaceError, GitIdentity, MergeOutcome, MirrorError, MirrorManager};
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
    Failed {
        reason: String,
        /// Set when the stage failed on a typed provider error the host
        /// circuit-breaker should act on (classified in
        /// `djinn_agent::supervisor_impl::stage`). Rides the bincode RPC frame
        /// so the host (`supervisor_runner.rs`) can feed the per-`(scope, model)`
        /// breaker; `None` for non-provider stage failures. `#[serde(default)]`
        /// keeps older frames decoding.
        #[serde(default)]
        provider_failure: Option<djinn_runtime::ProviderFailureClass>,
    },
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
        // Use the host-minted canonical id rather than minting our own — the
        // host's runtime, the `task_runs` row, every session, and the terminal
        // report must all share ONE id so post-session extraction can match
        // sessions back to the run the host dispatched.
        let run_id = spec.task_run_id.clone();
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

        // Try to clone on task_branch first (preserves prior cycle's commits
        // in the mirror — workspace.push_to_origin writes them back after
        // each successful run). Fall back to base_branch only on the very
        // first cycle of a task, when task_branch doesn't exist yet.
        //
        // The previous shape ("always clone on base_branch + ensure_branch")
        // silently RESET task_branch to base_branch's HEAD on every re-run,
        // throwing away every prior cycle's worker progress. Observed on
        // task avoy: 3/3 ACs met in cycle 1, dropped to 1/3 in cycle 2 after
        // CI bounced the task back to open.
        let workspace = match self
            .mirror
            .clone_ephemeral(&spec.project_id, &spec.task_branch)
            .await
        {
            Ok(ws) => {
                debug!(
                    task_run_id = %run_id,
                    branch = %spec.task_branch,
                    path = ?ws.path(),
                    "ephemeral workspace ready (continuing on existing task_branch)"
                );
                ws
            }
            Err(e) => {
                debug!(
                    task_run_id = %run_id,
                    branch = %spec.task_branch,
                    error = %e,
                    "task_branch not in mirror; cloning on base_branch (first cycle)"
                );
                self.mirror
                    .clone_ephemeral(&spec.project_id, &spec.base_branch)
                    .await?
            }
        };

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

        // First-cycle clone landed us on base_branch (or a re-clone where
        // task_branch doesn't yet exist locally). `git checkout -B
        // task/<short_id>` from base_branch's HEAD bootstraps a fresh
        // task branch; on the task_branch clone path it's a no-op (the
        // branch is already checked out from the mirror's HEAD).
        if let Err(e) = workspace.ensure_branch(&spec.task_branch).await {
            tracing::warn!(
                task_run_id = %run_id,
                task_id = %spec.task_id,
                branch = %spec.task_branch,
                error = %e,
                "supervisor: ensure_branch failed (push will likely fail later)"
            );
        }

        // ConflictRetry pre-merge.  Without this the K8s worker pod sees a
        // clean checkout of task_branch and gets a list of conflicting file
        // paths in its prompt but no on-disk merge state — and dutifully
        // refixes the latest CI signal in the activity log instead of
        // resolving the conflict, because the conflict is invisible to its
        // editing tools.  Doing `git merge --no-commit --no-ff origin/<base>`
        // here leaves standard <<<<<<<<=======>>>>>>>> markers on disk in the
        // conflicting files for the worker to edit out, and leaves
        // `.git/MERGE_HEAD` set so the post-worker auto-commit produces a
        // proper merge commit.  Failures are logged-and-skipped: the worker
        // still runs (just without the merge state), preserving the previous
        // behavior for that path.
        if spec.trigger == TaskRunTrigger::ConflictRetry {
            match workspace.try_merge(&spec.base_branch).await {
                Ok(MergeOutcome::Clean) => {
                    info!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        target = %spec.base_branch,
                        "supervisor: ConflictRetry pre-merge applied cleanly; staged result will be committed alongside any worker edits"
                    );
                }
                Ok(MergeOutcome::Conflicts { files }) => {
                    info!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        target = %spec.base_branch,
                        conflict_count = files.len(),
                        conflicting_files = ?files,
                        "supervisor: ConflictRetry pre-merge left conflicts in worker workspace"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        target = %spec.base_branch,
                        error = %e,
                        "supervisor: ConflictRetry pre-merge failed; worker will run without merge state on disk"
                    );
                }
            }
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

                // Walk the task through its DB status machine. Pre-stage
                // transitions move the row from open → in_progress (Worker)
                // or needs_task_review → in_task_review (Reviewer) so the
                // host-side planner stops seeing the task as "still open
                // work" and re-dispatching during the run. Failure is
                // logged-and-skipped — a re-dispatched run may already be
                // in the target status, which is `InvalidTransition` on
                // the second call but functionally idempotent.
                let pre_stage_action: Option<&'static str> = match role_kind {
                    RoleKind::Worker => Some("start"),
                    RoleKind::Reviewer => Some("task_review_start"),
                    // Planner grooms open planning/review tasks. Move them
                    // open → in_progress so (a) the host coordinator stops
                    // seeing them as ready and re-dispatching concurrently
                    // during the 60-90s run (the source of the Interrupted
                    // churn — multiple runs for one task fighting over the
                    // single slot), and (b) the post-planner `close`
                    // transition below is valid (close from in_progress).
                    RoleKind::Planner => Some("start"),
                    _ => None,
                };
                if let Some(action) = pre_stage_action
                    && let Err(e) = self
                        .services
                        .transition_task(spec.task_id.clone(), action.into(), None)
                        .await
                {
                    tracing::warn!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        role = %role_kind.as_str(),
                        action = %action,
                        error = %e,
                        "supervisor: pre-stage status transition skipped (likely already in target state)"
                    );
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
                        // Author as the task's creator (resolved host-side at
                        // dispatch) so the commit is attributed to the human and
                        // Vercel's commit-author check authorizes the build. The
                        // PR is still opened by the App, so the creator can
                        // review/approve their own commits. Falls back to the bot
                        // identity for system/patrol tasks with no human creator.
                        let identity = GitIdentity {
                            name: spec.commit_author_name.as_deref().unwrap_or("djinn-bot"),
                            email: spec
                                .commit_author_email
                                .as_deref()
                                .unwrap_or("bot@djinn.local"),
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
                                // Push the new commit to the mirror IMMEDIATELY
                                // — before the subsequent stage (reviewer) runs
                                // and before the post-stage transitions fire.
                                // Without this, the worker's commits live only
                                // in the ephemeral workspace until open_pr's
                                // own push_to_origin runs at the very end. If
                                // anything cancels between now and open_pr
                                // (server restart, planner kill, cancel-token
                                // race), the commits are LOST and the host's
                                // coordinator-tick supervisor_pr_open sees
                                // the stale mirror task_branch — pushing the
                                // OLD commit to the PR. Observed on avoy:
                                // worker fixed CI failure in cycle 2, but PR
                                // #502 stayed pinned to cycle 1's commit
                                // because the supervisor body never reached
                                // its terminal open_pr.
                                //
                                // Eager push makes the commit durable to
                                // the mirror right after the stage that
                                // produced it. Idempotent (refspec
                                // task_branch:task_branch). Best-effort —
                                // a push failure here only means we'll
                                // re-try at open_pr time; the run keeps
                                // going.
                                if let Err(e) = workspace
                                    .push_to_origin(&spec.task_branch)
                                    .await
                                {
                                    tracing::warn!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        role = %role_kind.as_str(),
                                        branch = %spec.task_branch,
                                        error = %e,
                                        "supervisor: eager push_to_origin failed (open_pr will retry)"
                                    );
                                } else {
                                    tracing::debug!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        role = %role_kind.as_str(),
                                        branch = %spec.task_branch,
                                        "supervisor: pushed worker commit to mirror eagerly"
                                    );
                                }
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
                        // Worker finished cleanly → submit_task_review
                        // (in_progress → needs_task_review). Architect has no
                        // analogous transition in the current state machine.
                        //
                        // Gate on the cancel token: a stall-kill / preempt can
                        // flip cancel mid-stage and the agent may still emit a
                        // late StageOutcome. We must NOT advance the task on a
                        // cancelled run — doing so walked tasks all the way to
                        // `approved` with no pushed task_branch (the kw7s
                        // PR-open loop). Leave it in_progress for redispatch.
                        if role_kind == RoleKind::Worker {
                            if self.services.cancel().is_cancelled() {
                                tracing::debug!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    "supervisor: run cancelled — skipping submit_task_review (task stays in_progress for redispatch)"
                                );
                            } else if let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "submit_task_review".into(),
                                    None,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %e,
                                    "supervisor: post-worker submit_task_review transition skipped"
                                );
                            }
                        }
                    }
                    StageOutcome::ReviewerApproved => {
                        // Reviewer signed off → task_review_approve
                        // (in_task_review → approved). The host's PR-open
                        // path then fires pr_created (approved → pr_draft).
                        //
                        // Cancel-gated for the same reason as submit_task_review
                        // above: a cancelled reviewer run must not approve a task
                        // (that's how kw7s reached `approved` with no branch).
                        if self.services.cancel().is_cancelled() {
                            tracing::debug!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                "supervisor: run cancelled — skipping task_review_approve (task stays in_task_review for redispatch)"
                            );
                        } else if let Err(e) = self
                            .services
                            .transition_task(
                                spec.task_id.clone(),
                                "task_review_approve".into(),
                                None,
                            )
                            .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: task_review_approve transition skipped"
                            );
                        }
                    }
                    StageOutcome::PlannerExecute | StageOutcome::VerifierPassed => {
                        // Planner finished a "decision=execute" submit_grooming —
                        // close the planning/review task so the coordinator's
                        // ready-task sweep doesn't keep re-dispatching it. The
                        // legacy slot lifecycle used to do this via
                        // role.on_complete → apply_transition_and_dispatch;
                        // commit 4de6f49c7 stripped that to fix the
                        // worker/reviewer race and accidentally left planner
                        // tasks dispatching every ~30s in a tight loop
                        // (observed on patrol planning task k4my: 10 sessions
                        // in 8 minutes).
                        //
                        // Planner tasks use the SIMPLE lifecycle
                        // (open → in_progress → closed); their terminal state
                        // is `closed`, not `approved`. The earlier intent of
                        // `submit_for_merge` (→ approved) was wrong: it routes
                        // the task into process_approved_tasks' PR pipeline,
                        // which a plan/groom task has no durable artifacts for
                        // — a fresh re-dispatch loop. The pre-stage `start`
                        // above moved the task to in_progress, so `close` is a
                        // valid transition here.
                        //   planning|decomposition|review → close
                        //   other                         → no transition
                        if role_kind == RoleKind::Planner {
                            let action = match task.issue_type.as_str() {
                                "planning" | "decomposition" | "review" => Some("close"),
                                _ => None,
                            };
                            if let Some(action) = action
                                && let Err(e) = self
                                    .services
                                    .transition_task(
                                        spec.task_id.clone(),
                                        action.into(),
                                        None,
                                    )
                                    .await
                            {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    issue_type = %task.issue_type,
                                    action = %action,
                                    error = %e,
                                    "supervisor: post-planner transition skipped"
                                );
                            }
                        }
                    }
                    StageOutcome::PlannerClose { reason } => {
                        result = Some(TaskRunOutcome::Closed { reason: reason.clone() });
                        // Also fire a real DB transition so the task row
                        // matches the run outcome. Same issue-type-aware
                        // routing as PlannerExecute.
                        if role_kind == RoleKind::Planner {
                            let action = match task.issue_type.as_str() {
                                "planning" | "decomposition" | "review" => Some("close"),
                                _ => None,
                            };
                            if let Some(action) = action
                                && let Err(e) = self
                                    .services
                                    .transition_task(
                                        spec.task_id.clone(),
                                        action.into(),
                                        Some(reason),
                                    )
                                    .await
                            {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    action = %action,
                                    error = %e,
                                    "supervisor: planner-close transition skipped"
                                );
                            }
                        }
                        break;
                    }
                    StageOutcome::Escalate { reason } => {
                        result = Some(TaskRunOutcome::Escalated { reason });
                        break;
                    }
                    StageOutcome::ReviewerRejected { feedback } => {
                        // Reviewer rejected → task_review_reject
                        // (in_task_review → open). The reject action
                        // requires_reason, so pass the feedback string. The
                        // failed-run TaskRunOutcome reason includes the same
                        // feedback for caller log parity.
                        if let Err(e) = self
                            .services
                            .transition_task(
                                spec.task_id.clone(),
                                "task_review_reject".into(),
                                Some(feedback.clone()),
                            )
                            .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: task_review_reject transition skipped"
                            );
                        }
                        result = Some(TaskRunOutcome::Failed {
                            stage: "reviewer".into(),
                            reason: format!("review rejected: {feedback}"),
                            provider_failure: None,
                        });
                        break;
                    }
                    StageOutcome::VerifierFailed { reason } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: "verifier".into(),
                            reason,
                            provider_failure: None,
                        });
                        break;
                    }
                    StageOutcome::Failed {
                        reason,
                        provider_failure,
                    } => {
                        // Planner patrol tasks (issue_type=review) must close
                        // even on Failed — the LLM sometimes finishes without
                        // calling submit_grooming (StageOutcome::Failed via
                        // "finalized via unexpected tool", or no finalize at
                        // all), and the task otherwise stays `open` and the
                        // coordinator re-dispatches it every ~30s in a tight
                        // loop. Observed on n6k8 "Planner patrol: board
                        // health review" after k4my had the same pattern.
                        if role_kind == RoleKind::Planner
                            && task.issue_type == "review"
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "close".into(),
                                    Some(reason.clone()),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: planner-Failed close transition skipped"
                            );
                        }
                        result = Some(TaskRunOutcome::Failed {
                            stage: role_kind.as_str().into(),
                            reason,
                            // Carry the typed provider-error class (if any) the
                            // reply loop produced through to the host report so
                            // the host breaker can act on it.
                            provider_failure,
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
                        SupervisorFlow::Spike => {
                            // The Architect is a read-only consultant (ADR-051):
                            // it records spike findings to memory but never
                            // transitions the board. So a completed Spike would
                            // otherwise stay `open`, and the coordinator would
                            // re-dispatch it every ~30s in a tight loop (the same
                            // failure mode the planner-review `close` above guards
                            // against) — and, worse, block every task that depends
                            // on the spike. Force-close it here on success.
                            // Planning closes via the Planner agent's own board
                            // transitions, so it needs no force-close.
                            let reason = format!(
                                "{} flow completed (last stage: {:?})",
                                spec.flow.as_str(),
                                last_stage_role
                            );
                            if let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "close".into(),
                                    Some(reason.clone()),
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %e,
                                    "supervisor: spike-completion close transition skipped"
                                );
                            }
                            TaskRunOutcome::Closed { reason }
                        }
                        SupervisorFlow::Planning => TaskRunOutcome::Closed {
                            reason: format!(
                                "{} flow completed (last stage: {:?})",
                                spec.flow.as_str(),
                                last_stage_role
                            ),
                        },
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
        assert!(
            StageOutcome::Failed {
                reason: "x".into(),
                provider_failure: None,
            }
            .is_terminal()
        );
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

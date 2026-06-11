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
use djinn_workspace::{
    EphemeralWorkspaceError, GitIdentity, MergeOutcome, MergeParentOutcome, MirrorError,
    MirrorManager,
};
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
    PlannerClose {
        reason: String,
    },
    ReviewerApproved,
    ReviewerRejected {
        feedback: String,
    },
    VerifierPassed,
    VerifierFailed {
        reason: String,
    },
    ArchitectDone,
    Escalate {
        reason: String,
    },
    /// Lead `submit_decision(decision="approve")` — the work is complete and
    /// correct, the worker just couldn't self-certify. Maps to `lead_approve`
    /// (in_lead_intervention → approved); the supervisor then opens/updates the
    /// PR (same terminal path as a reviewer approval). NOT terminal: it falls
    /// through to the post-loop `open_pr`.
    LeadApproved,
    /// Lead `submit_decision(decision="approve_conflict")` — approved, but a
    /// merge conflict was found. Maps to `lead_approve_conflict`
    /// (in_lead_intervention → open + conflict metadata) so the coordinator
    /// re-dispatches a conflict-retry run.
    LeadApproveConflict {
        reason: String,
    },
    /// Lead `submit_decision(decision="reopen")` — the task was rescoped /
    /// guided / blocked-on-deps and should retry with a fresh worker. Maps to
    /// `lead_intervention_complete` (in_lead_intervention → open).
    LeadReopen {
        reason: String,
    },
    /// Lead `submit_decision(decision="decompose"|"force_close")` — terminal
    /// closure of the original task (decompose: replacement subtasks were
    /// already created by the Lead via MCP; force_close: redundant /
    /// already-landed work). Maps to `force_close` (→ closed).
    LeadClose {
        reason: String,
    },
    /// Lead `submit_decision(decision="escalate")` — the Lead could not resolve
    /// the task and it needs board/Planner-level review. Maps to
    /// `lead_intervention_complete` (in_lead_intervention → open) so the task
    /// leaves the lead queue (no dead-end), and produces an `Escalated` run
    /// outcome. Distinct from `LeadClose` (board re-review vs terminal closure)
    /// per the design review. The standing reopen-count safety net routes
    /// persistently-failing tasks to the Planner.
    LeadEscalate {
        reason: String,
    },
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
                // Lead decisions that fire their own terminal board transition
                // and short-circuit the (single-stage) run. `LeadApproved` is
                // intentionally absent — it falls through to the post-loop
                // `open_pr`, the same shape as a reviewer approval.
                | StageOutcome::LeadApproveConflict { .. }
                | StageOutcome::LeadReopen { .. }
                | StageOutcome::LeadClose { .. }
                | StageOutcome::LeadEscalate { .. }
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

        // Reset tracked-file mtimes to their last-touched commit time so cargo's
        // path-crate fingerprints match the shared CARGO_TARGET_DIR across runs
        // (the ephemeral clone gave every file a fresh checkout mtime). Done
        // AFTER the branch is settled and BEFORE the proactive sync / stages: a
        // file the sync-merge then rewrites picks up a current mtime (it really
        // changed → legit recompile), while every byte-identical file keeps its
        // commit-time mtime and reuses the cached artifact. Best-effort; never
        // fails the run.
        workspace.normalize_mtimes().await;

        // Proactive dispatch-time sync.  The task branch is REUSED across
        // cycles (clone_ephemeral + ensure_branch above never recreate it), so
        // without re-anchoring it onto the moving target it drifts behind
        // `base_branch` and the task loops: every cycle re-reviews stale code,
        // the PR carries phantom diffs against the advanced target, and the
        // merge-queue keeps bouncing it.  We fix that by running the SAME merge
        // the ConflictRetry path uses (`origin/<base>` → task branch) at EVERY
        // dispatch, gated by a cheap no-op guard.
        //
        // Skipped for ConflictRetry, which already merges below with its own
        // (DB-sourced) conflict context.
        //
        // No-op guard: if `origin/<base>` is already an ancestor of HEAD the
        // branch is current — no merge, no commit, no churn.  This is the
        // first-cycle case (ensure_branch just cut the branch from
        // `origin/<base>`), and the steady state once a cycle has merged.
        //
        // Clean merge: `try_merge` stages the result with `--no-commit`.  The
        // downstream `WorkerDone` auto-commit would pick it up (MERGE_HEAD is
        // set), but we commit it HERE, explicitly, so the merge lands even when
        // the worker session makes NO further edits — a behind-base re-review
        // must still record the merge commit, otherwise the guard never trips
        // and the next cycle re-merges forever.  `git commit` with MERGE_HEAD
        // set produces a proper merge commit; it clears MERGE_HEAD, so the
        // worker's own later commit is an ordinary one on top.
        //
        // Conflicting merge: leave the markers on disk and fall through into
        // the worker stage exactly like ConflictRetry — the worker resolves the
        // conflict in-session via its editing tools, and the staged result is
        // committed by the post-worker auto-commit.  (The worker-prompt file
        // list is sourced independently from the task's DB
        // `merge_conflict_metadata` in `execute_stage`; the on-disk merge state
        // is what makes the conflict visible to the worker's tools.)
        //
        // All failures are logged-and-skipped: a fetch/merge hiccup must not
        // abort the run — the worker still runs against the (un-synced) branch,
        // preserving prior behavior.
        if spec.trigger != TaskRunTrigger::ConflictRetry {
            match workspace.is_up_to_date_with(&spec.base_branch).await {
                Ok(true) => {
                    debug!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        target = %spec.base_branch,
                        "supervisor: task branch already current with target; skipping proactive sync"
                    );
                }
                Ok(false) => match workspace.try_merge(&spec.base_branch).await {
                    Ok(MergeOutcome::Clean) => {
                        // Commit the staged merge now so it lands even with no
                        // worker edits. Author as the task creator (resolved
                        // host-side), mirroring the post-worker commit path;
                        // falls back to the bot identity for system tasks.
                        let identity = GitIdentity {
                            name: spec.commit_author_name.as_deref().unwrap_or("djinn-bot"),
                            email: spec
                                .commit_author_email
                                .as_deref()
                                .unwrap_or("bot@djinn.local"),
                        };
                        let message =
                            format!("Merge {} into {}", spec.base_branch, spec.task_branch);
                        match workspace.commit(&message, identity).await {
                            Ok(true) => {
                                info!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    target = %spec.base_branch,
                                    "supervisor: proactive sync merged target into task branch and committed the merge"
                                );
                                // Push the merge commit to the mirror EAGERLY.
                                // The post-worker auto-commit only pushes when
                                // it produced a commit (worker edits present);
                                // a behind-base re-review where the worker makes
                                // NO edits would otherwise leave this merge
                                // commit stranded in the ephemeral clone and the
                                // mirror's task_branch never advances — so the
                                // next cycle re-merges forever. Idempotent
                                // (task_branch:task_branch); best-effort — a
                                // failure here just means open_pr / the worker
                                // push retries.
                                if let Err(e) = workspace.push_to_origin(&spec.task_branch).await {
                                    tracing::warn!(
                                        task_run_id = %run_id,
                                        task_id = %spec.task_id,
                                        branch = %spec.task_branch,
                                        error = %e,
                                        "supervisor: proactive sync eager push failed (worker/open_pr push will retry)"
                                    );
                                }
                            }
                            Ok(false) => {
                                // try_merge staged a non-empty diff (we were
                                // behind), so this should not happen; if it
                                // does the merge produced no tree change and
                                // nothing needs committing.
                                debug!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    target = %spec.base_branch,
                                    "supervisor: proactive sync merge staged no tree change; nothing to commit"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    target = %spec.base_branch,
                                    error = %e,
                                    "supervisor: proactive sync merge commit failed; worker runs with merge staged but uncommitted"
                                );
                            }
                        }
                    }
                    Ok(MergeOutcome::Conflicts { files }) => {
                        info!(
                            task_run_id = %run_id,
                            task_id = %spec.task_id,
                            target = %spec.base_branch,
                            conflict_count = files.len(),
                            conflicting_files = ?files,
                            "supervisor: proactive sync left conflicts on disk; worker will resolve them in-session"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_run_id = %run_id,
                            task_id = %spec.task_id,
                            target = %spec.base_branch,
                            error = %e,
                            "supervisor: proactive sync merge failed; worker will run without merge state on disk"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        target = %spec.base_branch,
                        error = %e,
                        "supervisor: proactive sync up-to-date check failed; skipping sync"
                    );
                }
            }
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
        //
        // When the pre-merge leaves CONFLICTS for the worker, we snapshot the
        // exact SHA of `origin/<merge_target>` here (`pending_merge_target_sha`).
        // The post-worker `WorkerDone` arm uses it to ENFORCE that the worker's
        // resolution lands as a true two-parent merge commit — workers run
        // their own git commands and frequently clear `.git/MERGE_HEAD`
        // (`merge --abort` / `reset` on "unmerged paths") then hand-commit a
        // single parent, which leaves the branch's merge-base with the target
        // unchanged so GitHub keeps the PR CONFLICTING forever (the 3hrr loop).
        // See `Workspace::enforce_merge_parent`.
        let mut pending_merge_target_sha: Option<String> = None;
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
                    // Snapshot the merge target tip so the post-worker arm can
                    // re-assert it as the merge's second parent no matter what
                    // the model does to `.git` state during resolution.
                    match workspace
                        .resolve_ref(&format!("origin/{}", spec.base_branch))
                        .await
                    {
                        Ok(sha) => pending_merge_target_sha = Some(sha),
                        Err(e) => {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                target = %spec.base_branch,
                                error = %e,
                                "supervisor: ConflictRetry pre-merge could not snapshot merge-target SHA; merge-parent enforcement disabled for this run"
                            );
                        }
                    }
                    info!(
                        task_run_id = %run_id,
                        task_id = %spec.task_id,
                        target = %spec.base_branch,
                        conflict_count = files.len(),
                        conflicting_files = ?files,
                        merge_target_sha = ?pending_merge_target_sha,
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
                    // The Worker enters from one of two source states depending
                    // on the flow: a fresh/reopened task is `open` (walk
                    // `start`), but a `ReviewResponse` redo re-enters from
                    // `needs_task_review` (the worker already submitted once and
                    // its branch wasn't durable, so the host routed a redo rather
                    // than a reviewer-only resume). `start` is legal ONLY from
                    // `open`, so on the redo it no-op'd and the task stayed at
                    // `needs_task_review` — the post-worker `submit_verification`
                    // (legal only from `in_progress`) then also no-op'd and
                    // verification was skipped before review (task u4fx). Walk
                    // `resume_worker` (needs_task_review → in_progress) on the
                    // redo so `submit_verification` succeeds, exactly like the
                    // NewTask path.
                    RoleKind::Worker => {
                        if matches!(spec.flow, SupervisorFlow::ReviewResponse) {
                            Some("resume_worker")
                        } else {
                            Some("start")
                        }
                    }
                    RoleKind::Reviewer => Some("task_review_start"),
                    // Planner grooms open planning/review tasks. Move them
                    // open → in_progress so (a) the host coordinator stops
                    // seeing them as ready and re-dispatching concurrently
                    // during the 60-90s run (the source of the Interrupted
                    // churn — multiple runs for one task fighting over the
                    // single slot), and (b) the post-planner `close`
                    // transition below is valid (close from in_progress).
                    RoleKind::Planner => Some("start"),
                    // Spike runs the Architect as its sole stage. Move it
                    // open → in_progress so the board reflects the running
                    // architect — without this the spike sat `open` for the
                    // entire ~10min run and jumped straight to `closed`, so the
                    // FE never showed it as in-progress — and so the coordinator
                    // stops seeing it as ready and re-dispatching during the run
                    // (same rationale as Worker/Planner above). The
                    // spike-completion `close` below then fires from in_progress,
                    // a valid simple-lifecycle transition.
                    RoleKind::Architect => Some("start"),
                    // Lead intervention runs the Lead as its sole stage. Move
                    // the task needs_lead_intervention → in_lead_intervention so
                    // (a) the board reflects the active Lead, (b) the host
                    // coordinator stops seeing it as ready and re-dispatching
                    // during the run, and (c) the terminal lead_* transitions
                    // below (lead_approve / lead_intervention_complete /
                    // force_close) are valid — they all require the task to be
                    // in_lead_intervention.
                    RoleKind::Lead => Some("lead_intervention_start"),
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
                        let message = format!("{}: {}", task.short_id, task.title);
                        match workspace.commit(&message, identity).await {
                            Ok(true) => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    task_run_id = %run_id,
                                    role = %role_kind.as_str(),
                                    "supervisor: committed worker/architect changes"
                                );
                                // The push that makes this commit durable in the
                                // mirror happens UNCONDITIONALLY just below the
                                // match — see the comment there. We no longer
                                // push inside this arm: workers commonly commit
                                // their own edits via shell, leaving the tree
                                // clean and this arm un-taken, yet their work
                                // still needs pushing. One push site covers both.
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

                        // ── Merge-parent guarantee (ConflictRetry only) ─────
                        //
                        // On a ConflictRetry run the dispatch-time pre-merge
                        // staged a conflicted merge of `origin/<merge_target>`
                        // and snapshotted that tip as `pending_merge_target_sha`.
                        // The intended path is that the auto-commit above (with
                        // `.git/MERGE_HEAD` set) records a true two-parent merge.
                        // But workers run their own git commands and frequently
                        // clear MERGE_HEAD (`merge --abort` / `reset` on
                        // "unmerged paths") then hand-commit a single parent —
                        // the content is right but git history doesn't record
                        // the merge, so the branch's merge-base with the target
                        // is unchanged and GitHub keeps the PR CONFLICTING
                        // forever (the 3hrr infinite-retry loop).
                        //
                        // ENFORCE the merge here, regardless of what the model
                        // did to `.git` state: if the target isn't already an
                        // ancestor of HEAD, construct a two-parent
                        // "merge-completion" commit (tree = worker's resolved
                        // content, parents = [worker HEAD, merge_target_sha]).
                        // Then assert the ancestor property holds; if it STILL
                        // fails, fail the stage loudly rather than pushing a
                        // "resolution" that silently leaves the PR conflicting.
                        if role_kind == RoleKind::Worker
                            && let Some(merge_target_sha) = pending_merge_target_sha.as_deref()
                        {
                            let identity = GitIdentity {
                                name: spec.commit_author_name.as_deref().unwrap_or("djinn-bot"),
                                email: spec
                                    .commit_author_email
                                    .as_deref()
                                    .unwrap_or("bot@djinn.local"),
                            };
                            match workspace
                                .enforce_merge_parent(merge_target_sha, identity)
                                .await
                            {
                                Ok(MergeParentOutcome::AlreadyMerged) => {
                                    info!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        merge_target_sha,
                                        "supervisor: ConflictRetry merge parent already recorded by worker auto-commit (MERGE_HEAD survived)"
                                    );
                                }
                                Ok(MergeParentOutcome::Recovered { new_head }) => {
                                    info!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        merge_target_sha,
                                        new_head = %new_head,
                                        "supervisor: ConflictRetry worker lost the merge parent; reconstructed two-parent merge-completion commit"
                                    );
                                }
                                Err(e) => {
                                    // The resolution could not be turned into a
                                    // true merge (still-unmerged index, or the
                                    // ancestor assertion failed). Do NOT push a
                                    // conflicting "resolution" — fail loudly so
                                    // the run redispatches as ConflictRetry with
                                    // the metadata still set.
                                    tracing::error!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        role = %role_kind.as_str(),
                                        merge_target_sha,
                                        error = %e,
                                        "supervisor: ConflictRetry merge-parent enforcement FAILED — refusing to push an unmerged resolution; failing the stage for redispatch"
                                    );
                                    result = Some(TaskRunOutcome::Failed {
                                        stage: "worker".into(),
                                        reason: format!(
                                            "merge-parent enforcement failed: could not record a two-parent merge of {merge_target_sha} (the resolution would leave the PR conflicting): {e}"
                                        ),
                                        provider_failure: None,
                                    });
                                    break;
                                }
                            }
                        }

                        // Push task_branch to the mirror UNCONDITIONALLY after
                        // the stage — not just when the auto-commit above
                        // produced a new commit. Workers frequently commit their
                        // own edits via shell during the session, so the tree is
                        // already clean by the time we get here and
                        // `workspace.commit` returns Ok(false): the eager push
                        // inside the Ok(true) arm above is then skipped and the
                        // worker's commits live only in the ephemeral TempDir,
                        // lost if the Pod is deadline-killed / evicted before
                        // open_pr (the only other push). The mirror is a mounted
                        // PVC so the refspec `task_branch:task_branch` is cheap
                        // and idempotent when there's nothing new to push.
                        //
                        // Loud on failure (error!, not warn!) — a push failure
                        // here is the durability seam: the worker's progress is
                        // at risk of loss on kill. Still best-effort: the run
                        // keeps going (open_pr retries the push), it's just
                        // visible now.
                        if let Err(e) = workspace.push_to_origin(&spec.task_branch).await {
                            tracing::error!(
                                task_id = %task.short_id,
                                task_run_id = %run_id,
                                role = %role_kind.as_str(),
                                branch = %spec.task_branch,
                                error = %e,
                                "supervisor: post-stage push_to_origin failed — worker progress not yet durable in mirror (open_pr will retry)"
                            );
                        } else {
                            tracing::debug!(
                                task_id = %task.short_id,
                                task_run_id = %run_id,
                                role = %role_kind.as_str(),
                                branch = %spec.task_branch,
                                "supervisor: pushed task_branch to mirror after stage (durable)"
                            );
                        }
                        // Worker finished cleanly → submit_verification
                        // (in_progress → verifying). The run ends after this
                        // stage (the worker-only sequence has no reviewer leg);
                        // the HOST then spawns the slot-free verification
                        // pipeline against the durable task_branch. Verification
                        // green moves verifying → needs_task_review (which the
                        // coordinator re-dispatches as a reviewer-only
                        // ReviewResume), verification red releases the task for
                        // worker rework — so verification runs BETWEEN the
                        // worker and the reviewer, as designed. (Previously this
                        // fired `submit_task_review` and an in-pod reviewer leg
                        // ran back-to-back, so verification only happened later
                        // at the pre-PR gate — the bug this rewiring fixes.)
                        // Architect has no analogous transition in the current
                        // state machine.
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
                                    "supervisor: run cancelled — skipping submit_verification (task stays in_progress for redispatch)"
                                );
                            } else if let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "submit_verification".into(),
                                    None,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %e,
                                    "supervisor: post-worker submit_verification transition skipped"
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
                                "planning" | "decomposition" | "review" | "epic_breakdown" => {
                                    Some("close")
                                }
                                _ => None,
                            };
                            if let Some(action) = action
                                && let Err(e) = self
                                    .services
                                    .transition_task(spec.task_id.clone(), action.into(), None)
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
                        result = Some(TaskRunOutcome::Closed {
                            reason: reason.clone(),
                        });
                        // Also fire a real DB transition so the task row
                        // matches the run outcome. Same issue-type-aware
                        // routing as PlannerExecute.
                        if role_kind == RoleKind::Planner {
                            let action = match task.issue_type.as_str() {
                                "planning" | "decomposition" | "review" | "epic_breakdown" => {
                                    Some("close")
                                }
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
                    // ── Lead intervention decisions ──────────────────────────
                    // All are cancel-gated like the worker/reviewer transitions
                    // above: a stall-kill / preempt can flip cancel mid-stage
                    // and the agent may still emit a late outcome. We must NOT
                    // transition the board on a cancelled run — leave the task
                    // in `in_lead_intervention` for a clean redispatch.
                    StageOutcome::LeadApproved => {
                        // Work is complete + correct; the worker just couldn't
                        // self-certify. lead_approve: in_lead_intervention →
                        // approved. Do NOT set `result` — fall through to the
                        // post-loop `open_pr` (approved → pr_draft), the same
                        // terminal path as a reviewer approval, so the PR is
                        // pushed/undrafted immediately rather than waiting for
                        // the coordinator's next approved-task sweep.
                        if self.services.cancel().is_cancelled() {
                            tracing::debug!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                "supervisor: run cancelled — skipping lead_approve (task stays in_lead_intervention for redispatch)"
                            );
                            result = Some(TaskRunOutcome::Interrupted);
                            break;
                        }
                        if let Err(e) = self
                            .services
                            .transition_task(spec.task_id.clone(), "lead_approve".into(), None)
                            .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: lead_approve transition skipped"
                            );
                        }
                    }
                    StageOutcome::LeadApproveConflict { reason } => {
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "lead_approve_conflict".into(),
                                    Some(reason.clone()),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: lead_approve_conflict transition skipped"
                            );
                        }
                        result = Some(TaskRunOutcome::Closed { reason });
                        break;
                    }
                    StageOutcome::LeadReopen { reason } => {
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "lead_intervention_complete".into(),
                                    Some(reason.clone()),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: lead_intervention_complete transition skipped"
                            );
                        }
                        result = Some(TaskRunOutcome::Closed { reason });
                        break;
                    }
                    StageOutcome::LeadClose { reason } => {
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "force_close".into(),
                                    Some(reason.clone()),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: lead force_close transition skipped"
                            );
                        }
                        result = Some(TaskRunOutcome::Closed { reason });
                        break;
                    }
                    StageOutcome::LeadEscalate { reason } => {
                        // Lead couldn't resolve → return to the board (open) for
                        // re-dispatch / Planner safety net. Distinct from
                        // LeadClose (board re-review vs terminal closure).
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "lead_intervention_complete".into(),
                                    Some(reason.clone()),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: lead escalate transition skipped"
                            );
                        }
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
                            // transitions the board itself. The pre-stage
                            // transition above moved the task open → in_progress;
                            // close it here (in_progress → closed) on success.
                            // Without this terminal close a completed Spike would
                            // linger in_progress (and, once released back to open
                            // by the stall reaper, be re-dispatched every ~30s in
                            // a tight loop — the same failure mode the
                            // planner-review `close` above guards against — and,
                            // worse, block every task that depends on the spike).
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
                        // Worker-only flows (NewTask / ReviewResponse /
                        // ConflictRetry) end at the worker stage: verification
                        // runs BEFORE the reviewer now, so there is NO PR to
                        // open here. The worker already fired `submit_verification`
                        // (in_progress → verifying) above; signal the host with a
                        // `WorkerSubmitted` outcome so it spawns the slot-free
                        // verification pipeline. Detect "the last stage was the
                        // worker" rather than enumerating the flows so a future
                        // flow that ends at the worker inherits the right path.
                        // The PR is opened later, after a reviewer-only
                        // ReviewResume run that a green verification re-dispatches.
                        SupervisorFlow::NewTask
                        | SupervisorFlow::ReviewResponse
                        | SupervisorFlow::ConflictRetry
                            if last_stage_role == Some(RoleKind::Worker) =>
                        {
                            info!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                flow = ?spec.flow,
                                "supervisor: worker stage complete; task submitted to verification (no PR opened here)"
                            );
                            TaskRunOutcome::WorkerSubmitted
                        }
                        // The reviewer-only ReviewResume (and the Lead `approve`
                        // decision) DO open a PR: ReviewResume's reviewer approved
                        // the already-verified diff (in_task_review → approved),
                        // and lead_approve already moved the task to `approved`.
                        // open_pr pushes the branch and fires pr_created
                        // (approved → pr_draft). The Lead flow only reaches here on
                        // the `approve` decision (every other lead decision set
                        // `result` and broke).
                        SupervisorFlow::NewTask
                        | SupervisorFlow::ReviewResponse
                        | SupervisorFlow::ReviewResume
                        | SupervisorFlow::ConflictRetry
                        | SupervisorFlow::Lead => {
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
            // The worker stage genuinely succeeded and handed off to the
            // verification pipeline; the task-run itself completed cleanly.
            TaskRunOutcome::WorkerSubmitted => TaskRunStatus::Completed,
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
        assert!(
            StageOutcome::ReviewerRejected {
                feedback: "x".into()
            }
            .is_terminal()
        );
        assert!(StageOutcome::VerifierFailed { reason: "x".into() }.is_terminal());
        assert!(!StageOutcome::WorkerDone.is_terminal());
        assert!(!StageOutcome::PlannerExecute.is_terminal());
        assert!(!StageOutcome::ReviewerApproved.is_terminal());
        assert!(!StageOutcome::VerifierPassed.is_terminal());
        assert!(!StageOutcome::ArchitectDone.is_terminal());
    }
}

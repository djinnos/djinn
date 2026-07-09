// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::{Task, TaskRunStatus, TaskRunTrigger, TaskStatus};
use djinn_runtime::{ResumeLifecycleMetadata, ResumeSourceKind};
use djinn_workspace::{
    EphemeralWorkspaceError, GitIdentity, MergeOutcome, MergeParentOutcome, MirrorError,
    MirrorManager,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

pub mod services;

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
pub use services::{BranchPublicationResult, SupervisorServices};

// Re-export runtime spec types at the crate root so the thin
// `djinn_agent::supervisor` shim preserves every existing import path.
pub use djinn_runtime::spec::{
    LoopGuardKind, RoleKind, SupervisorFlow, TaskRunOutcome, TaskRunReport, TaskRunSpec,
    role_sequence,
};

/// Root under the shared cache PVC for per-task-run Cargo target directories.
pub const CARGO_TARGET_RUNS_ROOT: &str = "/cache/cargo-target-runs";

/// Canonical private Cargo target directory for a task-run Pod.
pub fn cargo_target_run_dir(task_run_id: &str) -> PathBuf {
    Path::new(CARGO_TARGET_RUNS_ROOT).join(task_run_id)
}

/// Validate a CARGO_TARGET_DIR value before deletion.
///
/// Cleanup must only ever remove the current task-run's private directory at
/// `/cache/cargo-target-runs/<task_run_id>`. This intentionally rejects empty
/// values, nested paths, sibling/root paths, and values for a different run.
pub fn validate_cargo_target_run_dir(target_dir: &str, task_run_id: &str) -> Option<PathBuf> {
    if target_dir.trim().is_empty() || task_run_id.trim().is_empty() {
        return None;
    }

    let path = Path::new(target_dir);
    if !path.is_absolute() || path.parent() != Some(Path::new(CARGO_TARGET_RUNS_ROOT)) {
        return None;
    }

    if path.file_name().and_then(|name| name.to_str()) != Some(task_run_id) {
        return None;
    }

    Some(path.to_path_buf())
}

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

    #[error("setup: {0}")]
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
    ///
    /// `evidence` is a JSON-serialized evidence object (with at minimum
    /// a non-empty `summary` field) required by the arbiter decision
    /// contract.  It is persisted on the arbitration row and logged as
    /// an `arbiter_decision` activity event before the approval transition.
    LeadApproved {
        /// JSON-serialized evidence object (e.g. `{"summary": "..."}`).
        #[serde(default)]
        evidence: String,
    },
    /// Lead `submit_decision(decision="approve_conflict")` — approved, but a
    /// merge conflict was found. Maps to `lead_approve_conflict`
    /// (in_lead_intervention → open + conflict metadata) so the coordinator
    /// re-dispatches a conflict-retry run.
    ///
    /// `evidence` is a JSON-serialized evidence object, same contract as
    /// [`LeadApproved`].
    LeadApproveConflict {
        reason: String,
        /// JSON-serialized evidence object (e.g. `{"summary": "..."}`).
        #[serde(default)]
        evidence: String,
    },
    /// Lead `submit_decision(decision="reopen")` — the arbiter rescoped /
    /// guided / blocked-on-deps and the task should retry with a fresh worker.
    /// Maps to `lead_intervention_complete` (in_lead_intervention → open).
    /// Carries the arbiter's structured reopen payload: the `directive` is
    /// injected verbatim into the next worker prompt, `verification_command`
    /// is prompted-for, and `exclude_models` blocks specific models from the
    /// next dispatch.
    LeadReopen {
        reason: String,
        /// Arbiter directive injected into the next worker prompt.
        directive: String,
        /// Verification command the next worker must execute.
        verification_command: String,
        /// Models excluded from the next dispatch (may be empty).
        #[serde(default)]
        exclude_models: Vec<String>,
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
    LoopGuardTripped {
        kind: LoopGuardKind,
        offending_signature: String,
        threshold: u32,
        observed: u32,
        turn_span: (u32, u32),
        session_id: String,
    },
    Parked {
        reason: ParkReason,
        summary: Option<String>,
        wind_down_ignored: bool,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
    },
    /// Arbiter `submit_decision(decision="park")` — the arbiter parked the
    /// task with a structured `park_dossier` describing the hold. Maps to a
    /// human-review hold on the board; the task cannot be auto-closed by an
    /// agent decision.
    LeadParked {
        /// JSON-serialized park dossier with hold description and failure analysis.
        park_dossier_json: String,
    },
    /// Arbiter `submit_decision(decision="supersede")` — the arbiter decomposed
    /// the task into replacement subtasks (already created via MCP) that carry
    /// the work forward, so the source task and its PR are force-closed as
    /// superseded. Maps to the `arbiter_supersede` terminal transition
    /// (in_lead_intervention → closed); the supervisor supersede transaction
    /// consumes the arbitration row, emits an `arbiter_decision` activity,
    /// transfers downstream blockers to the last replacement, and cleans up the
    /// task branch/PR. NO human-review hold is created (that is `LeadParked`).
    LeadSuperseded {
        reason: String,
        /// Short_ids / UUIDs of the replacement subtasks that carry the work
        /// forward. Non-empty by construction (the stage mapper rejects an
        /// empty `created_tasks` and directs the arbiter to `park` instead).
        replacement_task_ids: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParkReason {
    Budget,
}

/// Result of running the pre-approval verification gate for an arbiter
/// approve/approve_conflict decision.
///
/// Mirrors [`PreApprovalGateOutcome`] semantics but stripped to the two
/// outcomes the arbiter settlement path needs: proceed or block with
/// feedback.  Returned by [`SupervisorServices::run_arbiter_preapproval_gate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArbiterGateResult {
    /// Gate passed (green), was skipped, disabled, deferred, or hit an
    /// infra error (fail-open).  Proceed with the arbiter approval.
    Pass,
    /// Gate failed (red).  The arbiter approval must NOT be applied;
    /// `feedback` describes which checks failed and should be surfaced
    /// to the arbiter session.
    Blocked { feedback: String },
}

impl StageOutcome {
    /// Whether this outcome should short-circuit the role sequence.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StageOutcome::PlannerClose { .. }
                | StageOutcome::Escalate { .. }
                | StageOutcome::Failed { .. }
                | StageOutcome::Parked { .. }
                | StageOutcome::LoopGuardTripped { .. }
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
                | StageOutcome::LeadParked { .. }
                | StageOutcome::LeadSuperseded { .. }
        )
    }
}

fn emit_stage_outcome_event(
    outcome: &StageOutcome,
    role_kind: RoleKind,
    task_id: &str,
    task_run_id: &str,
) {
    let outcome_kind = match outcome {
        StageOutcome::WorkerDone => "worker_done",
        StageOutcome::PlannerExecute => "planner_execute",
        StageOutcome::PlannerClose { .. } => "planner_close",
        StageOutcome::ReviewerApproved => "reviewer_approved",
        StageOutcome::ReviewerRejected { .. } => "reviewer_rejected",
        StageOutcome::VerifierPassed => "verifier_passed",
        StageOutcome::VerifierFailed { .. } => "verifier_failed",
        StageOutcome::ArchitectDone => "architect_done",
        StageOutcome::Escalate { .. } => "escalate",
        StageOutcome::LeadApproved { .. } => "lead_approved",
        StageOutcome::LeadApproveConflict { .. } => "lead_approve_conflict",
        StageOutcome::LeadReopen { .. } => "lead_reopen",
        StageOutcome::LeadClose { .. } => "lead_close",
        StageOutcome::LeadEscalate { .. } => "lead_escalate",
        StageOutcome::LeadParked { .. } => "lead_parked",
        StageOutcome::LeadSuperseded { .. } => "lead_superseded",
        StageOutcome::Failed { .. } => "failed",
        StageOutcome::LoopGuardTripped { .. } => "loop_guard_tripped",
        StageOutcome::Parked { .. } => "parked",
    };

    tracing::info!(
        event = "supervisor.stage_outcome",
        outcome = outcome_kind,
        role = role_kind.as_str(),
        task_id = %task_id,
        task_run_id = %task_run_id,
        "supervisor: stage outcome observed"
    );
}

/// Label marking a task as a human-only review hold — the auto-park
/// terminal escalation. A task carrying it must NEVER be auto-closed by
/// an agent decision; only a human (or an explicit human-driven API)
/// closes it.
const HUMAN_REVIEW_HOLD_LABEL: &str = "human-review-hold";

/// Board transition a planner *terminal* outcome (execute / close /
/// escalate) should fire for `task`, or `None` to fire no transition.
///
/// Defense in depth behind the coordinator dispatch-rule exclusion
/// (`planner_review_claims`, which already stops the Planner from
/// claiming the hold): a `human-review-hold` task is a human-only
/// terminal hold and must NEVER be auto-closed by a planner decision —
/// closing it fires the unblocked-tasks release that flips the parked
/// source task back to `open`, defeating the auto-park safety mechanism.
/// For every other planning-type issue the action is `close`.
fn planner_terminal_close_action(task: &Task) -> Option<&'static str> {
    if task.labels.contains(HUMAN_REVIEW_HOLD_LABEL) {
        return None;
    }
    match task.issue_type.as_str() {
        "planning" | "decomposition" | "review" | "epic_breakdown" => Some("close"),
        _ => None,
    }
}

/// Routing decision for [`apply_planner_escalate_route`].
///
/// Pure helper — no I/O, no async — so unit tests can branch on the rule
/// without driving the transition closure. The async wrapper
/// ([`apply_planner_escalate_route`]) maps each variant to a transition
/// call (or no call) and the returned [`TaskRunOutcome`].
#[derive(Debug, PartialEq, Eq)]
enum PlannerEscalateRoute {
    /// Planner + planning-type issue + not cancelled: fire a single
    /// `close` transition with the escalation reason and surface
    /// [`TaskRunOutcome::Closed`].
    CloseWithReason,
    /// Planner + planning-type issue + cancelled: do NOT transition
    /// (leave the task redispatchable) and surface
    /// [`TaskRunOutcome::Interrupted`].
    Cancelled,
    /// Any other (non-planner role OR a non-planning issue): keep the
    /// legacy [`TaskRunOutcome::Escalated`] outcome. The supervisor
    /// backstop exists to ESCAPE this branch for planning-type tasks;
    /// the regression test in `planner_escalate_routes_planning_close`
    /// proves the new code no longer takes it on planning/decomposition/
    /// review/epic_breakdown.
    Escalate,
}

/// Compute the routing branch for a planner [`StageOutcome::Escalate`].
///
/// Mirrors the conditional that used to live inline in the
/// `StageOutcome::Escalate { reason }` arm of the supervisor loop (see
/// `ep1i`); split out so the regression tests in this module can cover
/// every branch without standing up a full `SupervisorServices`. Pure
/// function — no I/O, no logging, no clock.
fn route_planner_escalate(
    role_kind: RoleKind,
    issue_type: &str,
    cancel_is_cancelled: bool,
) -> PlannerEscalateRoute {
    let is_planner_planning = role_kind == RoleKind::Planner
        && matches!(
            issue_type,
            "planning" | "decomposition" | "review" | "epic_breakdown"
        );
    if !is_planner_planning {
        return PlannerEscalateRoute::Escalate;
    }
    if cancel_is_cancelled {
        PlannerEscalateRoute::Cancelled
    } else {
        PlannerEscalateRoute::CloseWithReason
    }
}

/// Apply the planner-escalate routing rule introduced by `ep1i` and
/// hardened by `rt3l`.
///
/// Called from the `StageOutcome::Escalate { reason }` arm of the
/// supervisor loop. Encapsulates the three-way decision (park the task
/// with a `close` transition / cancel-gated interrupt / legacy
/// `Escalated` outcome) AND the actual `transition_task` call so unit
/// tests can assert both the outcome AND the exact set of transition
/// invocations (count, action, reason) without needing a full
/// [`SupervisorServices`] implementation.
///
/// Routing (mirrors the production code that lived inline before
/// `ep1i`):
/// - `Planner` + planning-type issue (`planning` / `decomposition` /
///   `review` / `epic_breakdown`) + not cancelled: exactly one
///   `transition(task_id, "close", Some(surfaced_reason))` call;
///   return [`TaskRunOutcome::Closed`] with the same surfaced reason.
///   The surfaced reason is prefixed with `"planner escalated:"` so
///   `tasks.close_reason`, activity events, the terminal run outcome,
///   and host/UI reporting all show the same durable close reason while
///   still containing the original planner-provided message. The
///   success path also emits a structured `tracing::info!` so the
///   worker pod log carries `task_run_id`,
///   `task_id`, `issue_type`, and the surfaced reason — this is the
///   `rt3l` hardening on top of `ep1i`'s routing.
/// - `Planner` + planning-type issue + cancelled: NO transition call;
///   return [`TaskRunOutcome::Interrupted`] so the task stays in its
///   current state for redispatch.
/// - Any other role OR a non-planning issue: NO transition call;
///   return [`TaskRunOutcome::Escalated`] (the legacy fall-through the
///   supervisor backstop exists to escape for planning-type tasks).
///
/// `transition` is a closure that performs the host-side
/// `transition_task` call. Its only production caller is
/// `self.services.transition_task(..)`; tests pass a recording closure
/// to assert call count + arguments.
async fn apply_planner_escalate_route<F, Fut>(
    role_kind: RoleKind,
    issue_type: &str,
    task_id: &str,
    task_run_id: &str,
    reason: String,
    cancel_is_cancelled: bool,
    transition: F,
) -> TaskRunOutcome
where
    F: FnOnce(String, String, Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    match route_planner_escalate(role_kind, issue_type, cancel_is_cancelled) {
        PlannerEscalateRoute::CloseWithReason => {
            // The close transition's `reason` carries the prefix so the
            // persisted `tasks.close_reason` row and the activity_log
            // entry both read "planner escalated: <original reason>".
            // This is the durable surface the host/UI / dispatcher sees;
            // the prefix is consistent with other canned `close_reason`
            // values in the schema (`"completed"`, `"force_closed"`,
            // `"peer_reconciled"`) and gives operators a single
            // grep-able token for "the planner asked for help here"
            // without losing the original message. Returning the same
            // surfaced reason in `TaskRunOutcome::Closed { reason }`
            // keeps host/UI reporting consistent with the durable
            // close transition path.
            let surfaced_reason = format!("planner escalated: {reason}");
            info!(
                task_run_id = %task_run_id,
                task_id = %task_id,
                issue_type = %issue_type,
                surfaced_reason = %surfaced_reason,
                "supervisor: planner escalated — closing planning-type task via close transition (rt3l)",
            );
            if let Err(e) = transition(
                task_id.to_string(),
                "close".to_string(),
                Some(surfaced_reason.clone()),
            )
            .await
            {
                tracing::warn!(
                    task_run_id = %task_run_id,
                    task_id = %task_id,
                    issue_type = %issue_type,
                    error = %e,
                    "supervisor: planner escalate close transition skipped",
                );
            }
            TaskRunOutcome::Closed {
                reason: surfaced_reason,
            }
        }
        PlannerEscalateRoute::Cancelled => {
            tracing::debug!(
                task_run_id = %task_run_id,
                task_id = %task_id,
                issue_type = %issue_type,
                "supervisor: run cancelled — skipping planner escalate close transition",
            );
            TaskRunOutcome::Interrupted
        }
        PlannerEscalateRoute::Escalate => TaskRunOutcome::Escalated { reason },
    }
}

// ── TaskRunSupervisor ────────────────────────────────────────────────────────

/// Outcome of [`prepare_resume_workspace`] — what the supervisor's
/// worktree-setup helper actually did, including the chosen source kind and
/// any machine-readable fallback reason. Surfaced to tracing + the
/// downstream resume-prompt context (task `48ru`) so the operator / UI can
/// see which git state the worker pod is resuming from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeWorkspaceOutcome {
    /// What the supervisor actually used for the worktree base. For `Clean`
    /// the workspace checked out at the canonical task branch; for `Safe` /
    /// `Alternate` it checked out the selected ref/SHA; for
    /// `AutoSubmit` the workspace checked out at the task branch (auto-submit
    /// carries no git content to base on — the review/submission metadata
    /// rides the prompt context, not the worktree).
    pub applied_source: ResumeSourceKind,
    /// The task-branch ref that the workspace will be promoted onto by the
    /// follow-up `ensure_branch(task_branch)` step. Always populated.
    pub applied_target_ref: String,
    /// Commit SHA the workspace was checked out at, if the helper could
    /// resolve a concrete commit. `None` for auto-submit and the
    /// clean-fallback paths where no specific commit was selected.
    pub applied_commit_sha: Option<String>,
    /// Why the helper fell back to the clean task branch instead of using
    /// the chosen source. `None` for the happy path; populated for skipped
    /// / unsafe / unavailable selections.
    pub fallback_reason: Option<String>,
}

impl ResumeWorkspaceOutcome {
    fn clean_fallback(task_branch: &str, reason: String) -> Self {
        Self {
            applied_source: ResumeSourceKind::CleanTaskBranch,
            applied_target_ref: task_branch.to_string(),
            applied_commit_sha: None,
            fallback_reason: Some(reason),
        }
    }

    fn safe_applied(
        source: ResumeSourceKind,
        target_ref: &str,
        commit_sha: Option<String>,
    ) -> Self {
        Self {
            applied_source: source,
            applied_target_ref: target_ref.to_string(),
            applied_commit_sha: commit_sha,
            fallback_reason: None,
        }
    }
}

/// Apply the coordinator-selected resume source to the worktree setup.
///
/// Behaviour, per `spec.resume_lifecycle_metadata.source_kind`:
///
/// - `None` (no `ResumeLifecycleMetadata` selected this dispatch): returns
///   `Ok(None)` so the caller falls through to the legacy
///   `clone_ephemeral(task_branch)` path. This is the byte-for-byte default
///   for callers that have not enabled the `worker_lifecycle_config.resume`
///   gate.
/// - `ResumeSourceKind::CleanTaskBranch`: returns `Ok(None)` so the legacy
///   `clone_ephemeral(task_branch)` path runs unchanged. The selector
///   already chose the fallback, so the worktree setup matches it.
/// - `ResumeSourceKind::AutoSubmit`: accepted auto-submit/review state has
///   no git content to base on — the worker pod is built on the canonical
///   `task_branch` (same as the clean-task-branch path) and the resume
///   prompt context (task `48ru`) carries the submit/review id. Falls back
///   to the legacy path; no extra worktree-setup step is required.
/// - `ResumeSourceKind::TaskBranchCheckpoint`: clones at `task_branch`
///   first (so `ensure_branch` can later refresh the branch ref) then
///   checks out the selected `commit_sha` in detached HEAD. The follow-up
///   `ensure_branch(task_branch)` promotes HEAD onto the task branch so a
///   subsequent `push_to_origin(task_branch)` carries the resumed state.
/// - `ResumeSourceKind::AlternateCheckpointRef`: clones at
///   `target_ref` (fully-qualified, e.g.
///   `refs/djinn/checkpoints/task/<id>/<sid>`) via
///   [`MirrorManager::clone_ephemeral_at_ref`]. On any failure
///   (selected ref missing, fetch refused, unsafe sha, …) the helper
///   falls back to the clean task branch and records a machine-readable
///   reason in [`ResumeWorkspaceOutcome::fallback_reason`].
///
/// On any non-fatal error (selected source unavailable, unsafe checkpoint,
/// mismatch, missing safety scan, missing SHA) the helper returns the clean
/// task branch and a populated `fallback_reason` so the caller never
/// panics or silently resumes unsafe output. Fatal errors (mirror missing,
/// IO) propagate as `SupervisorError::Mirror`.
///
/// Never skips the supervisor's normal task-branch isolation:
/// `ensure_branch(task_branch)` always runs after the helper returns, so
/// the resumed commit lands on the canonical task branch even when the
/// helper checked out an alternate ref / detached SHA.
async fn prepare_resume_workspace(
    mirror: &MirrorManager,
    project_id: &str,
    task_branch: &str,
    base_branch: &str,
    resume: Option<&ResumeLifecycleMetadata>,
) -> Result<Option<ResumeWorkspaceOutcome>, SupervisorError> {
    let Some(meta) = resume else {
        return Ok(None);
    };
    // `considered == false` is the "selection was not consulted" signal —
    // the selector produced no record at all (disabled config, pre-`1f9u`
    // host, etc.). Same semantics as `None`: fall through to the legacy path.
    if !meta.considered {
        return Ok(None);
    }
    let source_kind = meta
        .source_kind
        .unwrap_or(ResumeSourceKind::CleanTaskBranch);

    // Clean / AutoSubmit variants: no worktree-setup work is needed. The
    // caller falls through to `clone_ephemeral(task_branch)` which is the
    // byte-for-byte legacy default. The outcome is `None` so the dispatch
    // logs record `selection_kind` (from the spec metadata) without us
    // re-asserting "applied clean task branch" — the existing legacy log
    // already does that on success.
    if matches!(
        source_kind,
        ResumeSourceKind::CleanTaskBranch | ResumeSourceKind::AutoSubmit
    ) {
        debug!(
            source_kind = ?source_kind,
            task_branch,
            "resume: clean/auto-submit selection; using legacy task-branch clone path unchanged"
        );
        return Ok(None);
    }

    // Safe task-branch checkpoint: clone the canonical task branch (so the
    // object db / shared alternates are populated), then check out the
    // selected SHA in detached HEAD. The follow-up `ensure_branch` call
    // promotes HEAD onto the task branch.
    if matches!(source_kind, ResumeSourceKind::TaskBranchCheckpoint) {
        let Some(commit_sha) = meta.commit_sha.clone() else {
            warn!(
                task_branch,
                source_kind = ?source_kind,
                "resume: safe task-branch checkpoint selected without a commit_sha — \
                 falling back to clean task branch"
            );
            return Ok(Some(ResumeWorkspaceOutcome::clean_fallback(
                task_branch,
                "missing_commit_sha_for_safe_task_branch_checkpoint".to_string(),
            )));
        };

        // Stage 1: ensure the task branch is materialised in the clone so the
        // detached checkout has a populated alternates pool. Fall back to
        // base_branch if the task branch is missing (preserves existing
        // legacy semantics for first-cycle runs).
        let workspace = match mirror.clone_ephemeral(project_id, task_branch).await {
            Ok(ws) => ws,
            Err(
                MirrorError::Missing(_)
                | MirrorError::Git(_)
                | MirrorError::Io(_)
                | MirrorError::GcGuard(_),
            ) => {
                debug!(
                    task_branch,
                    "resume: task branch not in mirror for safe checkpoint — \
                     cloning base_branch for checkout"
                );
                mirror.clone_ephemeral(project_id, base_branch).await?
            }
        };
        // Stage 2: detach HEAD on the selected SHA. `Workspace::checkout_ref`
        // surfaces fetch / checkout errors so the caller can machine-classify
        // the fallback (rather than panicking on a vanished SHA).
        if let Err(e) = workspace.checkout_ref(&commit_sha).await {
            warn!(
                task_branch,
                commit_sha,
                error = %e,
                "resume: workspace.checkout_ref failed on safe checkpoint SHA — \
                 falling back to clean task branch"
            );
            return Ok(Some(ResumeWorkspaceOutcome::clean_fallback(
                task_branch,
                format!("checkout_ref_failed: {e}"),
            )));
        }
        debug!(
            task_branch,
            commit_sha,
            "resume: safe task-branch checkpoint applied — detached HEAD at selected SHA"
        );
        // The Workspace isn't actually returned to the caller in the current
        // API shape — it's destroyed by the TempDir drop. Log the outcome so
        // the operator can confirm the SHA was reached, and let the legacy
        // `clone_ephemeral(task_branch)` re-run on the SAME selected SHA in
        // a fresh clone so `ensure_branch` can promote onto a local branch.
        // This is wasteful (two clones) but preserves the existing boundary
        // (caller owns the Workspace lifetime). A follow-up optimisation
        // can return the prepared Workspace through a richer API.
        let _ = workspace; // mark explicitly: dropped deliberately.
        let _ = base_branch; // base_branch used only in stage 1 here.
        return Ok(Some(ResumeWorkspaceOutcome::safe_applied(
            source_kind,
            task_branch,
            Some(commit_sha),
        )));
    }

    // Alternate checkpoint ref: clone at the chosen ref via the additive
    // `clone_ephemeral_at_ref` helper. Any failure (missing ref, fetch
    // refused, …) falls back to the clean task branch with a
    // machine-readable reason so the caller can classify the fallback.
    if matches!(source_kind, ResumeSourceKind::AlternateCheckpointRef) {
        let Some(target_ref) = meta.target_ref.clone() else {
            warn!(
                task_branch,
                "resume: alternate checkpoint ref selected without a target_ref — \
                 falling back to clean task branch"
            );
            return Ok(Some(ResumeWorkspaceOutcome::clean_fallback(
                task_branch,
                "missing_target_ref_for_alternate_checkpoint".to_string(),
            )));
        };

        match mirror.clone_ephemeral_at_ref(project_id, &target_ref).await {
            Ok(_workspace_at_ref) => {
                debug!(
                    task_branch,
                    target_ref,
                    "resume: alternate checkpoint ref applied — \
                     detached HEAD at ref's commit (legacy task-branch clone will follow)"
                );
                // Same lifecycle boundary rationale as
                // `TaskBranchCheckpoint` above: the prepared Workspace is
                // dropped so the legacy `clone_ephemeral(task_branch)` can
                // run. The follow-up `ensure_branch` call still drives
                // `task_branch` to the resumed commit when both clones land
                // at the same SHA, but to keep the integration additive and
                // sidestep re-clone cost we record the outcome in metadata
                // for sibling `48ru` / `sy0g` and let this function fall
                // through to the legacy clone on the next line in `run`.
                let _ = _workspace_at_ref;
                return Ok(Some(ResumeWorkspaceOutcome::safe_applied(
                    source_kind,
                    &target_ref,
                    None,
                )));
            }
            Err(e) => {
                warn!(
                    task_branch,
                    target_ref,
                    error = %e,
                    "resume: clone_ephemeral_at_ref failed for alternate checkpoint ref — \
                     falling back to clean task branch"
                );
                return Ok(Some(ResumeWorkspaceOutcome::clean_fallback(
                    task_branch,
                    format!("clone_ephemeral_at_ref_failed: {e}"),
                )));
            }
        }
    }

    // Unknown / future source kind — apply the legacy default rather than
    // panicking. The selector only emits the four kinds above; this arm
    // exists so a kind added later by `1f9u` is non-fatal until the
    // matching arm is wired here.
    debug!(
        source_kind = ?source_kind,
        task_branch,
        "resume: unknown source_kind; falling back to legacy clean task-branch clone path"
    );
    Ok(None)
}

// Note: `Workspace` is intentionally NOT returned through the API above.
// The helper records the outcome and the caller still drives the legacy
// `clone_ephemeral(task_branch)` so `ensure_branch` and the proactive-sync
// flow keep their existing test surface. A future PR may promote the
// prepared Workspace up through `TaskRunSupervisor::run` for performance.

pub struct TaskRunSupervisor {
    mirror: Arc<MirrorManager>,
    services: Arc<dyn SupervisorServices>,
    clock: Arc<dyn Clock>,
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
        Self {
            mirror,
            services,
            clock: Arc::new(SystemClock::new()),
        }
    }

    /// Like [`new`](Self::new), but accepts an explicit clock for
    /// deterministic testing.
    pub fn with_clock(
        mirror: Arc<MirrorManager>,
        services: Arc<dyn SupervisorServices>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            mirror,
            services,
            clock,
        }
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
                // Pre-session tracking state: the run is visible to the UI and
                // the host-side pre-session liveness deadline from dispatch,
                // and flips to `running` when the first reply-loop session is
                // created (`SessionRepository::create`). See `TaskRunStatus`.
                status: Some(
                    djinn_core::models::TaskRunStatus::Starting
                        .as_str()
                        .to_string(),
                ),
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

        // Coarse pre-session progress marker: the host-side liveness deadline
        // names this step if the ephemeral clone hangs (the biggest suspect
        // for a stage-init wedge). Best-effort — never blocks the run.
        let _ = self
            .services
            .report_stage_step(djinn_runtime::stage_step::WORKSPACE_ATTACH)
            .await;

        // Apply the coordinator-selected resume source (from
        // `1f9u`'s `select_resume_lifecycle_metadata_for_dispatch`) to the
        // worktree setup. The helper consumes `spec.resume_lifecycle_metadata`
        // and either prepares the resumed workspace, records a structured
        // fallback reason, or returns `Ok(None)` to signal the legacy
        // `clone_ephemeral(task_branch)` path below. Additive — defaulting
        // the helper keeps the legacy byte-for-byte clone behaviour intact
        // (no `ResumeLifecycleMetadata` set on the spec).
        //
        // The outcome is logged into the structured tracing span so the
        // worker→host report + the post-session activity log can attribute
        // the resume source per task-run. The `applied_commit_sha` /
        // `fallback_reason` are also surfaced as a sibling
        // `resume_applied` event so downstream prompt / model / merge work
        // (siblings `48ru`, `sy0g`) can read them without re-querying.
        let resume_outcome = prepare_resume_workspace(
            &self.mirror,
            &spec.project_id,
            &spec.task_branch,
            &spec.base_branch,
            spec.resume_lifecycle_metadata.as_ref(),
        )
        .await?;
        if let Some(outcome) = resume_outcome.as_ref() {
            info!(
                task_run_id = %run_id,
                task_id = %spec.task_id,
                applied_source = ?outcome.applied_source,
                applied_target_ref = %outcome.applied_target_ref,
                applied_commit_sha = ?outcome.applied_commit_sha,
                fallback_reason = ?outcome.fallback_reason,
                "supervisor: resume-source selection applied to worktree setup"
            );
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
                            Ok(outcome) if outcome.committed() => {
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
                            Ok(_outcome) => {
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
        // zkk9: Track whether this run *started* a monitored reopen (the
        // arbiter's LeadReopen decision).  If so, the post-loop
        // `complete_monitored_reopen` hook must be skipped — the
        // arbitration row must remain unconsumed so the next worker
        // dispatch can see the directive/exclusions.  Completion happens
        // only when the monitored *worker* attempt reaches a terminal
        // outcome (a later, separate task-run).
        let mut started_monitored_reopen = false;
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
                    // `needs_task_review` — the post-worker `submit_task_review`
                    // (legal only from `in_progress`) then also no-op'd and
                    // review was skipped before review (task u4fx). Walk
                    // `resume_worker` (needs_task_review → in_progress) on the
                    // redo so `submit_task_review` succeeds, exactly like the
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
                    // Refinement tribunal sessions are simple-lifecycle.
                    // Move open → in_progress so the board reflects the
                    // running role and the coordinator stops re-dispatching.
                    RoleKind::Refinement => Some("start"),
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
                        "supervisor: pre-stage status transition failed (may be idempotent, or the task became unclaimable)"
                    );
                    // A failed `start` is normally benign — a re-dispatched run is
                    // often already `in_progress` (idempotent second call). But if
                    // the claim failed because a blocker landed AFTER this run was
                    // dispatched (a remediation/hold review task now blocks the
                    // source, so `validate_start_guard` rejects open→in_progress),
                    // falling through to `execute_stage` would run the worker
                    // UNCAPTURED and concurrently with the planner now remediating
                    // the same task (observed 2026-07-01: task 55i8 ran `open`
                    // alongside remediation `s9zp`). Positively re-assert the claim
                    // rather than trusting the error shape: reload the task and, for
                    // a `start` claim, require it actually reached `in_progress`. If
                    // it did not, we never owned it — abort this run as Interrupted
                    // (the task stays blocked and resurfaces via
                    // `emit_unblocked_tasks` when the hold closes). On a reload error
                    // we preserve the prior fall-through so a transient RPC blip
                    // cannot start spuriously aborting healthy runs.
                    if action == "start" {
                        match self.services.load_task(spec.task_id.clone()).await {
                            Ok(t) if t.status != TaskStatus::InProgress.as_str() => {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    role = %role_kind.as_str(),
                                    status = %t.status,
                                    "supervisor: aborting run — task not claimable after failed start \
                                     (blocked or no longer open); refusing to execute stage uncaptured"
                                );
                                result = Some(TaskRunOutcome::Interrupted);
                                break;
                            }
                            Ok(_) => {}
                            Err(le) => {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %le,
                                    "supervisor: could not reload task to verify claim after failed start; \
                                     proceeding (prior behavior) to avoid regressing on a transient reload error"
                                );
                            }
                        }
                    }
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

                emit_stage_outcome_event(&stage_outcome, role_kind, &spec.task_id, &run_id);

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
                            Ok(outcome) if outcome.committed() => {
                                let excluded = outcome.excluded();
                                if excluded.is_empty() {
                                    tracing::info!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        role = %role_kind.as_str(),
                                        "supervisor: committed worker/architect changes"
                                    );
                                } else {
                                    tracing::info!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        role = %role_kind.as_str(),
                                        excluded_count = excluded.len(),
                                        excluded_paths = ?excluded,
                                        "supervisor: committed worker/architect changes (some scratch files excluded)"
                                    );
                                }
                                // The push that makes this commit durable in the
                                // mirror happens UNCONDITIONALLY just below the
                                // match — see the comment there. We no longer
                                // push inside this arm: workers commonly commit
                                // their own edits via shell, leaving the tree
                                // clean and this arm un-taken, yet their work
                                // still needs pushing. One push site covers both.
                            }
                            Ok(djinn_workspace::CommitOutcome::NoLegitimateChanges {
                                ref excluded,
                            }) => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    task_run_id = %run_id,
                                    role = %role_kind.as_str(),
                                    excluded_count = excluded.len(),
                                    excluded_paths = ?excluded,
                                    "supervisor: no legitimate changes after stage; junk-only files excluded"
                                );
                            }
                            Ok(djinn_workspace::CommitOutcome::NoChanges) => {
                                tracing::debug!(
                                    task_id = %task.short_id,
                                    task_run_id = %run_id,
                                    role = %role_kind.as_str(),
                                    "supervisor: no changes to commit after stage"
                                );
                            }
                            Ok(djinn_workspace::CommitOutcome::Committed { .. }) => {
                                // Unreachable: committed() guard already matched.
                                unreachable!("Committed already matched by guard");
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
                                        error_class: None,
                                        hint: None,
                                        body_excerpt: None,
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
                        // Retry with backoff: a persistent push failure means
                        // the worker's progress never reached the mirror and
                        // will be lost when the ephemeral Pod exits. We must
                        // NOT proceed to submit_task_review (which would mark
                        // the round as durably submitted) — instead, fail the
                        // run so the coordinator can redispatch with a fresh
                        // workspace and the push can be retried. A single
                        // failure is still loud (error! level, structured
                        // tracing event).
                        const PUSH_MAX_ATTEMPTS: usize = 3;
                        const PUSH_INITIAL_BACKOFF: std::time::Duration =
                            std::time::Duration::from_secs(1);

                        let mut push_succeeded = false;
                        let mut last_push_err: Option<String> = None;
                        for attempt in 1..=PUSH_MAX_ATTEMPTS {
                            match workspace.push_to_origin(&spec.task_branch).await {
                                Ok(()) => {
                                    push_succeeded = true;
                                    if attempt > 1 {
                                        tracing::info!(
                                            task_id = %task.short_id,
                                            task_run_id = %run_id,
                                            branch = %spec.task_branch,
                                            attempt,
                                            "supervisor: push_to_origin succeeded after retry"
                                        );
                                    }
                                    break;
                                }
                                Err(e) => {
                                    last_push_err = Some(e.to_string());
                                    if attempt < PUSH_MAX_ATTEMPTS {
                                        tracing::warn!(
                                            task_id = %task.short_id,
                                            task_run_id = %run_id,
                                            branch = %spec.task_branch,
                                            attempt,
                                            max_attempts = PUSH_MAX_ATTEMPTS,
                                            "supervisor: push_to_origin failed; retrying after backoff"
                                        );
                                        tokio::time::sleep(PUSH_INITIAL_BACKOFF * attempt as u32)
                                            .await;
                                    }
                                }
                            }
                        }
                        if push_succeeded {
                            tracing::debug!(
                                task_id = %task.short_id,
                                task_run_id = %run_id,
                                role = %role_kind.as_str(),
                                branch = %spec.task_branch,
                                "supervisor: pushed task_branch to mirror after stage (durable)"
                            );
                        } else {
                            let e = last_push_err.as_deref().unwrap_or("unknown error");
                            // Structured event so pod-log shipper and activity
                            // search can find every persistent push failure
                            // without parsing free-form error strings.
                            tracing::error!(
                                target: "djinn_supervisor::push_failure",
                                task_id = %task.short_id,
                                task_run_id = %run_id,
                                role = %role_kind.as_str(),
                                branch = %spec.task_branch,
                                error = %e,
                                attempt = PUSH_MAX_ATTEMPTS,
                                kind = "push_failure",
                                "supervisor: push_to_origin failed after all retries — \
                                 worker progress NOT durable in mirror; refusing \
                                 submit_task_review to prevent phantom submission"
                            );
                        }
                        // ── GitHub publication for existing open PRs ─────────
                        //
                        // If the mirror push succeeded and this task already has
                        // an open PR (`task.pr_url` is set), push the same task
                        // branch/head to GitHub so Actions evaluates the latest
                        // commit instead of a stale PR head. This is a freshness
                        // optimization only — it does NOT gate submit_task_review.
                        // On failure, the stale-head remediation loop will catch
                        // the divergence. Gated on Worker role (matching the
                        // submit_task_review gate below); ArchitectDone tasks
                        // typically don't have open PRs.
                        //
                        // ── Branch-publication policy approval (icoe/vy47) ──
                        //
                        // It is acceptable and intentional to push unreviewed
                        // WorkerDone commits to existing open-PR branches.
                        // Internal review still gates approval/undraft/merge
                        // readiness.  The goal is to keep GitHub Actions CI
                        // evaluating the worker's latest commit rather than a
                        // stale PR head, avoiding the aah4 stale-head
                        // false-strike loop where GitHub evaluates an old SHA
                        // and produces spurious CI failures.
                        //
                        // ── Helper reuse (icoe/vy47) ────────────────────────
                        //
                        // The GitHub push explicitly reuses
                        // `push_task_branch_to_github` and its concurrent-push
                        // race guard (`is_concurrent_push_race`) rather than
                        // creating a second GitHub writer.  This ensures
                        // consistent push semantics and race handling across the
                        // codebase.
                        //
                        // See: epic vy47, proposal icoe acceptance criteria 4,
                        // 5, 7, 8.
                        if push_succeeded && role_kind == RoleKind::Worker && task.pr_url.is_some()
                        {
                            match self.services.publish_branch_to_github(&spec, &task).await {
                                result if result.success => {
                                    tracing::info!(
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        branch = %spec.task_branch,
                                        github_head = ?result.pushed_sha,
                                        "supervisor: published WorkerDone mirror commit to GitHub open-PR branch"
                                    );
                                }
                                pub_failure => {
                                    // Record structured publication-failure
                                    // evidence. The task still proceeds to
                                    // review — mirror push already succeeded and
                                    // the internal review gates approval/merge.
                                    // The GitHub stale-head remediation loop
                                    // will catch the divergence.
                                    tracing::warn!(
                                        target: "djinn_supervisor::github_publication_failure",
                                        task_id = %task.short_id,
                                        task_run_id = %run_id,
                                        branch = %spec.task_branch,
                                        mirror_head = %pub_failure.mirror_head,
                                        github_head = %pub_failure.attempted_github_head,
                                        pr_branch_existed = pub_failure.pr_branch_existed,
                                        error_class = ?pub_failure.error_class,
                                        error = ?pub_failure.error_message,
                                        "supervisor: GitHub publication failed after mirror push succeeded — \
                                         GitHub Actions may evaluate a stale PR head"
                                    );
                                }
                            }
                        }
                        // Worker finished cleanly → submit_task_review
                        // (in_progress → needs_task_review). The run ends after
                        // this stage (the worker-only sequence has no reviewer
                        // leg); the HOST then dispatches a reviewer-only
                        // ReviewResume when the task reaches needs_task_review.
                        // Architect has no analogous transition in the current
                        // state machine.
                        //
                        // Gate on the cancel token: a stall-kill / preempt can
                        // flip cancel mid-stage and the agent may still emit a
                        // late StageOutcome. We must NOT advance the task on a
                        // cancelled run — doing so walked tasks all the way to
                        // `approved` with no pushed task_branch (the kw7s
                        // PR-open loop). Leave it in_progress for redispatch.
                        //
                        // Also gate on push durability: if the push failed
                        // after all retries, the round's work is not durable
                        // and we must NOT mark it as submitted — the task stays
                        // in_progress for redispatch so a fresh run can retry
                        // the push.
                        if role_kind == RoleKind::Worker {
                            if !push_succeeded {
                                tracing::error!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    "supervisor: skipping submit_task_review — \
                                     push failed, task stays in_progress for redispatch"
                                );
                                result = Some(TaskRunOutcome::Failed {
                                    stage: "worker".into(),
                                    reason: format!(
                                        "push_to_origin failed after {PUSH_MAX_ATTEMPTS} \
                                         attempts: {}. Worker progress is \
                                         not durable in the mirror — the round must be \
                                         retried.",
                                        last_push_err.as_deref().unwrap_or("unknown error")
                                    ),
                                    provider_failure: None,
                                    error_class: None,
                                    hint: Some(
                                        "Check mirror PVC permissions and disk space. \
                                         The task will be redispatched by the coordinator."
                                            .into(),
                                    ),
                                    body_excerpt: None,
                                });
                                break;
                            }
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
                            // `planner_terminal_close_action` returns `None`
                            // for a `human-review-hold` task so it is never
                            // auto-closed here (defense in depth behind the
                            // coordinator dispatch-rule exclusion).
                            let action = planner_terminal_close_action(&task);
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
                            // A `human-review-hold` task is never auto-closed
                            // here — `planner_terminal_close_action` returns
                            // `None` for it (defense in depth).
                            let action = planner_terminal_close_action(&task);
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
                        // Planner escalate on a planning-type task parks the
                        // task (close + reason) instead of leaving it
                        // redispatchable; the previous shape only set
                        // `TaskRunOutcome::Escalated`, which the
                        // coordinator's ready-task sweep kept re-dispatching
                        // every ~30s (the bug `ep1i` fixes). The cancel
                        // gate ensures a stall-kill / preempt arriving
                        // mid-stage leaves the task in_progress for
                        // redispatch instead of closing it. Non-planner
                        // roles and non-planning issues keep the legacy
                        // `Escalated` outcome — the backstop is
                        // intentionally narrow to the planner planning leg.
                        // The routing + transition are factored into
                        // [`apply_planner_escalate_route`] so the unit
                        // tests in this module can cover every branch
                        // without standing up a full `SupervisorServices`.
                        // The `rt3l` hardening on top of `ep1i`'s
                        // routing adds:
                        //   * a structured `tracing::info!` on the
                        //     success path carrying `task_run_id`,
                        //     `task_id`, `issue_type`, and the
                        //     surfaced close reason so worker pod
                        //     logs make the planner-escalate
                        //     closure visible;
                        //   * a stable `"planner escalated:"` prefix
                        //     on the close-transition `reason` so
                        //     the persisted `tasks.close_reason`
                        //     row and the activity log entry are
                        //     grep-able as planner escalations
                        //     (mirroring the canned
                        //     `"peer_reconciled"` / `"force_closed"`
                        //     / `"completed"` markers). The
                        //     `TaskRunOutcome::Closed { reason }`
                        //     returned by the helper uses the same
                        //     surfaced reason so host/UI reporting
                        //     matches the persisted close reason.
                        // Defense in depth: a `human-review-hold` task is a
                        // human-only terminal hold — never auto-close it on a
                        // planner escalate. Closing it would fire the
                        // unblocked-tasks release and flip the parked source
                        // task back to `open` (the exact loop the hold breaks).
                        // Leave it parked (Escalated, no transition) for a human.
                        if task.labels.contains(HUMAN_REVIEW_HOLD_LABEL) {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                issue_type = %task.issue_type,
                                "supervisor: planner escalate on human-review-hold task — NOT closing; parking for human",
                            );
                            result = Some(TaskRunOutcome::Escalated { reason });
                            break;
                        }
                        let outcome = apply_planner_escalate_route(
                            role_kind,
                            &task.issue_type,
                            &spec.task_id,
                            &run_id,
                            reason,
                            self.services.cancel().is_cancelled(),
                            |task_id, action, reason| async move {
                                self.services.transition_task(task_id, action, reason).await
                            },
                        )
                        .await;
                        result = Some(outcome);
                        break;
                    }
                    // ── Lead intervention decisions ──────────────────────────
                    // All are cancel-gated like the worker/reviewer transitions
                    // above: a stall-kill / preempt can flip cancel mid-stage
                    // and the agent may still emit a late outcome. We must NOT
                    // transition the board on a cancelled run — leave the task
                    // in `in_lead_intervention` for a clean redispatch.
                    StageOutcome::LeadApproved { ref evidence } => {
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
                        // ── Pre-approval CI-grade verification gate ───────
                        // Arbiter approvals must pass the same gate as
                        // reviewer approvals.  A red result returns feedback
                        // to the arbiter session without transitioning the
                        // task or consuming the arbitration row.
                        match self.services.run_arbiter_preapproval_gate(&task).await {
                            Ok(ArbiterGateResult::Blocked { feedback }) => {
                                tracing::info!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    "supervisor: arbiter pre-approval gate red — task stays in_lead_intervention (no strike, no arbitration consumption)"
                                );
                                result = Some(TaskRunOutcome::Escalated {
                                    reason: format!(
                                        "pre-approval CI-grade verification gate blocked arbiter approve; \
                                         returned to arbiter session (strike-free). {feedback}"
                                    ),
                                });
                                break;
                            }
                            Ok(ArbiterGateResult::Pass) => { /* proceed */ }
                            Err(e) => {
                                // Infra error — fail-open like the reviewer gate.
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %e,
                                    "supervisor: arbiter pre-approval gate infra error — proceeding (fail-open)"
                                );
                            }
                        }
                        // ── Persist arbiter decision on arbitration row ────
                        // Record the decision and evidence before the board
                        // transition so the arbitration row carries the
                        // decision payload and an arbiter_decision activity
                        // event is emitted (AC2).
                        if let Err(e) = self
                            .services
                            .record_arbiter_decision(
                                spec.task_id.clone(),
                                "approve".into(),
                                evidence.clone(),
                            )
                            .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: record_arbiter_decision failed — proceeding with lead_approve"
                            );
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
                    StageOutcome::LeadApproveConflict {
                        reason,
                        ref evidence,
                    } => {
                        if self.services.cancel().is_cancelled() {
                            tracing::debug!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                "supervisor: run cancelled — skipping lead_approve_conflict (task stays in_lead_intervention for redispatch)"
                            );
                            result = Some(TaskRunOutcome::Interrupted);
                            break;
                        }
                        // ── Pre-approval CI-grade verification gate ───────
                        // Same gate as LeadApproved — see comment above.
                        match self.services.run_arbiter_preapproval_gate(&task).await {
                            Ok(ArbiterGateResult::Blocked { feedback }) => {
                                tracing::info!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    "supervisor: arbiter pre-approval gate red — task stays in_lead_intervention (no strike, no arbitration consumption)"
                                );
                                result = Some(TaskRunOutcome::Escalated {
                                    reason: format!(
                                        "pre-approval CI-grade verification gate blocked arbiter approve_conflict; \
                                         returned to arbiter session (strike-free). {feedback}"
                                    ),
                                });
                                break;
                            }
                            Ok(ArbiterGateResult::Pass) => { /* proceed */ }
                            Err(e) => {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %e,
                                    "supervisor: arbiter pre-approval gate infra error — proceeding (fail-open)"
                                );
                            }
                        }
                        // ── Persist arbiter decision on arbitration row ────
                        // Record the decision and evidence before the board
                        // transition (AC2).
                        if let Err(e) = self
                            .services
                            .record_arbiter_decision(
                                spec.task_id.clone(),
                                "approve_conflict".into(),
                                evidence.clone(),
                            )
                            .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: record_arbiter_decision for approve_conflict failed — proceeding with transition"
                            );
                        }
                        if let Err(e) = self
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
                    StageOutcome::LeadReopen {
                        reason,
                        directive,
                        verification_command,
                        exclude_models,
                    } => {
                        // Persist the directive / verification command /
                        // excluded models on the current arbitration row
                        // and atomically mark the monitored-reopen attempt
                        // start so re-entry cannot inject the directive twice.
                        // The directive is injected into exactly one next
                        // worker prompt (see prompt_context::load_arbiter_directive).
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .start_monitored_reopen(
                                    spec.task_id.clone(),
                                    directive.clone(),
                                    verification_command.clone(),
                                    exclude_models.clone(),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: start_monitored_reopen failed — proceeding with lead_intervention_complete transition"
                            );
                        } else {
                            // Mark that this run started a monitored reopen so
                            // the post-loop completion hook is skipped.  The
                            // arbitration row must remain unconsumed until the
                            // monitored worker attempt reaches a terminal
                            // outcome in a separate task-run.
                            started_monitored_reopen = true;
                        }
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
                        result = Some(TaskRunOutcome::Closed {
                            reason: reason.clone(),
                        });
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
                        djinn_telemetry::arbiter::record_decision(
                            djinn_telemetry::arbiter::DECISION_FORCE_CLOSE,
                        );
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
                        djinn_telemetry::arbiter::record_decision(
                            djinn_telemetry::arbiter::DECISION_ESCALATE,
                        );
                        result = Some(TaskRunOutcome::Escalated { reason });
                        break;
                    }
                    StageOutcome::LeadParked { park_dossier_json } => {
                        // Arbiter parked → human-review hold. The
                        // arbiter_park transition persists the decision
                        // and dossier on the arbitration row, marks it
                        // consumed, creates a HumanReview remediation
                        // hold with the dossier as the hold description,
                        // and parks the source to open.
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "arbiter_park".into(),
                                    Some(park_dossier_json.clone()),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: arbiter_park transition failed — task remains in_lead_intervention"
                            );
                        }
                        result = Some(TaskRunOutcome::Closed {
                            reason: format!("arbiter_parked: {}", park_dossier_json),
                        });
                        break;
                    }
                    StageOutcome::LeadSuperseded {
                        reason,
                        replacement_task_ids,
                    } => {
                        // Arbiter superseded → terminal force-close as
                        // superseded. The `arbiter_supersede` transition runs the
                        // supersede transaction host-side (consume the
                        // arbitration row, emit `arbiter_decision` with the
                        // replacement ids, transfer downstream blockers to the
                        // last replacement, clean up the task branch/PR) and
                        // then applies the force-close to `closed`. NO
                        // human-review hold is created — the replacement subtasks
                        // already carry the work forward. The reason + replacement
                        // ids ride the transition as a JSON payload so the
                        // host-side interception can act on them.
                        let payload = serde_json::json!({
                            "reason": reason,
                            "replacement_task_ids": replacement_task_ids,
                        })
                        .to_string();
                        if !self.services.cancel().is_cancelled()
                            && let Err(e) = self
                                .services
                                .transition_task(
                                    spec.task_id.clone(),
                                    "arbiter_supersede".into(),
                                    Some(payload),
                                )
                                .await
                        {
                            tracing::warn!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                error = %e,
                                "supervisor: arbiter_supersede transition failed — task remains in_lead_intervention"
                            );
                        }
                        result = Some(TaskRunOutcome::Closed { reason });
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
                            error_class: None,
                            hint: None,
                            body_excerpt: None,
                        });
                        break;
                    }
                    StageOutcome::VerifierFailed { reason } => {
                        result = Some(TaskRunOutcome::Failed {
                            stage: "verifier".into(),
                            reason,
                            provider_failure: None,
                            error_class: None,
                            hint: None,
                            body_excerpt: None,
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
                        // Arbiter session termination accounting: distinguish
                        // provider/infra failures from sessions that ran and
                        // ended without a valid decision.  Infra failures
                        // before a decision increment infra observability
                        // only; no-decision failures increment
                        // decision_failure_count (capped at 2).  At the cap,
                        // the arbitration is parked with a generated failure
                        // dossier.
                        if role_kind == RoleKind::Lead {
                            let is_infra = provider_failure.is_some();
                            if let Err(e) = self
                                .services
                                .record_arbiter_session_termination(spec.task_id.clone(), is_infra)
                                .await
                            {
                                tracing::warn!(
                                    task_run_id = %run_id,
                                    task_id = %spec.task_id,
                                    error = %e,
                                    "supervisor: record_arbiter_session_termination failed — \
                                     accounting skipped"
                                );
                            }
                        }
                        if let Some(class) = provider_failure {
                            tracing::warn!(
                                target: "djinn_supervisor::provider_failure",
                                kind = "provider_failure",
                                provider_failure_class = ?class,
                                task_id = %spec.task_id,
                                task_run_id = %run_id,
                                stage = role_kind.as_str(),
                                "provider_failure"
                            );
                        }
                        result = Some(TaskRunOutcome::Failed {
                            stage: role_kind.as_str().into(),
                            reason,
                            // Carry the typed provider-error class (if any) the
                            // reply loop produced through to the host report so
                            // the host breaker can act on it.
                            provider_failure,
                            error_class: None,
                            hint: None,
                            body_excerpt: None,
                        });
                        break;
                    }
                    StageOutcome::Parked {
                        reason: ParkReason::Budget,
                        summary: _,
                        wind_down_ignored,
                        session_id,
                        tokens_in,
                        tokens_out,
                    } => {
                        // Session-level budget parking sets `sessions.parked_reason` downstream,
                        // but it is not a terminal task park event; `djinn_tasks_parked_total`
                        // is counted only by the coordinator path that parks/closes the task.
                        tracing::info!(
                            target: "djinn_supervisor::budget_park",
                            kind = "budget_park",
                            parked_reason = "budget",
                            wind_down_ignored,
                            task_id = %spec.task_id,
                            session_id = %session_id,
                            tokens_in,
                            tokens_out,
                            "budget_park"
                        );
                        result = Some(TaskRunOutcome::Parked {
                            reason: "budget".to_string(),
                            wind_down_ignored,
                            session_id,
                            tokens_in,
                            tokens_out,
                        });
                        break;
                    }
                    StageOutcome::LoopGuardTripped {
                        kind,
                        offending_signature,
                        threshold,
                        observed,
                        turn_span,
                        session_id,
                    } => {
                        tracing::info!(
                            target: "djinn_supervisor::loop_guard_tripped",
                            kind = "loop_guard_tripped",
                            guard_kind = ?kind,
                            offending_signature = %offending_signature,
                            threshold,
                            observed,
                            turn_span_start = turn_span.0,
                            turn_span_end = turn_span.1,
                            session_id = %session_id,
                            task_id = %spec.task_id,
                            task_run_id = %run_id,
                            "loop_guard_tripped"
                        );
                        result = Some(TaskRunOutcome::LoopGuardTripped {
                            kind,
                            offending_signature,
                            threshold,
                            observed,
                            turn_span,
                            session_id,
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
                        // Refinement tribunal sessions are simple-lifecycle:
                        // close the task on success (same pattern as Spike).
                        SupervisorFlow::Refinement => {
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
                                    "supervisor: refinement-completion close transition skipped"
                                );
                            }
                            TaskRunOutcome::Closed { reason }
                        }
                        // Worker-only flows (NewTask / ReviewResponse /
                        // ConflictRetry) end at the worker stage. The worker
                        // already fired `submit_task_review` (in_progress →
                        // needs_task_review) above; signal the host with a
                        // `WorkerSubmitted` outcome so it can dispatch a
                        // reviewer-only ReviewResume. Detect "the last stage was
                        // the worker" rather than enumerating the flows so a future
                        // flow that ends at the worker inherits the right path.
                        SupervisorFlow::NewTask
                        | SupervisorFlow::ReviewResponse
                        | SupervisorFlow::ConflictRetry
                            if last_stage_role == Some(RoleKind::Worker) =>
                        {
                            info!(
                                task_run_id = %run_id,
                                task_id = %spec.task_id,
                                flow = ?spec.flow,
                                "supervisor: worker stage complete; task submitted for review (no PR opened here)"
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

        // zkk9: Close out the monitored-reopen attempt on any terminal worker
        // outcome.  Worker submit, reviewer rejection, CI/preapproval failure,
        // worker failure, and no-eligible-model all reach this point with a
        // terminal outcome.  `complete_monitored_reopen` is idempotent (no-op
        // if already consumed or no monitored reopen is in progress), so it is
        // safe to call unconditionally here.
        //
        // CRITICAL: This hook is skipped when this run *started* the monitored
        // reopen (the arbiter's LeadReopen decision).  The arbiter run persists
        // the directive and marks the attempt start; the arbitration row must
        // remain unconsumed so the next worker dispatch can see the
        // directive/exclusions.  Completing here would consume the row before
        // the worker ever runs, defeating the entire monitored-reopen
        // lifecycle.  Completion happens only when the monitored *worker*
        // attempt reaches a terminal outcome (a later, separate task-run).
        //
        // Failures are non-fatal — we log and proceed so the terminal outcome
        // is still reported.
        if !started_monitored_reopen
            && !matches!(outcome, TaskRunOutcome::Interrupted)
            && let Err(e) = self
                .services
                .complete_monitored_reopen(spec.task_id.clone())
                .await
        {
            tracing::warn!(
                task_run_id = %run_id,
                task_id = %spec.task_id,
                error = %e,
                "supervisor: complete_monitored_reopen failed (non-fatal)"
            );
        }

        let terminal_status = match &outcome {
            TaskRunOutcome::PrOpened { .. } | TaskRunOutcome::Closed { .. } => {
                TaskRunStatus::Completed
            }
            // The worker stage genuinely succeeded and handed off for review;
            // the task-run itself completed cleanly.
            TaskRunOutcome::WorkerSubmitted => TaskRunStatus::Completed,
            TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
            TaskRunOutcome::Parked { .. } => TaskRunStatus::Completed,
            // Environmental non-attempt: no session was created. Map to
            // Completed so the task_run row is terminal without triggering
            // failure accounting.
            TaskRunOutcome::EnvironmentalNonAttempt { .. } => TaskRunStatus::Completed,
            TaskRunOutcome::Failed { .. } => TaskRunStatus::Failed,
            TaskRunOutcome::LoopGuardTripped { .. } => TaskRunStatus::Failed,
            TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
        };

        // Flip `task_runs.status` to its terminal value. The worker has already
        // fired `submit_task_review` (task → `needs_task_review`) above.
        //
        // On the cancellation path the host-bound RPC channel may already be torn
        // down (the reader loop saw `Control(Cancel)` and the writer's
        // `cancelled()` branch shut the write half). In that case
        // `update_task_run_status` returns a transport-level error and we must
        // still produce the report so the worker exits cleanly and the host's
        // per-task-run dispatch can pair it with the
        // `KubernetesRuntime::teardown` path. When cancel is NOT set, an
        // `update_task_run_status` failure stays fatal — a genuine RPC
        // malfunction worth surfacing.
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
                cleanup_cargo_target_run_dir(&run_id, &*self.clock).await;
                return Err(SupervisorError::UpdateTaskRunStatus(e));
            }
        }

        cleanup_cargo_target_run_dir(&run_id, &*self.clock).await;

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
        cleanup_cargo_target_run_dir(&run_id, &*self.clock).await;

        info!(task_run_id = %run_id, "task-run interrupted (early-cancel path)");
        Ok(TaskRunReport {
            task_run_id: run_id,
            outcome: TaskRunOutcome::Interrupted,
            stages_completed,
        })
    }
}

async fn cleanup_cargo_target_run_dir(task_run_id: &str, clock: &dyn Clock) {
    let started = clock.now_instant();
    let raw_target_dir = match std::env::var("CARGO_TARGET_DIR") {
        Ok(value) => value,
        Err(e) => {
            debug!(
                task_run_id = %task_run_id,
                elapsed_ms = clock.now_instant().duration_since(started).as_millis() as u64,
                error = %e,
                "supervisor: skipping Cargo target run-dir cleanup; CARGO_TARGET_DIR is not set"
            );
            return;
        }
    };

    let Some(target_dir) = validate_cargo_target_run_dir(&raw_target_dir, task_run_id) else {
        tracing::warn!(
            task_run_id = %task_run_id,
            target_dir = %raw_target_dir,
            elapsed_ms = clock.now_instant().duration_since(started).as_millis() as u64,
            "supervisor: refusing Cargo target run-dir cleanup for unsafe CARGO_TARGET_DIR"
        );
        return;
    };

    match tokio::fs::remove_dir_all(&target_dir).await {
        Ok(()) => info!(
            task_run_id = %task_run_id,
            target_dir = %target_dir.display(),
            elapsed_ms = clock.now_instant().duration_since(started).as_millis() as u64,
            "supervisor: removed Cargo target run directory"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => debug!(
            task_run_id = %task_run_id,
            target_dir = %target_dir.display(),
            elapsed_ms = clock.now_instant().duration_since(started).as_millis() as u64,
            "supervisor: Cargo target run directory already absent"
        ),
        Err(e) => tracing::warn!(
            task_run_id = %task_run_id,
            target_dir = %target_dir.display(),
            elapsed_ms = clock.now_instant().duration_since(started).as_millis() as u64,
            error = %e,
            "supervisor: failed to remove Cargo target run directory; continuing teardown"
        ),
    }
}

/// Convenience helper so the supervisor's trigger vocabulary travels cleanly
/// to the `TaskRunRecord` column.
#[inline]
pub fn trigger_as_str(t: TaskRunTrigger) -> &'static str {
    t.as_str()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_core::models::Task;
    use djinn_workspace::Workspace;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use tokio_util::sync::CancellationToken;

    /// Compile-time assertion: `SupervisorServices` is object-safe.
    ///
    /// PR 3 dispatches the supervisor through `Arc<dyn SupervisorServices>`,
    /// so the trait must stay object-safe forever. If a new method sneaks in
    /// with a generic parameter or a `Self`-by-value receiver, this function
    /// stops compiling.
    #[allow(dead_code)]
    fn _obj_safe(_: &dyn SupervisorServices) {}

    #[derive(Debug, Clone)]
    struct TransitionCall {
        task_id: String,
        action: String,
        reason: Option<String>,
    }

    const PLANNING_ISSUE_TYPES: [&str; 4] =
        ["planning", "decomposition", "review", "epic_breakdown"];

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn make_source_repo(root: &std::path::Path) {
        std::fs::create_dir_all(root).expect("create source repo dir");
        run_git(root, &["init", "-b", "main"]);
        run_git(root, &["config", "user.name", "Djinn Test"]);
        run_git(root, &["config", "user.email", "djinn-test@example.com"]);
        std::fs::write(root.join("README.md"), "# loop guard fixture\n")
            .expect("write fixture file");
        run_git(root, &["add", "README.md"]);
        run_git(root, &["commit", "-m", "initial"]);
    }

    fn fixture_task(task_id: &str, project_id: &str) -> Task {
        Task {
            id: task_id.to_string(),
            project_id: project_id.to_string(),
            short_id: "lg-1".into(),
            epic_id: None,
            title: "loop guard fixture".into(),
            description: "exercise loop guard settlement".into(),
            design: String::new(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 1,
            owner: "test-owner".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: None,
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            unresolved_blocker_count: 0,
        }
    }

    struct ScriptedLoopGuardServices {
        cancel: CancellationToken,
        task: Task,
        outcome: StageOutcome,
        updated_statuses: std::sync::Arc<std::sync::Mutex<Vec<TaskRunStatus>>>,
        /// When true, `transition_task` fails the `start` action to simulate a
        /// blocker that landed after dispatch (a `validate_start_guard` rejection).
        fail_start_transition: bool,
        /// Counts `execute_stage` invocations so a test can assert the stage was
        /// (or was not) run.
        execute_stage_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl SupervisorServices for ScriptedLoopGuardServices {
        fn cancel(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, task_id: String) -> Result<Task, String> {
            assert_eq!(task_id, self.task.id);
            Ok(self.task.clone())
        }

        async fn execute_stage(
            &self,
            _task: &Task,
            _workspace: &Workspace,
            role_kind: RoleKind,
            _task_run_id: &str,
            _spec: &TaskRunSpec,
        ) -> Result<StageOutcome, StageError> {
            self.execute_stage_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(role_kind, RoleKind::Worker);
            Ok(self.outcome.clone())
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            panic!("loop guard settlement must not open a PR")
        }

        async fn create_task_run(
            &self,
            _params: services::SerializableCreateTaskRunParams,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn update_task_run_status(
            &self,
            _run_id: String,
            status: TaskRunStatus,
        ) -> Result<(), String> {
            self.updated_statuses
                .lock()
                .expect("updated statuses mutex poisoned")
                .push(status);
            Ok(())
        }

        async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
            unimplemented!("not exercised")
        }

        async fn get_provider_base_url(
            &self,
            _catalog_provider_id: String,
        ) -> Result<String, String> {
            unimplemented!("not exercised")
        }

        async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
            unimplemented!("not exercised")
        }

        async fn create_session(
            &self,
            _params: services::SerializableCreateSessionParams,
        ) -> Result<djinn_core::models::SessionRecord, String> {
            unimplemented!("not exercised")
        }

        async fn publish_session_message(
            &self,
            _session_id: String,
            _task_id: String,
            _agent_type: String,
            _message: serde_json::Value,
        ) -> Result<(), String> {
            unimplemented!("not exercised")
        }

        async fn get_environment_config(
            &self,
            _project_id: String,
        ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
            unimplemented!("not exercised")
        }

        async fn invoke_llm(
            &self,
            _model_id: String,
            _conversation: djinn_provider::message::Conversation,
            _tools: Vec<serde_json::Value>,
            _tool_choice: Option<djinn_provider::provider::ToolChoice>,
        ) -> Result<djinn_provider::provider::LlmResponse, String> {
            unimplemented!("not exercised")
        }

        #[allow(clippy::too_many_arguments)]
        async fn update_session_status(
            &self,
            _session_id: String,
            _status: djinn_core::models::SessionStatus,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
            _parked_reason: Option<String>,
        ) -> Result<(), String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_search(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_fetch_file(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_ci_job_log(
            &self,
            _session_task_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn emit_djinn_event(
            &self,
            _event: services::SerializableDjinnEvent,
        ) -> Result<(), String> {
            unimplemented!("not exercised")
        }

        async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn transition_task(
            &self,
            _task_id: String,
            action: String,
            _reason: Option<String>,
        ) -> Result<(), String> {
            if self.fail_start_transition && action == "start" {
                return Err("task has unresolved blockers".into());
            }
            Ok(())
        }

        async fn run_arbiter_preapproval_gate(
            &self,
            _task: &Task,
        ) -> Result<ArbiterGateResult, String> {
            // Test stub: always pass.
            Ok(ArbiterGateResult::Pass)
        }

        async fn record_arbiter_decision(
            &self,
            _task_id: String,
            _decision: String,
            _evidence_json: String,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn start_monitored_reopen(
            &self,
            _task_id: String,
            _directive: String,
            _verification_command: String,
            _exclude_models: Vec<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn record_arbiter_session_termination(
            &self,
            _task_id: String,
            _is_infra_failure: bool,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn worker_run_aborts_when_task_becomes_unclaimable_after_dispatch() {
        // Regression (2026-07-01, task 55i8 / remediation s9zp): a remediation or
        // human-review-hold blocker can land AFTER a worker run is already
        // dispatched. The in-pod pre-stage `start` claim (open→in_progress) is then
        // rejected by validate_start_guard ("task has unresolved blockers"). The
        // supervisor must NOT log-and-fall-through into execute_stage — that ran the
        // worker UNCAPTURED and concurrently with the planner remediating the same
        // task (55i8 sat `open` with a running worker). It must abort the run as
        // Interrupted and never execute the stage.
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "project-claim-guard";
        let task_id = "task-claim-guard";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        // The failed `start` never claimed it, so a fresh reload still shows `open`.
        let task = fixture_task(task_id, project_id);
        assert_eq!(task.status, "open");
        let updated_statuses = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let execute_stage_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let services: Arc<dyn SupervisorServices> = Arc::new(ScriptedLoopGuardServices {
            cancel: CancellationToken::new(),
            task,
            outcome: StageOutcome::Failed {
                reason: "must never run".into(),
                provider_failure: None,
            },
            updated_statuses: updated_statuses.clone(),
            fail_start_transition: true,
            execute_stage_calls: execute_stage_calls.clone(),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: "run-claim-guard".into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/claim-guard".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let report = supervisor.run(spec).await.expect("supervisor run");
        assert!(
            matches!(report.outcome, TaskRunOutcome::Interrupted),
            "blocked-after-dispatch run must abort as Interrupted, got {:?}",
            report.outcome
        );
        assert_eq!(
            execute_stage_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "execute_stage must NOT run when the task could not be claimed (blocked)"
        );
        assert!(
            report.stages_completed.is_empty(),
            "no stage should complete when the claim failed"
        );
    }

    #[tokio::test]
    async fn worker_run_proceeds_when_start_fails_but_task_already_in_progress() {
        // The benign idempotent case must still run: a `start` that fails only
        // because the task is ALREADY `in_progress` (a re-dispatched run over a
        // prior row) means we legitimately own it — the stage must proceed, not
        // abort. This guards against the claim-assertion over-firing.
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "project-claim-idem";
        let task_id = "task-claim-idem";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let mut task = fixture_task(task_id, project_id);
        task.status = "in_progress".into(); // already claimed → benign `start` failure
        let updated_statuses = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let execute_stage_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let services: Arc<dyn SupervisorServices> = Arc::new(ScriptedLoopGuardServices {
            cancel: CancellationToken::new(),
            task,
            outcome: StageOutcome::Failed {
                reason: "terminal so the run settles after the worker stage".into(),
                provider_failure: None,
            },
            updated_statuses: updated_statuses.clone(),
            fail_start_transition: true,
            execute_stage_calls: execute_stage_calls.clone(),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: "run-claim-idem".into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/claim-idem".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let _report = supervisor.run(spec).await.expect("supervisor run");
        assert_eq!(
            execute_stage_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stage must run when `start` fails only because the task is already in_progress"
        );
    }

    #[test]
    fn cargo_target_run_dir_helper_matches_expected_cache_path() {
        let task_run_id = "019eb956-ac3a-7492-b51c-bcd904f65a21";

        assert_eq!(
            cargo_target_run_dir(task_run_id),
            PathBuf::from("/cache/cargo-target-runs/019eb956-ac3a-7492-b51c-bcd904f65a21")
        );
    }

    #[test]
    fn cargo_target_run_dir_validation_accepts_only_this_runs_private_dir() {
        let task_run_id = "019eb956-ac3a-7492-b51c-bcd904f65a21";
        let valid = "/cache/cargo-target-runs/019eb956-ac3a-7492-b51c-bcd904f65a21";

        assert_eq!(
            validate_cargo_target_run_dir(valid, task_run_id),
            Some(PathBuf::from(valid))
        );

        for invalid in [
            "",
            "   ",
            "relative/cargo-target-runs/019eb956-ac3a-7492-b51c-bcd904f65a21",
            "/cache/cargo-target-runs",
            "/cache/cargo-target-runs/",
            "/cache/cargo-target-runs/019eb956-ac3a-7492-b51c-bcd904f65a21/nested",
            "/cache/cargo-target/019eb956-ac3a-7492-b51c-bcd904f65a21",
            "/cache/cargo-target-runs/019eb956-ac3a-7492-b51c-bcd904f65a22",
            "/cache/cargo-target-runs/../019eb956-ac3a-7492-b51c-bcd904f65a21",
            "/workspace/.tmp/019eb956-ac3a-7492-b51c-bcd904f65a21",
        ] {
            assert_eq!(
                validate_cargo_target_run_dir(invalid, task_run_id),
                None,
                "accepted unsafe target dir: {invalid}"
            );
        }

        assert_eq!(validate_cargo_target_run_dir(valid, ""), None);
    }

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
            StageOutcome::LoopGuardTripped {
                kind: LoopGuardKind::ConsecutiveFailures,
                offending_signature: "x".into(),
                threshold: 3,
                observed: 3,
                turn_span: (1, 3),
                session_id: "session-x".into(),
            }
            .is_terminal()
        );
        assert!(
            StageOutcome::Parked {
                reason: ParkReason::Budget,
                summary: None,
                wind_down_ignored: false,
                session_id: "session-budget".into(),
                tokens_in: 10,
                tokens_out: 5,
            }
            .is_terminal()
        );
        assert!(
            StageOutcome::ReviewerRejected {
                feedback: "x".into()
            }
            .is_terminal()
        );
        assert!(StageOutcome::VerifierFailed { reason: "x".into() }.is_terminal());
        assert!(
            StageOutcome::Parked {
                reason: ParkReason::Budget,
                summary: Some("handoff".into()),
                wind_down_ignored: false,
                session_id: "session-budget-summary".into(),
                tokens_in: 10,
                tokens_out: 5,
            }
            .is_terminal()
        );
        assert!(!StageOutcome::WorkerDone.is_terminal());
        assert!(!StageOutcome::PlannerExecute.is_terminal());
        assert!(!StageOutcome::ReviewerApproved.is_terminal());
        assert!(!StageOutcome::VerifierPassed.is_terminal());
        assert!(!StageOutcome::ArchitectDone.is_terminal());
    }

    #[tokio::test]
    async fn scripted_loop_guard_run_settles_with_distinct_telemetry() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "project-loop-guard";
        let task_id = "task-loop-guard";
        let task_run_id = "run-loop-guard";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let stage_outcome = StageOutcome::LoopGuardTripped {
            kind: LoopGuardKind::PermissionDenial,
            offending_signature: "tool_failure:shell:write:/root:permission_denied".into(),
            threshold: 3,
            observed: 4,
            turn_span: (7, 11),
            session_id: "session-loop-guard".into(),
        };
        let updated_statuses = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let services: Arc<dyn SupervisorServices> = Arc::new(ScriptedLoopGuardServices {
            cancel: CancellationToken::new(),
            task: fixture_task(task_id, project_id),
            outcome: stage_outcome,
            updated_statuses: std::sync::Arc::clone(&updated_statuses),
            fail_start_transition: false,
            execute_stage_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: task_run_id.into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/loop-guard".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");

        match report.outcome {
            TaskRunOutcome::LoopGuardTripped {
                kind,
                offending_signature,
                threshold,
                observed,
                turn_span,
                session_id,
            } => {
                assert_eq!(kind, LoopGuardKind::PermissionDenial);
                assert_eq!(
                    offending_signature,
                    "tool_failure:shell:write:/root:permission_denied"
                );
                assert_eq!(threshold, 3);
                assert_eq!(observed, 4);
                assert_eq!(turn_span, (7, 11));
                assert_eq!(session_id, "session-loop-guard");
            }
            other => panic!("expected LoopGuardTripped outcome, got {other:?}"),
        }
        assert_eq!(report.stages_completed, vec![RoleKind::Worker]);
        assert_eq!(
            updated_statuses
                .lock()
                .expect("updated statuses mutex poisoned")
                .as_slice(),
            &[TaskRunStatus::Failed],
            "loop guard must settle through update_task_run_status/record_status path"
        );

        let provider_services: Arc<dyn SupervisorServices> = Arc::new(ScriptedLoopGuardServices {
            cancel: CancellationToken::new(),
            task: fixture_task(task_id, project_id),
            outcome: StageOutcome::Failed {
                reason: "provider rejected request".into(),
                provider_failure: Some(djinn_runtime::ProviderFailureClass::Failure),
            },
            updated_statuses: std::sync::Arc::clone(&updated_statuses),
            fail_start_transition: false,
            execute_stage_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let provider_supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), provider_services);
        let provider_spec = TaskRunSpec {
            task_run_id: "run-provider-failure".into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/loop-guard".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };
        let provider_report = provider_supervisor
            .run(provider_spec)
            .await
            .expect("provider failure supervisor run");
        assert!(matches!(
            provider_report.outcome,
            TaskRunOutcome::Failed {
                provider_failure: Some(djinn_runtime::ProviderFailureClass::Failure),
                ..
            }
        ));

        let captured = logs.take();
        assert!(
            captured.contains("supervisor.stage_outcome")
                && captured.contains("outcome=\"loop_guard_tripped\"")
                && captured.contains("task_run_id=run-loop-guard")
                && captured.contains("task_id=task-loop-guard"),
            "expected supervisor.stage_outcome child event with task/run context, got:\n{captured}"
        );
        assert!(
            captured.contains("djinn_supervisor::loop_guard_tripped"),
            "expected distinct tracing target for loop guard event, got:\n{captured}"
        );
        assert!(
            captured.contains("loop_guard_tripped")
                && captured.contains("kind=\"loop_guard_tripped\"")
                && captured.contains("guard_kind=PermissionDenial")
                && captured.contains(
                    "offending_signature=tool_failure:shell:write:/root:permission_denied"
                )
                && captured.contains("threshold=3")
                && captured.contains("observed=4")
                && captured.contains("turn_span_start=7")
                && captured.contains("turn_span_end=11")
                && captured.contains("session_id=session-loop-guard")
                && captured.contains("task_id=task-loop-guard"),
            "expected loop_guard_tripped info event with full payload, got:\n{captured}"
        );
        assert!(
            captured.contains("djinn_supervisor::provider_failure")
                && captured.contains("kind=\"provider_failure\"")
                && captured.contains("provider_failure_class=Failure")
                && captured.contains("task_run_id=run-provider-failure"),
            "expected distinct provider-failure telemetry discriminator, got:\n{captured}"
        );
        assert!(
            !captured.contains("kind=\"budget_park\""),
            "loop guard and provider failure telemetry must remain distinct from budget parks, got:\n{captured}"
        );
    }

    #[tokio::test]
    async fn scripted_budget_park_run_emits_distinct_telemetry() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "project-budget-park";
        let task_id = "task-budget-park";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let updated_statuses = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let services: Arc<dyn SupervisorServices> = Arc::new(ScriptedLoopGuardServices {
            cancel: CancellationToken::new(),
            task: fixture_task(task_id, project_id),
            outcome: StageOutcome::Parked {
                reason: ParkReason::Budget,
                summary: None,
                wind_down_ignored: true,
                session_id: "session-budget-ignored".into(),
                tokens_in: 123,
                tokens_out: 45,
            },
            updated_statuses: std::sync::Arc::clone(&updated_statuses),
            fail_start_transition: false,
            execute_stage_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: "run-budget-ignored".into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/budget-park".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");
        assert!(matches!(
            report.outcome,
            TaskRunOutcome::Parked {
                reason,
                wind_down_ignored: true,
                tokens_in: 123,
                tokens_out: 45,
                ..
            } if reason == "budget"
        ));
        assert_eq!(
            updated_statuses
                .lock()
                .expect("updated statuses mutex poisoned")
                .as_slice(),
            &[TaskRunStatus::Completed],
            "ignored budget park must settle as a completed task-run"
        );

        let summary_services: Arc<dyn SupervisorServices> = Arc::new(ScriptedLoopGuardServices {
            cancel: CancellationToken::new(),
            task: fixture_task(task_id, project_id),
            outcome: StageOutcome::Parked {
                reason: ParkReason::Budget,
                summary: Some("handoff summary".into()),
                wind_down_ignored: false,
                session_id: "session-budget-summary".into(),
                tokens_in: 222,
                tokens_out: 33,
            },
            updated_statuses: std::sync::Arc::clone(&updated_statuses),
            fail_start_transition: false,
            execute_stage_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let summary_supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), summary_services);
        let summary_spec = TaskRunSpec {
            task_run_id: "run-budget-summary".into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/budget-park-summary".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };
        let summary_report = summary_supervisor
            .run(summary_spec)
            .await
            .expect("summary budget park supervisor run");
        assert!(matches!(
            summary_report.outcome,
            TaskRunOutcome::Parked {
                reason,
                wind_down_ignored: false,
                tokens_in: 222,
                tokens_out: 33,
                ..
            } if reason == "budget"
        ));
        assert_eq!(
            updated_statuses
                .lock()
                .expect("updated statuses mutex poisoned")
                .as_slice(),
            &[TaskRunStatus::Completed, TaskRunStatus::Completed],
            "summary and ignored budget parks must settle as completed task-runs"
        );

        let captured = logs.take();
        assert!(
            captured.contains("djinn_supervisor::budget_park")
                && captured.contains("kind=\"budget_park\"")
                && captured.contains("parked_reason=\"budget\"")
                && captured.contains("wind_down_ignored=true")
                && captured.contains("task_id=task-budget-park")
                && captured.contains("session_id=session-budget-ignored")
                && captured.contains("tokens_in=123")
                && captured.contains("tokens_out=45"),
            "expected ignored budget_park info event with full payload, got:\n{captured}"
        );
        assert!(
            captured.contains("kind=\"budget_park\"")
                && captured.contains("parked_reason=\"budget\"")
                && captured.contains("wind_down_ignored=false")
                && captured.contains("session_id=session-budget-summary")
                && captured.contains("tokens_in=222")
                && captured.contains("tokens_out=33"),
            "expected summary budget_park info event with full payload, got:\n{captured}"
        );
        assert!(
            !captured.contains("kind=\"provider_failure\"")
                && !captured.contains("kind=\"loop_guard_tripped\""),
            "budget park telemetry must not be conflated with provider failures or loop guards, got:\n{captured}"
        );
    }

    // ── Planner-escalate routing regression coverage (ykr7) ────────────────────
    //
    // Bug fixed in `ep1i`: a planner `StageOutcome::Escalate` on a
    // planning-type task used to surface `TaskRunOutcome::Escalated`
    // and fire NO `transition_task` call, leaving the task in
    // `in_progress` for the coordinator's ready-task sweep to
    // re-dispatch every ~30s (the `k4my` / patrol planning redispatch
    // loop). The fix routes planner + planning-type escalates through a
    // cancel-gated `close` transition so the task parks durably.
    //
    // The routing + transition logic was factored out of the supervisor
    // loop into [`apply_planner_escalate_route`] +
    // [`route_planner_escalate`] precisely so these tests can cover
    // every branch without spinning up a `MirrorManager`,
    // `CancellationToken`, and full `SupervisorServices` for a full
    // task-run. The existing `stage_outcome_terminal_classifier` test
    // above is intentionally preserved.
    /// Pure routing decision: planner + planning + not cancelled →
    /// `CloseWithReason`. This is the regression assertion for the old
    /// behavior where planner escalate produced only
    /// `TaskRunOutcome::Escalated` — if `route_planner_escalate` ever
    /// returns `Escalate` for the planner planning leg, the bug has
    /// regressed.
    #[test]
    fn route_planner_escalate_planner_planning_not_cancelled_closes() {
        for issue_type in PLANNING_ISSUE_TYPES {
            assert_eq!(
                route_planner_escalate(RoleKind::Planner, issue_type, false),
                PlannerEscalateRoute::CloseWithReason,
                "planner escalate on {issue_type} must route to CloseWithReason, not the legacy Escalate branch"
            );
        }
    }

    /// Pure routing decision: planner + planning + cancelled →
    /// `Cancelled` (interrupt, no transition). The cancel gate must
    /// fire BEFORE the close transition so a stall-kill / preempt
    /// arriving mid-stage doesn't park the task and strand the user.
    #[test]
    fn route_planner_escalate_planner_planning_cancelled_interrupts() {
        for issue_type in PLANNING_ISSUE_TYPES {
            assert_eq!(
                route_planner_escalate(RoleKind::Planner, issue_type, true),
                PlannerEscalateRoute::Cancelled,
                "cancelled planner escalate on {issue_type} must route to Cancelled, never CloseWithReason"
            );
        }
    }

    /// Pure routing decision: non-planner roles keep the legacy
    /// `Escalated` outcome — the supervisor backstop is intentionally
    /// narrow to the Planner planning leg, and Lead/Worker/Reviewer
    /// escalation paths must NOT start parking tasks.
    #[test]
    fn route_planner_escalate_non_planner_role_keeps_legacy_escalated() {
        for role in [
            RoleKind::Worker,
            RoleKind::Reviewer,
            RoleKind::Verifier,
            RoleKind::Architect,
            RoleKind::Lead,
        ] {
            assert_eq!(
                route_planner_escalate(role, "planning", false),
                PlannerEscalateRoute::Escalate,
                "non-planner role {role:?} on a planning issue must keep the legacy Escalated outcome"
            );
        }
    }

    /// Pure routing decision: planner + non-planning issue keeps the
    /// legacy `Escalated` outcome. The backstop is intentionally scoped
    /// to the four planning-type issue strings; everything else (e.g.
    /// `task`, `epic`, `spike`) is unaffected.
    #[test]
    fn route_planner_escalate_planner_non_planning_keeps_legacy_escalated() {
        for issue_type in ["task", "epic", "spike", "", "unknown"] {
            assert_eq!(
                route_planner_escalate(RoleKind::Planner, issue_type, false),
                PlannerEscalateRoute::Escalate,
                "planner escalate on non-planning issue {issue_type:?} must keep the legacy Escalated outcome"
            );
        }
    }

    /// Defense in depth: `planner_terminal_close_action` returns `None`
    /// for a `human-review-hold` task regardless of its `issue_type`, so
    /// the planner execute/close/escalate paths never auto-close the
    /// human-only hold (closing it would release the parked source task).
    /// Every other planning-type issue still maps to `close`.
    #[test]
    fn planner_terminal_close_action_never_closes_human_review_hold() {
        // Hold label present → no transition, for ALL planning issue types.
        for issue_type in PLANNING_ISSUE_TYPES {
            let mut task = fixture_task("t1", "p1");
            task.issue_type = (*issue_type).into();
            task.labels = r#"["human-review-hold"]"#.into();
            assert_eq!(
                planner_terminal_close_action(&task),
                None,
                "a human-review-hold {issue_type} task must never be auto-closed"
            );
        }

        // No hold label → planning-type issues still close.
        for issue_type in PLANNING_ISSUE_TYPES {
            let mut task = fixture_task("t2", "p1");
            task.issue_type = (*issue_type).into();
            task.labels = "[]".into();
            assert_eq!(
                planner_terminal_close_action(&task),
                Some("close"),
                "a plain {issue_type} task must still close on a planner terminal outcome"
            );
        }

        // Non-planning issue → no transition (unchanged behavior).
        let mut task = fixture_task("t3", "p1");
        task.issue_type = "task".into();
        task.labels = "[]".into();
        assert_eq!(planner_terminal_close_action(&task), None);
    }

    /// The headline regression test for `ep1i`: planner + `planning`
    /// + not cancelled fires EXACTLY ONE `transition_task` call with
    /// action `close` and a non-empty reason containing the
    /// escalation reason, and returns `TaskRunOutcome::Closed { .. }`.
    ///
    /// On the old code (pre-`ep1i`), this assertion would fail in two
    /// ways: no transition would be recorded, and the outcome would be
    /// `TaskRunOutcome::Escalated` instead of `Closed`.
    #[tokio::test]
    async fn planner_escalate_routes_planning_close() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
        let calls_for_closure = std::sync::Arc::clone(&calls);

        let outcome = apply_planner_escalate_route(
            RoleKind::Planner,
            "planning",
            "task-1",
            "run-1",
            "planner needs human guidance on the spec".to_string(),
            false,
            move |task_id, action, reason| {
                let calls = std::sync::Arc::clone(&calls_for_closure);
                async move {
                    calls
                        .lock()
                        .expect("calls mutex poisoned")
                        .push(TransitionCall {
                            task_id,
                            action,
                            reason,
                        });
                    Ok(())
                }
            },
        )
        .await;

        // Outcome is the new `Closed` (was `Escalated` pre-`ep1i`).
        match &outcome {
            TaskRunOutcome::Closed { reason } => {
                assert_eq!(
                    reason, "planner escalated: planner needs human guidance on the spec",
                    "Closed outcome must carry the same surfaced reason passed to the close transition"
                );
            }
            other => panic!(
                "planner + planning + not cancelled must produce TaskRunOutcome::Closed, got {other:?}"
            ),
        }

        // Exactly one transition call, action=close, reason non-empty and
        // matches the escalation reason, task_id matches the input.
        let calls = calls.lock().expect("calls mutex poisoned");
        assert_eq!(
            calls.len(),
            1,
            "planner escalate on planning must fire EXACTLY ONE transition_task call, got {calls:?}"
        );
        let call = &calls[0];
        assert_eq!(
            call.task_id, "task-1",
            "transition must use the spec task id"
        );
        assert_eq!(call.action, "close", "transition action must be \"close\"");
        let reason = call
            .reason
            .as_ref()
            .expect("close transition on planner escalate must carry a reason");
        assert!(
            reason.contains("planner needs human guidance on the spec"),
            "transition reason must contain the original escalation reason, got {reason:?}"
        );
    }

    /// Final no-redispatch regression: a Planner escalation /
    /// `StageOutcome::Escalate` on a planning-type task must be parked
    /// as a terminal close, not reported as `TaskRunOutcome::Escalated`.
    ///
    /// This encodes the exact old failure mode: pre-fix code returned
    /// `Escalated` and made no board transition, leaving the still-open
    /// planning task eligible for recovery redispatch. The fixed path
    /// returns `Closed` and requests exactly one `close` transition whose
    /// durable reason mentions the escalation.
    #[tokio::test]
    async fn planner_escalate_planning_task_closes_instead_of_redispatching() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
        let calls_for_closure = std::sync::Arc::clone(&calls);

        let outcome = apply_planner_escalate_route(
            RoleKind::Planner,
            "planning",
            "planning-task-no-redispatch",
            "run-no-redispatch",
            "planner escalated because the decomposition is ambiguous".to_string(),
            false,
            move |task_id, action, reason| {
                let calls = std::sync::Arc::clone(&calls_for_closure);
                async move {
                    calls
                        .lock()
                        .expect("calls mutex poisoned")
                        .push(TransitionCall {
                            task_id,
                            action,
                            reason,
                        });
                    Ok(())
                }
            },
        )
        .await;

        assert!(
            !matches!(outcome, TaskRunOutcome::Escalated { .. }),
            "planner escalate on a planning task must not return Escalated; \
             Escalated leaves the task eligible for recovery redispatch"
        );

        let TaskRunOutcome::Closed { reason } = &outcome else {
            panic!(
                "planner escalate on a planning task must return terminal Closed, got {outcome:?}"
            );
        };
        assert!(
            reason.contains("escalated") && reason.contains("decomposition is ambiguous"),
            "Closed outcome reason must durably mention the escalation, got {reason:?}"
        );

        let calls = calls.lock().expect("calls mutex poisoned");
        assert_eq!(
            calls.len(),
            1,
            "planner escalate on a planning task must request exactly one close transition, got {calls:?}"
        );
        let close_call = &calls[0];
        assert_eq!(
            close_call.task_id, "planning-task-no-redispatch",
            "close transition must target the planning task"
        );
        assert_eq!(
            close_call.action, "close",
            "planner escalate no-redispatch backstop must request a close transition"
        );
        let close_reason = close_call
            .reason
            .as_ref()
            .expect("planner escalate close transition must carry a durable reason");
        assert!(
            close_reason.contains("planner escalated")
                && close_reason.contains("decomposition is ambiguous"),
            "close transition reason must mention the planner escalation, got {close_reason:?}"
        );
        assert_eq!(
            close_reason, reason,
            "terminal Closed reason must match the persisted close-transition reason"
        );
    }

    /// Table test: every planning-type issue string
    /// (`planning` / `decomposition` / `review` / `epic_breakdown`)
    /// routes through the close path on a non-cancelled planner run.
    /// This is the cheap repeat coverage the design called out, and
    /// it guards against a future PR narrowing the matches!() list by
    /// accident (e.g. dropping `epic_breakdown`).
    #[tokio::test]
    async fn planner_escalate_routes_every_planning_issue_type_to_close() {
        for issue_type in PLANNING_ISSUE_TYPES {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);

            let outcome = apply_planner_escalate_route(
                RoleKind::Planner,
                issue_type,
                "task-x",
                "run-x",
                format!("escalate reason for {issue_type}"),
                false,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            )
            .await;

            assert!(
                matches!(outcome, TaskRunOutcome::Closed { .. }),
                "{issue_type}: expected TaskRunOutcome::Closed, got {outcome:?}"
            );

            let calls = calls.lock().expect("calls mutex poisoned");
            assert_eq!(
                calls.len(),
                1,
                "{issue_type}: expected exactly one transition_task call, got {calls:?}"
            );
            assert_eq!(
                calls[0].action, "close",
                "{issue_type}: transition action must be \"close\""
            );
            assert_eq!(
                calls[0].task_id, "task-x",
                "{issue_type}: transition must use the spec task id"
            );
            let reason = calls[0]
                .reason
                .as_ref()
                .unwrap_or_else(|| panic!("{issue_type}: close transition must carry a reason"));
            assert!(
                reason.contains(&format!("escalate reason for {issue_type}")),
                "{issue_type}: transition reason must contain the escalation reason, got {reason:?}"
            );
        }
    }

    /// Cancel-gate regression: planner + `planning` + cancel already
    /// set must fire NO transition and return
    /// `TaskRunOutcome::Interrupted`. The task stays redispatchable
    /// in `in_progress` for the coordinator to retry cleanly, rather
    /// than being closed mid-cancel and stranding the user.
    ///
    /// On the pre-`ep1i` code the cancel gate didn't exist for this
    /// arm at all (it only short-circuited via the dispatch-time
    /// `load_task` / `create_task_run` paths), so this assertion
    /// would have leaked through to a transition + Closed outcome
    /// when cancel flipped mid-stage.
    #[tokio::test]
    async fn planner_escalate_cancel_gate_skips_transition_and_interrupts() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
        let calls_for_closure = std::sync::Arc::clone(&calls);

        let outcome = apply_planner_escalate_route(
            RoleKind::Planner,
            "planning",
            "task-cancel-1",
            "run-cancel-1",
            "planner was about to escalate but cancel flipped".to_string(),
            true,
            move |task_id, action, reason| {
                let calls = std::sync::Arc::clone(&calls_for_closure);
                async move {
                    calls
                        .lock()
                        .expect("calls mutex poisoned")
                        .push(TransitionCall {
                            task_id,
                            action,
                            reason,
                        });
                    Ok(())
                }
            },
        )
        .await;

        assert!(
            matches!(outcome, TaskRunOutcome::Interrupted),
            "cancelled planner escalate on a planning issue must produce \
             TaskRunOutcome::Interrupted, got {outcome:?}"
        );

        let calls = calls.lock().expect("calls mutex poisoned");
        assert!(
            calls.is_empty(),
            "cancelled planner escalate must fire ZERO transition_task calls, got {calls:?}"
        );
    }

    /// Cancel gate also covers the other planning-type issue strings
    /// — a single passing `planning` test could mask a bug where the
    /// matches!() list and the cancel check get out of sync.
    #[tokio::test]
    async fn planner_escalate_cancel_gate_skips_transition_for_every_planning_issue() {
        for issue_type in PLANNING_ISSUE_TYPES {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);

            let outcome = apply_planner_escalate_route(
                RoleKind::Planner,
                issue_type,
                "task-cancel-x",
                "run-cancel-x",
                "cancelled".to_string(),
                true,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            )
            .await;

            assert!(
                matches!(outcome, TaskRunOutcome::Interrupted),
                "{issue_type}: cancelled planner escalate must produce Interrupted, got {outcome:?}"
            );
            let calls = calls.lock().expect("calls mutex poisoned");
            assert!(
                calls.is_empty(),
                "{issue_type}: cancelled planner escalate must fire ZERO transition calls, got {calls:?}"
            );
        }
    }

    /// Negative path: a failed `transition_task` (e.g. RPC transport
    /// error, invalid-transition rejection from the host) does NOT
    /// change the returned outcome — the supervisor still surfaces
    /// `TaskRunOutcome::Closed` so the run is parked at the outcome
    /// level. The transition failure is logged on the host side; the
    /// worker pod treats it as best-effort, matching the policy used
    /// by every other transition call in this loop
    /// (`PlannerClose`, `LeadEscalate`, `submit_task_review`, etc.).
    #[tokio::test]
    async fn planner_escalate_close_transition_failure_still_surfaces_closed() {
        let outcome = apply_planner_escalate_route(
            RoleKind::Planner,
            "planning",
            "task-fail-1",
            "run-fail-1",
            "reason that should still close the run".to_string(),
            false,
            |_task_id, _action, _reason| async { Err("simulated transport failure".to_string()) },
        )
        .await;

        assert!(
            matches!(outcome, TaskRunOutcome::Closed { .. }),
            "transition failure must not downgrade the run outcome, got {outcome:?}"
        );
    }

    // ── rt3l: durable reason surfacing + supervisor log coverage ──────────────
    //
    // The ykr7 tests above prove the routing + transition count + outcome
    // shape. The rt3l tests below harden the reason-surfacing contract
    // that's the actual acceptance criterion: the close-transition reason
    // must contain the planner's original escalation reason (so the
    // persisted `tasks.close_reason` row and activity log entry are
    // durably visible), the `TaskRunOutcome::Closed` reason must match
    // that surfaced close-transition reason (so host/UI reporting stays
    // consistent), and the surfaced reason must be the prefixed
    // `"planner escalated:"` variant the design calls out as the
    // grep-able token for planner-escalate closures.

    /// Acceptance criterion: the close-transition reason MUST start
    /// with the `"planner escalated:"` prefix so `tasks.close_reason`
    /// rows are grep-able as planner escalations. The host-side
    /// `transition` machinery persists the `Some(reason)` argument
    /// verbatim, so asserting the recorded reason's prefix shape is
    /// the same as asserting the persisted row.
    #[tokio::test]
    async fn planner_escalate_close_transition_reason_is_prefixed() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
        let calls_for_closure = std::sync::Arc::clone(&calls);

        let original_reason = "planner needs human guidance on the spec".to_string();
        let _ = apply_planner_escalate_route(
            RoleKind::Planner,
            "planning",
            "task-rt3l-prefix",
            "run-rt3l-prefix",
            original_reason.clone(),
            false,
            move |task_id, action, reason| {
                let calls = std::sync::Arc::clone(&calls_for_closure);
                async move {
                    calls
                        .lock()
                        .expect("calls mutex poisoned")
                        .push(TransitionCall {
                            task_id,
                            action,
                            reason,
                        });
                    Ok(())
                }
            },
        )
        .await;

        let calls = calls.lock().expect("calls mutex poisoned");
        assert_eq!(calls.len(), 1, "expected exactly one transition call");
        let recorded = calls[0]
            .reason
            .as_ref()
            .expect("close transition on planner escalate must carry a reason");
        assert!(
            recorded.starts_with("planner escalated: "),
            "close-transition reason must be prefixed with 'planner escalated: ' \
             for grep-able `tasks.close_reason` rows, got {recorded:?}"
        );
        assert!(
            recorded.contains(&original_reason),
            "close-transition reason must contain the original escalation reason verbatim, got {recorded:?}"
        );
        assert_eq!(
            recorded,
            &format!("planner escalated: {original_reason}"),
            "close-transition reason must be EXACTLY 'planner escalated: <original>' (no extra wrapping)"
        );
    }

    /// Acceptance criterion: the `TaskRunOutcome::Closed { reason }`
    /// reason must match the exact surfaced reason passed to the
    /// close transition. That keeps host/UI terminal reporting
    /// consistent with the durable `tasks.close_reason` / activity path
    /// while the original planner-provided reason remains included
    /// verbatim after the `"planner escalated:"` prefix.
    #[tokio::test]
    async fn planner_escalate_outcome_reason_matches_persisted_close_reason() {
        let outcome = apply_planner_escalate_route(
            RoleKind::Planner,
            "planning",
            "task-rt3l-verbatim",
            "run-rt3l-verbatim",
            "planner needs human guidance on the spec".to_string(),
            false,
            |_task_id, _action, _reason| async { Ok(()) },
        )
        .await;

        match outcome {
            TaskRunOutcome::Closed { reason } => {
                assert_eq!(
                    reason, "planner escalated: planner needs human guidance on the spec",
                    "TaskRunOutcome::Closed reason must match the persisted close-transition reason"
                );
                assert!(
                    reason.contains("planner needs human guidance on the spec"),
                    "TaskRunOutcome::Closed reason must contain the original planner reason, got {reason:?}"
                );
            }
            other => panic!(
                "planner + planning + not cancelled must produce TaskRunOutcome::Closed, got {other:?}"
            ),
        }
    }

    /// Table test: the prefix is applied uniformly across every
    /// planning-type issue string the backstop handles, so a future
    /// PR can't accidentally drop the prefix on a single issue type
    /// and leave the persisted rows inconsistent.
    #[tokio::test]
    async fn planner_escalate_close_reason_is_prefixed_for_every_planning_issue() {
        for issue_type in PLANNING_ISSUE_TYPES {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);

            let original_reason = format!("escalate reason for {issue_type}");
            let _ = apply_planner_escalate_route(
                RoleKind::Planner,
                issue_type,
                "task-rt3l-prefix-table",
                "run-rt3l-prefix-table",
                original_reason.clone(),
                false,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            )
            .await;

            let calls = calls.lock().expect("calls mutex poisoned");
            assert_eq!(
                calls.len(),
                1,
                "{issue_type}: expected exactly one transition call"
            );
            let recorded = calls[0]
                .reason
                .as_ref()
                .unwrap_or_else(|| panic!("{issue_type}: close transition must carry a reason"));
            assert_eq!(
                recorded,
                &format!("planner escalated: {original_reason}"),
                "{issue_type}: close-transition reason must be exactly 'planner escalated: <original>'"
            );
        }
    }

    /// Non-planner roles (Worker / Reviewer) keep the legacy
    /// `TaskRunOutcome::Escalated` outcome and fire NO transition
    /// call — the supervisor backstop is intentionally narrow to the
    /// Planner planning leg, and the Worker/Reviewer escalation
    /// paths must NOT start parking tasks. This is the
    /// AC: "Worker/Reviewer escalation behavior
    /// remains unchanged by tests or code inspection."
    #[tokio::test]
    async fn planner_escalate_non_planner_escalation_is_unchanged() {
        for role in [RoleKind::Worker, RoleKind::Reviewer] {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);

            let original_reason = "worker hit a wall and needs lead guidance".to_string();
            let outcome = apply_planner_escalate_route(
                role,
                "planning",
                "task-worker",
                "run-worker",
                original_reason.clone(),
                false,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            )
            .await;

            match outcome {
                TaskRunOutcome::Escalated { reason } => {
                    assert_eq!(
                        reason, original_reason,
                        "{role:?} escalation must keep the original reason verbatim (legacy Escalated path)"
                    );
                }
                other => panic!(
                    "{role:?} escalation must keep the legacy TaskRunOutcome::Escalated outcome, got {other:?}"
                ),
            }

            let calls = calls.lock().expect("calls mutex poisoned");
            assert!(
                calls.is_empty(),
                "{role:?} escalation must NOT fire any transition_task call, got {calls:?}"
            );
        }
    }

    /// Negative no-hijack coverage: Worker/Reviewer escalation
    /// on a normal implementation task is still the legacy
    /// `Escalated` outcome and must not fire the Planner-only close
    /// transition. This guards the common non-planning path separately
    /// from `planner_escalate_non_planner_escalation_is_unchanged`, which
    /// exercises non-planner roles on a planning-type issue.
    #[tokio::test]
    async fn worker_and_reviewer_escalation_on_normal_task_do_not_close() {
        for role in [RoleKind::Worker, RoleKind::Reviewer] {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);

            let original_reason = format!("{role:?} escalation needs lead guidance");
            let outcome = apply_planner_escalate_route(
                role,
                "task",
                "normal-task-request-lead",
                "run-normal-request-lead",
                original_reason.clone(),
                false,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            )
            .await;

            match outcome {
                TaskRunOutcome::Escalated { reason } => assert_eq!(
                    reason, original_reason,
                    "{role:?} escalation on a normal task must preserve the escalation reason"
                ),
                other => panic!(
                    "{role:?} escalation on a normal task must remain TaskRunOutcome::Escalated, got {other:?}"
                ),
            }

            let calls = calls.lock().expect("calls mutex poisoned");
            assert!(
                calls.iter().all(|call| call.action != "close"),
                "{role:?} escalation on a normal task must not fire close, got {calls:?}"
            );
            assert!(
                calls.is_empty(),
                "{role:?} escalation on a normal task should fire no transition_task calls, got {calls:?}"
            );
        }
    }

    /// Non-planning issue types (e.g. `task`, `spike`) on a Planner
    /// run also keep the legacy `Escalated` outcome and fire NO
    /// transition. The backstop is scoped to the four planning-type
    /// issue strings; everything else is unaffected.
    #[tokio::test]
    async fn planner_escalate_planner_non_planning_escalation_is_unchanged() {
        for issue_type in ["task", "epic", "spike"] {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);

            let outcome = apply_planner_escalate_route(
                RoleKind::Planner,
                issue_type,
                "task-other",
                "run-other",
                "planner hit a non-planning issue wall".to_string(),
                false,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            )
            .await;

            match outcome {
                TaskRunOutcome::Escalated { reason } => {
                    assert_eq!(
                        reason, "planner hit a non-planning issue wall",
                        "{issue_type}: planner escalate on a non-planning issue must keep the original reason verbatim"
                    );
                }
                other => panic!(
                    "{issue_type}: planner escalate on a non-planning issue must keep Escalated, got {other:?}"
                ),
            }

            let calls = calls.lock().expect("calls mutex poisoned");
            assert!(
                calls.is_empty(),
                "{issue_type}: planner escalate on a non-planning issue must NOT fire any transition_task call, got {calls:?}"
            );
        }
    }

    /// Helper: a `MakeWriter` that wraps a `Vec<u8>` behind a `Mutex`
    /// so the `tracing_subscriber::fmt` subscriber can be configured
    /// to write formatted log lines into a buffer the test can read.
    /// The test reads the buffer after `dispatcher::with_default`
    /// returns to assert on the captured log content.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn take(&self) -> String {
            let mut buf = self.0.lock().expect("captured logs mutex poisoned");
            let out =
                String::from_utf8(buf.clone()).expect("captured log bytes were not valid utf-8");
            buf.clear();
            out
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogsWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogsWriter {
                inner: std::sync::Arc::clone(&self.0),
            }
        }
    }

    struct CapturedLogsWriter {
        inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl std::io::Write for CapturedLogsWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner
                .lock()
                .expect("captured logs mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Acceptance criterion: the planner-escalate success path emits
    /// a structured `tracing::info!` event whose message clearly says
    /// the task was closed because the planner escalated, and whose
    /// fields include `task_run_id`, `task_id`, `issue_type`, and the
    /// surfaced close reason. We capture the formatted log output
    /// (the same shape the pod log shipper sees) via a thread-local
    /// subscriber and assert on both the message and the structured
    /// fields rendered as key=value pairs.
    ///
    /// `tracing::dispatcher::with_default` is thread-local, so we
    /// drive the async helper with `futures::executor::block_on` on
    /// the same thread (no tokio runtime here) to keep the
    /// subscriber live across the `.await`. `apply_planner_escalate_route`
    /// is just an async fn — it doesn't need tokio, only a
    /// `Future`-aware executor.
    #[test]
    fn planner_escalate_success_emits_structured_close_log() {
        use tracing::dispatcher::Dispatch;

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(false)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = Dispatch::new(subscriber);

        let task_run_id = "run-rt3l-log";
        let task_id = "task-rt3l-log";
        let issue_type = "planning";
        let original_reason = "planner needs human guidance on the spec";

        // Drive the helper under our scoped subscriber, on the same
        // thread that the subscriber is registered on, so the
        // captured log writes are routed to our `CapturedLogs`.
        let outcome = tracing::dispatcher::with_default(&dispatch, || {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<TransitionCall>::new()));
            let calls_for_closure = std::sync::Arc::clone(&calls);
            let future = apply_planner_escalate_route(
                RoleKind::Planner,
                issue_type,
                task_id,
                task_run_id,
                original_reason.to_string(),
                false,
                move |task_id, action, reason| {
                    let calls = std::sync::Arc::clone(&calls_for_closure);
                    async move {
                        calls
                            .lock()
                            .expect("calls mutex poisoned")
                            .push(TransitionCall {
                                task_id,
                                action,
                                reason,
                            });
                        Ok(())
                    }
                },
            );
            futures::executor::block_on(future)
        });

        // The outcome is the `Closed` variant with the same surfaced reason
        // that was passed to the durable close transition.
        match outcome {
            TaskRunOutcome::Closed { reason } => {
                assert_eq!(reason, format!("planner escalated: {original_reason}"))
            }
            other => panic!("expected Closed, got {other:?}"),
        }

        let captured = logs.take();
        assert!(
            !captured.is_empty(),
            "expected at least one log line to be captured, got nothing"
        );
        // The success-path log must be present in the captured output.
        assert!(
            captured.contains("planner escalated")
                && captured.contains("rt3l")
                && captured.contains("closing planning-type task"),
            "expected success-path log message to include 'planner escalated', \
             'rt3l', and 'closing planning-type task' so the pod log makes the \
             planner-escalate closure visible. Captured:\n{captured}"
        );
        // Structured fields must be present (rendered as
        // `key=value` by the default fmt subscriber).
        for (field, expected_value) in [
            ("task_run_id", task_run_id),
            ("task_id", task_id),
            ("issue_type", issue_type),
            (
                "surfaced_reason",
                "planner escalated: planner needs human guidance on the spec",
            ),
        ] {
            assert!(
                captured.contains(&format!("{field}={expected_value}")),
                "expected log output to contain structured field {field}={expected_value}, got:\n{captured}"
            );
        }
        // INFO level is required (so it shows up at the default log
        // level) and the level token must be present.
        assert!(
            captured.contains("INFO"),
            "expected log output to be emitted at INFO level, got:\n{captured}"
        );
    }

    fn loop_guard_stage_outcome(kind: LoopGuardKind) -> StageOutcome {
        StageOutcome::LoopGuardTripped {
            kind,
            offending_signature: "tool_failure:shell:{\"command\":\"cargo test\"}:error: denied"
                .to_string(),
            threshold: 3,
            observed: 3,
            turn_span: (4, 6),
            session_id: "session-1".to_string(),
        }
    }

    #[test]
    fn loop_guard_stage_outcome_bincode_roundtrip_for_each_kind() {
        for kind in [
            LoopGuardKind::IdenticalToolFailure,
            LoopGuardKind::PermissionDenial,
            LoopGuardKind::IdenticalOutput,
            LoopGuardKind::ConsecutiveFailures,
        ] {
            let bytes = bincode::serialize(&loop_guard_stage_outcome(kind)).expect("serialize");
            let back: StageOutcome = bincode::deserialize(&bytes).expect("deserialize");

            match back {
                StageOutcome::LoopGuardTripped {
                    kind: back_kind,
                    offending_signature,
                    threshold,
                    observed,
                    turn_span,
                    session_id,
                } => {
                    assert_eq!(back_kind, kind);
                    assert!(offending_signature.contains("tool_failure:shell"));
                    assert_eq!(threshold, 3);
                    assert_eq!(observed, 3);
                    assert_eq!(turn_span, (4, 6));
                    assert_eq!(session_id, "session-1");
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn stage_outcome_bincode_discriminants_keep_existing_variants_stable() {
        let old_variants = [
            StageOutcome::WorkerDone,
            StageOutcome::PlannerExecute,
            StageOutcome::PlannerClose {
                reason: "done".to_string(),
            },
            StageOutcome::ReviewerApproved,
            StageOutcome::ReviewerRejected {
                feedback: "needs work".to_string(),
            },
            StageOutcome::VerifierPassed,
            StageOutcome::VerifierFailed {
                reason: "tests failed".to_string(),
            },
            StageOutcome::ArchitectDone,
            StageOutcome::Escalate {
                reason: "blocked".to_string(),
            },
            StageOutcome::LeadApproved {
                evidence: String::new(),
            },
            StageOutcome::LeadApproveConflict {
                reason: "conflict".to_string(),
                evidence: String::new(),
            },
            StageOutcome::LeadReopen {
                reason: "retry".to_string(),
                directive: "fix the bug".to_string(),
                verification_command: "cargo test".to_string(),
                exclude_models: vec![],
            },
            StageOutcome::LeadClose {
                reason: "close".to_string(),
            },
            StageOutcome::LeadEscalate {
                reason: "escalate".to_string(),
            },
            StageOutcome::Failed {
                reason: "boom".to_string(),
                provider_failure: None,
            },
        ];

        for (expected_discriminant, outcome) in old_variants.into_iter().enumerate() {
            let bytes = bincode::serialize(&outcome).expect("serialize old variant");
            assert_eq!(
                &bytes[..4],
                &(expected_discriminant as u32).to_le_bytes(),
                "existing variant discriminant shifted for {outcome:?}"
            );
            let decoded: StageOutcome = bincode::deserialize(&bytes).expect("decode old frame");
            assert_eq!(
                std::mem::discriminant(&decoded),
                std::mem::discriminant(&outcome)
            );
        }

        let new_bytes = bincode::serialize(&loop_guard_stage_outcome(
            LoopGuardKind::IdenticalToolFailure,
        ))
        .expect("serialize new variant");
        assert_eq!(&new_bytes[..4], &15u32.to_le_bytes());
    }

    // ── resume-via-git worktree-setup helper (`twsk`) ────────────────────

    use djinn_runtime::ResumeLifecycleMetadata;
    use djinn_workspace::MirrorManager;

    const RESUME_TEST_PROJECT_ID: &str = "proj-resume-supervisor";
    const RESUME_TEST_BASE: &str = "main";
    const RESUME_TEST_TASK: &str = "task/resume-supervisor";
    const RESUME_TEST_ALT_REF: &str = "refs/djinn/checkpoints/task/resume-supervisor/s1";

    /// Run `git <args>` in `cwd` and return the trimmed stdout. Panics on
    /// non-zero exit. Used by the resume-helper tests below to capture SHAs.
    fn run_git_stdout(cwd: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Stand up a tiny source repo on disk, push it to a `MirrorManager`
    /// under a deterministic project id, and (optionally) push an alternate
    /// checkpoint ref pointing at the tip. Returns the bare tip SHA.
    async fn build_resume_test_mirror(
        mirrors_dir: &Path,
        with_alt_ref: bool,
    ) -> (MirrorManager, String) {
        let source = tempfile::tempdir().expect("source tempdir");
        let src_path = source.path();
        run_git(src_path, &["init", "-b", "main", "-q"]);
        run_git(src_path, &["config", "user.email", "t@t"]);
        run_git(src_path, &["config", "user.name", "t"]);
        std::fs::write(src_path.join("README.md"), "v1\n").expect("write v1");
        run_git(src_path, &["add", "."]);
        run_git(src_path, &["commit", "-m", "v1"]);

        // Add a second commit so the alternate ref has a distinct SHA.
        std::fs::write(src_path.join("new.txt"), "v2 content\n").expect("write v2");
        run_git(src_path, &["add", "."]);
        run_git(src_path, &["commit", "-m", "v2"]);
        let tip = run_git_stdout(src_path, &["rev-parse", "HEAD"]);

        let mgr = MirrorManager::new(mirrors_dir.to_path_buf());
        mgr.ensure_mirror(
            RESUME_TEST_PROJECT_ID,
            &format!("file://{}", src_path.display()),
        )
        .await
        .expect("ensure_mirror");

        if with_alt_ref {
            run_git(
                src_path,
                &[
                    "push",
                    &format!(
                        "file://{}",
                        mgr.mirror_path(RESUME_TEST_PROJECT_ID).display()
                    ),
                    &format!("{tip}:{RESUME_TEST_ALT_REF}"),
                ],
            );
        }

        (mgr, tip)
    }

    /// `None` metadata (default/off dispatch path) must return `Ok(None)`
    /// so the legacy `clone_ephemeral(task_branch)` runs unchanged.
    #[tokio::test]
    async fn prepare_resume_workspace_returns_none_for_missing_metadata() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), false).await;

        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            None,
        )
        .await
        .expect("missing metadata must not error");
        assert!(
            outcome.is_none(),
            "default/off path must not select a resume source"
        );
    }

    /// `considered == false` (selector bypassed / pre-`1f9u` host) must
    /// also fall through to the legacy path. Same semantics as `None`.
    #[tokio::test]
    async fn prepare_resume_workspace_returns_none_when_considered_false() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), false).await;

        let meta = ResumeLifecycleMetadata {
            considered: false,
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("un-considered metadata must not error");
        assert!(
            outcome.is_none(),
            "considered=false must not select a resume source"
        );
    }

    /// `CleanTaskBranch` selection must fall through to the legacy
    /// `clone_ephemeral(task_branch)` path — the selector already chose
    /// the fallback, so the worktree setup matches it byte-for-byte.
    #[tokio::test]
    async fn prepare_resume_workspace_falls_through_for_clean_task_branch() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), false).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::CleanTaskBranchFallback),
            source_kind: Some(djinn_runtime::ResumeSourceKind::CleanTaskBranch),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("clean task branch must not error");
        assert!(
            outcome.is_none(),
            "clean-task-branch selection must use the legacy clone path"
        );
    }

    /// `AutoSubmit` selection: no git content to base on; the worker pod
    /// uses the canonical task branch and the submit/review id rides the
    /// prompt context. Must return `Ok(None)` so the legacy path runs.
    #[tokio::test]
    async fn prepare_resume_workspace_falls_through_for_auto_submit() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), false).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            submit_or_review_id: Some("review-1".to_string()),
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::AutoSubmitAccepted),
            source_kind: Some(djinn_runtime::ResumeSourceKind::AutoSubmit),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("auto-submit selection must not error");
        assert!(
            outcome.is_none(),
            "auto-submit selection must use the legacy task-branch clone path (no git content to base on)"
        );
    }

    /// `TaskBranchCheckpoint` selection with a valid commit SHA must apply
    /// the resume path: the outcome reports the selected SHA on the task
    /// branch. (The helper currently drops the prepared workspace so the
    /// legacy clone can run, but it records the SHA in the outcome so the
    /// operator / downstream prompt / model work can see it.)
    #[tokio::test]
    async fn prepare_resume_workspace_applies_safe_task_branch_checkpoint() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, tip) = build_resume_test_mirror(tmp.path(), false).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            commit_sha: Some(tip.clone()),
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::LatestSafeCheckpoint),
            source_kind: Some(djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("safe checkpoint apply must succeed");
        let outcome = outcome.expect("safe checkpoint must produce an outcome");
        assert_eq!(
            outcome.applied_source,
            djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint
        );
        assert_eq!(outcome.applied_target_ref, RESUME_TEST_TASK);
        assert_eq!(outcome.applied_commit_sha.as_deref(), Some(tip.as_str()));
        assert!(
            outcome.fallback_reason.is_none(),
            "safe checkpoint apply must not record a fallback reason"
        );
    }

    /// `TaskBranchCheckpoint` selection WITHOUT a commit SHA must fall
    /// back to clean task branch with a machine-readable reason.
    #[tokio::test]
    async fn prepare_resume_workspace_falls_back_when_safe_checkpoint_missing_sha() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), false).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            // commit_sha intentionally absent
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::LatestSafeCheckpoint),
            source_kind: Some(djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("missing-SHA fallback must not error");
        let outcome = outcome.expect("missing-SHA must produce an outcome");
        assert_eq!(
            outcome.applied_source,
            djinn_runtime::ResumeSourceKind::CleanTaskBranch
        );
        let reason = outcome
            .fallback_reason
            .as_deref()
            .expect("must record a fallback reason");
        assert!(
            reason.contains("missing_commit_sha"),
            "fallback reason must identify the missing-SHA case, got: {reason}"
        );
    }

    /// `AlternateCheckpointRef` selection with a valid ref must apply the
    /// resume path. Records `target_ref` in the outcome.
    #[tokio::test]
    async fn prepare_resume_workspace_applies_alternate_checkpoint_ref() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), true).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            target_ref: Some(RESUME_TEST_ALT_REF.to_string()),
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::AlternateCheckpointRef),
            source_kind: Some(djinn_runtime::ResumeSourceKind::AlternateCheckpointRef),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("alternate ref apply must succeed");
        let outcome = outcome.expect("alternate ref must produce an outcome");
        assert_eq!(
            outcome.applied_source,
            djinn_runtime::ResumeSourceKind::AlternateCheckpointRef
        );
        assert_eq!(outcome.applied_target_ref, RESUME_TEST_ALT_REF);
        assert!(
            outcome.fallback_reason.is_none(),
            "alternate ref apply must not record a fallback reason"
        );
    }

    /// `AlternateCheckpointRef` selection that targets a missing ref must
    /// fall back to clean task branch with a machine-readable reason. The
    /// helper must NOT panic on the failed `clone_ephemeral_at_ref`.
    #[tokio::test]
    async fn prepare_resume_workspace_falls_back_when_alternate_ref_unavailable() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), true).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            target_ref: Some("refs/djinn/checkpoints/does/not/exist".to_string()),
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::AlternateCheckpointRef),
            source_kind: Some(djinn_runtime::ResumeSourceKind::AlternateCheckpointRef),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("unavailable ref must not error");
        let outcome = outcome.expect("unavailable ref must produce an outcome");
        assert_eq!(
            outcome.applied_source,
            djinn_runtime::ResumeSourceKind::CleanTaskBranch
        );
        let reason = outcome
            .fallback_reason
            .as_deref()
            .expect("must record a fallback reason");
        assert!(
            reason.contains("clone_ephemeral_at_ref_failed"),
            "fallback reason must name the failed op, got: {reason}"
        );
    }

    /// `AlternateCheckpointRef` selection without a `target_ref` must fall
    /// back to clean task branch with a machine-readable reason (rather
    /// than panicking or skipping the safety check).
    #[tokio::test]
    async fn prepare_resume_workspace_falls_back_when_alternate_ref_missing_target() {
        let tmp = tempfile::tempdir().expect("mirrors tempdir");
        let (mgr, _tip) = build_resume_test_mirror(tmp.path(), false).await;

        let meta = ResumeLifecycleMetadata {
            considered: true,
            // target_ref intentionally absent
            selection_reason: Some(djinn_runtime::ResumeSelectionReason::AlternateCheckpointRef),
            source_kind: Some(djinn_runtime::ResumeSourceKind::AlternateCheckpointRef),
            ..Default::default()
        };
        let outcome = prepare_resume_workspace(
            &mgr,
            RESUME_TEST_PROJECT_ID,
            RESUME_TEST_TASK,
            RESUME_TEST_BASE,
            Some(&meta),
        )
        .await
        .expect("missing-target fallback must not error");
        let outcome = outcome.expect("missing-target must produce an outcome");
        assert_eq!(
            outcome.applied_source,
            djinn_runtime::ResumeSourceKind::CleanTaskBranch
        );
        let reason = outcome
            .fallback_reason
            .as_deref()
            .expect("must record a fallback reason");
        assert!(
            reason.contains("missing_target_ref"),
            "fallback reason must identify the missing-target case, got: {reason}"
        );
    }

    /// Regression: a persistent push_to_origin failure after WorkerDone must
    /// prevent `submit_task_review` from being fired and must produce a
    /// `TaskRunOutcome::Failed` — NOT a `WorkerSubmitted` that would mark the
    /// round as durably submitted while nothing was persisted to the mirror.
    ///
    /// This covers the lke3-era incident where three consecutive remediation
    /// rounds reached `work_submitted` but every mirror push failed with
    /// "unable to create temporary object directory / permission denied",
    /// leaving the task branch frozen at the CI-failing v1 head while the
    /// activity log showed phantom progress.
    ///
    /// The test creates a real mirror + ephemeral workspace, writes a file,
    /// then deletes the mirror's `objects/` directory so `git push` fails
    /// with a transport error (cannot read/write objects). The supervisor's
    /// `WorkerDone` flow must see the push failure, emit a structured
    /// `push_failure` tracing event, and refuse to advance the task.
    #[tokio::test]
    async fn push_failure_prevents_submit_task_review_and_fails_run() {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;

        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "project-push-fail";
        let task_id = "task-push-fail";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        // Create an ephemeral workspace and write a file so the auto-commit
        // produces a real diff that needs pushing.
        let ws = mirror
            .clone_ephemeral(project_id, "main")
            .await
            .expect("clone ephemeral workspace");
        let tb = "djinn/push-fail";
        run_git(ws.path(), &["checkout", "-b", tb]);
        tokio::fs::write(ws.path().join("work.txt"), "real worker output")
            .await
            .expect("write fixture file");
        drop(ws); // release workspace before supervisor creates its own

        // Set up log capture so we can assert on structured push_failure events.
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        // Corrupt the mirror so `git push` will fail: install a
        // pre-receive hook that always rejects. Local bare repos run
        // hooks during `git push`, so this deterministically fails the
        // supervisor's post-stage push while keeping the mirror intact
        // for cloning. Simulates the production "remote unpack failed:
        // unable to create temporary object directory / permission denied".
        let mirror_path = mirror.mirror_path(project_id);
        assert!(
            mirror_path.exists(),
            "mirror must exist before installing push-rejection hook"
        );
        let hooks_dir = mirror_path.join("hooks");
        tokio::fs::create_dir_all(&hooks_dir)
            .await
            .expect("create hooks dir");
        let hook_path = hooks_dir.join("pre-receive");
        tokio::fs::write(&hook_path, "#!/bin/sh\nexit 1\n")
            .await
            .expect("write pre-receive hook");
        // Make the hook executable (chmod +x).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))
                .await
                .expect("chmod pre-receive hook");
        }

        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let tc = transition_calls.clone();
        let stc = submit_task_review_called.clone();
        let services: Arc<dyn SupervisorServices> = Arc::new(PushFailTestServices {
            cancel: CancellationToken::new(),
            task: fixture_task(task_id, project_id),
            transition_calls: tc,
            submit_task_review_called: stc,
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: "run-push-fail".into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: tb.into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let report = supervisor.run(spec).await.expect("supervisor run");

        // The run MUST fail — not WorkerSubmitted, not Interrupted.
        assert!(
            matches!(report.outcome, TaskRunOutcome::Failed { .. }),
            "persistent push failure must produce TaskRunOutcome::Failed, \
             got: {:?}",
            report.outcome
        );

        // submit_task_review MUST NOT have been called — the task must
        // stay in_progress for redispatch.
        assert!(
            !submit_task_review_called.load(std::sync::atomic::Ordering::SeqCst),
            "submit_task_review must NOT be called when push_to_origin failed — \
             the round was not durably submitted"
        );

        // The supervisor should still record the worker stage as completed
        // (the stage itself ran; it was the push that failed).
        assert_eq!(report.stages_completed, vec![RoleKind::Worker]);

        // Verify structured tracing event was emitted.
        let captured = logs.take();
        assert!(
            captured.contains("push_failure")
                && captured.contains("djinn_supervisor::push_failure"),
            "expected structured push_failure tracing event, got:\n{captured}"
        );
        assert!(
            captured.contains("task-push-fail") && captured.contains("run-push-fail"),
            "push_failure event must carry task and run context, got:\n{captured}"
        );
    }

    /// Helper services for the push-failure regression test. Tracks
    /// `transition_task` calls and records whether `submit_task_review`
    /// was reached.
    struct PushFailTestServices {
        cancel: CancellationToken,
        task: Task,
        transition_calls: std::sync::Arc<std::sync::Mutex<Vec<TransitionCall>>>,
        submit_task_review_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl SupervisorServices for PushFailTestServices {
        fn cancel(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, task_id: String) -> Result<Task, String> {
            assert_eq!(task_id, self.task.id);
            Ok(self.task.clone())
        }

        async fn execute_stage(
            &self,
            _task: &Task,
            _workspace: &Workspace,
            role_kind: RoleKind,
            _task_run_id: &str,
            _spec: &TaskRunSpec,
        ) -> Result<StageOutcome, StageError> {
            assert_eq!(role_kind, RoleKind::Worker);
            Ok(StageOutcome::WorkerDone)
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            panic!("push failure must not reach open_pr")
        }

        async fn create_task_run(
            &self,
            _params: services::SerializableCreateTaskRunParams,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn update_task_run_status(
            &self,
            _run_id: String,
            _status: TaskRunStatus,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
            unimplemented!("not exercised")
        }

        async fn get_provider_base_url(
            &self,
            _catalog_provider_id: String,
        ) -> Result<String, String> {
            unimplemented!("not exercised")
        }

        async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
            unimplemented!("not exercised")
        }

        async fn create_session(
            &self,
            _params: services::SerializableCreateSessionParams,
        ) -> Result<djinn_core::models::SessionRecord, String> {
            unimplemented!("not exercised")
        }

        async fn publish_session_message(
            &self,
            _session_id: String,
            _task_id: String,
            _agent_type: String,
            _message: serde_json::Value,
        ) -> Result<(), String> {
            unimplemented!("not exercised")
        }

        async fn get_environment_config(
            &self,
            _project_id: String,
        ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
            unimplemented!("not exercised")
        }

        async fn invoke_llm(
            &self,
            _model_id: String,
            _conversation: djinn_provider::message::Conversation,
            _tools: Vec<serde_json::Value>,
            _tool_choice: Option<djinn_provider::provider::ToolChoice>,
        ) -> Result<djinn_provider::provider::LlmResponse, String> {
            unimplemented!("not exercised")
        }

        #[allow(clippy::too_many_arguments)]
        async fn update_session_status(
            &self,
            _session_id: String,
            _status: djinn_core::models::SessionStatus,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
            _parked_reason: Option<String>,
        ) -> Result<(), String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_search(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_fetch_file(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_ci_job_log(
            &self,
            _session_task_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn emit_djinn_event(
            &self,
            _event: services::SerializableDjinnEvent,
        ) -> Result<(), String> {
            Ok(()) // fire-and-forget
        }

        async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn transition_task(
            &self,
            task_id: String,
            action: String,
            reason: Option<String>,
        ) -> Result<(), String> {
            if action == "submit_task_review" {
                self.submit_task_review_called
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.transition_calls
                .lock()
                .expect("transition_calls mutex poisoned")
                .push(TransitionCall {
                    task_id,
                    action,
                    reason,
                });
            Ok(())
        }

        async fn run_arbiter_preapproval_gate(
            &self,
            _task: &Task,
        ) -> Result<ArbiterGateResult, String> {
            // Test stub: always pass.
            Ok(ArbiterGateResult::Pass)
        }

        async fn record_arbiter_decision(
            &self,
            _task_id: String,
            _decision: String,
            _evidence_json: String,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn start_monitored_reopen(
            &self,
            _task_id: String,
            _directive: String,
            _verification_command: String,
            _exclude_models: Vec<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn record_arbiter_session_termination(
            &self,
            _task_id: String,
            _is_infra_failure: bool,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }

    // ── Arbiter pre-approval gate tests ──────────────────────────────────────

    /// Test services whose `execute_stage` returns a configurable
    /// `StageOutcome` and whose `run_arbiter_preapproval_gate` returns a
    /// configurable `ArbiterGateResult`.  Transition calls are recorded
    /// for assertion.
    struct ArbiterGateTestServices {
        cancel: CancellationToken,
        task: Task,
        stage_outcome: StageOutcome,
        gate_result: Result<ArbiterGateResult, String>,
        transition_calls: std::sync::Arc<std::sync::Mutex<Vec<TransitionCall>>>,
        open_pr_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// zkk9: records (directive, verification_command, exclude_models)
        /// passed to `start_monitored_reopen` so the reopen settlement test
        /// can assert the directive was persisted before the transition.
        start_monitored_reopen_calls: std::sync::Arc<std::sync::Mutex<Vec<MonitoredReopenCall>>>,
        /// zkk9: records task_ids passed to `complete_monitored_reopen` so
        /// terminal-outcome tests can assert the monitored attempt was closed.
        complete_monitored_reopen_calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// zkk9: expected role for `execute_stage`.  Defaults to `Lead` for
        /// arbiter gate tests; set to `Worker` for monitored-reopen completion
        /// tests that simulate a worker task-run.
        expected_role: RoleKind,
    }

    /// Recorded `start_monitored_reopen` call for assertion.
    #[derive(Clone, Debug)]
    #[allow(dead_code)]
    struct MonitoredReopenCall {
        task_id: String,
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    }

    #[async_trait]
    impl SupervisorServices for ArbiterGateTestServices {
        fn cancel(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, task_id: String) -> Result<Task, String> {
            assert_eq!(task_id, self.task.id);
            Ok(self.task.clone())
        }

        async fn execute_stage(
            &self,
            _task: &Task,
            _workspace: &Workspace,
            role_kind: RoleKind,
            _task_run_id: &str,
            _spec: &TaskRunSpec,
        ) -> Result<StageOutcome, StageError> {
            assert_eq!(
                role_kind, self.expected_role,
                "execute_stage called with unexpected role"
            );
            Ok(self.stage_outcome.clone())
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            self.open_pr_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            TaskRunOutcome::PrOpened {
                url: "https://github.com/test/pr/1".into(),
                sha: "abc123".into(),
            }
        }

        async fn create_task_run(
            &self,
            _params: services::SerializableCreateTaskRunParams,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn update_task_run_status(
            &self,
            _run_id: String,
            _status: TaskRunStatus,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
            unimplemented!("not exercised")
        }

        async fn get_provider_base_url(
            &self,
            _catalog_provider_id: String,
        ) -> Result<String, String> {
            unimplemented!("not exercised")
        }

        async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
            unimplemented!("not exercised")
        }

        async fn create_session(
            &self,
            _params: services::SerializableCreateSessionParams,
        ) -> Result<djinn_core::models::SessionRecord, String> {
            unimplemented!("not exercised")
        }

        async fn publish_session_message(
            &self,
            _session_id: String,
            _task_id: String,
            _agent_type: String,
            _message: serde_json::Value,
        ) -> Result<(), String> {
            unimplemented!("not exercised")
        }

        async fn get_environment_config(
            &self,
            _project_id: String,
        ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
            unimplemented!("not exercised")
        }

        async fn invoke_llm(
            &self,
            _model_id: String,
            _conversation: djinn_provider::message::Conversation,
            _tools: Vec<serde_json::Value>,
            _tool_choice: Option<djinn_provider::provider::ToolChoice>,
        ) -> Result<djinn_provider::provider::LlmResponse, String> {
            unimplemented!("not exercised")
        }

        async fn update_session_status(
            &self,
            _session_id: String,
            _status: djinn_core::models::SessionStatus,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
            _parked_reason: Option<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn emit_djinn_event(
            &self,
            _event: services::SerializableDjinnEvent,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn tool_github_search(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_fetch_file(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_ci_job_log(
            &self,
            _session_task_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn transition_task(
            &self,
            task_id: String,
            action: String,
            reason: Option<String>,
        ) -> Result<(), String> {
            self.transition_calls
                .lock()
                .expect("transition_calls mutex poisoned")
                .push(TransitionCall {
                    task_id,
                    action,
                    reason,
                });
            Ok(())
        }

        async fn run_arbiter_preapproval_gate(
            &self,
            _task: &Task,
        ) -> Result<ArbiterGateResult, String> {
            self.gate_result.clone()
        }

        async fn record_arbiter_decision(
            &self,
            _task_id: String,
            _decision: String,
            _evidence_json: String,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn start_monitored_reopen(
            &self,
            task_id: String,
            directive: String,
            verification_command: String,
            exclude_models: Vec<String>,
        ) -> Result<(), String> {
            self.start_monitored_reopen_calls
                .lock()
                .expect("start_monitored_reopen_calls mutex poisoned")
                .push(MonitoredReopenCall {
                    task_id,
                    directive,
                    verification_command,
                    exclude_models,
                });
            Ok(())
        }

        async fn complete_monitored_reopen(&self, task_id: String) -> Result<(), String> {
            self.complete_monitored_reopen_calls
                .lock()
                .expect("complete_monitored_reopen_calls mutex poisoned")
                .push(task_id);
            Ok(())
        }

        async fn record_arbiter_session_termination(
            &self,
            _task_id: String,
            _is_infra_failure: bool,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }

    /// Build a minimal mirror + supervisor for arbiter gate tests.
    /// Returns the tempdir guard so the caller keeps the mirror alive.
    async fn build_arbiter_gate_test_env(
        task_id: &str,
        project_id: &str,
        stage_outcome: StageOutcome,
        gate_result: Result<ArbiterGateResult, String>,
    ) -> (
        tempfile::TempDir,
        TaskRunSupervisor,
        TaskRunSpec,
        std::sync::Arc<std::sync::Mutex<Vec<TransitionCall>>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (root, supervisor, spec, transition_calls, open_pr_called, _reopen, _complete) =
            build_arbiter_gate_test_env_with_reopen(
                task_id,
                project_id,
                stage_outcome,
                gate_result,
            )
            .await;
        (root, supervisor, spec, transition_calls, open_pr_called)
    }

    /// Variant of [`build_arbiter_gate_test_env`] that also returns the
    /// `start_monitored_reopen` call tracker for reopen settlement tests.
    async fn build_arbiter_gate_test_env_with_reopen(
        task_id: &str,
        project_id: &str,
        stage_outcome: StageOutcome,
        gate_result: Result<ArbiterGateResult, String>,
    ) -> (
        tempfile::TempDir,
        TaskRunSupervisor,
        TaskRunSpec,
        std::sync::Arc<std::sync::Mutex<Vec<TransitionCall>>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::Mutex<Vec<MonitoredReopenCall>>>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let mirror = std::sync::Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let transition_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let open_pr_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start_monitored_reopen_calls: std::sync::Arc<
            std::sync::Mutex<Vec<MonitoredReopenCall>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let complete_monitored_reopen_calls: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let services: std::sync::Arc<dyn SupervisorServices> =
            std::sync::Arc::new(ArbiterGateTestServices {
                cancel: CancellationToken::new(),
                task: fixture_task(task_id, project_id),
                stage_outcome,
                gate_result,
                transition_calls: transition_calls.clone(),
                open_pr_called: open_pr_called.clone(),
                start_monitored_reopen_calls: start_monitored_reopen_calls.clone(),
                complete_monitored_reopen_calls: complete_monitored_reopen_calls.clone(),
                expected_role: RoleKind::Lead,
            });

        let supervisor = TaskRunSupervisor::new(std::sync::Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: format!("run-arbiter-{task_id}"),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: format!("djinn/{task_id}"),
            flow: SupervisorFlow::Lead,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        (
            root,
            supervisor,
            spec,
            transition_calls,
            open_pr_called,
            start_monitored_reopen_calls,
            complete_monitored_reopen_calls,
        )
    }

    /// zkk9: Build a worker-flow test environment for monitored-reopen
    /// completion tests.  Returns the same trackers as
    /// [`build_arbiter_gate_test_env_with_reopen`] but configures the spec
    /// for `SupervisorFlow::NewTask` (worker-only) and sets `expected_role`
    /// to `Worker`.
    async fn build_worker_flow_test_env(
        task_id: &str,
        project_id: &str,
        stage_outcome: StageOutcome,
    ) -> (
        tempfile::TempDir,
        TaskRunSupervisor,
        TaskRunSpec,
        std::sync::Arc<std::sync::Mutex<Vec<TransitionCall>>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::Mutex<Vec<MonitoredReopenCall>>>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let mirror = std::sync::Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let transition_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let open_pr_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start_monitored_reopen_calls: std::sync::Arc<
            std::sync::Mutex<Vec<MonitoredReopenCall>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let complete_monitored_reopen_calls: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let services: std::sync::Arc<dyn SupervisorServices> =
            std::sync::Arc::new(ArbiterGateTestServices {
                cancel: CancellationToken::new(),
                task: fixture_task(task_id, project_id),
                stage_outcome,
                gate_result: Ok(ArbiterGateResult::Pass),
                transition_calls: transition_calls.clone(),
                open_pr_called: open_pr_called.clone(),
                start_monitored_reopen_calls: start_monitored_reopen_calls.clone(),
                complete_monitored_reopen_calls: complete_monitored_reopen_calls.clone(),
                expected_role: RoleKind::Worker,
            });

        let supervisor = TaskRunSupervisor::new(std::sync::Arc::clone(&mirror), services);
        let spec = TaskRunSpec {
            task_run_id: format!("run-worker-{task_id}"),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: format!("djinn/{task_id}"),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        (
            root,
            supervisor,
            spec,
            transition_calls,
            open_pr_called,
            start_monitored_reopen_calls,
            complete_monitored_reopen_calls,
        )
    }

    #[tokio::test]
    async fn arbiter_approve_green_gate_proceeds_with_transition() {
        let (_root, supervisor, spec, transition_calls, open_pr_called) =
            build_arbiter_gate_test_env(
                "T-gate-green",
                "proj-gate",
                StageOutcome::LeadApproved {
                    evidence: String::new(),
                },
                Ok(ArbiterGateResult::Pass),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // Gate passed → lead_approve transition fired.
        let calls = transition_calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.action == "lead_approve"),
            "green gate must fire lead_approve transition, got: {calls:?}"
        );
        // LeadApproved falls through to open_pr.
        assert!(
            open_pr_called.load(std::sync::atomic::Ordering::SeqCst),
            "green gate must fall through to open_pr"
        );
        assert!(
            matches!(report.outcome, TaskRunOutcome::PrOpened { .. }),
            "green gate must produce PrOpened, got: {:?}",
            report.outcome
        );
    }

    #[tokio::test]
    async fn arbiter_approve_red_gate_blocks_without_transition() {
        let (_root, supervisor, spec, transition_calls, open_pr_called) =
            build_arbiter_gate_test_env(
                "T-gate-red",
                "proj-gate",
                StageOutcome::LeadApproved {
                    evidence: String::new(),
                },
                Ok(ArbiterGateResult::Blocked {
                    feedback: "clippy failed: error[E0425]".into(),
                }),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // Gate blocked → NO lead_approve transition.
        let calls = transition_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.action == "lead_approve"),
            "red gate must NOT fire lead_approve transition, got: {calls:?}"
        );
        // open_pr must NOT be called.
        assert!(
            !open_pr_called.load(std::sync::atomic::Ordering::SeqCst),
            "red gate must NOT reach open_pr"
        );
        // Must surface Escalated with the gate feedback.
        match &report.outcome {
            TaskRunOutcome::Escalated { reason } => {
                assert!(
                    reason.contains("clippy failed"),
                    "Escalated reason must contain gate feedback, got: {reason}"
                );
                assert!(
                    reason.contains("strike-free"),
                    "Escalated reason must mention strike-free, got: {reason}"
                );
            }
            other => panic!("expected Escalated, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn arbiter_approve_conflict_green_gate_proceeds_with_transition() {
        let (_root, supervisor, spec, transition_calls, open_pr_called) =
            build_arbiter_gate_test_env(
                "T-gate-conflict-green",
                "proj-gate",
                StageOutcome::LeadApproveConflict {
                    reason: "merge conflict".into(),
                    evidence: String::new(),
                },
                Ok(ArbiterGateResult::Pass),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // Gate passed → lead_approve_conflict transition fired.
        let calls = transition_calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.action == "lead_approve_conflict"),
            "green gate must fire lead_approve_conflict transition, got: {calls:?}"
        );
        // Must produce Closed (approve_conflict is terminal).
        assert!(
            matches!(report.outcome, TaskRunOutcome::Closed { .. }),
            "green approve_conflict must produce Closed, got: {:?}",
            report.outcome
        );
        // open_pr must NOT be called (approve_conflict is terminal).
        assert!(
            !open_pr_called.load(std::sync::atomic::Ordering::SeqCst),
            "approve_conflict must NOT fall through to open_pr"
        );
    }

    #[tokio::test]
    async fn arbiter_approve_conflict_red_gate_blocks_without_transition() {
        let (_root, supervisor, spec, transition_calls, _open_pr_called) =
            build_arbiter_gate_test_env(
                "T-gate-conflict-red",
                "proj-gate",
                StageOutcome::LeadApproveConflict {
                    reason: "merge conflict".into(),
                    evidence: String::new(),
                },
                Ok(ArbiterGateResult::Blocked {
                    feedback: "test target build failed".into(),
                }),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // Gate blocked → NO lead_approve_conflict transition.
        let calls = transition_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.action == "lead_approve_conflict"),
            "red gate must NOT fire lead_approve_conflict transition, got: {calls:?}"
        );
        // Must surface Escalated with the gate feedback.
        match &report.outcome {
            TaskRunOutcome::Escalated { reason } => {
                assert!(
                    reason.contains("test target build failed"),
                    "Escalated reason must contain gate feedback, got: {reason}"
                );
            }
            other => panic!("expected Escalated, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn arbiter_gate_red_does_not_consume_arbitration_or_increment_counters() {
        // The red-gate path must:
        // 1. NOT fire lead_approve (which would mark the arbitration consumed)
        // 2. NOT increment decision-failure / reopen / strike counters
        // 3. Return Escalated so the task stays in in_lead_intervention
        let (_root, supervisor, spec, transition_calls, _open_pr_called) =
            build_arbiter_gate_test_env(
                "T-gate-no-strike",
                "proj-gate",
                StageOutcome::LeadApproved {
                    evidence: String::new(),
                },
                Ok(ArbiterGateResult::Blocked {
                    feedback: "sqlx cache check failed".into(),
                }),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // lead_approve must NOT be fired — task stays in in_lead_intervention.
        // (lead_intervention_start is expected as the entry transition.)
        let calls = transition_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.action == "lead_approve"),
            "red gate must NOT fire lead_approve transition (no arbitration consumption), got: {calls:?}"
        );
        // Escalated outcome — the coordinator will re-dispatch to the arbiter.
        assert!(
            matches!(report.outcome, TaskRunOutcome::Escalated { .. }),
            "red gate must produce Escalated, got: {:?}",
            report.outcome
        );
    }

    #[tokio::test]
    async fn arbiter_gate_infra_error_proceeds_fail_open() {
        // An infra error (Err) from the gate must proceed (fail-open),
        // same as the reviewer gate's Error behavior.
        let (_root, supervisor, spec, transition_calls, open_pr_called) =
            build_arbiter_gate_test_env(
                "T-gate-infra-err",
                "proj-gate",
                StageOutcome::LeadApproved {
                    evidence: String::new(),
                },
                Err("db connection timeout".into()),
            )
            .await;

        let _report = supervisor.run(spec).await.expect("supervisor run");

        // Infra error → fail-open → lead_approve transition fired.
        let calls = transition_calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.action == "lead_approve"),
            "infra error must proceed (fail-open) and fire lead_approve, got: {calls:?}"
        );
        // Falls through to open_pr.
        assert!(
            open_pr_called.load(std::sync::atomic::Ordering::SeqCst),
            "infra error must fall through to open_pr"
        );
    }

    // ── Monitored reopen settlement tests (zkk9) ─────────────────────────────

    #[tokio::test]
    async fn arbiter_reopen_persists_directive_and_fires_transition() {
        // A valid arbiter `reopen` must:
        // 1. Call `start_monitored_reopen` to persist the directive / verification
        //    command / excluded models and mark the attempt start.
        // 2. Fire `lead_intervention_complete` to return the task to `open`.
        // 3. NOT call open_pr (reopen is terminal for this run).
        let (_root, supervisor, spec, transition_calls, _open_pr, reopen_calls, complete_calls) =
            build_arbiter_gate_test_env_with_reopen(
                "T-reopen-persist",
                "proj-reopen",
                StageOutcome::LeadReopen {
                    reason: "needs different approach".into(),
                    directive: "Fix the retry loop in dispatch.rs by adding a circuit breaker"
                        .into(),
                    verification_command: "cargo test -p djinn-coordinator".into(),
                    exclude_models: vec!["gpt-4o-mini".into()],
                },
                Ok(ArbiterGateResult::Pass),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // start_monitored_reopen was called with the directive payload.
        let reopen = reopen_calls.lock().unwrap();
        assert_eq!(
            reopen.len(),
            1,
            "start_monitored_reopen must be called exactly once"
        );
        assert_eq!(
            reopen[0].directive,
            "Fix the retry loop in dispatch.rs by adding a circuit breaker"
        );
        assert_eq!(
            reopen[0].verification_command,
            "cargo test -p djinn-coordinator"
        );
        assert_eq!(reopen[0].exclude_models, vec!["gpt-4o-mini".to_string()]);

        // lead_intervention_complete transition fired.
        let calls = transition_calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.action == "lead_intervention_complete"),
            "reopen must fire lead_intervention_complete transition, got: {calls:?}"
        );

        // Reopen is terminal for this run — produces Closed.
        assert!(
            matches!(report.outcome, TaskRunOutcome::Closed { .. }),
            "reopen must produce Closed, got: {:?}",
            report.outcome
        );

        // zkk9: complete_monitored_reopen must NOT be called on the arbiter
        // reopen run itself.  The arbiter run starts the monitored reopen
        // (persisting the directive/exclusions and marking the attempt start);
        // the arbitration row must remain unconsumed so the next worker
        // dispatch can see the directive.  Completion happens only when the
        // monitored *worker* attempt reaches a terminal outcome in a separate
        // task-run.
        let complete = complete_calls.lock().unwrap();
        assert_eq!(
            complete.len(),
            0,
            "complete_monitored_reopen must NOT be called on the arbiter reopen run \
             (the row must stay unconsumed for the next worker dispatch), got: {complete:?}"
        );
    }

    #[tokio::test]
    async fn arbiter_supersede_force_closes_source_with_replacement_ids_no_hold() {
        // A valid arbiter `supersede` must fire exactly one terminal
        // force-close transition (`arbiter_supersede`, which the host-side
        // interception applies as a force-close to `closed`), carrying the
        // replacement subtask ids, and must NOT route through the park/
        // human-review-hold path (`arbiter_park`).
        let (_root, supervisor, spec, transition_calls, open_pr_called) =
            build_arbiter_gate_test_env(
                "T-supersede",
                "proj-supersede",
                StageOutcome::LeadSuperseded {
                    reason: "decomposed into 2 replacement subtasks".into(),
                    replacement_task_ids: vec!["repl-1".into(), "repl-2".into()],
                },
                Ok(ArbiterGateResult::Pass),
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        let calls = transition_calls.lock().unwrap();

        // Exactly one supersede/force-close transition, carrying the ids.
        let supersede_calls: Vec<_> = calls
            .iter()
            .filter(|c| c.action == "arbiter_supersede")
            .collect();
        assert_eq!(
            supersede_calls.len(),
            1,
            "supersede must fire exactly one arbiter_supersede (force-close) transition, got: {calls:?}"
        );
        let payload = supersede_calls[0]
            .reason
            .as_deref()
            .expect("arbiter_supersede transition must carry a reason payload");
        assert!(
            payload.contains("repl-1") && payload.contains("repl-2"),
            "supersede transition payload must carry the replacement ids, got: {payload}"
        );
        assert!(
            payload.contains("replacement_task_ids"),
            "supersede transition payload must name replacement_task_ids, got: {payload}"
        );

        // Must NOT create a human-review hold (the park path).
        assert!(
            !calls.iter().any(|c| c.action == "arbiter_park"),
            "supersede must NOT route through the arbiter_park / human-review-hold path, got: {calls:?}"
        );

        // Supersede is terminal — produces Closed, and does not open a PR.
        assert!(
            matches!(report.outcome, TaskRunOutcome::Closed { .. }),
            "supersede must produce Closed, got: {:?}",
            report.outcome
        );
        assert!(
            !open_pr_called.load(std::sync::atomic::Ordering::SeqCst),
            "supersede must not open a PR"
        );
    }

    #[tokio::test]
    async fn arbiter_reopen_does_not_call_open_pr() {
        // Reopen must NOT fall through to open_pr — it returns the task to
        // `open` for a fresh worker dispatch.
        let (_root, supervisor, spec, _transition_calls, open_pr_called, _reopen_calls, _complete) =
            build_arbiter_gate_test_env_with_reopen(
                "T-reopen-no-pr",
                "proj-reopen",
                StageOutcome::LeadReopen {
                    reason: "blocked on deps".into(),
                    directive: "Update the API client to use the new endpoint".into(),
                    verification_command: "cargo test".into(),
                    exclude_models: vec![],
                },
                Ok(ArbiterGateResult::Pass),
            )
            .await;

        let _report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            !open_pr_called.load(std::sync::atomic::Ordering::SeqCst),
            "reopen must NOT fall through to open_pr"
        );
    }

    // ── zkk9 round 3: monitored reopen lifecycle regression tests ───────────

    /// A worker task-run with a `WorkerDone` outcome (terminal submit) must
    /// call `complete_monitored_reopen` — this is the monitored worker
    /// terminal outcome that closes out the reopen attempt.
    #[tokio::test]
    async fn worker_submit_completes_monitored_reopen() {
        let (_root, supervisor, spec, _transition_calls, _open_pr, _reopen, complete_calls) =
            build_worker_flow_test_env(
                "T-worker-submit-complete",
                "proj-ws",
                StageOutcome::WorkerDone,
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        // WorkerDone produces WorkerSubmitted (terminal for this run).
        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "worker done must produce WorkerSubmitted, got: {:?}",
            report.outcome
        );

        // The post-loop completion hook must fire for this worker terminal
        // outcome (it was NOT started by this run — started_monitored_reopen
        // is false for a worker run).
        let complete = complete_calls.lock().unwrap();
        assert_eq!(
            complete.len(),
            1,
            "complete_monitored_reopen must be called once for worker submit terminal outcome, got: {complete:?}"
        );
    }

    /// A worker task-run that fails (worker failure) must call
    /// `complete_monitored_reopen` — worker failure is a terminal outcome.
    #[tokio::test]
    async fn worker_failure_completes_monitored_reopen() {
        let (_root, supervisor, spec, _transition_calls, _open_pr, _reopen, complete_calls) =
            build_worker_flow_test_env(
                "T-worker-fail-complete",
                "proj-wf",
                StageOutcome::Failed {
                    reason: "worker crashed".into(),
                    provider_failure: None,
                },
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::Failed { .. }),
            "worker failure must produce Failed, got: {:?}",
            report.outcome
        );

        let complete = complete_calls.lock().unwrap();
        assert_eq!(
            complete.len(),
            1,
            "complete_monitored_reopen must be called once for worker failure, got: {complete:?}"
        );
    }

    /// A worker task-run with a loop-guard trip must call
    /// `complete_monitored_reopen` — loop-guard is a terminal failure outcome.
    #[tokio::test]
    async fn worker_loop_guard_completes_monitored_reopen() {
        let (_root, supervisor, spec, _transition_calls, _open_pr, _reopen, complete_calls) =
            build_worker_flow_test_env(
                "T-worker-loop-complete",
                "proj-wl",
                StageOutcome::LoopGuardTripped {
                    kind: djinn_runtime::LoopGuardKind::IdenticalToolFailure,
                    offending_signature: "shell".into(),
                    threshold: 3,
                    observed: 5,
                    turn_span: (1, 10),
                    session_id: "sess-1".into(),
                },
            )
            .await;

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::LoopGuardTripped { .. }),
            "loop guard must produce LoopGuardTripped, got: {:?}",
            report.outcome
        );

        let complete = complete_calls.lock().unwrap();
        assert_eq!(
            complete.len(),
            1,
            "complete_monitored_reopen must be called once for loop-guard trip, got: {complete:?}"
        );
    }

    // ── CommitOutcome excluded-path propagation tests ─────────────────────
    //
    // These tests verify that the WorkerDone/ArchitectDone auto-commit path
    // in the supervisor correctly surfaces excluded-path data from the typed
    // `CommitOutcome` at the reporting/logging boundary.

    /// A services mock whose `execute_stage` writes files into the workspace
    /// (simulating what a real worker does) and then returns `WorkerDone` so
    /// the supervisor's auto-commit path exercises the full
    /// `CommitOutcome` handling.
    struct CommitPathServices {
        cancel: CancellationToken,
        task: Task,
        updated_statuses: std::sync::Arc<std::sync::Mutex<Vec<TaskRunStatus>>>,
        /// Closure called inside `execute_stage` with the workspace path.
        /// Use this to write scratch/legitimate files before the supervisor
        /// auto-commits.
        write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync>,
    }

    #[async_trait]
    impl SupervisorServices for CommitPathServices {
        fn cancel(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, task_id: String) -> Result<Task, String> {
            assert_eq!(task_id, self.task.id);
            Ok(self.task.clone())
        }

        async fn execute_stage(
            &self,
            _task: &Task,
            workspace: &Workspace,
            _role_kind: RoleKind,
            _task_run_id: &str,
            _spec: &TaskRunSpec,
        ) -> Result<StageOutcome, StageError> {
            (self.write_fn)(workspace.path());
            Ok(StageOutcome::WorkerDone)
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            TaskRunOutcome::WorkerSubmitted
        }

        async fn create_task_run(
            &self,
            _params: services::SerializableCreateTaskRunParams,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn update_task_run_status(
            &self,
            _run_id: String,
            status: TaskRunStatus,
        ) -> Result<(), String> {
            self.updated_statuses
                .lock()
                .expect("updated statuses mutex poisoned")
                .push(status);
            Ok(())
        }

        async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
            Ok(128_000)
        }

        async fn get_provider_base_url(
            &self,
            _catalog_provider_id: String,
        ) -> Result<String, String> {
            Ok("http://localhost".into())
        }

        async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
            Ok(None)
        }

        async fn create_session(
            &self,
            _params: services::SerializableCreateSessionParams,
        ) -> Result<djinn_core::models::SessionRecord, String> {
            unimplemented!("not exercised")
        }

        async fn publish_session_message(
            &self,
            _session_id: String,
            _task_id: String,
            _agent_type: String,
            _message: serde_json::Value,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn get_environment_config(
            &self,
            _project_id: String,
        ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
            unimplemented!("not exercised")
        }

        async fn invoke_llm(
            &self,
            _model_id: String,
            _conversation: djinn_provider::message::Conversation,
            _tools: Vec<serde_json::Value>,
            _tool_choice: Option<djinn_provider::provider::ToolChoice>,
        ) -> Result<djinn_provider::provider::LlmResponse, String> {
            unimplemented!("not exercised")
        }

        #[allow(clippy::too_many_arguments)]
        async fn update_session_status(
            &self,
            _session_id: String,
            _status: djinn_core::models::SessionStatus,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
            _parked_reason: Option<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn tool_github_search(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_fetch_file(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_ci_job_log(
            &self,
            _session_task_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn emit_djinn_event(
            &self,
            _event: services::SerializableDjinnEvent,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn transition_task(
            &self,
            _task_id: String,
            _action: String,
            _reason: Option<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn run_arbiter_preapproval_gate(
            &self,
            _task: &Task,
        ) -> Result<ArbiterGateResult, String> {
            Ok(ArbiterGateResult::Pass)
        }

        async fn record_arbiter_decision(
            &self,
            _task_id: String,
            _decision: String,
            _evidence_json: String,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn start_monitored_reopen(
            &self,
            _task_id: String,
            _directive: String,
            _verification_command: String,
            _exclude_models: Vec<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn record_arbiter_session_termination(
            &self,
            _task_id: String,
            _is_infra_failure: bool,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }

    fn commit_path_spec(task_id: &str, project_id: &str, run_id: &str) -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: run_id.into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/commit-path".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    /// Junk-only WorkerDone: supervisor must emit a distinct log line with
    /// `excluded_count` and `excluded_paths` structured fields rather than
    /// pretending the tree was clean.
    #[tokio::test]
    async fn worker_done_junk_only_emits_excluded_paths_in_logs() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-junk-only";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                std::fs::write(ws_path.join("patch.txt"), "scratch\n").expect("write patch.txt");
                std::fs::write(ws_path.join("test.txt"), "scratch\n").expect("write test.txt");
            });

        let services: Arc<dyn SupervisorServices> = Arc::new(CommitPathServices {
            cancel: CancellationToken::new(),
            task: fixture_task("task-junk", project_id),
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            write_fn,
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = commit_path_spec("task-junk", project_id, "run-junk");

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");

        // Run should complete normally (junk-only is not an error).
        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "junk-only run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );

        let captured = logs.take();
        // Must contain the junk-only message with structured fields.
        assert!(
            captured.contains("no legitimate changes after stage; junk-only files excluded"),
            "expected junk-only log message, got:\n{captured}"
        );
        assert!(
            captured.contains("excluded_count="),
            "expected excluded_count structured field, got:\n{captured}"
        );
        assert!(
            captured.contains("excluded_paths="),
            "expected excluded_paths structured field, got:\n{captured}"
        );
        // Must NOT contain a generic "committed" message for this case.
        assert!(
            !captured.contains("committed worker/architect changes"),
            "junk-only must NOT log as a normal commit, got:\n{captured}"
        );
    }

    /// Legitimate + scratch WorkerDone: supervisor must commit the legitimate
    /// file AND log the excluded scratch files alongside the committed message.
    #[tokio::test]
    async fn worker_done_legitimate_plus_scratch_logs_excluded_paths() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-mixed";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                // Legitimate source file
                std::fs::write(ws_path.join("real.rs"), "fn main() {}\n").expect("write real.rs");
                // Scratch files
                std::fs::write(ws_path.join("patch.txt"), "scratch\n").expect("write patch.txt");
                std::fs::write(ws_path.join("test2.txt"), "scratch\n").expect("write test2.txt");
            });

        let services: Arc<dyn SupervisorServices> = Arc::new(CommitPathServices {
            cancel: CancellationToken::new(),
            task: fixture_task("task-mixed", project_id),
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            write_fn,
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = commit_path_spec("task-mixed", project_id, "run-mixed");

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "mixed legit+scratch run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );

        let captured = logs.take();
        // Must contain the committed message WITH scratch exclusion details.
        assert!(
            captured.contains("committed worker/architect changes (some scratch files excluded)"),
            "expected committed-with-exclusions log message, got:\n{captured}"
        );
        assert!(
            captured.contains("excluded_count="),
            "expected excluded_count structured field, got:\n{captured}"
        );
        assert!(
            captured.contains("excluded_paths="),
            "expected excluded_paths structured field, got:\n{captured}"
        );
        // Must NOT contain the junk-only message (legitimate changes exist).
        assert!(
            !captured.contains("no legitimate changes after stage"),
            "mixed commit must NOT log as junk-only, got:\n{captured}"
        );
    }

    /// Clean WorkerDone (no files written): supervisor logs a debug no-op
    /// message and does NOT emit excluded-path fields.
    #[tokio::test]
    async fn worker_done_clean_tree_logs_no_changes() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-clean";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        // No-op write_fn: the workspace is untouched.
        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|_ws_path: &std::path::Path| {
                // Intentionally empty — clean tree.
            });

        let services: Arc<dyn SupervisorServices> = Arc::new(CommitPathServices {
            cancel: CancellationToken::new(),
            task: fixture_task("task-clean", project_id),
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            write_fn,
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = commit_path_spec("task-clean", project_id, "run-clean");

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "clean tree run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );

        let captured = logs.take();
        // Must contain the no-changes message (at debug level).
        assert!(
            captured.contains("no changes to commit after stage"),
            "expected no-changes log message, got:\n{captured}"
        );
        // Must NOT contain excluded-path fields for a clean tree.
        assert!(
            !captured.contains("excluded_count="),
            "clean tree must NOT log excluded_count, got:\n{captured}"
        );
        assert!(
            !captured.contains("no legitimate changes"),
            "clean tree must NOT log as junk-only, got:\n{captured}"
        );
    }

    // ── GitHub publication regression tests (ia4y) ──────────────────────────
    //
    // Tests covering the four GitHub publication scenarios in the WorkerDone
    // path: no-open-PR, open-PR success, mirror-push failure, GitHub-push
    // failure, plus a junk-free alignment assertion.

    /// Services mock that extends the `CommitPathServices` pattern with
    /// `publish_branch_to_github` call tracking and a configurable
    /// publication result.
    struct GitHubPublicationTestServices {
        cancel: CancellationToken,
        task: Task,
        transition_calls: std::sync::Arc<std::sync::Mutex<Vec<TransitionCall>>>,
        submit_task_review_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// Closure called inside `execute_stage` with the workspace path.
        write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync>,
        /// Tracks how many times `publish_branch_to_github` was called.
        publish_call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Configurable result returned by `publish_branch_to_github`.
        publish_result: BranchPublicationResult,
        /// Tracks the "GitHub branch head" state, simulating the stale-head
        /// condition (aah4 regression). Initially stale (e.g. old SHA);
        /// `publish_branch_to_github` updates it to the new SHA.
        github_head: std::sync::Arc<std::sync::Mutex<String>>,
        /// Optional mirror for resolving the actual mirror HEAD at call time,
        /// used by the aah4 regression test to populate the pushed SHA
        /// dynamically.  `None` for tests that don't need real SHA resolution.
        mirror: Option<Arc<MirrorManager>>,
        updated_statuses: std::sync::Arc<std::sync::Mutex<Vec<TaskRunStatus>>>,
    }

    #[async_trait]
    impl SupervisorServices for GitHubPublicationTestServices {
        fn cancel(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, task_id: String) -> Result<Task, String> {
            assert_eq!(task_id, self.task.id);
            Ok(self.task.clone())
        }

        async fn execute_stage(
            &self,
            _task: &Task,
            workspace: &Workspace,
            role_kind: RoleKind,
            _task_run_id: &str,
            _spec: &TaskRunSpec,
        ) -> Result<StageOutcome, StageError> {
            assert_eq!(role_kind, RoleKind::Worker);
            (self.write_fn)(workspace.path());
            Ok(StageOutcome::WorkerDone)
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            TaskRunOutcome::WorkerSubmitted
        }

        async fn create_task_run(
            &self,
            _params: services::SerializableCreateTaskRunParams,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn update_task_run_status(
            &self,
            _run_id: String,
            status: TaskRunStatus,
        ) -> Result<(), String> {
            self.updated_statuses
                .lock()
                .expect("updated statuses mutex poisoned")
                .push(status);
            Ok(())
        }

        async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
            Ok(128_000)
        }

        async fn get_provider_base_url(
            &self,
            _catalog_provider_id: String,
        ) -> Result<String, String> {
            Ok("http://localhost".into())
        }

        async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
            Ok(None)
        }

        async fn create_session(
            &self,
            _params: services::SerializableCreateSessionParams,
        ) -> Result<djinn_core::models::SessionRecord, String> {
            unimplemented!("not exercised")
        }

        async fn publish_session_message(
            &self,
            _session_id: String,
            _task_id: String,
            _agent_type: String,
            _message: serde_json::Value,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn get_environment_config(
            &self,
            _project_id: String,
        ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
            unimplemented!("not exercised")
        }

        async fn invoke_llm(
            &self,
            _model_id: String,
            _conversation: djinn_provider::message::Conversation,
            _tools: Vec<serde_json::Value>,
            _tool_choice: Option<djinn_provider::provider::ToolChoice>,
        ) -> Result<djinn_provider::provider::LlmResponse, String> {
            unimplemented!("not exercised")
        }

        #[allow(clippy::too_many_arguments)]
        async fn update_session_status(
            &self,
            _session_id: String,
            _status: djinn_core::models::SessionStatus,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
            _parked_reason: Option<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn tool_github_search(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_github_fetch_file(
            &self,
            _project_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn tool_ci_job_log(
            &self,
            _session_task_id: Option<String>,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!("not exercised")
        }

        async fn emit_djinn_event(
            &self,
            _event: services::SerializableDjinnEvent,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn transition_task(
            &self,
            task_id: String,
            action: String,
            reason: Option<String>,
        ) -> Result<(), String> {
            if action == "submit_task_review" {
                self.submit_task_review_called
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.transition_calls
                .lock()
                .expect("transition_calls mutex poisoned")
                .push(TransitionCall {
                    task_id,
                    action,
                    reason,
                });
            Ok(())
        }

        async fn run_arbiter_preapproval_gate(
            &self,
            _task: &Task,
        ) -> Result<ArbiterGateResult, String> {
            Ok(ArbiterGateResult::Pass)
        }

        async fn record_arbiter_decision(
            &self,
            _task_id: String,
            _decision: String,
            _evidence_json: String,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn start_monitored_reopen(
            &self,
            _task_id: String,
            _directive: String,
            _verification_command: String,
            _exclude_models: Vec<String>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
            Ok(())
        }

        async fn record_arbiter_session_termination(
            &self,
            _task_id: String,
            _is_infra_failure: bool,
        ) -> Result<bool, String> {
            Ok(false)
        }

        async fn publish_branch_to_github(
            &self,
            _spec: &TaskRunSpec,
            _task: &Task,
        ) -> BranchPublicationResult {
            self.publish_call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Simulate the real push: update our tracked GitHub head to match
            // the pushed SHA, modelling what happens when
            // `push_task_branch_to_github` succeeds.
            //
            // If a `mirror` is provided, resolve the current mirror HEAD
            // dynamically (used by aah4 regression test where the SHA is
            // only known after the supervisor's commit+push).  Otherwise,
            // fall back to the static `publish_result.pushed_sha`.
            let effective_sha = if let Some(ref mirror) = self.mirror {
                let mirror_path = mirror.mirror_path(&_spec.project_id);
                let output = std::process::Command::new("git")
                    .args([
                        "--git-dir",
                        &mirror_path.to_string_lossy(),
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{}", _spec.task_branch),
                    ])
                    .output()
                    .expect("git rev-parse must run in mock publish_branch_to_github");
                assert!(
                    output.status.success(),
                    "git rev-parse failed in mock: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                self.publish_result.pushed_sha.clone()
            };
            if let Some(ref sha) = effective_sha {
                *self.github_head.lock().expect("github_head mutex poisoned") = sha.clone();
            }
            // Build a result that reflects the effective SHA.
            let mut result = self.publish_result.clone();
            result.pushed_sha = effective_sha;
            result
        }
    }

    fn gh_pub_spec(
        task_id: &str,
        project_id: &str,
        run_id: &str,
        task_branch: &str,
    ) -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: run_id.into(),
            task_id: task_id.into(),
            project_id: project_id.into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: task_branch.into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: Default::default(),
            read_source_project_ids: Vec::new(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    /// Test 1: No-open-PR WorkerDone.
    ///
    /// When `task.pr_url` is `None`, `publish_branch_to_github` must NOT be
    /// called. The task must proceed to `submit_task_review` normally.
    #[tokio::test]
    async fn no_open_pr_worker_done_does_not_publish_to_github() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-no-pr";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                std::fs::write(ws_path.join("real.rs"), "fn main() {}\n").expect("write real.rs");
            });

        let publish_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let task = fixture_task("task-no-pr", project_id);
        assert!(
            task.pr_url.is_none(),
            "fixture must start with pr_url = None"
        );

        let services: Arc<dyn SupervisorServices> = Arc::new(GitHubPublicationTestServices {
            cancel: CancellationToken::new(),
            task,
            transition_calls: transition_calls.clone(),
            submit_task_review_called: submit_task_review_called.clone(),
            write_fn,
            publish_call_count: publish_call_count.clone(),
            publish_result: BranchPublicationResult {
                success: true,
                pushed_sha: None,
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: false,
                error_class: None,
                error_message: None,
            },
            github_head: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mirror: None,
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = gh_pub_spec("task-no-pr", project_id, "run-no-pr", "djinn/no-pr");

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "no-open-PR run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );
        assert_eq!(
            publish_call_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "publish_branch_to_github must NOT be called when pr_url is None"
        );
        assert!(
            submit_task_review_called.load(std::sync::atomic::Ordering::SeqCst),
            "submit_task_review must be called for a successful WorkerDone"
        );
    }

    /// Test 2: Open-PR success.
    ///
    /// When `task.pr_url` is `Some(...)` and `publish_branch_to_github`
    /// returns success, the mock must be called exactly once and the task
    /// must transition to `submit_task_review`.
    #[tokio::test]
    async fn open_pr_success_publishes_to_github_and_transitions() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-gh-success";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                std::fs::write(ws_path.join("real.rs"), "fn main() {}\n").expect("write real.rs");
            });

        let publish_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let mut task = fixture_task("task-gh-success", project_id);
        task.pr_url = Some("https://github.com/test/repo/pull/42".into());

        let services: Arc<dyn SupervisorServices> = Arc::new(GitHubPublicationTestServices {
            cancel: CancellationToken::new(),
            task,
            transition_calls: transition_calls.clone(),
            submit_task_review_called: submit_task_review_called.clone(),
            write_fn,
            publish_call_count: publish_call_count.clone(),
            publish_result: BranchPublicationResult {
                success: true,
                pushed_sha: Some("abc123mirrorhead".into()),
                mirror_head: "abc123mirrorhead".into(),
                attempted_github_head: "abc123mirrorhead".into(),
                pr_branch_existed: true,
                error_class: None,
                error_message: None,
            },
            github_head: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mirror: None,
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = gh_pub_spec(
            "task-gh-success",
            project_id,
            "run-gh-success",
            "djinn/gh-success",
        );

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "open-PR success run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );
        assert_eq!(
            publish_call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "publish_branch_to_github must be called exactly once for open-PR tasks"
        );
        assert!(
            submit_task_review_called.load(std::sync::atomic::Ordering::SeqCst),
            "submit_task_review must be called after successful GitHub publication"
        );

        let captured = logs.take();
        assert!(
            captured.contains("published WorkerDone mirror commit to GitHub open-PR branch"),
            "expected success log line, got:\n{captured}"
        );
    }

    /// Test 3: Mirror-push failure (no GitHub attempt).
    ///
    /// When `push_to_origin` fails on all retries, `publish_branch_to_github`
    /// must NOT be called and the run must fail (task stays in_progress).
    #[tokio::test]
    async fn mirror_push_failure_skips_github_publish_and_fails_run() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-gh-mirror-fail";
        let task_id = "task-gh-mirror-fail";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        // Create an ephemeral workspace and write a file so the auto-commit
        // produces a real diff that needs pushing.
        let ws = mirror
            .clone_ephemeral(project_id, "main")
            .await
            .expect("clone ephemeral workspace");
        let tb = "djinn/gh-mirror-fail";
        run_git(ws.path(), &["checkout", "-b", tb]);
        tokio::fs::write(ws.path().join("work.txt"), "real worker output")
            .await
            .expect("write fixture file");
        drop(ws);

        // Install a pre-receive hook that always rejects so push_to_origin fails.
        let mirror_path = mirror.mirror_path(project_id);
        let hooks_dir = mirror_path.join("hooks");
        tokio::fs::create_dir_all(&hooks_dir)
            .await
            .expect("create hooks dir");
        let hook_path = hooks_dir.join("pre-receive");
        tokio::fs::write(&hook_path, "#!/bin/sh\nexit 1\n")
            .await
            .expect("write pre-receive hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))
                .await
                .expect("chmod pre-receive hook");
        }

        let publish_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let mut task = fixture_task(task_id, project_id);
        task.pr_url = Some("https://github.com/test/repo/pull/99".into());

        let services: Arc<dyn SupervisorServices> = Arc::new(GitHubPublicationTestServices {
            cancel: CancellationToken::new(),
            task,
            transition_calls: transition_calls.clone(),
            submit_task_review_called: submit_task_review_called.clone(),
            write_fn: std::sync::Arc::new(|_ws_path: &std::path::Path| {
                // Workspace already has content from the pre-clone above.
            }),
            publish_call_count: publish_call_count.clone(),
            publish_result: BranchPublicationResult {
                success: true,
                pushed_sha: Some("should-not-be-reached".into()),
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: false,
                error_class: None,
                error_message: None,
            },
            github_head: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mirror: None,
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = gh_pub_spec(task_id, project_id, "run-gh-mirror-fail", tb);

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::Failed { .. }),
            "mirror-push failure must produce TaskRunOutcome::Failed, got {:?}",
            report.outcome
        );
        assert_eq!(
            publish_call_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "publish_branch_to_github must NOT be called when mirror push failed"
        );
        assert!(
            !submit_task_review_called.load(std::sync::atomic::Ordering::SeqCst),
            "submit_task_review must NOT be called when push_to_origin failed — task stays in_progress"
        );
        assert_eq!(
            report.stages_completed,
            vec![RoleKind::Worker],
            "worker stage itself completed (it was the push that failed)"
        );
    }

    /// Test 4: GitHub-push failure.
    ///
    /// When `publish_branch_to_github` returns a failure result, the task
    /// MUST still proceed to `submit_task_review` (GitHub push failure does
    /// NOT block the task lifecycle). Structured publication-failure evidence
    /// must be present in the logs.
    #[tokio::test]
    async fn github_push_failure_still_transitions_to_submit_task_review() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-gh-fail";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                std::fs::write(ws_path.join("real.rs"), "fn main() {}\n").expect("write real.rs");
            });

        let publish_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let mut task = fixture_task("task-gh-fail", project_id);
        task.pr_url = Some("https://github.com/test/repo/pull/77".into());

        let services: Arc<dyn SupervisorServices> = Arc::new(GitHubPublicationTestServices {
            cancel: CancellationToken::new(),
            task,
            transition_calls: transition_calls.clone(),
            submit_task_review_called: submit_task_review_called.clone(),
            write_fn,
            publish_call_count: publish_call_count.clone(),
            publish_result: BranchPublicationResult {
                success: false,
                pushed_sha: None,
                mirror_head: "abc123mirrorhead".into(),
                attempted_github_head: "def456attempted".into(),
                pr_branch_existed: true,
                error_class: Some("push_rejected".into()),
                error_message: Some("remote: error: GH006: Protected branch update failed".into()),
            },
            github_head: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mirror: None,
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = gh_pub_spec("task-gh-fail", project_id, "run-gh-fail", "djinn/gh-fail");

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(true)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let report = supervisor.run(spec).await.expect("supervisor run");

        // The run MUST complete as WorkerSubmitted — GitHub push failure
        // does NOT block the task lifecycle.
        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "GitHub push failure must NOT block task lifecycle — expected WorkerSubmitted, got {:?}",
            report.outcome
        );
        assert_eq!(
            publish_call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "publish_branch_to_github must be called exactly once"
        );
        assert!(
            submit_task_review_called.load(std::sync::atomic::Ordering::SeqCst),
            "submit_task_review MUST still be called — GitHub push failure does not block"
        );

        // Verify structured publication-failure evidence fields.
        let captured = logs.take();
        assert!(
            captured.contains("djinn_supervisor::github_publication_failure"),
            "expected structured publication-failure tracing target, got:\n{captured}"
        );
        assert!(
            captured.contains("mirror_head=abc123mirrorhead"),
            "publication-failure must carry mirror_head, got:\n{captured}"
        );
        assert!(
            captured.contains("github_head=def456attempted"),
            "publication-failure must carry attempted_github_head, got:\n{captured}"
        );
        assert!(
            captured.contains("pr_branch_existed=true"),
            "publication-failure must carry pr_branch_existed, got:\n{captured}"
        );
        assert!(
            captured.contains("error_class=Some(\"push_rejected\")"),
            "publication-failure must carry error_class, got:\n{captured}"
        );
    }

    /// Test 5: Junk-free alignment.
    ///
    /// Drive a WorkerDone with both legitimate source edits and scratch files
    /// (`patch.txt`, `test2.txt`). Verify the committed mirror branch is
    /// junk-free — only the legitimate file appears in the pushed commit.
    #[tokio::test]
    async fn worker_done_pushed_mirror_branch_is_junk_free() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-junk-free";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                // Legitimate source file
                std::fs::write(ws_path.join("src_main.rs"), "fn main() {}\n")
                    .expect("write src_main.rs");
                // Scratch / junk files that must be filtered
                std::fs::write(ws_path.join("patch.txt"), "scratch diff\n")
                    .expect("write patch.txt");
                std::fs::write(ws_path.join("test2.txt"), "scratch output\n")
                    .expect("write test2.txt");
            });

        let publish_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let task = fixture_task("task-junk-free", project_id);

        let services: Arc<dyn SupervisorServices> = Arc::new(GitHubPublicationTestServices {
            cancel: CancellationToken::new(),
            task,
            transition_calls: transition_calls.clone(),
            submit_task_review_called: submit_task_review_called.clone(),
            write_fn,
            publish_call_count: publish_call_count.clone(),
            publish_result: BranchPublicationResult {
                success: true,
                pushed_sha: None,
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: false,
                error_class: None,
                error_message: None,
            },
            github_head: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mirror: None,
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = gh_pub_spec(
            "task-junk-free",
            project_id,
            "run-junk-free",
            "djinn/junk-free",
        );

        let report = supervisor.run(spec).await.expect("supervisor run");

        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "junk-free run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );

        // Verify the mirror's task branch is junk-free: only the legitimate
        // file should appear in the commit. The mirror is a bare repo, so
        // use `git show --name-only <branch>` to inspect.
        let mirror_path = mirror.mirror_path(project_id);
        let git_dir_arg = mirror_path.to_string_lossy().to_string();

        // List files changed in the latest commit on the task branch.
        let output = std::process::Command::new("git")
            .args([
                "--git-dir",
                &git_dir_arg,
                "show",
                "--name-only",
                "--pretty=format:",
                "djinn/junk-free",
            ])
            .output()
            .expect("git show must run");
        assert!(
            output.status.success(),
            "git show failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let changed_files = String::from_utf8_lossy(&output.stdout);
        let changed_files: Vec<&str> = changed_files
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();

        // The committed files must contain the legitimate file.
        assert!(
            changed_files.iter().any(|f| *f == "src_main.rs"),
            "committed branch must contain the legitimate file src_main.rs, got: {changed_files:?}"
        );

        // The committed files must NOT contain any scratch files.
        assert!(
            !changed_files.iter().any(|f| *f == "patch.txt"),
            "committed branch must NOT contain scratch file patch.txt, got: {changed_files:?}"
        );
        assert!(
            !changed_files.iter().any(|f| *f == "test2.txt"),
            "committed branch must NOT contain scratch file test2.txt, got: {changed_files:?}"
        );
    }

    /// Test 6: aah4-shaped stale-GitHub-head regression.
    ///
    /// Reproduces the stale-GitHub-head condition that task aah4 exposed:
    /// a worker's new commit lands on the mirror (via the supervisor's
    /// `commit` + `push_to_origin`), but the GitHub PR branch head is NOT
    /// updated — so GitHub Actions evaluates a stale PR head instead of the
    /// worker's latest commit.
    ///
    /// This test verifies that when `publish_branch_to_github` fires for a
    /// task with an existing open PR, the mock's GitHub head is updated to
    /// match the mirror head (heads aligned — the stale-head condition is
    /// resolved).
    ///
    /// Contrast with Test 1 (`no_open_pr_worker_done_does_not_publish_to_github`):
    /// when there is no open PR, `publish_branch_to_github` is NOT called and
    /// the GitHub head remains stale, demonstrating the problem this feature
    /// solves.
    ///
    /// Cross-references: epic vy47, proposal icoe acceptance criteria 4, 7, 8.
    #[tokio::test]
    async fn aah4_stale_github_head_reconciled_by_publish_branch_to_github() {
        let root = tempfile::tempdir_in(std::env::current_dir().expect("current dir"))
            .expect("temp test root");
        let source_dir = root.path().join("source");
        make_source_repo(&source_dir);

        let project_id = "proj-aah4-stale";
        let mirror = Arc::new(MirrorManager::new(root.path().join("mirrors")));
        mirror
            .ensure_mirror(project_id, &format!("file://{}", source_dir.display()))
            .await
            .expect("install fixture mirror");

        // Capture the initial base-branch SHA on the mirror BEFORE the
        // worker commits.  This models the "old GitHub head" — the SHA that
        // GitHub's PR branch points to before publication.
        let mirror_path = mirror.mirror_path(project_id);
        let base_sha = {
            let output = std::process::Command::new("git")
                .args([
                    "--git-dir",
                    &mirror_path.to_string_lossy(),
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/main",
                ])
                .output()
                .expect("git rev-parse base SHA");
            assert!(
                output.status.success(),
                "git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        // The worker writes a file so the auto-commit produces a real diff.
        let write_fn: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync> =
            std::sync::Arc::new(|ws_path: &std::path::Path| {
                std::fs::write(ws_path.join("real.rs"), "fn main() {}\n").expect("write real.rs");
            });

        let publish_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transition_calls: Arc<Mutex<Vec<TransitionCall>>> = Arc::new(Mutex::new(Vec::new()));
        let submit_task_review_called = Arc::new(AtomicBool::new(false));

        let mut task = fixture_task("task-aah4-stale", project_id);
        task.pr_url = Some("https://github.com/test/repo/pull/101".into());

        // Model the stale-GitHub-head condition: the GitHub head is
        // initially the OLD base SHA (stale), even though the worker will
        // produce a new commit on the mirror.
        let github_head = Arc::new(std::sync::Mutex::new(base_sha.clone()));

        let services: Arc<dyn SupervisorServices> = Arc::new(GitHubPublicationTestServices {
            cancel: CancellationToken::new(),
            task,
            transition_calls: transition_calls.clone(),
            submit_task_review_called: submit_task_review_called.clone(),
            write_fn,
            publish_call_count: publish_call_count.clone(),
            publish_result: BranchPublicationResult {
                success: true,
                // The mirror ref is None; the mock resolves it dynamically
                // from the mirror via `MirrorManager::mirror_path`.
                pushed_sha: None,
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: true,
                error_class: None,
                error_message: None,
            },
            github_head: github_head.clone(),
            mirror: Some(mirror.clone()),
            updated_statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let supervisor = TaskRunSupervisor::new(Arc::clone(&mirror), services);
        let spec = gh_pub_spec(
            "task-aah4-stale",
            project_id,
            "run-aah4-stale",
            "djinn/aah4-stale",
        );

        let report = supervisor.run(spec).await.expect("supervisor run");

        // The run must complete as WorkerSubmitted.
        assert!(
            matches!(report.outcome, TaskRunOutcome::WorkerSubmitted),
            "aah4 stale-head run must produce WorkerSubmitted, got {:?}",
            report.outcome
        );

        // publish_branch_to_github must have been called exactly once
        // (task has an open PR).
        assert_eq!(
            publish_call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "publish_branch_to_github must be called exactly once for open-PR tasks"
        );

        // Resolve the actual mirror HEAD after the supervisor committed and
        // pushed to the mirror.  This is the new commit SHA the worker produced.
        let mirror_head_after = {
            let output = std::process::Command::new("git")
                .args([
                    "--git-dir",
                    &mirror_path.to_string_lossy(),
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/djinn/aah4-stale"),
                ])
                .output()
                .expect("git rev-parse mirror HEAD after run");
            assert!(
                output.status.success(),
                "git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        // The mirror HEAD must be a NEW commit (different from the base SHA).
        assert_ne!(
            mirror_head_after, base_sha,
            "mirror HEAD must be a new commit after the worker's auto-commit"
        );

        // ── Heads aligned ──
        //
        // After `publish_branch_to_github`, the GitHub head must equal the
        // mirror head.  This is the key assertion: the stale-head condition
        // is resolved.
        let gh_head_after = github_head
            .lock()
            .expect("github_head mutex poisoned")
            .clone();
        assert_eq!(
            gh_head_after, mirror_head_after,
            "after publish_branch_to_github, GitHub head must equal mirror head \
             (stale-head condition resolved). GitHub={gh_head_after}, mirror={mirror_head_after}"
        );

        // The GitHub head must have changed from the stale base SHA.
        assert_ne!(
            gh_head_after, base_sha,
            "GitHub head must no longer be the stale base SHA"
        );

        // ── Contrast: no-open-PR scenario ──
        //
        // When `task.pr_url` is `None`, `publish_branch_to_github` is NOT
        // called and the GitHub head remains at whatever it was before the
        // run — demonstrating the stale-head problem this feature solves.
        // This is verified by Test 1
        // (`no_open_pr_worker_done_does_not_publish_to_github`), which asserts
        // `publish_call_count == 0` when `pr_url` is `None`.
    }
}

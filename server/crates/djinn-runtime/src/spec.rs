// djinn:allow-oversize — runtime spec types over size-guard threshold; split when touched substantively.
//! Wire-capable task-run spec + outcome types.
//!
//! These types were previously `djinn_agent::supervisor::{spec, flow}`; Phase
//! 2 PR 1 moved them here so that the future in-container supervisor (running
//! inside `djinn-agent-worker`) can share the exact type definitions with the
//! host-side coordinator without re-exporting them across an `AppState`-heavy
//! crate boundary.
//!
//! All types derive `Serialize + Deserialize` so they can ride a
//! `bincode::serialize`/`deserialize` frame between the host and the
//! container (bincode 1.3 is serde-driven, so no separate `Encode`/`Decode`
//! derives are needed).

use std::collections::HashMap;
use std::fmt;

use djinn_core::models::TaskRunTrigger;
use djinn_core::tool_error::ErrorClass;
use serde::{Deserialize, Serialize};

/// Which role executes at each stage of a task-run.
///
/// Not the same as `djinn-agent`'s existing `AgentRole` trait objects — this
/// is a lightweight enum suitable for flow templates and telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    Planner,
    Worker,
    Reviewer,
    Verifier,
    Architect,
    Lead,
    /// Refinement tribunal role (advocate, adversary, or judge). The concrete
    /// agent type is resolved from `task.agent_type` at the role-overrides layer.
    Refinement,
}

/// Reply-loop guard family that terminated a degenerate session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardKind {
    IdenticalToolFailure,
    PermissionDenial,
    IdenticalOutput,
    ConsecutiveFailures,
}

/// Typed error used inside the agent reply loop before it is mapped onto a
/// terminal [`TaskRunOutcome`] / supervisor [`StageOutcome`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopGuardTrip {
    pub kind: LoopGuardKind,
    pub offending_signature: String,
    pub threshold: u32,
    pub observed: u32,
    pub turn_span: (u32, u32),
    pub session_id: String,
}

impl fmt::Display for LoopGuardTrip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "loop guard tripped: kind={:?} signature={} observed={} threshold={} turn_span={:?} session_id={}",
            self.kind,
            self.offending_signature,
            self.observed,
            self.threshold,
            self.turn_span,
            self.session_id
        )
    }
}

impl std::error::Error for LoopGuardTrip {}

impl RoleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::Planner => "planner",
            RoleKind::Worker => "worker",
            RoleKind::Reviewer => "reviewer",
            RoleKind::Verifier => "verifier",
            RoleKind::Architect => "architect",
            RoleKind::Lead => "lead",
            RoleKind::Refinement => "refinement",
        }
    }
}

/// Template for a task-run's role sequence.
///
/// `NewTask` is the canonical "work" flow: plan, execute, review, verify,
/// PR. `ReviewResponse` and `ConflictRetry` re-enter mid-flow when the
/// planner's decision is already implicit in the task's existence. `Spike`
/// routes the architect onto scoped research tasks the planner created
/// during a prior NewTask. `Planning` runs the planner alone — useful for
/// explicit "just re-plan this" invocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorFlow {
    NewTask,
    ReviewResponse,
    ConflictRetry,
    Spike,
    Planning,
    /// Reviewer-only resume: a prior run's worker stage already completed and
    /// its commits are durable on the mirror task_branch, but the run died in
    /// (or before) the reviewer stage — typically a Job-deadline / pod kill of
    /// the reviewer session. The host resolves this from `ReviewResponse` ONLY
    /// when a cheap durability check confirms the worker output is present
    /// (task_branch exists + is ahead of base), so re-running the worker would
    /// redo identical work. The sequence is `[Reviewer]`; workspace setup still
    /// clones task_branch first (so the reviewer sees the worker's diff), and
    /// the reviewer's `task_review_start` pre-stage transition is valid because
    /// the task is `needs_task_review` on the resume. When the worker output is
    /// NOT durable the host keeps `ReviewResponse` (full worker redo).
    ReviewResume,
    /// Lead intervention: a single-stage flow that runs the Lead agent on a
    /// task parked in `needs_lead_intervention`. The Lead inspects the stuck
    /// task and ends with `submit_decision`, which the supervisor maps to the
    /// terminal board transition (approve / reopen / decompose / close /
    /// escalate). Without this flow, lead-intervention tasks fell through to
    /// `NewTask` and looped worker→reviewer forever (the dead-end that wedged
    /// 82g0/78y9).
    Lead,
    /// Proposal-refinement tribunal: a single-stage flow that runs one
    /// refinement role (advocate, adversary, or judge) on a proposal. The
    /// concrete agent type is resolved from `task.agent_type` at the
    /// role-overrides layer.
    Refinement,
}

impl SupervisorFlow {
    pub fn role_sequence(self) -> &'static [RoleKind] {
        role_sequence(self)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SupervisorFlow::NewTask => "new_task",
            SupervisorFlow::ReviewResponse => "review_response",
            SupervisorFlow::ConflictRetry => "conflict_retry",
            SupervisorFlow::Spike => "spike",
            SupervisorFlow::Planning => "planning",
            SupervisorFlow::ReviewResume => "review_resume",
            SupervisorFlow::Lead => "lead",
            SupervisorFlow::Refinement => "refinement",
        }
    }
}

/// Free-function form of [`SupervisorFlow::role_sequence`] — exposed at the
/// crate root so call sites that only need the sequence can avoid pulling in
/// the full `SupervisorFlow` enum scope (matches the `lib.rs` re-export).
pub fn role_sequence(flow: SupervisorFlow) -> &'static [RoleKind] {
    use RoleKind::*;
    match flow {
        // Worker-only. The run ENDS after the worker stage: the supervisor
        // fires `submit_task_review` (in_progress → needs_task_review). The
        // coordinator re-dispatches a reviewer-only `ReviewResume` run (the
        // worker output is durable on the mirror task_branch).
        //
        // The wave-planner already broke the work down upstream, so no
        // upfront Planner stage here.
        SupervisorFlow::NewTask => &[Worker],
        // ReviewResponse (reviewer rejected / human asked for more) and
        // ConflictRetry (merge-conflict fixup) both re-enter at the worker and
        // hand off to review, exactly like NewTask — so they are also
        // worker-only.
        SupervisorFlow::ReviewResponse | SupervisorFlow::ConflictRetry => &[Worker],
        // Reviewer-only resume: the worker stage already ran on a prior run and
        // its commits are durable on the mirror task_branch (the host verified
        // this before choosing the flow). The task already moved to
        // needs_task_review. Skip straight to the reviewer, which reviews the
        // diff cloned from task_branch.
        SupervisorFlow::ReviewResume => &[Reviewer],
        SupervisorFlow::Spike => &[Architect],
        SupervisorFlow::Planning => &[Planner],
        // Single-stage: the Lead is the only actor. Its `submit_decision`
        // drives the terminal board transition (handled in the supervisor
        // body); there is no follow-on worker/reviewer in the same task-run —
        // `reopen` returns the task to `open` and the coordinator starts a
        // clean subsequent run.
        SupervisorFlow::Lead => &[Lead],
        // Single-stage refinement: the tribunal role (advocate, adversary, or
        // judge) runs once. The concrete agent type is resolved from
        // `task.agent_type` in the role-overrides layer.
        SupervisorFlow::Refinement => &[Refinement],
    }
}

/// Input to `TaskRunSupervisor::run`.
///
/// All runtime-variable data the supervisor needs to execute one task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRunSpec {
    /// The canonical task-run id, minted once by the host coordinator before
    /// `prepare`. This is the SINGLE id for the whole run: the K8s runtime
    /// derives its resource name / registry key from it, the in-pod
    /// `TaskRunSupervisor` uses it for the `task_runs` row + every session, and
    /// the terminal `TaskRunReport` carries it back. Unifying it here removes
    /// the old split where `prepare` and the supervisor each minted their own
    /// `Uuid`, leaving the host's report id pointing at a row that never
    /// existed (the bug that silently disabled post-session extraction).
    pub task_run_id: String,
    // Exact dispatch attempt identity; never inferred from mutable state.
    #[serde(default)]
    pub task_attempt_id: Option<String>,
    pub task_id: String,
    pub project_id: String,
    pub trigger: TaskRunTrigger,
    /// Existing branch in the mirror to start from (e.g. `main`).
    pub base_branch: String,
    /// Branch the task-run commits onto; created locally from `base_branch`
    /// when needed. Pushed to origin at PR-open time.
    pub task_branch: String,
    pub flow: SupervisorFlow,
    /// Optional per-role model override.  When a [`RoleKind`] key is present,
    /// `execute_stage` uses the mapped `provider/model` id for that stage
    /// instead of the catalog-default fallback.  The coordinator populates
    /// this from its per-role model resolution (dispatch priorities + project
    /// `model_preference`) so the supervisor path keeps parity with the
    /// legacy `run_task_lifecycle` model selection.  Empty = fall back to
    /// catalog-default for every stage.
    pub model_id_per_role: HashMap<RoleKind, String>,
    /// Project IDs this task-run may READ in addition to its own
    /// `project_id` (the write target). Resolved at dispatch from the
    /// task's epic `epic_read_sources`. The worker materializes each
    /// read-only alongside the primary workspace and the agent's prompt
    /// is told it may read them. Empty for tasks without read-source
    /// grants. `#[serde(default)]` keeps older specs (host/worker version
    /// skew during a rolling deploy) deserializable.
    #[serde(default)]
    pub read_source_project_ids: Vec<String>,
    /// Immutable knowledge-packing configuration resolved by the host at startup.
    #[serde(default)]
    pub knowledge_injection: djinn_core::models::KnowledgeInjectionConfig,
    /// The project's GitHub owner (org/user), used to scope private-dependency
    /// fetching in the worker Pod: `GOPRIVATE=github.com/<owner>/*` plus a git
    /// `url.insteadOf` rewrite so `go mod download` / cargo git deps / pnpm git
    /// deps authenticate to that org's private repos with the installation
    /// token below. Derived from `projects.github_owner` at dispatch — never a
    /// hardcoded org. `None`/empty disables the rewrite. `#[serde(default)]` for
    /// host/worker version skew during a rolling deploy.
    #[serde(default)]
    pub github_owner: Option<String>,
    /// Short-lived GitHub App installation token for the project's owner,
    /// minted host-side at dispatch (rotates ~hourly). Injected into the Pod's
    /// git config (`url.insteadOf`) so the agent's build/test commands can pull
    /// the org's PRIVATE transitive deps. Rides the per-task-run Secret like the
    /// rest of the spec; lives only for the Pod's lifetime.
    #[serde(default)]
    pub github_install_token: Option<String>,
    /// Git author `name` for commits the supervisor creates on the task
    /// branch. Resolved host-side at dispatch from the task's
    /// `created_by_user_id` (the human who triggered the task). `None` for
    /// older specs or host/worker version skew during a rolling deploy — the
    /// supervisor falls back to the bot identity. `#[serde(default)]` keeps older
    /// specs deserializable.
    /// identity. `#[serde(default)]` keeps older specs deserializable.
    #[serde(default)]
    pub commit_author_name: Option<String>,
    /// Git author `email` paired with [`Self::commit_author_name`]. Set to the
    /// GitHub per-user no-reply form
    /// `<github_id>+<github_login>@users.noreply.github.com`, which links the
    /// commit to that GitHub account so it's attributed to the human AND
    /// Vercel's commit-author authorization (which rejects commits whose
    /// author email matches no GitHub account) lets the deployment through.
    /// The PR itself is still opened by the App (`djinn-bot[bot]`), so the
    /// creator can review/approve their own commits.
    #[serde(default)]
    pub commit_author_email: Option<String>,
    /// Passive resume-via-git lifecycle metadata selected by the coordinator
    /// at re-dispatch time. This is a serde-only mirror of the coordinator's
    /// `ResumeLifecycleMetadata` (deliberately duplicated so the runtime/spec
    /// crate does not gain a coordinator dependency). `None` for the
    /// default/off path: when resume selection is disabled, or the task was
    /// not a re-dispatch candidate, the field stays absent and the existing
    /// dispatch behavior is preserved byte-for-byte on the wire.
    ///
    /// The field carries the full selection output: chosen source kind,
    /// checkpoint SHA or submit/review id when applicable, target ref, prior
    /// session/lineage context, and machine-readable rejected-candidate skip
    /// reasons (in the `extra.skipped` array). Downstream prompt/model/merge
    /// work (siblings `48ru`, `twsk`, `sy0g`) consume this from the
    /// task-run lifecycle payload rather than re-querying the coordinator.
    #[serde(default)]
    pub resume_lifecycle_metadata: Option<ResumeLifecycleMetadata>,
    /// Whether this task-run is a linked refinement evidence spike that must
    /// run under the read-only evidence-spike tool profile.  The worker pod
    /// reads this at stage-execution time to select
    /// `tool_schemas_evidence_spike()` and to pass `is_evidence_spike` into
    /// the reply-loop dispatch gate.  `false` (the `#[serde(default)]`
    /// sentinel) means "use the normal role tool surface".
    ///
    /// Derived at dispatch from the task's labels (`refinement-evidence` +
    /// `read-only`) via `djinn_core::models::task::is_evidence_spike`.
    #[serde(default)]
    pub is_evidence_spike: bool,
}

/// Resume-via-git lifecycle metadata selected by the coordinator at
/// re-dispatch time. This is a serde-only mirror of the coordinator's
/// `ResumeLifecycleMetadata` (see `djinn_coordinator::worker_lifecycle`). The
/// field shapes are intentionally aligned so the coordinator can serialize its
/// selection directly into a [`TaskRunSpec`] without a translation layer, and
/// the worker can deserialize the same shape when reading the spec off the
/// bincode wire.
///
/// All fields are `#[serde(default)]` so the older worker pods (running before
/// this additive change rolled out) continue to deserialize new specs
/// without an `EOF` on bincode decode. The default `None`/`false` value on
/// `considered` is the "resume selection was not consulted" signal; the
/// presence of `selection_reason` is the "a selection was made" signal.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResumeLifecycleMetadata {
    /// Additive exact identity supplied by the coordinator. Mixed-version
    /// launchers omit either/both values, which deliberately remains NULL.
    #[serde(default)]
    pub dispatch_owner_incarnation_id: Option<String>,
    #[serde(default)]
    pub dispatch_group_id: Option<String>,
    /// Whether resume selection was considered for this dispatch/session.
    #[serde(default)]
    pub considered: bool,
    /// Selected checkpoint identifier, if any.
    #[serde(default)]
    pub checkpoint_id: Option<String>,
    /// Commit SHA selected as the resume base.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Outcome of resume selection.
    #[serde(default)]
    pub selection_reason: Option<ResumeSelectionReason>,
    /// Chosen source kind (mirror of `djinn_coordinator::dispatch::resume_source::ResumeSourceKind`).
    /// `None` when the selection reason is not tied to a single source class
    /// (e.g. `MergeConflict`).
    #[serde(default)]
    pub source_kind: Option<ResumeSourceKind>,
    /// Target ref the future integration should check out. The selector only
    /// records it; the worker pod does not mutate the worktree from this
    /// task — that is `twsk`'s responsibility.
    #[serde(default)]
    pub target_ref: Option<String>,
    /// Submit/review id for accepted auto-submit candidates. `None` for
    /// checkpoint and clean-task-branch selections.
    #[serde(default)]
    pub submit_or_review_id: Option<String>,
    /// Prior session or lineage identifier that produced the chosen source.
    /// Used by the resume-prompt context (`48ru`) and as a hint for
    /// observability + decision telemetry.
    #[serde(default)]
    pub prior_session_lineage: Option<String>,
    /// Machine-readable record of every rejected candidate and its skip
    /// reason. Typed (not `serde_json::Value`) so the bincode wire format
    /// used for the worker→host frame can serialize/deserialize it.
    #[serde(default)]
    pub skipped: Vec<RejectedResumeSourceWire>,
    /// Previous model used before the termination that triggered this
    /// resume. Populated by the coordinator from model-rotation metadata
    /// when available. Used by the resume-prompt note (`48ru`) and
    /// model-rotation logic.
    #[serde(default)]
    pub previous_model: Option<String>,
    /// New/current model selected after model failover. When the
    /// coordinator's model-rotation metadata indicates a rotation from
    /// `previous_model` to a different model, this field carries the
    /// target model. Used by the resume-prompt note so the worker knows
    /// which model it is running on after failover.
    #[serde(default)]
    pub new_model: Option<String>,
    /// Human-readable failover/termination reason supplied by the
    /// coordinator. Populated from model-rotation reason metadata when
    /// available. Used by the resume-prompt note so the worker knows
    /// why the prior session was terminated and a new model was chosen.
    #[serde(default)]
    pub failover_reason: Option<String>,
    /// Last durable-progress summary from the prior session, when
    /// available. Used by the resume-prompt note to give the worker
    /// context about what was accomplished before termination.
    #[serde(default)]
    pub last_durable_progress_summary: Option<String>,
    /// Suggested verification command from the prior session's
    /// auto-submit/checkpoint metadata, when available. Used by the
    /// resume-prompt note so the worker can re-verify quickly.
    #[serde(default)]
    pub verification_command: Option<String>,
}

/// Rejected candidate + machine-readable skip reason. Typed mirror of
/// `djinn_coordinator::dispatch::resume_source::RejectedResumeSource`, sized
/// for the bincode worker→host wire frame (no `serde_json::Value` in the
/// fields). Snake-case serde names match the coordinator definition so the
/// two stay wire-compatible.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RejectedResumeSourceWire {
    #[serde(default)]
    pub kind: Option<ResumeSourceKind>,
    #[serde(default)]
    pub target_ref: Option<String>,
    #[serde(default)]
    pub checkpoint_sha: Option<String>,
    #[serde(default)]
    pub submit_or_review_id: Option<String>,
    /// Reuse [`ResumeSelectionReason`] for the bucket the candidate fell
    /// into (e.g. `CheckpointUnsafe`, `MergeConflict`, `CheckpointMissing`,
    /// `Disabled`). This is the closest stable enum the runtime surface
    /// already exposes; siblings `48ru`/`twsk` interpret the value the
    /// same way the coordinator does.
    #[serde(default)]
    pub reason: Option<ResumeSelectionReason>,
}

/// Machine-readable resume source kind chosen by the selector. Mirror of
/// `djinn_coordinator::dispatch::resume_source::ResumeSourceKind`; aligned
/// `snake_case` serde names so the two definitions are wire-compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSourceKind {
    /// Accepted auto-submit/review state from the normal submission path.
    AutoSubmit,
    /// A safety-scanned checkpoint commit on the task branch.
    TaskBranchCheckpoint,
    /// A safety-scanned checkpoint commit on an alternate checkpoint ref.
    AlternateCheckpointRef,
    /// Clean task branch fallback when no prior output can be resumed safely.
    CleanTaskBranch,
}

/// Machine-readable classification for resume checkpoint selection decisions.
/// Mirror of `djinn_coordinator::ResumeSelectionReason`; aligned `snake_case`
/// serde names so the two definitions are wire-compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSelectionReason {
    AutoSubmitAccepted,
    LatestSafeCheckpoint,
    AlternateCheckpointRef,
    CleanTaskBranchFallback,
    NewerTaskBranch,
    CheckpointMissing,
    CheckpointUnsafe,
    MergeConflict,
    Disabled,
}

/// Wire-capable classification of a stage failure that was caused by a typed
/// provider error.
///
/// The typed `djinn_provider::provider::error::ProviderError` lives only inside
/// the worker pod (the reply loop calls the provider directly there), and it is
/// NOT serde-serializable, so it cannot ride the bincode report frame back to
/// the host. The host owns the *persistent* model circuit-breaker
/// (`HealthTracker`); a fast provider rejection that produces no token stall —
/// bad credential, persistent malformed request, repeated 5xx — therefore never
/// reached the host breaker before this field existed, and dispatch kept
/// re-selecting a model that is structurally broken for that user.
///
/// `stage.rs` downcasts the reply-loop's terminal error to `ProviderError` and
/// folds it into one of these *breaker-relevant* classes; the host
/// (`supervisor_runner.rs`) maps the class back onto `record_failure` /
/// `record_stall` using the task creator's scope. Only the classes the breaker
/// actually acts on are represented — failures the breaker deliberately ignores
/// (ContextOverflow, EmptyCompletion) and untyped/legacy errors carry `None` so
/// they never trip it. A hard `Transport` death folds into `Failure` (gentle
/// consecutive-failure breaker) so a model that dies instantly on every dispatch
/// auto-disables instead of being re-selected forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderFailureClass {
    /// Auth / persistent-invalid-request / invalid-output / 5xx provider
    /// internal — a "quiet but broken" failure. Feeds the gentler
    /// consecutive-failure breaker (`record_failure`): a one-off may be
    /// transient, so it only demotes the model after it repeats.
    Failure,
    /// Rate-limit / quota (throttle). Feeds the immediate-failover breaker
    /// (`record_stall`), matching the coordinator's throttle→stall intent: a
    /// throttled credential should fail over to the next model at once with a
    /// cooldown that outlasts the task's redispatch ladder.
    ///
    /// `retry_after_ms` carries the provider-stated reset window (parsed from a
    /// `Retry-After` header / rate-limit-reset, see
    /// `ProviderError::retry_after_ms()`) when the provider supplied one. The
    /// coordinator uses it as a *floor* under the escalating redispatch cooldown
    /// (A6): a multi-hour quota window must not be probed on the fixed ~30-min
    /// ladder. `None` when the provider stated no reset, in which case the
    /// ordinary ladder applies. `#[serde(default)]` keeps it additive over the
    /// bincode wire (positional, non-self-describing): a version-matched worker
    /// is required for the field to decode, exactly as documented on the enum.
    Throttle {
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
    /// Auth / credential failure (401/403 — e.g. a revoked or invalid OAuth
    /// token). Deterministic, not transient: a retry with the same dead
    /// credential always fails. The host trips the breaker IMMEDIATELY (like a
    /// throttle, via `record_stall`) so dispatch fails over to the user's next
    /// model at once instead of probing the dead one three times, AND drives
    /// credential-revocation surfacing (mark the credential revoked + notify the
    /// owner to reconnect). Added last to preserve the bincode discriminants of
    /// `Failure`/`Throttle` on the worker→host report wire.
    AuthInvalid,
}

/// Terminal outcome of a task-run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskRunOutcome {
    PrOpened {
        url: String,
        sha: String,
    },
    /// Planner decided the task should not execute.
    Closed {
        reason: String,
    },
    /// Planner/architect surfaced a question that blocks automated execution
    /// (e.g. ambiguous scope, missing design decision).
    Escalated {
        reason: String,
    },
    Failed {
        stage: String,
        reason: String,
        /// Set when the failure was a typed provider error the host breaker
        /// should act on (see [`ProviderFailureClass`]). `None` for non-LLM
        /// failures (git push, PR open) and for provider errors the breaker
        /// deliberately ignores. `#[serde(default)]` keeps serde formats that can
        /// omit fields decoding this as `None`.
        #[serde(default)]
        provider_failure: Option<ProviderFailureClass>,
        /// Machine-readable class for structured tool/provider-write failures.
        ///
        /// Keep these additive fields serialized even when `None`: the
        /// worker→host report wire uses bincode, whose struct fields are
        /// positional rather than self-describing. `skip_serializing_if` would
        /// omit `None` bytes and make same-version bincode decoding hit EOF.
        #[serde(default)]
        error_class: Option<ErrorClass>,
        /// Actionable recovery hint for the agent/operator, when available.
        #[serde(default)]
        hint: Option<String>,
        /// Bounded upstream response/detail excerpt for compact rendering.
        #[serde(default)]
        body_excerpt: Option<String>,
    },
    Interrupted,
    /// The worker stage completed and the supervisor fired `submit_task_review`
    /// (in_progress → needs_task_review). The run ends here — no PR is opened.
    /// The coordinator's next dispatch pass picks up the `needs_task_review`
    /// task and runs the reviewer. This is the terminal outcome of every
    /// worker-only flow (NewTask / ReviewResponse / ConflictRetry); it maps to a
    /// `Completed` task-run status (the worker stage genuinely succeeded) so it
    /// feeds `record_success` and never trips the model breaker.
    ///
    /// Added LAST to preserve the bincode discriminants of the existing variants
    /// on the worker→host `TerminalReport` wire (bincode is positional/
    /// non-self-describing, so reordering would break cross-version decoding
    /// during a rolling deploy — same rationale as `ProviderFailureClass::AuthInvalid`).
    WorkerSubmitted,
    /// Reply-loop guard terminated a degenerate session before the task could
    /// make progress. Added LAST to preserve bincode discriminants of all
    /// existing worker→host terminal-report variants.
    LoopGuardTripped {
        kind: LoopGuardKind,
        offending_signature: String,
        threshold: u32,
        observed: u32,
        turn_span: (u32, u32),
        session_id: String,
    },
    /// The worker deliberately parked after budget wind-down. Added LAST to
    /// preserve bincode discriminants of all existing terminal-report variants.
    Parked {
        reason: String,
        wind_down_ignored: bool,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
    },
    /// A blocking pre-task command failed, timed out, or was cancelled, or a
    /// required service readiness check failed before any agent session or work
    /// attempt was created.  The run is classified as an environmental
    /// non-attempt: no `TaskRunSupervisor::run` was invoked, no agent
    /// session/work attempt exists, and no quality strike, arbiter penalty, or
    /// park-rung penalty should be applied.
    ///
    /// `stages_completed` is always empty for this outcome.
    ///
    /// Added LAST to preserve bincode discriminants of all existing
    /// terminal-report variants.
    EnvironmentalNonAttempt {
        /// Machine-readable reason: `pre_task_failed`, `pre_task_timed_out`,
        /// `pre_task_cancelled`, or `service_readiness_failed`.
        reason: String,
    },
}

/// Return value of `TaskRunSupervisor::run`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRunReport {
    pub task_run_id: String,
    pub outcome: TaskRunOutcome,
    pub stages_completed: Vec<RoleKind>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_flow_is_worker_only() {
        // Planner ran upstream as a Planning task; NewTask is the worker's
        // domain and doesn't re-plan. The reviewer leg no longer rides this
        // run: the worker submits to task review, and
        // a passing task review re-dispatches a reviewer-only ReviewResume.
        let seq = SupervisorFlow::NewTask.role_sequence();
        assert!(!seq.contains(&RoleKind::Planner));
        assert!(!seq.contains(&RoleKind::Reviewer));
        assert_eq!(seq, &[RoleKind::Worker]);
    }

    #[test]
    fn review_response_and_conflict_retry_are_worker_only() {
        // Both re-enter at the worker and must verify before the next review,
        // so neither carries a reviewer stage anymore.
        assert_eq!(
            SupervisorFlow::ReviewResponse.role_sequence(),
            &[RoleKind::Worker]
        );
        assert_eq!(
            SupervisorFlow::ConflictRetry.role_sequence(),
            &[RoleKind::Worker]
        );
    }

    #[test]
    fn review_resume_is_reviewer_only() {
        // The reviewer leg arrives exclusively via the ReviewResume path now;
        // ReviewResume stays reviewer-only.
        assert_eq!(
            SupervisorFlow::ReviewResume.role_sequence(),
            &[RoleKind::Reviewer]
        );
    }

    #[test]
    fn spike_flow_is_architect_only() {
        assert_eq!(
            SupervisorFlow::Spike.role_sequence(),
            &[RoleKind::Architect]
        );
    }

    #[test]
    fn review_response_skips_planner() {
        let seq = SupervisorFlow::ReviewResponse.role_sequence();
        assert!(!seq.contains(&RoleKind::Planner));
        assert!(seq.contains(&RoleKind::Worker));
    }

    #[test]
    fn worker_submitted_outcome_bincode_roundtrip() {
        // The terminal worker outcome (hand-off to review) must survive the
        // worker→host bincode frame.
        let report = TaskRunReport {
            task_run_id: "run-ws".to_string(),
            outcome: TaskRunOutcome::WorkerSubmitted,
            stages_completed: vec![RoleKind::Worker],
        };
        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back.outcome, TaskRunOutcome::WorkerSubmitted));
        assert_eq!(back.stages_completed, vec![RoleKind::Worker]);
    }

    #[test]
    fn task_run_spec_bincode_roundtrip() {
        let mut per_role = HashMap::new();
        per_role.insert(RoleKind::Planner, "anthropic/claude-sonnet-4.5".to_string());
        per_role.insert(RoleKind::Worker, "anthropic/claude-opus-4.7".to_string());

        let spec = TaskRunSpec {
            task_run_id: "019e6a03-8aef-7201-9c9d-d7ba17613a0b".to_string(),
            task_attempt_id: None,
            task_id: "task-abc".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-abc".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: per_role,
            read_source_project_ids: vec!["proj-read-1".to_string()],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig {
                knowledge_injection_budget_bytes: 4_096,
                knowledge_injection_line_cap_bytes: 256,
                knowledge_injection_limit: 3,
                injection_starvation_threshold_percent: 50,
                injection_starvation_query_floor: 20,
                retrieval_health_window_minutes: 1_440,
            },
            github_owner: None,
            github_install_token: None,
            commit_author_name: Some("Ada Lovelace".to_string()),
            commit_author_email: Some("1+ada@users.noreply.github.com".to_string()),
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.task_run_id, spec.task_run_id);
        assert_eq!(back.task_id, spec.task_id);
        assert_eq!(back.project_id, spec.project_id);
        assert_eq!(back.trigger, spec.trigger);
        assert_eq!(back.base_branch, spec.base_branch);
        assert_eq!(back.task_branch, spec.task_branch);
        assert_eq!(back.read_source_project_ids, spec.read_source_project_ids);
        assert_eq!(back.flow, spec.flow);
        assert_eq!(back.model_id_per_role, spec.model_id_per_role);
        assert_eq!(back.knowledge_injection, spec.knowledge_injection);
        assert_eq!(back.commit_author_name, spec.commit_author_name);
        assert_eq!(back.commit_author_email, spec.commit_author_email);
    }

    /// AC: "Selection metadata attached to the dispatch/session lifecycle path
    /// includes chosen source kind, checkpoint SHA or submit/review id when
    /// applicable, target ref, prior session/lineage context, and
    /// rejected-candidate skip reasons." A `TaskRunSpec` with a populated
    /// `resume_lifecycle_metadata` must round-trip through bincode (the
    /// worker→host wire format) and surface all selection fields so the
    /// downstream prompt/model/merge work in siblings `48ru`/`twsk`/`sy0g`
    /// can read the chosen source, the target ref, the checkpoint SHA (or
    /// submit/review id), the prior session lineage, and the rejected-
    /// candidate skip reasons — every field needed for the integration
    /// without dropping the metadata on the wire.
    #[test]
    fn task_run_spec_carries_full_resume_lifecycle_metadata_through_bincode() {
        let mut per_role = HashMap::new();
        per_role.insert(RoleKind::Worker, "anthropic/claude-opus-4.7".to_string());

        let spec = TaskRunSpec {
            task_run_id: "019f1a03-8aef-7201-9c9d-d7ba17613a0b".to_string(),
            task_attempt_id: None,
            task_id: "task-resume".to_string(),
            project_id: "proj-xyz".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-resume".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: per_role,
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: Some(ResumeLifecycleMetadata {
                dispatch_owner_incarnation_id: Some(
                    "00000000-0000-7000-8000-000000000001".to_string(),
                ),
                dispatch_group_id: Some("00000000-0000-7000-8000-000000000002".to_string()),
                considered: true,
                checkpoint_id: Some("ckpt-1".to_string()),
                commit_sha: Some("deadbeef".to_string()),
                selection_reason: Some(ResumeSelectionReason::LatestSafeCheckpoint),
                source_kind: Some(ResumeSourceKind::TaskBranchCheckpoint),
                target_ref: Some("refs/heads/task/resume-target".to_string()),
                submit_or_review_id: Some("review-7".to_string()),
                prior_session_lineage: Some("session-prev".to_string()),
                skipped: vec![RejectedResumeSourceWire {
                    kind: Some(ResumeSourceKind::AutoSubmit),
                    target_ref: Some("refs/heads/task/resume-target".to_string()),
                    checkpoint_sha: None,
                    submit_or_review_id: Some("review-7".to_string()),
                    reason: Some(ResumeSelectionReason::CheckpointUnsafe),
                }],
                previous_model: Some("anthropic/claude-opus-4.7".to_string()),
                new_model: Some("openai/gpt-4.1".to_string()),
                failover_reason: Some("no_durable_progress_streak".to_string()),
                last_durable_progress_summary: Some("Implemented core feature".to_string()),
                verification_command: Some("cargo test".to_string()),
            }),
            is_evidence_spike: false,
        };

        // Bincode round-trip (the worker→host wire format).
        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        let meta = back
            .resume_lifecycle_metadata
            .as_ref()
            .expect("resume_lifecycle_metadata must round-trip through bincode");
        assert!(meta.considered, "considered flag must survive the wire");
        assert_eq!(meta.checkpoint_id.as_deref(), Some("ckpt-1"));
        assert_eq!(meta.commit_sha.as_deref(), Some("deadbeef"));
        assert_eq!(
            meta.selection_reason,
            Some(ResumeSelectionReason::LatestSafeCheckpoint)
        );
        // Every machine-readable selection field the AC requires must
        // be present after the round-trip.
        assert_eq!(
            meta.source_kind,
            Some(ResumeSourceKind::TaskBranchCheckpoint),
            "chosen source kind must reach the spec"
        );
        assert_eq!(
            meta.target_ref.as_deref(),
            Some("refs/heads/task/resume-target"),
            "target ref must reach the spec"
        );
        assert_eq!(
            meta.submit_or_review_id.as_deref(),
            Some("review-7"),
            "submit/review id must reach the spec when applicable"
        );
        assert_eq!(
            meta.prior_session_lineage.as_deref(),
            Some("session-prev"),
            "prior session/lineage context must reach the spec"
        );
        assert_eq!(
            meta.skipped.len(),
            1,
            "rejected-candidate skip reasons must reach the spec"
        );
        assert_eq!(
            meta.skipped[0].kind,
            Some(ResumeSourceKind::AutoSubmit),
            "rejected-candidate kind must reach the spec"
        );
        assert_eq!(
            meta.skipped[0].reason,
            Some(ResumeSelectionReason::CheckpointUnsafe),
            "rejected-candidate skip reason must reach the spec"
        );
        assert_eq!(
            meta.new_model.as_deref(),
            Some("openai/gpt-4.1"),
            "failover target model must reach the spec"
        );
        assert_eq!(
            meta.failover_reason.as_deref(),
            Some("no_durable_progress_streak"),
            "failover reason must reach the spec"
        );
    }

    /// Disabled / no-resume path: when the coordinator did not select a
    /// resume source, the spec keeps `resume_lifecycle_metadata` as `None`
    /// and the legacy default/off dispatch behavior is preserved byte-for-
    /// byte on the bincode wire.
    #[test]
    fn task_run_spec_default_off_has_no_resume_lifecycle_metadata() {
        let spec = TaskRunSpec {
            task_run_id: "run-default".to_string(),
            task_attempt_id: None,
            task_id: "task-default".to_string(),
            project_id: "proj-1".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-default".to_string(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");

        assert!(
            back.resume_lifecycle_metadata.is_none(),
            "default/off dispatch must not inject a resume metadata payload"
        );
    }

    #[test]
    fn task_run_spec_evidence_spike_flag_roundtrips_through_bincode() {
        let mut spec = TaskRunSpec {
            task_run_id: "run-ev".to_string(),
            task_attempt_id: None,
            task_id: "task-ev".to_string(),
            project_id: "proj-1".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-ev".to_string(),
            flow: SupervisorFlow::Spike,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: true,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");
        assert!(
            back.is_evidence_spike,
            "is_evidence_spike must survive bincode round-trip"
        );

        // Default value is false for normal tasks.
        spec.is_evidence_spike = false;
        let bytes2 = bincode::serialize(&spec).expect("serialize");
        let back2: TaskRunSpec = bincode::deserialize(&bytes2).expect("deserialize");
        assert!(
            !back2.is_evidence_spike,
            "non-evidence-spike spec must round-trip as false"
        );
    }

    #[test]
    fn task_run_spec_from_demand_evidence_contract_selects_evidence_spike_profile() {
        // Simulate the TaskRunSpec built for a spike created by the
        // `proposal_refinement_demand_evidence` contract (labels include
        // "refinement-evidence" and "read-only"). The selector in
        // djinn-core::is_evidence_spike must set the flag, and the runtime
        // spec must carry it through serialization.
        let mut spec = TaskRunSpec {
            task_run_id: "run-evidence".to_string(),
            task_attempt_id: None,
            task_id: "task-evidence".to_string(),
            project_id: "proj-1".to_string(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".to_string(),
            task_branch: "djinn/task-evidence".to_string(),
            flow: SupervisorFlow::Spike,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: true,
        };

        let bytes = bincode::serialize(&spec).expect("serialize");
        let back: TaskRunSpec = bincode::deserialize(&bytes).expect("deserialize");
        assert!(
            back.is_evidence_spike,
            "demand-evidence contract spike must carry the evidence-spike profile through the runtime spec"
        );

        // A normal Architect spike without the read-only marker must round-trip
        // with the flag unset.
        spec.is_evidence_spike = false;
        let bytes2 = bincode::serialize(&spec).expect("serialize");
        let back2: TaskRunSpec = bincode::deserialize(&bytes2).expect("deserialize");
        assert!(
            !back2.is_evidence_spike,
            "ordinary Architect spike must not be downgraded to evidence-spike profile"
        );
    }

    #[test]
    fn task_run_report_bincode_roundtrip() {
        let report = TaskRunReport {
            task_run_id: "run-1".to_string(),
            outcome: TaskRunOutcome::PrOpened {
                url: "https://github.com/o/r/pull/1".to_string(),
                sha: "deadbeef".to_string(),
            },
            stages_completed: vec![RoleKind::Planner, RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.task_run_id, report.task_run_id);
        assert_eq!(back.stages_completed, report.stages_completed);
        match back.outcome {
            TaskRunOutcome::PrOpened { url, sha } => {
                assert_eq!(url, "https://github.com/o/r/pull/1");
                assert_eq!(sha, "deadbeef");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn failed_outcome_throttle_with_retry_after_bincode_roundtrip() {
        // A6: the `Throttle { retry_after_ms }` shape must survive the bincode
        // wire so the host coordinator can floor the redispatch cooldown on a
        // provider-stated reset.
        let report = TaskRunReport {
            task_run_id: "run-2".to_string(),
            outcome: TaskRunOutcome::Failed {
                stage: "worker".to_string(),
                reason: "rate limited".to_string(),
                provider_failure: Some(ProviderFailureClass::Throttle {
                    retry_after_ms: Some(5 * 60 * 60 * 1000),
                }),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            stages_completed: vec![RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        match back.outcome {
            TaskRunOutcome::Failed {
                stage,
                reason,
                provider_failure,
                ..
            } => {
                assert_eq!(stage, "worker");
                assert_eq!(reason, "rate limited");
                assert_eq!(
                    provider_failure,
                    Some(ProviderFailureClass::Throttle {
                        retry_after_ms: Some(5 * 60 * 60 * 1000)
                    })
                );
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn failed_outcome_throttle_without_retry_after_bincode_roundtrip() {
        // The provider may state no reset; the field is then `None` and the
        // ordinary escalating ladder applies.
        let report = TaskRunReport {
            task_run_id: "run-3".to_string(),
            outcome: TaskRunOutcome::Failed {
                stage: "worker".to_string(),
                reason: "rate limited".to_string(),
                provider_failure: Some(ProviderFailureClass::Throttle {
                    retry_after_ms: None,
                }),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            stages_completed: vec![RoleKind::Worker],
        };

        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

        match back.outcome {
            TaskRunOutcome::Failed {
                provider_failure, ..
            } => assert_eq!(
                provider_failure,
                Some(ProviderFailureClass::Throttle {
                    retry_after_ms: None
                })
            ),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    fn loop_guard_outcome(kind: LoopGuardKind) -> TaskRunOutcome {
        TaskRunOutcome::LoopGuardTripped {
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
    fn loop_guard_outcome_bincode_roundtrip_for_each_kind() {
        for kind in [
            LoopGuardKind::IdenticalToolFailure,
            LoopGuardKind::PermissionDenial,
            LoopGuardKind::IdenticalOutput,
            LoopGuardKind::ConsecutiveFailures,
        ] {
            let report = TaskRunReport {
                task_run_id: "run-loop-guard".to_string(),
                outcome: loop_guard_outcome(kind),
                stages_completed: vec![RoleKind::Worker],
            };

            let bytes = bincode::serialize(&report).expect("serialize");
            let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");

            match back.outcome {
                TaskRunOutcome::LoopGuardTripped {
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
    fn task_run_outcome_bincode_discriminants_keep_existing_variants_stable() {
        let old_variants = [
            TaskRunOutcome::PrOpened {
                url: "https://example.test/pr/1".to_string(),
                sha: "abc123".to_string(),
            },
            TaskRunOutcome::Closed {
                reason: "done".to_string(),
            },
            TaskRunOutcome::Escalated {
                reason: "blocked".to_string(),
            },
            TaskRunOutcome::Failed {
                stage: "worker".to_string(),
                reason: "boom".to_string(),
                provider_failure: None,
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            TaskRunOutcome::Interrupted,
            TaskRunOutcome::WorkerSubmitted,
        ];

        for (expected_discriminant, outcome) in old_variants.into_iter().enumerate() {
            let bytes = bincode::serialize(&outcome).expect("serialize old variant");
            assert_eq!(
                &bytes[..4],
                &(expected_discriminant as u32).to_le_bytes(),
                "existing variant discriminant shifted for {outcome:?}"
            );
            let decoded: TaskRunOutcome = bincode::deserialize(&bytes).expect("decode old frame");
            assert_eq!(
                std::mem::discriminant(&decoded),
                std::mem::discriminant(&outcome)
            );
        }

        let new_bytes =
            bincode::serialize(&loop_guard_outcome(LoopGuardKind::IdenticalToolFailure))
                .expect("serialize new variant");
        assert_eq!(&new_bytes[..4], &6u32.to_le_bytes());
    }

    #[test]
    fn environmental_non_attempt_bincode_roundtrip() {
        let report = TaskRunReport {
            task_run_id: "run-env".to_string(),
            outcome: TaskRunOutcome::EnvironmentalNonAttempt {
                reason: "pre_task_failed".to_string(),
            },
            stages_completed: Vec::new(),
        };
        let bytes = bincode::serialize(&report).expect("serialize");
        let back: TaskRunReport = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.task_run_id, "run-env");
        assert!(back.stages_completed.is_empty());
        match back.outcome {
            TaskRunOutcome::EnvironmentalNonAttempt { reason } => {
                assert_eq!(reason, "pre_task_failed");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn environmental_non_attempt_discriminant_is_appended_after_parked() {
        // Parked is discriminant 7; EnvironmentalNonAttempt must be 8.
        let parked = TaskRunOutcome::Parked {
            reason: "budget".into(),
            wind_down_ignored: false,
            session_id: "s".into(),
            tokens_in: 0,
            tokens_out: 0,
        };
        let parked_bytes = bincode::serialize(&parked).expect("serialize parked");
        assert_eq!(&parked_bytes[..4], &7u32.to_le_bytes());

        let env = TaskRunOutcome::EnvironmentalNonAttempt {
            reason: "pre_task_failed".into(),
        };
        let env_bytes = bincode::serialize(&env).expect("serialize env");
        assert_eq!(&env_bytes[..4], &8u32.to_le_bytes());
    }
}

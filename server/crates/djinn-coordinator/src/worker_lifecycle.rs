// djinn:allow-oversize — lifecycle configuration DTOs and regression coverage exceed the size guard; split when touched substantively.
use serde::{Deserialize, Serialize};

/// Coordinator-side durable-progress lifecycle configuration.
///
/// These DTOs are intentionally passive: they give rollout, threshold, and
/// metadata payloads stable serde shapes for downstream detector/event wiring.
/// Defaults preserve current worker behavior: durable-progress detection is
/// shadow-only, no-progress enforcement is disabled, and no forced checkpoint,
/// auto-submit, resume, or model-rotation action is requested.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkerLifecycleConfig {
    /// Rollout switches for detector observation and destructive enforcement.
    #[serde(default)]
    pub rollout: DurableProgressRolloutConfig,
    /// Thresholds used to classify no-progress streaks when observation is on.
    #[serde(default)]
    pub no_progress_thresholds: NoProgressThresholdConfig,
    /// Placeholder config for sibling checkpoint-preservation work.
    #[serde(default)]
    pub checkpoint: CheckpointLifecycleConfig,
    /// Placeholder config for sibling auto-submit-if-green work.
    #[serde(default)]
    pub auto_submit: AutoSubmitLifecycleConfig,
    /// Placeholder config for sibling resume-via-git work.
    #[serde(default)]
    pub resume: ResumeLifecycleConfig,
    /// Placeholder config for future model-rotation enforcement.
    #[serde(default)]
    pub model_rotation: ModelRotationLifecycleConfig,
    /// Slow-verdict claim extension configuration. When the liveness
    /// classifier produces a `Slow` verdict for a stalled session and
    /// the hard runtime cap is not exceeded, the coordinator extends the
    /// claim by the configured quantum instead of killing the session.
    #[serde(default)]
    pub slow_extension: SlowExtensionConfig,
}

/// Rollout switches for durable-progress observation and no-progress action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableProgressRolloutConfig {
    /// Detector rollout state. Defaults to shadow mode so observations can be
    /// emitted without changing worker lifecycle behavior.
    #[serde(default = "default_durable_progress_detection_mode")]
    pub detection_mode: DurableProgressDetectionMode,
    /// No-progress enforcement rollout state. Defaults to disabled; even if the
    /// detector observes a terminal streak, workers are not forced to exit.
    #[serde(default)]
    pub no_progress_enforcement: NoProgressEnforcementMode,
    /// Gate for preservation checkpoint creation before a no-progress exit.
    /// Defaults false because checkpoint mechanics are owned by a sibling epic.
    #[serde(default)]
    pub checkpoint_before_no_progress_exit: bool,
    /// Gate for auto-submit-if-green execution. Defaults false; this task only
    /// defines the DTO shape consumed by sibling auto-submit work.
    #[serde(default)]
    pub auto_submit_if_green: bool,
    /// Gate for resume selection from preserved checkpoints. Defaults false;
    /// resume-via-git mechanics are owned by a sibling epic.
    #[serde(default)]
    pub resume_from_checkpoint: bool,
    /// Gate for changing models/providers after no-progress classification.
    /// Defaults false so existing model selection behavior is preserved.
    #[serde(default)]
    pub rotate_model_on_no_progress: bool,
}

impl Default for DurableProgressRolloutConfig {
    fn default() -> Self {
        Self {
            detection_mode: DurableProgressDetectionMode::Shadow,
            no_progress_enforcement: NoProgressEnforcementMode::Disabled,
            checkpoint_before_no_progress_exit: false,
            auto_submit_if_green: false,
            resume_from_checkpoint: false,
            rotate_model_on_no_progress: false,
        }
    }
}

/// Durable-progress detector rollout state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableProgressDetectionMode {
    /// Do not run or emit detector observations.
    Off,
    /// Run detector and publish observations only; do not affect worker control
    /// flow. This is the default for calibration.
    Shadow,
    /// Detector output is eligible for policy decisions once downstream safety
    /// mechanisms have been enabled.
    Enforce,
}

fn default_durable_progress_detection_mode() -> DurableProgressDetectionMode {
    DurableProgressDetectionMode::Shadow
}

/// No-progress enforcement rollout state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoProgressEnforcementMode {
    /// Never force worker exit for no-progress streaks.
    #[default]
    Disabled,
    /// Evaluate thresholds and emit would-have-enforced observations only.
    Shadow,
    /// Enforcement may force lifecycle action when all preservation gates pass.
    Enforce,
}

/// Thresholds used by durable-progress/no-progress evaluators.
///
/// Optional destructive-action thresholds default to `None`, so a missing config
/// cannot introduce forced exits or model rotation. Numeric observation defaults
/// are deliberately generous and are only meaningful once a detector is wired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoProgressThresholdConfig {
    /// Minimum evaluated turns before a no-progress streak can be classified.
    #[serde(default = "default_no_progress_min_evaluated_turns")]
    pub min_evaluated_turns: u32,
    /// Consecutive no-progress turns that should produce a warning/shadow event.
    #[serde(default = "default_no_progress_warning_turns")]
    pub warning_turns: u32,
    /// Consecutive no-progress turns after which rotation may be considered.
    /// Defaults to `None` so model rotation is not requested by default.
    #[serde(default)]
    pub model_rotation_turns: Option<u32>,
    /// Consecutive no-progress turns after which an exit may be considered.
    /// Defaults to `None` so workers are never forced out by default.
    #[serde(default)]
    pub forced_exit_turns: Option<u32>,
    /// Long-running command duration at which no-progress streak evaluation is
    /// suspended rather than counted as a non-progress turn.
    #[serde(default = "default_long_command_suspension_secs")]
    pub long_command_suspension_secs: u64,
    /// Consecutive flaky command observations tolerated before they stop being
    /// treated as inconclusive.
    #[serde(default = "default_flaky_command_grace_turns")]
    pub flaky_command_grace_turns: u32,
}

impl Default for NoProgressThresholdConfig {
    fn default() -> Self {
        Self {
            min_evaluated_turns: default_no_progress_min_evaluated_turns(),
            warning_turns: default_no_progress_warning_turns(),
            model_rotation_turns: None,
            forced_exit_turns: None,
            long_command_suspension_secs: default_long_command_suspension_secs(),
            flaky_command_grace_turns: default_flaky_command_grace_turns(),
        }
    }
}

fn default_no_progress_min_evaluated_turns() -> u32 {
    3
}
fn default_no_progress_warning_turns() -> u32 {
    10
}
fn default_long_command_suspension_secs() -> u64 {
    10 * 60
}
fn default_flaky_command_grace_turns() -> u32 {
    2
}

/// Why a no-progress streak was reset by a durable-progress observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableProgressResetReason {
    /// Tracked source/worktree changes were introduced.
    WorktreeChanged,
    /// The diff meaningfully moved closer to the requested task outcome.
    DiffImproved,
    /// A previously failing or unknown verification command became green.
    NewlyGreenVerification,
    /// Task metadata, acceptance criteria, or review state advanced.
    TaskStateAdvanced,
    /// Worker created a preservation checkpoint that future sessions can resume.
    CheckpointCreated,
    /// Worker submitted or otherwise completed the task.
    Submitted,
    /// A human/operator intervention reset lifecycle accounting.
    OperatorIntervention,
}

/// Why an evaluated turn did not reset durable-progress streak accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableProgressNoResetReason {
    /// Tool calls were read-only or did not change durable task state.
    ReadOnlyOrNoOpToolSuccess,
    /// Only ignored/generated paths changed.
    GeneratedOnlyChange,
    /// A command reran successfully but was already green before the turn.
    AlreadyGreenVerificationRerun,
    /// A command result is flaky/inconclusive and should not reset the streak.
    FlakyCommandResult,
    /// A long-running command suspended evaluation for this turn.
    LongCommandSuspended,
    /// The detector could not compare before/after state for this turn.
    SnapshotUnavailable,
}

/// Checkpoint metadata placeholder populated by preservation/checkpoint epics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckpointLifecycleMetadata {
    /// Stable checkpoint identifier, if one has been created.
    #[serde(default)]
    pub checkpoint_id: Option<String>,
    /// Git commit SHA containing the checkpointed worker output.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Branch or ref where the checkpoint commit was written.
    #[serde(default)]
    pub ref_name: Option<String>,
    /// Reason checkpoint creation was requested.
    #[serde(default)]
    pub requested_for: Option<CheckpointRequestReason>,
    /// Whether downstream safety scanning accepted the checkpoint.
    #[serde(default)]
    pub safety_scan: Option<CheckpointSafetyScanMetadata>,
    /// Coordinator-side preservation gate outcome. Populated when the
    /// coordinator requests preservation before a terminal failed/escalated/
    /// reap-adjacent session termination. `None` means no preservation gate
    /// was attempted (pre-contract behaviour or non-applicable path).
    #[serde(default)]
    pub preservation_outcome: Option<PreservationOutcome>,
    /// Free-form extension map for rollout-specific checkpoint details.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Coordinator-side classification of a checkpoint preservation attempt before
/// a terminal failed/escalated/reap-adjacent session state transition.
///
/// Variants use stable `snake_case` serde names so downstream consumers
/// (activity logs, metrics labels, lifecycle metadata JSON) share a single
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreservationOutcome {
    /// Preservation request was sent to the worker/runtime and the worker
    /// reported success. The coordinator did not verify the push/commit
    /// itself — it trusts the worker's result.
    Succeeded,
    /// Preservation was requested but the worker reported a failure.
    /// The configured failure-policy result was recorded; the terminal
    /// transition proceeds without blocking on retry.
    Failed,
    /// No live worker connection was available to service the request.
    /// The coordinator recorded an explicit "no worker" result rather than
    /// silently discarding potentially dirty output.
    UnavailableWorker,
    /// No runtime/RPC infrastructure exists in this environment (e.g.
    /// dev/test mode without Kubernetes or a live coordinator).
    RuntimeUnavailable,
    /// A clean/no-op skip: the coordinator determined that no preservation
    /// attempt was necessary (e.g. the session had zero tokens or was
    /// already finalized before the gate ran).
    CleanSkip,
}

impl PreservationOutcome {
    /// Returns whether this outcome should block the terminal transition
    /// given the configured failure policy.
    ///
    /// - `Succeeded` and `CleanSkip` never block — preservation succeeded
    ///   or was unnecessary.
    /// - `Failed`, `UnavailableWorker`, and `RuntimeUnavailable` block only
    ///   when the policy is [`PreservationFailurePolicy::Block`].
    ///   With the default [`PreservationFailurePolicy::RecordAndProceed`]
    ///   policy the failure is recorded as an explicit policy result and
    ///   the transition proceeds.
    pub fn should_block_transition(&self, policy: PreservationFailurePolicy) -> bool {
        match self {
            PreservationOutcome::Succeeded | PreservationOutcome::CleanSkip => false,
            PreservationOutcome::Failed
            | PreservationOutcome::UnavailableWorker
            | PreservationOutcome::RuntimeUnavailable => {
                matches!(policy, PreservationFailurePolicy::Block)
            }
        }
    }
}

/// Policy for handling preservation failures before terminal transitions.
///
/// When a preservation attempt does not succeed (outcome is `Failed`,
/// `UnavailableWorker`, or `RuntimeUnavailable`), this policy determines
/// whether the terminal transition proceeds or is blocked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreservationFailurePolicy {
    /// Record the failure as an explicit failure-policy result and proceed
    /// with the terminal transition. This is the default — the preservation
    /// gate recorded the outcome (for observability and future enforcement),
    /// but does not block the transition.
    #[default]
    RecordAndProceed,
    /// Block the terminal transition when preservation fails. The transition
    /// will be retried on the next coordinator tick, giving the worker
    /// another chance to preserve output. Used by y8pv enforcement once
    /// the worker-side RPC is fully wired.
    Block,
}

/// Structured result of a coordinator-side preservation gate attempt.
///
/// This is the return type of the internal `request_session_preservation`
/// helper and is not persisted as a standalone row — it is logged as an
/// activity entry and attached to [`CheckpointLifecycleMetadata`] via the
/// `preservation_outcome` field.
#[derive(Debug, Clone)]
pub struct PreservationGateResult {
    /// The classification of this preservation attempt.
    pub outcome: PreservationOutcome,
    /// Human-readable explanation for the outcome (e.g. "no live worker",
    /// "worker reported success", "runtime_ops unavailable").
    pub reason: String,
    /// Checkpoint commit SHA if the worker reported one, or `None` if
    /// preservation was skipped/failed.
    pub commit_sha: Option<String>,
    /// Checkpoint ref if the worker reported one, or `None`.
    pub ref_name: Option<String>,
    /// The task-run id that was the target of the preservation request, if
    /// known. Helps downstream consumers correlate with Kubernetes jobs.
    pub task_run_id: Option<String>,
    /// The session id targeted by the preservation request.
    pub session_id: String,
    /// The task id owning this session.
    pub task_id: String,
    /// Why preservation was requested (stall, zombie, terminal fail, etc.)
    pub trigger: &'static str,
}

impl PreservationGateResult {
    /// Build a result for the "runtime unavailable" case (no runtime_ops).
    pub fn runtime_unavailable(task_id: &str, session_id: &str, trigger: &'static str) -> Self {
        Self {
            outcome: PreservationOutcome::RuntimeUnavailable,
            reason: "runtime_ops not configured".to_string(),
            commit_sha: None,
            ref_name: None,
            task_run_id: None,
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            trigger,
        }
    }

    /// Build a result for the "no live worker" case.
    pub fn unavailable_worker(
        task_id: &str,
        session_id: &str,
        task_run_id: Option<&str>,
        trigger: &'static str,
    ) -> Self {
        Self {
            outcome: PreservationOutcome::UnavailableWorker,
            reason: "no live worker connection".to_string(),
            commit_sha: None,
            ref_name: None,
            task_run_id: task_run_id.map(str::to_owned),
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            trigger,
        }
    }

    /// Build a result for the "clean skip" case (no dirty output to preserve).
    pub fn clean_skip(
        task_id: &str,
        session_id: &str,
        trigger: &'static str,
        reason: &str,
    ) -> Self {
        Self {
            outcome: PreservationOutcome::CleanSkip,
            reason: reason.to_owned(),
            commit_sha: None,
            ref_name: None,
            task_run_id: None,
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            trigger,
        }
    }

    /// Build a result for the "worker reported success" case.
    pub fn succeeded(
        task_id: &str,
        session_id: &str,
        task_run_id: Option<&str>,
        trigger: &'static str,
        commit_sha: Option<String>,
        ref_name: Option<String>,
    ) -> Self {
        Self {
            outcome: PreservationOutcome::Succeeded,
            reason: "worker reported checkpoint success".to_string(),
            commit_sha,
            ref_name,
            task_run_id: task_run_id.map(str::to_owned),
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            trigger,
        }
    }

    /// Build a result for the "worker reported failure" case.
    pub fn failed(
        task_id: &str,
        session_id: &str,
        task_run_id: Option<&str>,
        trigger: &'static str,
        reason: String,
    ) -> Self {
        Self {
            outcome: PreservationOutcome::Failed,
            reason,
            commit_sha: None,
            ref_name: None,
            task_run_id: task_run_id.map(str::to_owned),
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            trigger,
        }
    }
}

/// Passive checkpoint rollout config; defaults do not request checkpoint writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointLifecycleConfig {
    /// Whether checkpoint creation is allowed at all.
    #[serde(default)]
    pub enabled: bool,
    /// Whether no-progress exits require a successful checkpoint first.
    #[serde(default)]
    pub require_before_no_progress_exit: bool,
    /// Optional branch/ref namespace for future checkpoint commits.
    #[serde(default)]
    pub ref_namespace: Option<String>,
    /// Policy for handling preservation failures before terminal transitions.
    /// Defaults to [`PreservationFailurePolicy::RecordAndProceed`] so the
    /// gate records the outcome without blocking. Set to
    /// [`PreservationFailurePolicy::Block`] once the worker-side RPC is
    /// fully wired and y8pv enforcement is ready.
    #[serde(default)]
    pub failure_policy: PreservationFailurePolicy,
}

/// Lifecycle reason for requesting a preservation checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRequestReason {
    DeadlineWindDown,
    NoProgressWindDown,
    Shutdown,
    Manual,
    PreResumeHandoff,
}

/// Safety-scan result metadata for a checkpoint candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSafetyScanMetadata {
    /// Whether the scanner accepted the checkpoint for later resume/submit.
    #[serde(default)]
    pub passed: bool,
    /// Scanner identifier/version that produced this decision.
    #[serde(default)]
    pub scanner: Option<String>,
    /// Human-readable rejection or warning reasons.
    #[serde(default)]
    pub findings: Vec<String>,
}

/// Auto-submit-if-green metadata placeholder populated by sibling epics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutoSubmitLifecycleMetadata {
    /// Whether auto-submit was considered for this lifecycle decision.
    #[serde(default)]
    pub considered: bool,
    /// Whether all freshness/verification gates were green at evaluation time.
    #[serde(default)]
    pub green: Option<bool>,
    /// Verification command or suite used as the canonical freshness gate.
    #[serde(default)]
    pub verification_command: Option<String>,
    /// Review/submission identifier created by auto-submit, when present.
    #[serde(default)]
    pub submission_id: Option<String>,
    /// Reason auto-submit did not happen or was deferred.
    #[serde(default)]
    pub skipped_reason: Option<AutoSubmitSkipReason>,
    /// Free-form extension map for rollout-specific auto-submit details.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Passive auto-submit rollout config; defaults do not submit worker output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoSubmitLifecycleConfig {
    /// Whether auto-submit-if-green is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Whether a fresh canonical verification result is required before submit.
    #[serde(default)]
    pub require_fresh_verification: bool,
    /// Optional name of the canonical verification gate.
    #[serde(default)]
    pub canonical_verification_gate: Option<String>,
}

/// Classification for why auto-submit was skipped or deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSubmitSkipReason {
    NotEnabled,
    NotGreen,
    VerificationStale,
    CheckpointMissing,
    ReviewRequired,
    SafetyScanFailed,
    MergeConflict,
}

/// Coordinator command-liveness view for no-progress enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoProgressCommandState {
    /// The coordinator has positive evidence that no command is currently in flight.
    Idle,
    /// A command is in flight and has been running for the given number of seconds.
    InFlight { running_secs: u64 },
    /// Command state is unavailable; destructive no-progress exits must defer.
    Unknown,
}

/// Pure policy decision for the no-durable-progress controlled-exit gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoProgressControlledExitDecision {
    /// Rollout/threshold settings do not permit action.
    Disabled,
    /// Threshold met only in shadow mode; emit observation, do not exit.
    ShadowWouldExit,
    /// Threshold not yet met.
    BelowThreshold,
    /// Threshold met, but command liveness requires deferral.
    DeferredForCommand,
    /// Threshold met and no in-flight/unknown command blocks a controlled exit.
    RequestExit,
}

/// Side-effect-free preservation branch used by controlled no-progress exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlledExitPreservationAction {
    /// A ymed auto-submit decision already accepted the output; checkpoint can be skipped.
    UseAutoSubmit,
    /// No auto-submit was accepted; request/await the 8yjx checkpoint preservation path.
    RequestCheckpoint,
    /// Preservation failed and the configured policy blocks the terminal transition.
    BlockForPreservationFailure,
    /// Preservation failed but the configured policy explicitly records/proceeds.
    RecordFailureAndProceed,
}

/// Decide whether a controlled exit should consume auto-submit, checkpoint, or policy result.
pub fn decide_controlled_exit_preservation_action(
    auto_submit_accepted: bool,
    checkpoint_outcome: Option<PreservationOutcome>,
    failure_policy: PreservationFailurePolicy,
) -> ControlledExitPreservationAction {
    if auto_submit_accepted {
        return ControlledExitPreservationAction::UseAutoSubmit;
    }
    let Some(outcome) = checkpoint_outcome else {
        return ControlledExitPreservationAction::RequestCheckpoint;
    };
    if outcome.should_block_transition(failure_policy) {
        ControlledExitPreservationAction::BlockForPreservationFailure
    } else if matches!(
        outcome,
        PreservationOutcome::Succeeded | PreservationOutcome::CleanSkip
    ) {
        ControlledExitPreservationAction::RequestCheckpoint
    } else {
        ControlledExitPreservationAction::RecordFailureAndProceed
    }
}

/// Evaluate the no-progress lifecycle gate without side effects.
pub fn evaluate_no_progress_controlled_exit(
    config: &WorkerLifecycleConfig,
    no_progress_streak: u32,
    command_state: NoProgressCommandState,
) -> NoProgressControlledExitDecision {
    let Some(threshold) = config.no_progress_thresholds.forced_exit_turns else {
        return NoProgressControlledExitDecision::Disabled;
    };
    if config.rollout.no_progress_enforcement == NoProgressEnforcementMode::Disabled {
        return NoProgressControlledExitDecision::Disabled;
    }
    if no_progress_streak < config.no_progress_thresholds.min_evaluated_turns
        || no_progress_streak < threshold
    {
        return NoProgressControlledExitDecision::BelowThreshold;
    }
    if config.rollout.no_progress_enforcement == NoProgressEnforcementMode::Shadow {
        return NoProgressControlledExitDecision::ShadowWouldExit;
    }
    match command_state {
        NoProgressCommandState::Idle => NoProgressControlledExitDecision::RequestExit,
        NoProgressCommandState::Unknown => NoProgressControlledExitDecision::DeferredForCommand,
        NoProgressCommandState::InFlight { running_secs } => {
            let _over_long_command_bound =
                running_secs >= config.no_progress_thresholds.long_command_suspension_secs;
            NoProgressControlledExitDecision::DeferredForCommand
        }
    }
}

/// Resume-via-git metadata placeholder populated by sibling epics.
///
/// Field shapes mirror `djinn_runtime::ResumeLifecycleMetadata` so the
/// coordinator can serialize its selection directly into a [`TaskRunSpec`]
/// without a translation layer, and the worker can deserialize the same shape
/// when reading the spec off the bincode wire. All fields are
/// `#[serde(default)]` so older worker pods continue to deserialize new specs
/// without an `EOF` on bincode decode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResumeLifecycleMetadata {
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
    /// Free-form extension map for rollout-specific resume details.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Previous model used before the termination that triggered this
    /// resume. Populated by the coordinator from model-rotation metadata or
    /// from prior-session failover activity when available. Mirrors the
    /// runtime `ResumeLifecycleMetadata::previous_model` field so the
    /// failover-aware worker resume note (`kv6i`) can report which model was
    /// tried before the rescue candidate.
    #[serde(default)]
    pub previous_model: Option<String>,
    /// New/current model selected after failover. When the failover chain
    /// advances past a failed provider, this is the candidate that ultimately
    /// accepted the dispatch (the rescued model's id). Mirrors the runtime
    /// `ResumeLifecycleMetadata::new_model` field so the worker resume note
    /// can tell the fallback worker which model it is now running on.
    #[serde(default)]
    pub new_model: Option<String>,
    /// Human-readable failover/termination reason supplied by the
    /// coordinator. Populated from model-rotation reason metadata, from
    /// failover-chain candidate events, or from prior-session preservation
    /// activity when available. Mirrors the runtime
    /// `ResumeLifecycleMetadata::failover_reason` field.
    #[serde(default)]
    pub failover_reason: Option<String>,
    /// Last durable-progress summary from the prior session, when
    /// available. Mirrors the runtime `ResumeLifecycleMetadata::
    /// last_durable_progress_summary` field so the worker resume note can
    /// preserve context for the fallback worker.
    #[serde(default)]
    pub last_durable_progress_summary: Option<String>,
    /// Suggested verification command from the prior session's
    /// auto-submit/checkpoint metadata, when available. Mirrors the runtime
    /// `ResumeLifecycleMetadata::verification_command` field so the fallback
    /// worker can re-verify quickly.
    #[serde(default)]
    pub verification_command: Option<String>,
}

/// Passive resume rollout config; defaults do not alter dispatch selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeLifecycleConfig {
    /// Whether resume-from-checkpoint selection is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Whether resume selection may prefer checkpoint refs over the task branch.
    #[serde(default)]
    pub prefer_checkpoint: bool,
    /// Optional maximum checkpoint age considered eligible for resume.
    #[serde(default)]
    pub max_checkpoint_age_secs: Option<u64>,
}

/// Environment-source tri-state for the resume lifecycle enable flag.
///
/// `Unset` means the environment variable was absent, so the DB/runtime value
/// decides. `True`/`False` are explicit operator overrides. An explicit `False`
/// is a **global rollback gate**: it suppresses DB-enabled resume behavior
/// regardless of what the DB says (proposal `phif` AC 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeLifecycleEnvFlag {
    /// Environment variable absent — defer to the DB/runtime value.
    Unset,
    /// Explicitly enabled via environment.
    True,
    /// Explicitly disabled via environment — global rollback gate.
    False,
}

impl ResumeLifecycleEnvFlag {
    /// Parse the `DJINN_WORKER_RESUME_LIFECYCLE_ENABLED` environment value.
    /// Accepts `1`/`true`/`yes` (case-insensitive) as `True`; anything else
    /// (including `0`/`false`/`no` and unrecognized values) as `False`
    /// (fail-safe: an unrecognized value disables resume rather than
    /// accidentally enabling it).
    pub fn from_value(val: &str) -> Self {
        match val.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Self::True,
            _ => Self::False,
        }
    }

    /// Read the flag from the [`ENV_RESUME_LIFECYCLE_ENABLED`] env var,
    /// returning `Unset` when the variable is absent.
    pub fn from_env() -> Self {
        match std::env::var(ENV_RESUME_LIFECYCLE_ENABLED) {
            Ok(v) => Self::from_value(&v),
            Err(_) => Self::Unset,
        }
    }
}

/// Environment variable name for the resume lifecycle global enable/rollback
/// gate.
pub const ENV_RESUME_LIFECYCLE_ENABLED: &str = "DJINN_WORKER_RESUME_LIFECYCLE_ENABLED";

impl ResumeLifecycleConfig {
    /// Resolve the effective resume lifecycle config from an environment flag
    /// and an optional DB/runtime-config source.
    ///
    /// Precedence (proposal `phif`):
    /// 1. **Env `False` is a global rollback gate** — always disabled, ignoring
    ///    the DB value entirely.
    /// 2. **Env `True`** — enabled, using DB fields where present.
    /// 3. **Env `Unset`** — the DB/runtime value decides. When the DB value is
    ///    `None` (no row / not configured), resume stays default-off.
    ///
    /// This is a pure function: it performs no I/O. The caller reads the env
    /// var and DB row, then hands the resolved values here. This separation
    /// makes the precedence logic unit-testable without process-env mutation.
    pub fn resolve(
        env_flag: ResumeLifecycleEnvFlag,
        db_config: Option<&ResumeLifecycleConfig>,
    ) -> ResumeLifecycleConfig {
        // Explicit env false is the global rollback gate: always disabled.
        if env_flag == ResumeLifecycleEnvFlag::False {
            return ResumeLifecycleConfig::default();
        }

        let db = db_config.cloned().unwrap_or_default();

        match env_flag {
            ResumeLifecycleEnvFlag::True => ResumeLifecycleConfig {
                enabled: true,
                prefer_checkpoint: db.prefer_checkpoint,
                max_checkpoint_age_secs: db.max_checkpoint_age_secs,
            },
            // Env unset: DB value decides. Default-off when DB is absent.
            ResumeLifecycleEnvFlag::Unset => db,
            // Already handled False above.
            ResumeLifecycleEnvFlag::False => unreachable!(),
        }
    }
}

/// Classification for resume checkpoint selection decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Model-rotation metadata placeholder for future no-progress enforcement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRotationLifecycleMetadata {
    /// Whether model rotation was considered for this no-progress decision.
    #[serde(default)]
    pub considered: bool,
    /// Reason classification for the rotation decision.
    #[serde(default)]
    pub reason: Option<ModelRotationReason>,
    /// Model/provider used before rotation.
    #[serde(default)]
    pub previous_model: Option<String>,
    /// Model/provider selected after rotation.
    #[serde(default)]
    pub next_model: Option<String>,
    /// Free-form extension map for rollout-specific rotation details.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Slow-verdict claim extension config for stall recovery.
///
/// When the liveness classifier produces a `Slow` verdict for a stalled
/// session and the hard runtime cap is not exceeded, the coordinator grants
/// up to `max_extensions` extensions before falling through to the kill
/// path. Each extension persists `slow_extended` evidence and records a
/// [`ClaimExtensionRecord`] without incrementing task retry/dispatch
/// failure attempts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlowExtensionConfig {
    /// Whether slow-extension claim grants are enabled.
    #[serde(default = "default_slow_extension_enabled")]
    pub enabled: bool,
    /// How many seconds each extension logically grants. Recorded in the
    /// `ClaimExtensionRecord` metadata; the coordinator re-evaluates the
    /// session on each tick.
    #[serde(default = "default_slow_extension_quantum_secs")]
    pub quantum_secs: u64,
    /// Maximum number of slow extensions per session before falling
    /// through to the kill path.
    #[serde(default = "default_slow_max_extensions")]
    pub max_extensions: u32,
}

impl Default for SlowExtensionConfig {
    fn default() -> Self {
        Self {
            enabled: default_slow_extension_enabled(),
            quantum_secs: default_slow_extension_quantum_secs(),
            max_extensions: default_slow_max_extensions(),
        }
    }
}

fn default_slow_extension_enabled() -> bool {
    true
}

fn default_slow_extension_quantum_secs() -> u64 {
    10 * 60 // 10 minutes
}

fn default_slow_max_extensions() -> u32 {
    3
}

/// Passive model-rotation rollout config; defaults do not change model choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRotationLifecycleConfig {
    /// Whether no-progress model rotation is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Whether rotation should be shadow-only even when considered.
    #[serde(default)]
    pub shadow_only: bool,
    /// Optional no-progress streak threshold for rotation. `None` disables it.
    #[serde(default)]
    pub threshold_turns: Option<u32>,
}

/// Classification for future model-rotation decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRotationReason {
    NoDurableProgressStreak,
    RepeatedReadOnlyNoOp,
    RepeatedFlakyVerification,
    ContextBudgetPressure,
    ProviderHealthDegraded,
    OperatorRequested,
    NotEligible,
}

/// Aggregated lifecycle metadata for a worker session/task row or event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkerLifecycleMetadata {
    /// Current consecutive evaluated turns without durable progress.
    #[serde(default)]
    pub no_progress_streak: u32,
    /// Last reason that reset the no-progress streak.
    #[serde(default)]
    pub last_reset_reason: Option<DurableProgressResetReason>,
    /// Last reason that an evaluated turn did not reset the streak.
    #[serde(default)]
    pub last_no_reset_reason: Option<DurableProgressNoResetReason>,
    /// Threshold config snapshot used for the latest lifecycle decision.
    #[serde(default)]
    pub thresholds: NoProgressThresholdConfig,
    /// Rollout config snapshot used for the latest lifecycle decision.
    #[serde(default)]
    pub rollout: DurableProgressRolloutConfig,
    /// Checkpoint state populated by preservation/checkpoint work.
    #[serde(default)]
    pub checkpoint: Option<CheckpointLifecycleMetadata>,
    /// Auto-submit state populated by auto-submit-if-green work.
    #[serde(default)]
    pub auto_submit: Option<AutoSubmitLifecycleMetadata>,
    /// Resume state populated by resume-via-git work.
    #[serde(default)]
    pub resume: Option<ResumeLifecycleMetadata>,
    /// Model-rotation state populated by future enforcement work.
    #[serde(default)]
    pub model_rotation: Option<ModelRotationLifecycleMetadata>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn worker_lifecycle_config_defaults_preserve_existing_behavior() {
        let config = WorkerLifecycleConfig::default();

        assert_eq!(
            config.rollout.detection_mode,
            DurableProgressDetectionMode::Shadow
        );
        assert_eq!(
            config.rollout.no_progress_enforcement,
            NoProgressEnforcementMode::Disabled
        );
        assert!(!config.rollout.checkpoint_before_no_progress_exit);
        assert!(!config.rollout.auto_submit_if_green);
        assert!(!config.rollout.resume_from_checkpoint);
        assert!(!config.rollout.rotate_model_on_no_progress);
        assert_eq!(config.no_progress_thresholds.forced_exit_turns, None);
        assert_eq!(config.no_progress_thresholds.model_rotation_turns, None);
        assert!(!config.checkpoint.enabled);
        assert!(!config.auto_submit.enabled);
        assert!(!config.resume.enabled);
        assert!(!config.model_rotation.enabled);
        assert!(config.slow_extension.enabled);
        assert_eq!(config.slow_extension.quantum_secs, 10 * 60);
        assert_eq!(config.slow_extension.max_extensions, 3);
    }

    fn enforcing_config() -> WorkerLifecycleConfig {
        WorkerLifecycleConfig {
            rollout: DurableProgressRolloutConfig {
                detection_mode: DurableProgressDetectionMode::Enforce,
                no_progress_enforcement: NoProgressEnforcementMode::Enforce,
                checkpoint_before_no_progress_exit: true,
                auto_submit_if_green: true,
                resume_from_checkpoint: false,
                rotate_model_on_no_progress: false,
            },
            no_progress_thresholds: NoProgressThresholdConfig {
                min_evaluated_turns: 2,
                warning_turns: 2,
                model_rotation_turns: None,
                forced_exit_turns: Some(4),
                long_command_suspension_secs: 600,
                flaky_command_grace_turns: 0,
            },
            checkpoint: CheckpointLifecycleConfig {
                enabled: true,
                require_before_no_progress_exit: true,
                ref_namespace: None,
                failure_policy: PreservationFailurePolicy::RecordAndProceed,
            },
            auto_submit: AutoSubmitLifecycleConfig {
                enabled: true,
                require_fresh_verification: true,
                canonical_verification_gate: Some("default".to_string()),
            },
            resume: ResumeLifecycleConfig::default(),
            model_rotation: ModelRotationLifecycleConfig::default(),
            slow_extension: SlowExtensionConfig::default(),
        }
    }

    #[test]
    fn no_progress_gate_default_off_disables_exit() {
        assert_eq!(
            evaluate_no_progress_controlled_exit(
                &WorkerLifecycleConfig::default(),
                99,
                NoProgressCommandState::Idle
            ),
            NoProgressControlledExitDecision::Disabled
        );
    }

    #[test]
    fn no_progress_gate_requests_exit_after_threshold_when_idle() {
        assert_eq!(
            evaluate_no_progress_controlled_exit(
                &enforcing_config(),
                4,
                NoProgressCommandState::Idle
            ),
            NoProgressControlledExitDecision::RequestExit
        );
    }

    #[test]
    fn no_progress_gate_defers_long_or_unknown_command_state() {
        assert_eq!(
            evaluate_no_progress_controlled_exit(
                &enforcing_config(),
                4,
                NoProgressCommandState::InFlight { running_secs: 1200 }
            ),
            NoProgressControlledExitDecision::DeferredForCommand
        );
        assert_eq!(
            evaluate_no_progress_controlled_exit(
                &enforcing_config(),
                4,
                NoProgressCommandState::Unknown
            ),
            NoProgressControlledExitDecision::DeferredForCommand
        );
    }

    #[test]
    fn no_progress_gate_shadow_observes_without_exit() {
        let mut config = enforcing_config();
        config.rollout.no_progress_enforcement = NoProgressEnforcementMode::Shadow;

        assert_eq!(
            evaluate_no_progress_controlled_exit(&config, 4, NoProgressCommandState::Idle),
            NoProgressControlledExitDecision::ShadowWouldExit
        );
    }

    #[test]
    fn controlled_exit_preservation_prefers_auto_submit() {
        assert_eq!(
            decide_controlled_exit_preservation_action(
                true,
                Some(PreservationOutcome::RuntimeUnavailable),
                PreservationFailurePolicy::Block
            ),
            ControlledExitPreservationAction::UseAutoSubmit
        );
    }

    #[test]
    fn controlled_exit_preservation_requests_checkpoint_fallback() {
        assert_eq!(
            decide_controlled_exit_preservation_action(
                false,
                None,
                PreservationFailurePolicy::RecordAndProceed
            ),
            ControlledExitPreservationAction::RequestCheckpoint
        );
        assert_eq!(
            decide_controlled_exit_preservation_action(
                false,
                Some(PreservationOutcome::Succeeded),
                PreservationFailurePolicy::RecordAndProceed
            ),
            ControlledExitPreservationAction::RequestCheckpoint
        );
    }

    #[test]
    fn controlled_exit_preservation_applies_failure_policy() {
        assert_eq!(
            decide_controlled_exit_preservation_action(
                false,
                Some(PreservationOutcome::Failed),
                PreservationFailurePolicy::RecordAndProceed
            ),
            ControlledExitPreservationAction::RecordFailureAndProceed
        );
        assert_eq!(
            decide_controlled_exit_preservation_action(
                false,
                Some(PreservationOutcome::Failed),
                PreservationFailurePolicy::Block
            ),
            ControlledExitPreservationAction::BlockForPreservationFailure
        );
    }

    #[test]
    fn worker_lifecycle_config_deserializes_missing_fields_to_safe_defaults()
    -> Result<(), serde_json::Error> {
        let config = serde_json::from_value::<WorkerLifecycleConfig>(json!({}))?;

        assert_eq!(config, WorkerLifecycleConfig::default());
        Ok(())
    }

    #[test]
    fn lifecycle_metadata_serializes_representative_json() -> Result<(), serde_json::Error> {
        let metadata = WorkerLifecycleMetadata {
            no_progress_streak: 7,
            last_reset_reason: Some(DurableProgressResetReason::NewlyGreenVerification),
            last_no_reset_reason: Some(DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess),
            thresholds: NoProgressThresholdConfig {
                min_evaluated_turns: 2,
                warning_turns: 5,
                model_rotation_turns: Some(8),
                forced_exit_turns: Some(13),
                long_command_suspension_secs: 900,
                flaky_command_grace_turns: 3,
            },
            rollout: DurableProgressRolloutConfig {
                detection_mode: DurableProgressDetectionMode::Shadow,
                no_progress_enforcement: NoProgressEnforcementMode::Shadow,
                checkpoint_before_no_progress_exit: true,
                auto_submit_if_green: true,
                resume_from_checkpoint: true,
                rotate_model_on_no_progress: true,
            },
            checkpoint: Some(CheckpointLifecycleMetadata {
                checkpoint_id: Some("ckpt-1".to_string()),
                commit_sha: Some("abc123".to_string()),
                ref_name: Some("refs/djinn/checkpoints/task-1".to_string()),
                requested_for: Some(CheckpointRequestReason::NoProgressWindDown),
                safety_scan: Some(CheckpointSafetyScanMetadata {
                    passed: true,
                    scanner: Some("safety-v1".to_string()),
                    findings: vec![],
                }),
                preservation_outcome: Some(PreservationOutcome::Succeeded),
                extra: serde_json::Map::new(),
            }),
            auto_submit: Some(AutoSubmitLifecycleMetadata {
                considered: true,
                green: Some(false),
                verification_command: Some("cargo test -p djinn-coordinator".to_string()),
                submission_id: None,
                skipped_reason: Some(AutoSubmitSkipReason::VerificationStale),
                extra: serde_json::Map::new(),
            }),
            resume: Some(ResumeLifecycleMetadata {
                considered: true,
                checkpoint_id: Some("ckpt-1".to_string()),
                commit_sha: Some("abc123".to_string()),
                selection_reason: Some(ResumeSelectionReason::LatestSafeCheckpoint),
                extra: serde_json::Map::new(),
                previous_model: Some("provider/old".to_string()),
                new_model: Some("provider/new".to_string()),
                failover_reason: Some("no_durable_progress_streak".to_string()),
                last_durable_progress_summary: Some("Wrote the parser module".to_string()),
                verification_command: Some("cargo test".to_string()),
            }),
            model_rotation: Some(ModelRotationLifecycleMetadata {
                considered: true,
                reason: Some(ModelRotationReason::NoDurableProgressStreak),
                previous_model: Some("provider/old".to_string()),
                next_model: Some("provider/new".to_string()),
                extra: serde_json::Map::new(),
            }),
        };

        let value = serde_json::to_value(&metadata)?;

        assert_eq!(
            value,
            json!({
                "no_progress_streak": 7,
                "last_reset_reason": "newly_green_verification",
                "last_no_reset_reason": "read_only_or_no_op_tool_success",
                "thresholds": {
                    "min_evaluated_turns": 2,
                    "warning_turns": 5,
                    "model_rotation_turns": 8,
                    "forced_exit_turns": 13,
                    "long_command_suspension_secs": 900,
                    "flaky_command_grace_turns": 3
                },
                "rollout": {
                    "detection_mode": "shadow",
                    "no_progress_enforcement": "shadow",
                    "checkpoint_before_no_progress_exit": true,
                    "auto_submit_if_green": true,
                    "resume_from_checkpoint": true,
                    "rotate_model_on_no_progress": true
                },
                "checkpoint": {
                    "checkpoint_id": "ckpt-1",
                    "commit_sha": "abc123",
                    "ref_name": "refs/djinn/checkpoints/task-1",
                    "requested_for": "no_progress_wind_down",
                    "safety_scan": {
                        "passed": true,
                        "scanner": "safety-v1",
                        "findings": []
                    },
                    "preservation_outcome": "succeeded",
                    "extra": {}
                },
                "auto_submit": {
                    "considered": true,
                    "green": false,
                    "verification_command": "cargo test -p djinn-coordinator",
                    "submission_id": null,
                    "skipped_reason": "verification_stale",
                    "extra": {}
                },
                "resume": {
                    "considered": true,
                    "checkpoint_id": "ckpt-1",
                    "commit_sha": "abc123",
                    "selection_reason": "latest_safe_checkpoint",
                    "extra": {},
                    "previous_model": "provider/old",
                    "new_model": "provider/new",
                    "failover_reason": "no_durable_progress_streak",
                    "last_durable_progress_summary": "Wrote the parser module",
                    "verification_command": "cargo test",
                },
                "model_rotation": {
                    "considered": true,
                    "reason": "no_durable_progress_streak",
                    "previous_model": "provider/old",
                    "next_model": "provider/new",
                    "extra": {}
                }
            })
        );
        Ok(())
    }

    // ── ResumeLifecycleConfig::resolve tests ──────────────────────
    // These tests cover proposal `phif` AC 1/3: default-off config,
    // DB-scoped enablement, and env rollback gate.

    #[test]
    fn resolve_default_off_when_env_unset_and_db_absent() {
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::Unset, None);
        assert!(
            !config.enabled,
            "default-off: env Unset + DB absent must produce disabled resume"
        );
        assert!(
            !config.prefer_checkpoint,
            "default-off: prefer_checkpoint must be false"
        );
        assert_eq!(
            config.max_checkpoint_age_secs, None,
            "default-off: max_checkpoint_age_secs must be None"
        );
    }

    #[test]
    fn resolve_default_off_when_env_unset_and_db_disabled() {
        let db = ResumeLifecycleConfig {
            enabled: false,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(600),
        };
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::Unset, Some(&db));
        assert!(
            !config.enabled,
            "env Unset + DB disabled must stay disabled"
        );
    }

    #[test]
    fn resolve_db_scoped_enablement_when_env_unset() {
        let db = ResumeLifecycleConfig {
            enabled: true,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(300),
        };
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::Unset, Some(&db));
        assert!(
            config.enabled,
            "env Unset + DB enabled must produce enabled resume"
        );
        assert!(
            config.prefer_checkpoint,
            "DB prefer_checkpoint must pass through"
        );
        assert_eq!(
            config.max_checkpoint_age_secs,
            Some(300),
            "DB max_checkpoint_age_secs must pass through"
        );
    }

    #[test]
    fn resolve_db_scoped_enablement_when_env_true() {
        let db = ResumeLifecycleConfig {
            enabled: true,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(120),
        };
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::True, Some(&db));
        assert!(
            config.enabled,
            "env True + DB enabled must produce enabled resume"
        );
        assert!(config.prefer_checkpoint);
        assert_eq!(config.max_checkpoint_age_secs, Some(120));
    }

    #[test]
    fn resolve_env_true_without_db_defaults_to_enabled() {
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::True, None);
        assert!(
            config.enabled,
            "env True + DB absent must enable resume with default fields"
        );
        assert!(
            !config.prefer_checkpoint,
            "env True without DB: prefer_checkpoint defaults to false"
        );
        assert_eq!(config.max_checkpoint_age_secs, None);
    }

    #[test]
    fn resolve_env_rollback_suppresses_db_enabled() {
        // Proposal phif AC 3: explicit env false is a global rollback gate.
        let db = ResumeLifecycleConfig {
            enabled: true,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(300),
        };
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::False, Some(&db));
        assert!(
            !config.enabled,
            "env False must suppress DB-enabled resume (global rollback gate)"
        );
        assert!(
            !config.prefer_checkpoint,
            "env False: prefer_checkpoint must be reset to default"
        );
        assert_eq!(
            config.max_checkpoint_age_secs, None,
            "env False: max_checkpoint_age_secs must be reset to default"
        );
    }

    #[test]
    fn resolve_env_rollback_suppresses_even_db_true_fields() {
        // Regression: env False returns default(), ignoring all DB fields.
        let db = ResumeLifecycleConfig {
            enabled: true,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(999),
        };
        let config = ResumeLifecycleConfig::resolve(ResumeLifecycleEnvFlag::False, Some(&db));
        assert_eq!(config, ResumeLifecycleConfig::default());
    }

    // ── ResumeLifecycleEnvFlag::from_value tests ──────────────────

    #[test]
    fn env_flag_from_value_truthy_variants() {
        for val in &["1", "true", "True", "TRUE", "yes", "Yes", "YES"] {
            assert_eq!(
                ResumeLifecycleEnvFlag::from_value(val),
                ResumeLifecycleEnvFlag::True,
                "expected True for value: {val}"
            );
        }
    }

    #[test]
    fn env_flag_from_value_falsy_variants() {
        for val in &["0", "false", "no", "off", "", "garbage", "2"] {
            assert_eq!(
                ResumeLifecycleEnvFlag::from_value(val),
                ResumeLifecycleEnvFlag::False,
                "expected False for value: {val}"
            );
        }
    }
}

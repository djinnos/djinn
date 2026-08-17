use super::*;
// djinn:allow-oversize

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCreateParams {
    /// Absolute project path.
    pub project: String,
    /// Parent epic ID - UUID or short_id (required).
    pub epic_id: Option<String>,
    pub title: String,
    /// Task type: "task" (default), "feature", "bug", "spike", "research", "planning", or "review".
    /// Spike, research, planning, and review use a simple lifecycle: open → in_progress → closed.
    /// Planning tasks are routed to the Planner; spike and review tasks are routed to the Architect.
    pub issue_type: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub labels: Option<Vec<String>>,
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Memory note permalinks to attach to this task at creation.
    pub memory_refs: Option<Vec<String>>,
    /// Task IDs (UUID or short_id) that block this task. Blockers are set atomically at creation.
    pub blocked_by: Option<Vec<String>>,
    /// Optional initial status. Allowed value: "open" (default).
    pub status: Option<String>,
    /// Specialist role name to route this task (e.g. "rust-expert").
    pub agent_type: Option<String>,
    /// Explicit typed execution eligibility metadata. This is never inferred
    /// from task text, labels, roles, or issue type.
    pub execution_context: Option<djinn_core::models::TaskExecutionContext>,
}

/// Parse the promoted current-head CI status from the task model, preserving
/// the upstream `unknown` default when no snapshot has been persisted yet.
pub fn task_ci_status(t: &Task) -> CiStatus {
    CiStatus::parse(&t.ci_status).unwrap_or(CiStatus::Unknown)
}

/// Derive the human/agent-facing CI gate state from the task's promoted CI
/// status and lifecycle status.
pub fn task_ci_gate_state(t: &Task) -> CiGateState {
    derive_gate_state(task_ci_status(t), &t.status)
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskUpdateParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    /// Labels to add.
    pub labels_add: Option<Vec<String>>,
    /// Labels to remove.
    pub labels_remove: Option<Vec<String>>,
    /// Full replacement for acceptance_criteria.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// New parent epic UUID or short_id.
    pub epic_id: Option<String>,
    /// Memory note permalinks to add to this task.
    pub memory_refs_add: Option<Vec<String>>,
    /// Memory note permalinks to remove from this task.
    pub memory_refs_remove: Option<Vec<String>>,
    /// Task IDs (UUID or short_id) to add as blockers of this task.
    pub blocked_by_add: Option<Vec<String>>,
    /// Task IDs (UUID or short_id) to remove as blockers of this task.
    pub blocked_by_remove: Option<Vec<String>>,
    /// Specialist role name to assign (set None/"" to clear).
    pub agent_type: Option<String>,
    /// Complete replacement for explicit typed execution eligibility metadata.
    /// Partial JSON updates are not supported.
    pub execution_context: Option<djinn_core::models::TaskExecutionContext>,
}

#[derive(Serialize, Clone, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AcceptanceCriterionItem {
    Text(String),
    Structured(AcceptanceCriterionStatus),
}

impl<'de> serde::Deserialize<'de> for AcceptanceCriterionItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) => Ok(AcceptanceCriterionItem::Text(s.clone())),
            serde_json::Value::Object(_) => {
                serde_json::from_value::<AcceptanceCriterionStatus>(value)
                    .map(AcceptanceCriterionItem::Structured)
                    .map_err(|_| {
                        serde::de::Error::custom(
                            "each acceptance criterion must be a plain string, e.g. \
                             [\"criterion 1\", \"criterion 2\"]. Objects with \
                             {\"criterion\": ..., \"met\": ...} are also accepted.",
                        )
                    })
            }
            other => Err(serde::de::Error::custom(format!(
                "each acceptance criterion must be a plain string, e.g. \
                 [\"criterion 1\", \"criterion 2\"], but got {}. \
                 Objects with {{\"criterion\": ..., \"met\": ...}} are also accepted.",
                match other {
                    serde_json::Value::Null => "null",
                    serde_json::Value::Bool(_) => "a boolean",
                    serde_json::Value::Number(_) => "a number",
                    serde_json::Value::Array(_) => "an array",
                    _ => "an unexpected type",
                }
            ))),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct AcceptanceCriterionStatus {
    pub criterion: String,
    #[serde(default)]
    pub met: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskShowParams {
    /// Absolute project path. Optional - task IDs are globally unique.
    pub project: Option<String>,
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskListParams {
    /// Absolute project path.
    pub project: String,
    /// Positive ("open") or negative ("!closed") status filter. A leading "!"
    /// matches every task whose status differs from the given value. The
    /// pseudo-status "merged" matches closed tasks that actually merged (have a
    /// merge-commit SHA, or opened a PR and closed as completed) — this is what
    /// backs the Kanban Merged column.
    pub status: Option<String>,
    /// Positive ("task") or negative ("!epic") issue_type filter.
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    /// Filter by label value.
    pub label: Option<String>,
    /// Full-text search on title and description.
    pub text: Option<String>,
    /// Sort order: "priority" (default), "created", "created_desc",
    /// "updated", "updated_desc", "closed".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCountParams {
    /// Absolute project path.
    pub project: String,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    /// Group results by: "status", "priority", "issue_type", or "epic".
    pub group_by: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCommentAddParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
    /// Comment body text.
    pub body: String,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersListParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockedListParams {
    /// Absolute project path. Optional - task IDs are globally unique.
    pub project: Option<String>,
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskReadyParams {
    /// Filter by label value.
    pub label: Option<String>,
    /// Filter by owner email.
    pub owner: Option<String>,
    /// Maximum priority to include (0=highest, higher numbers=lower priority).
    pub priority_max: Option<i64>,
    pub limit: Option<i64>,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskActivityListParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id (optional - omit to query all tasks).
    pub id: Option<String>,
    /// Filter by event_type (e.g. "status_changed", "comment").
    pub event_type: Option<String>,
    /// Filter by actor_role (e.g. "lead", "reviewer", "worker", "system").
    pub actor_role: Option<String>,
    /// ISO-8601 lower bound on created_at.
    pub from_time: Option<String>,
    /// ISO-8601 upper bound on created_at.
    pub to_time: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BoardHealthParams {
    /// Hours before an in_progress task is considered stale (default: 24).
    pub stale_threshold_hours: Option<i64>,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BoardReconcileParams {
    /// Hours before an in_progress task is considered stale (default: 24).
    pub stale_threshold_hours: Option<i64>,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskMemoryRefsParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskTransitionParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
    /// Transition action: start,
    /// submit_task_review, task_review_start,
    /// task_review_reject, task_review_reject_conflict, task_review_approve,
    /// pr_created, pr_undraft, pr_ci_failed, pr_conflict,
    /// pr_merge, pr_changes_requested,
    /// reopen, close, release, release_task_review, force_close,
    /// user_override.
    pub action: String,
    /// Required for:
    /// task_review_reject, task_review_reject_conflict,
    /// pr_changes_requested,
    /// reopen, release, release_task_review, force_close.
    pub reason: Option<String>,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
    /// Required when action = "user_override". Allowed values: open, in_progress,
    /// needs_task_review, in_task_review, approved, pr_draft, pr_review, closed.
    pub target_status: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskClaimParams {
    /// Filter by label value.
    pub label: Option<String>,
    /// Filter by owner email.
    pub owner: Option<String>,
    /// Maximum priority to include (0=highest).
    pub priority_max: Option<i64>,
    /// Session ID of the claiming agent (recorded as actor_id in the activity log).
    pub session_id: Option<String>,
    /// Absolute project path.
    pub project: String,
}

// ── Response structs ──────────────────────────────────────────────────────────

/// Snapshot of the current-head required-CI gate for a task's PR.
///
/// Populated from the repository-backed CI snapshot (`task_pr_ci_snapshots`).
/// `None` when no snapshot exists yet (e.g. task has no PR or has not been
/// polled). Downstream lifecycle/API code reads these fields directly instead
/// of scraping activity prose.
///
/// Derived fields (`gate_state`, `primary_blocking_check`, `summary_reason`,
/// `merge_blocked_reason`) are computed from the raw CI status combined with
/// the task's lifecycle status.  They expose human/agent-friendly gate
/// information without requiring consumers to re-derive policy.
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct CiGateSnapshot {
    /// Current required-CI status for the PR head.
    pub status: CiStatus,
    /// Derived CI gate state combining raw CI status with task lifecycle.
    ///
    /// Maps to the upstream low-risk design contract:
    /// - `passing` / `failing` / `pending` / `unknown` mirror `CiStatus`
    ///   when the task is not in `pr_draft`.
    /// - `awaiting_ci` when the task is in `pr_draft` *and* the raw CI
    ///   status is `pending` or `unknown` (CI has not completed yet).
    ///
    /// UI consumers render this value directly as the badge text.
    pub gate_state: CiGateState,
    /// Git SHA of the PR head this snapshot describes.
    pub head_sha: String,
    /// Names of required checks that are currently failing and blocking merge.
    pub blocking_required_check_names: Vec<String>,
    /// The single required check to triage first — the earliest-started
    /// blocking lane that actually executed and hard-failed.
    ///
    /// Selected by the PR poller from *structural execution evidence*
    /// (conclusion class, execution interval, annotation count, start order),
    /// never from name order and never from a list of job names. A check that
    /// was `cancelled`, or that never executed, is a symptom of a run-level
    /// abort rather than a cause, and is never selected.
    ///
    /// `None` when no blocking check carries causal information — i.e. `status`
    /// is `inconclusive` and the run should be retriggered, not remediated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_blocking_check: Option<String>,
    /// Bounded rendering of the GitHub annotations on
    /// [`Self::primary_blocking_check`].
    ///
    /// Runner-host failures — out of disk, runner process crash — surface ONLY
    /// as annotations, not as a check conclusion and often not in job logs
    /// either. This field is what lets a reader see `No space left on device`
    /// without opening GitHub.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_annotations: Option<String>,
    /// Stable fingerprint of the current failure signature (e.g. sorted
    /// failing check names + head SHA). `None` when not failing.
    pub failure_fingerprint: Option<String>,
    /// Human-readable summary of the current CI gate state.
    ///
    /// Derived from raw CI status and blocking check names.  Examples:
    /// - `"All required checks passed"` (passing)
    /// - `"Required check failing: clippy"` (failing with one check)
    /// - `"Required checks pending"` (pending)
    /// - `"CI state unknown"` (unknown)
    pub summary_reason: String,
    /// Reason merge/close is blocked by CI, if applicable.
    ///
    /// `Some(_)` when the raw CI status is not `passing` (i.e. failing,
    /// pending, or unknown).  `None` when CI is passing or when no
    /// snapshot exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_blocked_reason: Option<String>,
    /// ISO-8601 timestamp when this snapshot was first observed.
    pub first_seen_at: String,
    /// ISO-8601 timestamp when this snapshot was last observed/updated.
    pub last_seen_at: String,
    /// How many consecutive observations carried the same failure fingerprint
    /// for the same head SHA.
    pub same_signature_count: i64,
    /// Base SHA of the last remediation attempt for this failing signature.
    /// `None` when no remediation has been attempted yet.
    pub last_remediation_base_sha: Option<String>,
    /// GitHub PR number the CI snapshot belongs to, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i64>,
    // ── CI head reconciliation (m116) ──────────────────────────────────
    //
    // These additive fields expose the mirror-vs-GitHub head reconciliation
    // data sourced from the latest task-attempt evidence.  They are entirely
    // nullable and omitted from the payload when no evidence exists, so
    // existing `head_sha` consumers are unaffected.
    //
    // Scope boundary with proposal `ivek`: m116 owns branch-publication and
    // head-visibility mechanics (these fields) plus stale-head false-strike
    // suppression for unpublished mirror commits.  Proposal `ivek` remains
    // responsible for broader strike classification (typed reopen
    // classification, quality-strike guards, park-guard semantics) and
    // submission-integrity fingerprints.  See
    // `server/docs/ci-head-reconciliation/m116-consumer-compatibility.md`.
    /// Head SHA of the internal mirror branch, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror_head_sha: Option<String>,
    /// Head SHA of the GitHub PR branch, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_head_sha: Option<String>,
    /// `true` only when both heads are known and differ; `false` only when
    /// both are known and equal; absent/null-compatible when either side is
    /// unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heads_diverged: Option<bool>,
    /// Concise error string from the most recent publication/observation
    /// failure, when one is recorded.  Absent when no error is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_observation_error: Option<String>,
    /// Merge-queue (`merge_group`) failure lane for the current PR head.
    ///
    /// Present only when GitHub's merge queue rejected the PR at dequeue time
    /// (a PR head whose own required checks are green can still be dequeued if
    /// the heavy `merge_group` stages fail). Populated from the `mq_*` columns
    /// of `task_pr_ci_snapshots`; omitted entirely when no merge-queue failure
    /// has been recorded for the current head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_queue: Option<CiMergeQueueLane>,
}

/// The merge-queue (`merge_group`) failure lane surfaced in a
/// [`CiGateSnapshot`].
///
/// Mirrors `djinn_core::models::MergeQueueLane` in serialized form. GitHub's
/// merge queue runs the heavy CI stages on the ephemeral `merge_group` ref, so
/// a PR whose own head checks pass can still be rejected by the queue. This
/// lane records that queue verdict with its own failure fingerprint and
/// same-signature counting, independent of the PR-head lane.
#[derive(Clone, Serialize, schemars::JsonSchema)]
pub struct CiMergeQueueLane {
    /// Lane state, e.g. `"dequeued_failure"`.
    pub state: String,
    /// The `merge_group` Actions run id that failed, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<i64>,
    /// The `head_sha` of the failed merge-group run (the ephemeral queue ref
    /// head), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Names of the merge-group check runs that failed.
    pub failed_check_names: Vec<String>,
    /// Stable fingerprint of the merge-group failure signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_fingerprint: Option<String>,
    /// How many consecutive dequeue observations carried the same
    /// `failure_fingerprint` in this lane.
    pub same_signature_count: i64,
    /// ISO-8601 timestamp when this merge-queue lane state was first observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seen_at: Option<String>,
    /// ISO-8601 timestamp when this merge-queue lane state was last observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
}

/// Derived CI gate state combining raw CI status with task lifecycle.
///
/// Follows the upstream low-risk design: when a task is in `pr_draft` and the
/// raw CI status is `pending` or `unknown`, the gate state is `awaiting_ci`.
/// Otherwise it mirrors the raw CI status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiGateState {
    Passing,
    Failing,
    Pending,
    /// Every blocking required check was cancelled or never executed, so the
    /// run reached no verdict about the code. Warrants a retrigger, not a
    /// remediation attempt.
    Inconclusive,
    Unknown,
    /// Task is in `pr_draft` and CI has not completed yet (pending/unknown).
    AwaitingCi,
}

impl CiGateState {
    /// The wire/JSON string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passing => "passing",
            Self::Failing => "failing",
            Self::Pending => "pending",
            Self::Inconclusive => "inconclusive",
            Self::Unknown => "unknown",
            Self::AwaitingCi => "awaiting_ci",
        }
    }
}

impl std::fmt::Display for CiGateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Derive the [`CiGateState`] from the raw CI status and the task's lifecycle
/// status string.
///
/// When the task is in `pr_draft` and the raw CI status is `pending` or
/// `unknown`, returns `AwaitingCi`.  Otherwise returns the matching gate
/// state for the raw CI status.
fn derive_gate_state(ci_status: CiStatus, task_status: &str) -> CiGateState {
    if task_status == "pr_draft" && matches!(ci_status, CiStatus::Pending | CiStatus::Unknown) {
        CiGateState::AwaitingCi
    } else {
        match ci_status {
            CiStatus::Passing => CiGateState::Passing,
            CiStatus::Failing => CiGateState::Failing,
            CiStatus::Pending => CiGateState::Pending,
            // Inconclusive is surfaced as-is even in `pr_draft`: unlike
            // pending/unknown it is a *completed* run, and collapsing it into
            // `awaiting_ci` would hide the fact that a retrigger is owed.
            CiStatus::Inconclusive => CiGateState::Inconclusive,
            CiStatus::Unknown => CiGateState::Unknown,
        }
    }
}

/// Build a `CiGateSnapshot` DTO from a task's repository-backed CI fields.
///
/// Returns `None` when no snapshot exists (signalled by an absent `head_sha`),
/// preserving backward-compatible optional/null behavior for tasks without a
/// PR snapshot.
///
/// The four m116 reconciliation fields (`mirror_head_sha`, `github_head_sha`,
/// `heads_diverged`, `head_observation_error`) are additive and nullable;
/// their presence does not break consumers that only read `head_sha` or other
/// pre-existing fields.  See
/// `server/docs/ci-head-reconciliation/m116-consumer-compatibility.md` for
/// the full consumer-compatibility and `ivek`-boundary evidence.
pub fn task_ci_gate_snapshot(t: &Task) -> Option<CiGateSnapshot> {
    let head_sha = t.ci_head_sha.as_deref()?;
    let status = task_ci_status(t);
    let gate_state = task_ci_gate_state(t);
    let blocking_checks = parse_string_array(&t.ci_blocking_required_check_names);
    // Read the poller's ranked selection. NEVER re-derive this as
    // `blocking_checks[0]`: under a run-level cancel that element can be a
    // `needs:`-dependent aggregator that never executed, which is a symptom by
    // construction and cannot be a root cause. The poller ranks by structural
    // execution evidence and stores the winner; `None` means the run was
    // inconclusive and there is nothing to triage.
    let primary_blocking_check = t.ci_primary_blocking_check.clone();
    let summary_reason = match status {
        CiStatus::Passing => "All required checks passed".to_string(),
        CiStatus::Failing => {
            if let Some(ref check) = primary_blocking_check {
                format!("Required check failing: {check}")
            } else {
                "Required checks failing".to_string()
            }
        }
        CiStatus::Inconclusive => {
            let n = blocking_checks.len();
            format!(
                "CI inconclusive: all {n} blocking required check(s) were cancelled or never \
                 executed — no verdict about the code. Retrigger CI; do not remediate."
            )
        }
        CiStatus::Pending => "Required checks pending".to_string(),
        CiStatus::Unknown => "CI state unknown".to_string(),
    };
    let merge_blocked_reason = if status != CiStatus::Passing {
        Some(match status {
            CiStatus::Failing => {
                if let Some(ref check) = primary_blocking_check {
                    format!("Blocked by failing required check: {check}")
                } else {
                    "Blocked by failing required checks".to_string()
                }
            }
            CiStatus::Inconclusive => {
                "Blocked by an inconclusive CI run (every blocking required check was \
                 cancelled or never executed); awaiting a retrigger"
                    .to_string()
            }
            CiStatus::Pending => "Waiting for required checks to complete".to_string(),
            CiStatus::Unknown => "CI state unknown; cannot confirm merge safety".to_string(),
            CiStatus::Passing => unreachable!(),
        })
    } else {
        None
    };
    Some(CiGateSnapshot {
        status,
        gate_state,
        head_sha: head_sha.to_string(),
        blocking_required_check_names: blocking_checks,
        primary_blocking_check,
        failure_annotations: t.ci_failure_annotations.clone(),
        failure_fingerprint: t.ci_failure_fingerprint.clone(),
        summary_reason,
        merge_blocked_reason,
        first_seen_at: t.ci_first_seen_at.clone().unwrap_or_default(),
        last_seen_at: t.ci_last_seen_at.clone().unwrap_or_default(),
        same_signature_count: t.ci_same_signature_count,
        last_remediation_base_sha: t.ci_last_remediation_base_sha.clone(),
        pr_number: t.ci_pr_number,
        mirror_head_sha: t.ci_mirror_head_sha.clone(),
        github_head_sha: t.ci_github_head_sha.clone(),
        heads_diverged: t.ci_heads_diverged,
        head_observation_error: t.ci_head_observation_error.clone(),
        merge_queue: t.ci_mq_state.as_ref().map(|state| CiMergeQueueLane {
            state: state.clone(),
            run_id: t.ci_mq_run_id,
            head_sha: t.ci_mq_head_sha.clone(),
            failed_check_names: t
                .ci_mq_failed_check_names
                .as_deref()
                .map(parse_string_array)
                .unwrap_or_default(),
            failure_fingerprint: t.ci_mq_failure_fingerprint.clone(),
            same_signature_count: t.ci_mq_same_signature_count.unwrap_or(0),
            first_seen_at: t.ci_mq_first_seen_at.clone(),
            last_seen_at: t.ci_mq_last_seen_at.clone(),
        }),
    })
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskListResponse {
    pub tasks: Vec<TaskListItem>,
    pub total_count: i64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ErrorResponse {
    pub error: String,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum ErrorOr<T> {
    Ok(T),
    Error(ErrorResponse),
}

impl<T> schemars::JsonSchema for ErrorOr<T>
where
    T: schemars::JsonSchema,
{
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("ErrorOr{}", T::schema_name()).into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskResponse {
    pub id: String,
    pub short_id: String,
    pub epic_id: Option<String>,
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub status: String,
    pub priority: i64,
    pub owner: String,
    pub labels: Vec<String>,
    pub memory_refs: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterionItem>,
    pub reopen_count: i64,
    pub continuation_count: i64,
    pub total_reopen_count: i64,
    pub intervention_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_intervention_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
    /// JSON metadata about an active merge conflict (files, branches).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Map<String, serde_json::Value>>")]
    pub merge_conflict_metadata: Option<AnyJson>,
    /// URL of the associated pull request, once one has been opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// Specialist role name assigned to this task, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_context: Option<djinn_core::models::TaskExecutionContext>,
    /// Stable `users.id` of whoever this task belongs to (session creator, or
    /// the parent epic's creator for Planner-spawned tasks). `None` for tasks
    /// with no human owner. Resolve to a display name via the org user list.
    pub created_by_user_id: Option<String>,
    /// Set when force_close unblocks downstream tasks that may need replacement blockers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Current-head required-CI gate snapshot for this task's PR, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiGateSnapshot>,
    /// Top-level alias for `ci.status` (`passing`, `failing`, `pending`, or
    /// `unknown`) sourced from the durable current-head CI snapshot.
    pub ci_status: CiStatus,
    /// Top-level alias for `ci.gate_state`, including `awaiting_ci` for
    /// `pr_draft` + pending/unknown.
    pub ci_gate_state: CiGateState,
    /// The required check/job to triage first — the earliest-started blocking
    /// lane that actually executed and hard-failed. Never a cancelled lane and
    /// never a `needs:`-dependent aggregator that did not execute; both are
    /// symptoms of a run-level abort rather than causes. Absent when the run
    /// was inconclusive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_primary_blocking_check: Option<String>,
    /// Bounded rendering of the GitHub annotations on
    /// `ci_primary_blocking_check`. Runner-host failures (out of disk, runner
    /// crash) surface only as annotations, so this is often the only place the
    /// real cause appears.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_failure_annotations: Option<String>,
    /// Human-readable structured CI summary reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_summary_reason: Option<String>,
    /// Structured merge/close blocking reason when current-head CI is not passing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_merge_blocked_reason: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskShowResponse {
    #[serde(flatten)]
    pub task: TaskResponse,
    pub session_count: i64,
    pub active_session: Option<SessionRecordResponse>,
}

/// Lightweight session info included in task_list responses.
#[derive(Clone, Serialize)]
pub struct ActiveSessionSummary {
    pub session_id: String,
    pub agent_type: String,
    pub model_id: String,
    pub started_at: String,
    pub status: String,
}

impl schemars::JsonSchema for ActiveSessionSummary {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ActiveSessionSummary".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "agent_type": { "type": "string" },
                "model_id": { "type": "string" },
                "started_at": { "type": "string" },
                "status": { "type": "string" }
            },
            "required": ["session_id", "agent_type", "model_id", "started_at", "status"]
        })
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionRecordResponse {
    pub id: String,
    /// `None` for chat sessions (global, user-scoped); `Some(_)` for every
    /// other agent type. See `SessionRecord::project_id`.
    pub project_id: Option<String>,
    pub task_id: String,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Running totals of prompt-cache reads (hits) and writes (creation).
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Workspace path resolved from the session's attached `task_run`. `None`
    /// when no run is attached or the run has no recorded workspace.
    pub workspace_path: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskCountGroup {
    pub key: String,
    pub count: i64,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TaskCountSuccess {
    Groups { groups: Vec<TaskCountGroup> },
    TotalCount { total_count: i64 },
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskBlockerItemResponse {
    pub blocking_task_id: String,
    pub blocking_task_short_id: String,
    pub blocking_task_title: String,
    pub blocking_task_status: String,
    pub resolved: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskBlockersListResponse {
    pub blockers: Vec<TaskBlockerItemResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskBlockedItemResponse {
    pub task_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskBlockedListResponse {
    pub tasks: Vec<TaskBlockedItemResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskReadyResponse {
    pub tasks: Vec<TaskResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TaskClaimSuccess {
    Task(TaskResponse),
    NoTask { task: Option<TaskResponse> },
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ActivityEntryResponse {
    pub id: String,
    pub task_id: Option<String>,
    pub actor_id: String,
    pub actor_role: String,
    pub event_type: String,
    /// Renderer-friendly discriminator. Usually mirrors `event_type`; for
    /// structured activity payloads this is the semantic activity kind.
    pub kind: String,
    pub payload: AnyJson,
    /// Structured event details surfaced for operator UIs when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<AnyJson>,
    /// Human-readable event summary for activity-feed/timeline renderers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_at: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskActivityListResponse {
    pub entries: Vec<ActivityEntryResponse>,
    #[schemars(with = "i64")]
    pub count: usize,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthEpicStat {
    pub epic_id: String,
    pub short_id: String,
    pub title: String,
    pub total: i64,
    pub closed: i64,
    pub in_review: i64,
    /// Number of tasks in approved/pr_draft/pr_review states (PR pipeline).
    pub pr_ready: i64,
    pub pct_complete: f64,
    pub oldest_review_at: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthTaskItem {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: String,
    pub owner: String,
    pub epic_short_id: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthReviewItem {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: String,
    pub epic_short_id: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthRoleToolMismatchItem {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    pub issue_type: String,
    pub dispatched_role: String,
    pub expected_role: String,
    pub total_reopen_count: i64,
    pub session_count: i64,
    pub mismatch_signals: Vec<String>,
    pub reason: String,
    pub epic_short_id: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthLspWarning {
    /// e.g. "rust-analyzer", "typescript-language-server"
    pub server: String,
    /// Human-readable install instructions.
    pub message: String,
}

/// One bounded row of persisted liveness-classifier evidence surfaced via
/// `liveness_outcomes.recent`. The DB only returns a top-N slice plus counts,
/// so the model stays small even under incident volume.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthLivenessOutcomeItem {
    /// Verdict kind from the classifier taxonomy (e.g. `live`, `wedged`,
    /// `protocol_violation`).
    pub verdict: String,
    /// Coarse outcome bucket (`stalled`, `killed`, `protocol_violation`, …)
    /// — may be absent on rows persisted before the outcome column existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_kind: Option<String>,
    /// Machine-readable reason explaining the verdict/outcome (e.g.
    /// `idle_exceeded_threshold`, `no_progress_signals`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_reason: Option<String>,
    /// ISO-8601 UTC timestamp of when this evidence row was persisted.
    pub created_at: String,
    /// Task this evidence was attached to, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Session this evidence was attached to.
    pub session_id: String,
}

/// Bounded rollup of recent liveness-classifier outcomes on `board_health`.
/// `None` for payloads produced before this section existed (the field is
/// `#[serde(default)]` on the response, so old DB JSON deserializes cleanly).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthLivenessOutcomes {
    /// Total number of liveness-evidence rows surfaced in `recent`.
    pub total: i64,
    /// Count of surfaced outcomes grouped by `verdict` (e.g. `{"live": 12,
    /// "wedged": 3}`). The DB returns `HashMap<String, i64>`; absent keys
    /// are simply omitted from the rollup.
    #[serde(default)]
    pub by_verdict: HashMap<String, i64>,
    /// Bounded recent evidence rows (newest first).
    #[serde(default)]
    pub recent: Vec<BoardHealthLivenessOutcomeItem>,
}

/// One bounded row of protocol-violation evidence surfaced via
/// `protocol_violations.recent`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthProtocolViolationItem {
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_reason: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub session_id: String,
    /// Joined from `tasks.short_id` when the evidence row references a task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_short_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_status: Option<String>,
}

/// Bounded rollup of recent protocol-violation evidence on `board_health`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthProtocolViolations {
    pub total: i64,
    #[serde(default)]
    pub recent: Vec<BoardHealthProtocolViolationItem>,
}

/// Threshold configuration echoed on each stranded-ready finding so clients
/// can interpret severity without hard-coding the 30m/2x/6x ladder.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthStrandedThreshold {
    pub warning_minutes: i64,
    pub error_minutes: i64,
    pub critical_minutes: i64,
}

/// Dispatch-gate evaluation attached to a single stranded-ready finding.
/// The task's own `task_dispatch` build-lease row: the dispatcher's durable
/// record of a layer-1 build-admission attempt. Absent when the task has no
/// lease row, or when the ledger could not be read (see
/// `BoardHealthDispatchGateCoverage::unevaluated_gates`).
///
/// **Legacy population since the Kueue cutover.** The pre-create dispatch
/// reservation was stood down and nothing acquires a `task_dispatch` lease any
/// more, so this block is present only for rows that predate the cutover. Pool
/// occupancy is unaffected — `BoardHealthBuildCapacity` sums every consumer
/// kind, and the per-invocation cgroup lease still writes `task_invocation`
/// rows from inside the task-run Pod.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthBuildLease {
    /// `{task_id}:{generation}` — the lease key the dispatcher asked for.
    pub consumer_id: String,
    /// `queued`, `granted`, `launching`, `bound`, `active`, `suspect`, or
    /// `terminal`.
    pub state: String,
    /// Set only on a `terminal` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    /// Weighted capacity this row buys (0 for a non-double-charged escalation).
    pub weight: i64,
    pub enqueue_sequence: i64,
    /// Queued rows ahead of this one in the FIFO. Meaningful only while
    /// `state == "queued"`.
    pub queued_ahead: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Pool-wide build capacity as the dispatcher's own authority reads it.
/// Absent when the lease ledger could not be read.
///
/// The Kueue cutover deleted the pre-create admission ledger, so this is the
/// only occupancy authority **in this payload**. It is NOT the only gate that
/// can stop a build: Kueue's ClusterQueue now decides admission, and a Job it
/// has not admitted stays suspended with nothing recorded in `build_leases`.
/// `at_capacity: false` therefore still does not mean a dispatch can proceed —
/// read `BoardHealthKueueAdmission` for what Kueue itself decided.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthBuildCapacity {
    /// Always `build_leases`, so the payload names its own authority.
    #[serde(default)]
    pub authority: String,
    /// Weighted `SUM` over the occupying lease states, across every consumer
    /// kind (dispatch, invocation and warm all contend for one cap).
    pub occupancy: i64,
    /// `build_lease_caps.cap`.
    pub cap: i64,
    /// True only when the durable invocation-lease authority is armed to
    /// `enforce`. While it is off, shadowing, or absent entirely, the lease
    /// authority writes no dispatch rows and cannot be denying anything, so a
    /// full pool is not attributed to it. An absent authority row reports a real
    /// `false` here rather than making the whole capacity block unobservable.
    ///
    /// **Renamed from `enforcing`.** The bare name read as "build admission is
    /// enforcing", which used to be a different authority entirely. On
    /// 2026-07-29 this block reported `{occupancy: 1, cap: 3, enforcing: true,
    /// at_capacity: false}` for five hours while the since-deleted
    /// build-admission controller denied every single dispatch before capacity
    /// was measured. The name is kept scoped to the lease FIFO so it can never
    /// be misread that way again.
    pub lease_authority_enforcing: bool,
    /// `occupancy >= cap` with a positive cap.
    pub at_capacity: bool,
    /// Human-readable restatement of the authority boundary.
    #[serde(default)]
    pub note: String,
}

/// A legacy denial row's cause and streak evidence, enriched with the live
/// global build-lease capacity projection at report time.
///
/// The persisted denial row establishes only why and how often the historical
/// decision was denied. `scope`, `authority`, `occupancy`, and `cap` are not
/// historical row values: they are supplied by the live `build_leases` ledger
/// projection. Kueue admission is a separate authority reported in
/// [`BoardHealthKueueAdmission`].
///
/// **No writer remains.** Legacy rows can still be surfaced, but their absence
/// is the steady state and says nothing about whether Kueue admitted a Job.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthBuildAdmissionDenial {
    /// Always `global`: occupancy and cap cover every project and consumer in
    /// the shared build-lease pool, not only the denied task's project.
    ///
    /// Defaults to `global` when deserializing payloads emitted before this
    /// additive field existed.
    #[serde(default = "global_build_admission_denial_scope")]
    pub scope: String,
    /// Always `build_leases`: the live lease ledger is the authority for this
    /// report's occupancy and cap. It is neither the retired denial writer nor
    /// Kueue.
    ///
    /// Defaults to `build_leases` when deserializing payloads emitted before
    /// this additive field existed.
    #[serde(default = "build_leases_build_admission_denial_authority")]
    pub authority: String,
    /// `at_capacity`, `controller_not_admitting` or `authority_unavailable`.
    pub cause: String,
    /// The closed readiness gate, for `controller_not_admitting`. This is the
    /// field that existed only in container logs during the outage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<String>,
    /// The capacity authority's own words, for `authority_unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Report-time global weighted occupancy from `build_leases`, summed across
    /// every occupying lease and every project. `null` — never `0` — when the
    /// denial report has no live lease-ledger measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occupancy: Option<i64>,
    /// The singleton global `build_lease_caps.cap` paired with the report-time
    /// ledger-wide weighted occupancy; never a historical or project-scoped
    /// denial-row cap.
    pub cap: i64,
    /// The deciding process's admission epoch.
    #[serde(default)]
    pub server_epoch: String,
    /// Start of the uninterrupted denial streak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_denied_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied_at: Option<String>,
    /// Consecutive denials in this streak.
    pub denial_count: i64,
    /// Seconds since `denied_at`.
    pub age_seconds: i64,
    /// True while the record is recent enough to be blamed. A stale record is
    /// still reported — a denial row that outlives its condition is the #2661
    /// tombstone, and reporting-without-blaming is how it stays visible
    /// without becoming a false reason.
    pub fresh: bool,
    /// The staleness bound `fresh` is measured against.
    pub freshness_window_seconds: i64,
    #[serde(default)]
    pub note: String,
}

fn global_build_admission_denial_scope() -> String {
    "global".to_owned()
}

fn build_leases_build_admission_denial_authority() -> String {
    "build_leases".to_owned()
}

/// One row of the Kueue admission projection (migration 165) — Kueue's own
/// decision about one build Workload, as the leader's Workload reflector
/// observed it.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthKueueWorkload {
    /// The `task_runs.id` the Workload accounts for.
    pub task_run_id: String,
    /// `pending`, `admitted` or `finished`. Reversible between the first two:
    /// Kueue preempts admitted Workloads for quota and re-admits them later.
    pub admission: String,
    /// Kueue's own word for the state (`Preempted`, `ClusterQueueStopped`,
    /// `Deactivated`, ...). `null` is honest — Kueue offered no reason — never
    /// a stand-in for one that was lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The Workload object name, for `kubectl -n djinn describe workload <name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_name: Option<String>,
    /// Observed state CHANGES, not observations — a watch resync does not move
    /// this. A high count on a `pending` row is quota thrash.
    pub transitions: i64,
    /// The owning task, when a `task_runs` row exists to tie it to one.
    ///
    /// `null` is the NORMAL state of a genuinely-pending Workload: under
    /// create-then-admit the `task_runs` row is written by the in-pod
    /// supervisor, which cannot run until Kueue admits the Job. An entry is
    /// never attributed to a guessed task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_short_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Seconds since the Workload was first observed at all.
    pub first_seen_age_seconds: i64,
    /// Seconds since the last observation. Small while the reflector is alive.
    pub observed_age_seconds: i64,
}

/// What Kueue decided, read from the `kueue_workload_admission` projection.
///
/// This is a **different authority** from `BoardHealthBuildCapacity`. Since the
/// Kueue cutover the ClusterQueue decides build capacity, and a Job it has not
/// admitted sits suspended with no `build_leases` row at all — so a healthy
/// `build_capacity` used to be the last word available while the board was
/// wedged behind a quota nothing recorded. Migration 165 records it; this block
/// is where it surfaces.
///
/// **`projection_state` is the field to read first.** `no_workloads_observed`
/// means the relation is EMPTY, which is what `kueue.armed=false` — the shipped
/// default — looks like. It is explicitly not a stalled queue: nothing is
/// pending. It is equally not proof that Kueue admitted anything, because a
/// reflector that never started looks identical, which is why
/// `kueue_clusterqueue_admission` stays in
/// `BoardHealthDispatchGateCoverage::unevaluated_gates` in that state and moves
/// to `evaluated_gates` only under `observing`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthKueueAdmission {
    /// Always `kueue_workload_admission`, so the payload names its authority.
    #[serde(default)]
    pub authority: String,
    /// `no_workloads_observed` (relation empty) or `observing` (has rows).
    pub projection_state: String,
    pub total: i64,
    /// Build Workloads Kueue has NOT admitted: Jobs suspended behind
    /// ClusterQueue quota.
    pub pending: i64,
    pub admitted: i64,
    pub finished: i64,
    /// Rows with no `task_runs` row. Normal rather than alarming — see
    /// `BoardHealthKueueWorkload::task_id`.
    pub without_task_run: i64,
    /// Age of the stalest observation in the relation. A watch resync refreshes
    /// every row it replays, so a large value means nobody is watching Kueue,
    /// not that Kueue is idle. `null` when the relation is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalest_observation_age_seconds: Option<i64>,
    /// How long the longest-waiting pending Workload has been known about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending_first_seen_age_seconds: Option<i64>,
    /// Cap on `pending_task_runs`; the counts above it are exact regardless.
    #[serde(default)]
    pub pending_entry_limit: i64,
    /// The head of the pending queue, oldest-known first.
    #[serde(default)]
    pub pending_task_runs: Vec<BoardHealthKueueWorkload>,
    /// Human-readable restatement of the authority boundary.
    #[serde(default)]
    pub note: String,
}

/// What the dispatch-gate verdict actually covers.
///
/// This block exists because an empty `reasons` used to be indistinguishable
/// from "no gate was consulted". The stranded-ready section evaluates the gates
/// in `evaluated_gates`; the dispatcher applies many more, listed in
/// `unevaluated_gates`. `reasons` speaks for the former only.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthDispatchGateCoverage {
    /// Always `partial` — this section can never see the whole dispatch path.
    pub scope: String,
    /// Gates whose durable state this section read.
    #[serde(default)]
    pub evaluated_gates: Vec<String>,
    /// Gates the dispatcher applies that this section did not consult. Every
    /// entry is a way a task can be left queued while `gate_verdict` is
    /// `unexplained`.
    ///
    /// `kueue_clusterqueue_admission` moves between this list and
    /// `evaluated_gates` per call. It is here whenever
    /// `BoardHealthKueueAdmission::projection_state` is not `observing` — an
    /// empty projection is what both an unarmed cluster and a stopped reflector
    /// look like from Postgres — and moves to `evaluated_gates` once the
    /// projection has rows.
    #[serde(default)]
    pub unevaluated_gates: Vec<String>,
    /// Human-readable restatement of the bound, carried in the payload so an
    /// operator reading raw JSON cannot miss it.
    pub note: String,
}

/// Mirrors the DB-built JSON so callers can render the gate verdict and
/// surface machine-readable `reasons` (e.g. `no_eligible_model`,
/// `image_not_ready`, `build_lease_queued`, `build_pool_at_capacity`).
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthDispatchGate {
    /// Role the coordinator would dispatch this task to, derived from
    /// `task.status`/`task.issue_type`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluated_role: Option<String>,
    /// Toolset associated with `evaluated_role` (DB-derived; absent when
    /// the role has no registered toolset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset: Option<Vec<String>>,
    /// Currently chosen model for the task, when derivable from
    /// `dispatch_state.inflight_model_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requirement: Option<String>,
    /// True when no model was chosen or the chosen model is healthy /
    /// not in `unreachable`/`error`/`down`/`offline`/`unhealthy`.
    pub image_ready: bool,
    /// True when `dispatch_state.cooldown_until` is in the future.
    pub breaker_open: bool,
    /// True when a paused session exists for the task or a project/user
    /// `dispatch_pauses` row is active.
    pub manually_paused: bool,
    /// True when `dispatch_state.failure_streak >= 3` (rate-limit backoff).
    pub rate_limited: bool,
    /// True when a usable credential exists for the creator or as an
    /// org-shared fallback.
    pub credential_available: bool,
    /// The task's own `task_dispatch` build-lease row, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_lease: Option<BoardHealthBuildLease>,
    /// Pool-wide build capacity, when the lease ledger was readable. This is
    /// the only occupancy authority; pair it with `build_admission_denial`,
    /// which is what the dispatcher actually recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_capacity: Option<BoardHealthBuildCapacity>,
    /// The dispatcher's own recorded `DenialCause` for this task, when it has
    /// one. Absent means the most recent decision was not a denial: the
    /// permitted path deletes the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_admission_denial: Option<BoardHealthBuildAdmissionDenial>,
    /// What Kueue decided, pool-wide. Absent only when the projection could not
    /// be read — an EMPTY projection is still reported, as
    /// `projection_state: "no_workloads_observed"`, because "nothing is queued"
    /// and "nobody looked" are different answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kueue_admission: Option<BoardHealthKueueAdmission>,
    /// This task's own Kueue Workload, when the projection has a row that ties
    /// to a live `task_runs` row for it. A row that cannot be tied to one is
    /// never attributed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kueue_workload: Option<BoardHealthKueueWorkload>,
    /// Final gate verdict — `blocked` when an evaluated gate fired,
    /// `unexplained` when none did.
    ///
    /// **There is no `stranded` verdict.** It used to be emitted whenever
    /// `reasons` was empty, which for a task with no chosen model was
    /// structurally guaranteed — the payload asserted "nothing is wrong"
    /// about tasks the dispatcher was refusing for reasons it never
    /// consulted. `unexplained` says what an empty `reasons` actually means,
    /// and `coverage` says what it was allowed to look at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_verdict: Option<String>,
    /// Machine-readable gate reasons (`no_eligible_model`,
    /// `image_not_ready`, `build_lease_queued`, `build_lease_terminal`,
    /// `build_lease_occupied_without_session`, `build_pool_at_capacity`,
    /// `kueue_workload_pending`, `kueue_workload_admitted_without_session`).
    /// Always present in the serialized output (may be an empty array) so
    /// clients can rely on the key existing. Read it together with
    /// `coverage`: empty means "no EVALUATED gate fired".
    #[serde(default)]
    pub reasons: Vec<String>,
    /// The bound on `reasons` and `gate_verdict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<BoardHealthDispatchGateCoverage>,
    /// Last role the dispatcher actually attempted for this task, when
    /// known. Retained for backward compatibility with the initial
    /// board_health contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_dispatched_role: Option<String>,
    /// Future cooldown deadline, when set. Persists even when the
    /// breaker has cooled (`breaker_open == false`) so clients can see
    /// the most recent breaker state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<String>,
}

/// One stranded-ready finding: a ready/dispatchable task with no active
/// session and an unclaimed duration past the threshold.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthStrandedReadyFinding {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    pub owner: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_short_id: Option<String>,
    /// ISO-8601 UTC timestamp the task became unclaimed.
    pub unclaimed_since: String,
    /// `high` when derived from a recorded event (open transition, session
    /// release, or a blocker's close event); `low` for an `updated_at`-shaped
    /// fallback.
    pub unclaimed_since_confidence: String,
    /// Which signal produced `unclaimed_since`: `open_transition`,
    /// `session_release`, `blocker_cleared`, `blocker_task_updated_at`, or
    /// `task_updated_at`. The latest signal wins, so a task whose blocker
    /// merged at 04:24 reports strand time from 04:24 rather than from
    /// creation.
    #[serde(default)]
    pub unclaimed_since_basis: String,
    /// Elapsed minutes between `unclaimed_since` and the DB clock at
    /// query time.
    pub elapsed_minutes: i64,
    /// `warning` (>=30m), `error` (>=60m), `critical` (>=180m).
    pub severity: String,
    pub threshold: BoardHealthStrandedThreshold,
    pub dispatch_gate: BoardHealthDispatchGate,
    /// Present when a visible dispatch gate has been explaining this task's
    /// non-dispatch for longer than
    /// `BoardHealthStrandedReady::gate_exclusion_bound_minutes`.
    ///
    /// Such a task would otherwise have been excluded from this section
    /// entirely. It is reported instead, with the gate as evidence: a gate is
    /// a claim about transience, and a claim that has held for hours is not
    /// evidence of health. Absent on every task no gate is suppressing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_escalation: Option<BoardHealthGateEscalation>,
}

/// Why a stranded-ready finding was reported despite a gate that would
/// ordinarily have excluded it.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthGateEscalation {
    /// Always `true` when present. A block that says `false` is not an
    /// escalation and must not be read as one.
    pub escalated: bool,
    /// Which gate(s) were overridden: `breaker_cooldown`,
    /// `rate_limit_backoff`, `owner_credential`. A deliberate operator pause
    /// (`manual_dispatch_pause`) is never bounded and never appears here.
    #[serde(default)]
    pub overridden_gates: Vec<String>,
    /// The same strand clock as `elapsed_minutes` — there is only one.
    pub suppressed_minutes: i64,
    /// The bound the suppression exceeded.
    pub bound_minutes: i64,
    /// `bound_minutes` as a multiple of the base stranded threshold.
    #[serde(default)]
    pub bound_multiple: i64,
    /// Row evidence supporting the escalation (`cooldown_until`,
    /// `failure_streak`, `inflight_model_id`, `last_dispatched_role`,
    /// `has_owner_credential`).
    #[serde(default)]
    pub evidence: serde_json::Value,
    /// One human-readable line naming the gate, the model and the duration.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub note: String,
}

/// Bounded rollup of stranded-ready findings on `board_health`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthStrandedReady {
    pub total: i64,
    /// Base threshold (30 minutes) used to derive severity.
    pub threshold_minutes: i64,
    /// How long a visible dispatch gate may keep excusing a task before the
    /// task is reported anyway with the gate as evidence (180 minutes = 6× the
    /// base threshold). A manual operator pause is exempt.
    #[serde(default)]
    pub gate_exclusion_bound_minutes: i64,
    #[serde(default)]
    pub findings: Vec<BoardHealthStrandedReadyFinding>,
}

/// Bounded rollup of closed-parent orphan findings on `board_health`.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthClosedParentOpenChildren {
    pub total: i64,
    #[serde(default)]
    pub findings: Vec<BoardHealthClosedParentOpenChildrenFinding>,
}

/// One closed-parent orphan finding with evidence for a later repair snapshot.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthClosedParentOpenChildrenFinding {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_short_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_status: Option<String>,
    #[serde(default)]
    pub terminal_epic_ids: Vec<String>,
    #[serde(default)]
    pub terminal_proposal_ids: Vec<String>,
    #[serde(default)]
    pub other_open_parent_ids: Vec<String>,
    #[serde(default)]
    pub external_open_dependents: Vec<BoardHealthClosedParentDependent>,
    pub recommended_action: String,
    pub recommended_status: String,
    pub recommended_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_disposition: Option<AnyJson>,
}

/// External open task that depends on a closed-parent orphan.
#[derive(Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BoardHealthClosedParentDependent {
    pub task_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthResponse {
    pub epic_stats: Vec<BoardHealthEpicStat>,
    pub stale_tasks: Vec<BoardHealthTaskItem>,
    pub review_queue: Vec<BoardHealthReviewItem>,
    /// Current evaluator-classified phantom refinement runs across the board.
    #[serde(default)]
    pub refinement_phantom_active_count: i64,
    /// Materialized intents whose correlated role task is terminal and which
    /// have no durable successor.
    #[serde(default)]
    pub refinement_stalled_handoff_count: i64,
    /// Durable phantom-reap events committed within the database-time 24-hour window.
    #[serde(default)]
    pub refinement_phantom_reaps_24h: i64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub repeated_reopen_role_tool_mismatches: Vec<BoardHealthRoleToolMismatchItem>,
    pub stale_threshold_hours: i64,
    /// Per-project health issues blocking execution (project_id -> error message).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_issues: Option<HashMap<String, String>>,
    /// Missing LSP servers that should be installed for diagnostics to work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp_warnings: Option<Vec<BoardHealthLspWarning>>,
    /// Tasks merged per hour per epic in the past hour (epic_id → count).
    /// Exposed to the Architect for board health assessment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_throughput: Option<HashMap<String, usize>>,
    /// Health warnings (e.g. "github_not_connected") that do not block the
    /// query but indicate degraded operational readiness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
    /// Per-project PR creation errors (project_id → error message).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_errors: Option<HashMap<String, String>>,
    /// Bounded recent liveness-classifier outcomes (from epic 5ric
    /// persistence). `#[serde(default)]` keeps old DB payloads that
    /// pre-date this section deserializable.
    #[serde(default)]
    pub liveness_outcomes: BoardHealthLivenessOutcomes,
    /// Bounded recent protocol-violation evidence with a tasks LEFT JOIN.
    #[serde(default)]
    pub protocol_violations: BoardHealthProtocolViolations,
    /// Stranded-ready findings: ready/dispatchable tasks with no active
    /// session, unclaimed past the 30-minute threshold, with dispatch-gate
    /// evidence attached.
    #[serde(default)]
    pub stranded_ready: BoardHealthStrandedReady,
    /// Closed-parent orphans: non-closed tasks whose parent scopes are
    /// terminal, with terminal parent evidence, external-dependent evidence,
    /// and the recommended repair disposition.
    #[serde(default)]
    pub closed_parent_open_children: BoardHealthClosedParentOpenChildren,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardReconcileResponse {
    pub healed_tasks: i64,
    pub healed_task_ids: Vec<String>,
    pub recovered_tasks: i64,
    pub reviews_triggered: i64,
    pub stale_sessions_finalized: usize,
    pub stale_session_ids: Vec<String>,
    pub recovery_triggered: bool,
    pub stale_batch_worktrees_removed: usize,
    pub stale_batch_worktrees: Vec<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskMemoryRefsResponse {
    pub id: String,
    pub short_id: String,
    pub memory_refs: Vec<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskListItem {
    pub id: String,
    pub short_id: String,
    pub epic_id: Option<String>,
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub status: String,
    pub priority: i64,
    pub owner: String,
    pub labels: Vec<String>,
    pub memory_refs: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterionItem>,
    pub reopen_count: i64,
    pub continuation_count: i64,
    pub total_reopen_count: i64,
    pub intervention_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_intervention_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
    /// JSON metadata about an active merge conflict (files, branches).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Map<String, serde_json::Value>>")]
    pub merge_conflict_metadata: Option<AnyJson>,
    /// URL of the associated pull request, once one has been opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub unresolved_blocker_count: i64,
    /// Specialist role name assigned to this task, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_context: Option<djinn_core::models::TaskExecutionContext>,
    /// Stable `users.id` of whoever this task belongs to (session creator, or
    /// the parent epic's creator for Planner-spawned tasks). `None` for tasks
    /// with no human owner. Resolve to a display name via the org user list.
    pub created_by_user_id: Option<String>,
    /// Active running session for this task, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session: Option<ActiveSessionSummary>,
    /// Total number of sessions that have worked on this task.
    pub session_count: i64,
    /// Current-head required-CI gate snapshot for this task's PR, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiGateSnapshot>,
    /// Top-level alias for `ci.status` (`passing`, `failing`, `pending`, or
    /// `unknown`) sourced from the durable current-head CI snapshot.
    pub ci_status: CiStatus,
    /// Top-level alias for `ci.gate_state`, including `awaiting_ci` for
    /// `pr_draft` + pending/unknown.
    pub ci_gate_state: CiGateState,
    /// The required check/job to triage first — the earliest-started blocking
    /// lane that actually executed and hard-failed. Never a cancelled lane and
    /// never a `needs:`-dependent aggregator that did not execute; both are
    /// symptoms of a run-level abort rather than causes. Absent when the run
    /// was inconclusive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_primary_blocking_check: Option<String>,
    /// Bounded rendering of the GitHub annotations on
    /// `ci_primary_blocking_check`. Runner-host failures (out of disk, runner
    /// crash) surface only as annotations, so this is often the only place the
    /// real cause appears.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_failure_annotations: Option<String>,
    /// Human-readable structured CI summary reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_summary_reason: Option<String>,
    /// Structured merge/close blocking reason when current-head CI is not passing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_merge_blocked_reason: Option<String>,
}

// ── Conversion helpers ────────────────────────────────────────────────────────

pub fn parse_string_array(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub fn parse_acceptance_criteria_array(raw: &str) -> Vec<AcceptanceCriterionItem> {
    let parsed = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    parsed
        .into_iter()
        .map(|item| {
            serde_json::from_value::<AcceptanceCriterionItem>(item.clone())
                .unwrap_or_else(|_| AcceptanceCriterionItem::Text(item.to_string()))
        })
        .collect()
}

pub fn parse_any_json(raw: &str) -> AnyJson {
    AnyJson(serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({})))
}

pub fn task_to_response(t: &Task) -> TaskResponse {
    let ci = task_ci_gate_snapshot(t);
    TaskResponse {
        id: t.id.clone(),
        short_id: t.short_id.clone(),
        epic_id: t.epic_id.clone(),
        title: t.title.clone(),
        description: t.description.clone(),
        design: t.design.clone(),
        issue_type: t.issue_type.clone(),
        status: t.status.clone(),
        priority: t.priority,
        owner: t.owner.clone(),
        labels: parse_string_array(&t.labels),
        memory_refs: parse_string_array(&t.memory_refs),
        acceptance_criteria: parse_acceptance_criteria_array(&t.acceptance_criteria),
        reopen_count: t.reopen_count,
        continuation_count: t.continuation_count,
        total_reopen_count: t.total_reopen_count,
        intervention_count: t.intervention_count,
        last_intervention_at: t.last_intervention_at.clone(),
        created_at: t.created_at.clone(),
        updated_at: t.updated_at.clone(),
        closed_at: t.closed_at.clone(),
        close_reason: t.close_reason.clone(),
        merge_commit_sha: t.merge_commit_sha.clone(),
        merge_conflict_metadata: t
            .merge_conflict_metadata
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .map(AnyJson),
        pr_url: t.pr_url.clone(),
        agent_type: t.agent_type.clone(),
        execution_context: t.execution_context.clone(),
        created_by_user_id: Some(t.created_by_user_id.clone()),
        warning: None,
        ci_status: task_ci_status(t),
        ci_gate_state: task_ci_gate_state(t),
        ci_primary_blocking_check: ci.as_ref().and_then(|ci| ci.primary_blocking_check.clone()),
        ci_failure_annotations: ci.as_ref().and_then(|ci| ci.failure_annotations.clone()),
        ci_summary_reason: ci.as_ref().map(|ci| ci.summary_reason.clone()),
        ci_merge_blocked_reason: ci.as_ref().and_then(|ci| ci.merge_blocked_reason.clone()),
        ci,
    }
}

pub fn task_to_list_item(
    t: &Task,
    active_session: Option<ActiveSessionSummary>,
    session_count: i64,
) -> TaskListItem {
    let base = task_to_response(t);
    TaskListItem {
        id: base.id,
        short_id: base.short_id,
        epic_id: base.epic_id,
        title: base.title,
        description: base.description,
        design: base.design,
        issue_type: base.issue_type,
        status: base.status,
        priority: base.priority,
        owner: base.owner,
        labels: base.labels,
        memory_refs: base.memory_refs,
        acceptance_criteria: base.acceptance_criteria,
        reopen_count: base.reopen_count,
        continuation_count: base.continuation_count,
        total_reopen_count: base.total_reopen_count,
        intervention_count: base.intervention_count,
        last_intervention_at: base.last_intervention_at,
        created_at: base.created_at,
        updated_at: base.updated_at,
        closed_at: base.closed_at,
        close_reason: base.close_reason,
        merge_commit_sha: base.merge_commit_sha,
        merge_conflict_metadata: base.merge_conflict_metadata,
        pr_url: base.pr_url,
        unresolved_blocker_count: t.unresolved_blocker_count,
        agent_type: t.agent_type.clone(),
        execution_context: base.execution_context,
        created_by_user_id: base.created_by_user_id,
        active_session,
        session_count,
        ci_status: base.ci_status,
        ci_gate_state: base.ci_gate_state,
        ci_primary_blocking_check: base.ci_primary_blocking_check,
        ci_failure_annotations: base.ci_failure_annotations,
        ci_summary_reason: base.ci_summary_reason,
        ci_merge_blocked_reason: base.ci_merge_blocked_reason,
        ci: base.ci,
    }
}

pub fn not_found(id: &str) -> ErrorResponse {
    ErrorResponse {
        error: format!("task not found: {id}"),
    }
}

/// Validate and collect labels, returning the validated list or an error.
pub fn validate_labels(labels: &[String]) -> Result<Vec<String>, String> {
    validate_labels_count(labels.len())?;
    labels.iter().map(|l| validate_label(l)).collect()
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod types_tests;

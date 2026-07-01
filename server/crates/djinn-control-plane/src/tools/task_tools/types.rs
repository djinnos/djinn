use super::*;

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
    /// Primary blocking required check name, when CI is failing.
    ///
    /// `Some(_)` when `status == failing` and at least one required check
    /// failed.  This is the first element of `blocking_required_check_names`
    /// (sorted alphabetically) for compact display.  `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_blocking_check: Option<String>,
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
            CiStatus::Unknown => CiGateState::Unknown,
        }
    }
}

/// Build a `CiGateSnapshot` DTO from a task's repository-backed CI fields.
///
/// Returns `None` when no snapshot exists (signalled by an absent `head_sha`),
/// preserving backward-compatible optional/null behavior for tasks without a
/// PR snapshot.
pub fn task_ci_gate_snapshot(t: &Task) -> Option<CiGateSnapshot> {
    let head_sha = t.ci_head_sha.as_deref()?;
    let status = task_ci_status(t);
    let gate_state = task_ci_gate_state(t);
    let blocking_checks = parse_string_array(&t.ci_blocking_required_check_names);
    let primary_blocking_check = if status == CiStatus::Failing && !blocking_checks.is_empty() {
        // blocking_required_check_names is kept sorted alphabetically by
        // the pr_poller; the first element is the canonical primary blocker.
        Some(blocking_checks[0].clone())
    } else {
        None
    };
    let summary_reason = match status {
        CiStatus::Passing => "All required checks passed".to_string(),
        CiStatus::Failing => {
            if let Some(ref check) = primary_blocking_check {
                format!("Required check failing: {check}")
            } else {
                "Required checks failing".to_string()
            }
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
        failure_fingerprint: t.ci_failure_fingerprint.clone(),
        summary_reason,
        merge_blocked_reason,
        first_seen_at: t.ci_first_seen_at.clone().unwrap_or_default(),
        last_seen_at: t.ci_last_seen_at.clone().unwrap_or_default(),
        same_signature_count: t.ci_same_signature_count,
        last_remediation_base_sha: t.ci_last_remediation_base_sha.clone(),
        pr_number: t.ci_pr_number,
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
    /// Primary blocking required check/job when required CI is failing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_primary_blocking_check: Option<String>,
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
pub struct BoardHealthResponse {
    pub epic_stats: Vec<BoardHealthEpicStat>,
    pub stale_tasks: Vec<BoardHealthTaskItem>,
    pub review_queue: Vec<BoardHealthReviewItem>,
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
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoardHealthLspWarning {
    /// e.g. "rust-analyzer", "typescript-language-server"
    pub server: String,
    /// Human-readable install instructions.
    pub message: String,
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
    /// Primary blocking required check/job when required CI is failing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_primary_blocking_check: Option<String>,
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
        created_by_user_id: t.created_by_user_id.clone(),
        warning: None,
        ci_status: task_ci_status(t),
        ci_gate_state: task_ci_gate_state(t),
        ci_primary_blocking_check: ci.as_ref().and_then(|ci| ci.primary_blocking_check.clone()),
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
        created_by_user_id: base.created_by_user_id,
        active_session,
        session_count,
        ci_status: base.ci_status,
        ci_gate_state: base.ci_gate_state,
        ci_primary_blocking_check: base.ci_primary_blocking_check,
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

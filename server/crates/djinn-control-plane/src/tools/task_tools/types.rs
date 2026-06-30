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
#[derive(Serialize, schemars::JsonSchema)]
pub struct CiGateSnapshot {
    /// Current required-CI status for the PR head.
    pub status: CiStatus,
    /// Git SHA of the PR head this snapshot describes.
    pub head_sha: String,
    /// Names of required checks that are currently failing and blocking merge.
    pub blocking_required_check_names: Vec<String>,
    /// Stable fingerprint of the current failure signature (e.g. sorted
    /// failing check names + head SHA). `None` when not failing.
    pub failure_fingerprint: Option<String>,
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

/// Build a `CiGateSnapshot` DTO from a task's repository-backed CI fields.
///
/// Returns `None` when no snapshot exists (signalled by an absent `head_sha`),
/// preserving backward-compatible optional/null behavior for tasks without a
/// PR snapshot.
pub fn task_ci_gate_snapshot(t: &Task) -> Option<CiGateSnapshot> {
    let head_sha = t.ci_head_sha.as_deref()?;
    Some(CiGateSnapshot {
        status: CiStatus::parse(&t.ci_status).unwrap_or(CiStatus::Unknown),
        head_sha: head_sha.to_string(),
        blocking_required_check_names: parse_string_array(&t.ci_blocking_required_check_names),
        failure_fingerprint: t.ci_failure_fingerprint.clone(),
        first_seen_at: t.ci_first_seen_at.clone().unwrap_or_default(),
        last_seen_at: t.ci_last_seen_at.clone().unwrap_or_default(),
        same_signature_count: t.ci_same_signature_count,
        last_remediation_base_sha: t.ci_last_remediation_base_sha.clone(),
        pr_number: None,
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
        ci: task_ci_gate_snapshot(t),
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
mod tests {
    use super::*;
    use djinn_core::models::Task;

    fn task_with_merge_commit_sha(merge_commit_sha: Option<&str>) -> Task {
        Task {
            id: "task-1".into(),
            project_id: "project-1".into(),
            short_id: "T-1".into(),
            epic_id: Some("epic-1".into()),
            title: "Landed work".into(),
            description: "".into(),
            design: "".into(),
            issue_type: "task".into(),
            status: "closed".into(),
            priority: 0,
            owner: "".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-06-14T00:00:00Z".into(),
            updated_at: "2026-06-14T00:01:00Z".into(),
            closed_at: Some("2026-06-14T00:02:00Z".into()),
            close_reason: Some("completed".into()),
            merge_commit_sha: merge_commit_sha.map(str::to_owned),
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: None,
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn task_list_item_serialization_preserves_merge_commit_sha() {
        let sha = "abc123def4567890abc123def4567890abc123de";
        let task = task_with_merge_commit_sha(Some(sha));

        let list_item = task_to_list_item(&task, None, 0);
        let serialized = serde_json::to_value(&list_item).unwrap();

        assert_eq!(list_item.merge_commit_sha.as_deref(), Some(sha));
        assert_eq!(serialized["merge_commit_sha"], sha);
    }

    fn task_with_ci_snapshot() -> Task {
        let mut task = task_with_merge_commit_sha(None);
        task.ci_status = "failing".into();
        task.ci_head_sha = Some("deadbeefcafebabe00000000000000000000ffff".into());
        task.ci_blocking_required_check_names =
            r#"["Server Size Guard","clippy"]"#.into();
        task.ci_failure_fingerprint = Some("sha:deadbeef|checks:clippy,size".into());
        task.ci_first_seen_at = Some("2026-06-14T00:00:00Z".into());
        task.ci_last_seen_at = Some("2026-06-14T00:05:00Z".into());
        task.ci_same_signature_count = 3;
        task.ci_last_remediation_base_sha = Some("base1234567890".into());
        task
    }

    #[test]
    fn task_response_exposes_ci_gate_snapshot_when_present() {
        let task = task_with_ci_snapshot();
        let response = task_to_response(&task);
        let serialized = serde_json::to_value(&response).unwrap();

        let ci = serialized["ci"].as_object().expect("ci should be an object");
        assert_eq!(ci["status"], "failing");
        assert_eq!(ci["head_sha"], "deadbeefcafebabe00000000000000000000ffff");
        assert_eq!(ci["blocking_required_check_names"][0], "Server Size Guard");
        assert_eq!(ci["blocking_required_check_names"][1], "clippy");
        assert_eq!(ci["failure_fingerprint"], "sha:deadbeef|checks:clippy,size");
        assert_eq!(ci["first_seen_at"], "2026-06-14T00:00:00Z");
        assert_eq!(ci["last_seen_at"], "2026-06-14T00:05:00Z");
        assert_eq!(ci["same_signature_count"], 3);
        assert_eq!(ci["last_remediation_base_sha"], "base1234567890");
        // pr_number is skipped when None (not otherwise available from Task row).
        assert!(ci.get("pr_number").is_none());
    }

    #[test]
    fn task_response_omits_ci_when_snapshot_absent() {
        // Default task has ci_head_sha = None → no snapshot.
        let task = task_with_merge_commit_sha(None);
        let response = task_to_response(&task);
        let serialized = serde_json::to_value(&response).unwrap();

        assert!(serialized.get("ci").is_none() || serialized["ci"].is_null());
        assert!(response.ci.is_none());
    }

    #[test]
    fn task_list_item_exposes_ci_gate_snapshot_when_present() {
        let task = task_with_ci_snapshot();
        let list_item = task_to_list_item(&task, None, 0);
        let serialized = serde_json::to_value(&list_item).unwrap();

        assert_eq!(serialized["ci"]["status"], "failing");
        assert_eq!(
            serialized["ci"]["head_sha"],
            "deadbeefcafebabe00000000000000000000ffff"
        );
    }

    #[test]
    fn ci_status_enum_serializes_to_snake_case_wire_values() {
        assert_eq!(
            serde_json::to_value(CiStatus::Passing).unwrap(),
            "passing"
        );
        assert_eq!(
            serde_json::to_value(CiStatus::Failing).unwrap(),
            "failing"
        );
        assert_eq!(
            serde_json::to_value(CiStatus::Pending).unwrap(),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(CiStatus::Unknown).unwrap(),
            "unknown"
        );
    }
}

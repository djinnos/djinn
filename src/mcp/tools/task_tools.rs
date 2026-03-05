// MCP tools for task board operations (CRUD, listing, queries).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::{
    ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository,
};
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::AnyJson;
use crate::mcp::tools::validation::{
    validate_ac_count, validate_actor_id, validate_actor_role, validate_body, validate_description,
    validate_design, validate_issue_type, validate_label, validate_labels_count, validate_limit,
    validate_offset, validate_owner, validate_priority, validate_reason, validate_sort,
    validate_title,
};
use crate::models::session::SessionStatus;
use crate::models::task::{Task, TaskStatus, TransitionAction};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn not_found(id: &str) -> ErrorResponse {
    ErrorResponse {
        error: format!("task not found: {id}"),
    }
}

/// Validate and collect labels, returning the validated list or an error.
fn validate_labels(labels: &[String]) -> Result<Vec<String>, String> {
    validate_labels_count(labels.len())?;
    labels.iter().map(|l| validate_label(l)).collect()
}

// ── Param / response structs ─────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCreateParams {
    /// Absolute project path.
    pub project: String,
    /// Parent epic ID — UUID or short_id (required).
    pub epic_id: Option<String>,
    pub title: String,
    /// Task type: "task" (default), "feature", or "bug".
    pub issue_type: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub labels: Option<Vec<String>>,
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
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
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AcceptanceCriterionItem {
    Text(String),
    Structured(AcceptanceCriterionStatus),
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct AcceptanceCriterionStatus {
    pub criterion: String,
    #[serde(default)]
    pub met: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskShowParams {
    /// Absolute project path. Optional — task IDs are globally unique.
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
pub struct TaskBlockersAddParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id (the task being blocked).
    pub id: String,
    /// Task UUID or short_id of the task that blocks it.
    pub blocking_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersRemoveParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id (the blocked task).
    pub id: String,
    /// Task UUID or short_id of the blocker to remove.
    pub blocking_id: String,
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
    /// Absolute project path. Optional — task IDs are globally unique.
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
    /// Task UUID or short_id (optional — omit to query all tasks).
    pub id: Option<String>,
    /// Filter by event_type (e.g. "status_changed", "comment").
    pub event_type: Option<String>,
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
    /// Transition action: accept, start, submit_task_review, task_review_start,
    /// task_review_reject, task_review_reject_conflict, task_review_approve,
    /// reopen, close, release, release_task_review, force_close,
    /// user_override.
    pub action: String,
    /// Required for: task_review_reject, task_review_reject_conflict,
    /// reopen, release, release_task_review, force_close.
    pub reason: Option<String>,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
    /// Required when action = "user_override". Allowed values: draft, open, needs_task_review,
    /// in_task_review, in_progress, closed.
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
    fn new(error: impl Into<String>) -> Self {
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
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskShowResponse {
    #[serde(flatten)]
    pub task: TaskResponse,
    pub session_count: i64,
    pub active_session: Option<SessionRecordResponse>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionRecordResponse {
    pub id: String,
    pub project_id: String,
    pub task_id: String,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub worktree_path: Option<String>,
    pub goose_session_id: Option<String>,
    pub continuation_of: Option<String>,
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
    pub payload: AnyJson,
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
pub struct BoardHealthResponse {
    pub epic_stats: Vec<BoardHealthEpicStat>,
    pub stale_tasks: Vec<BoardHealthTaskItem>,
    pub review_queue: Vec<BoardHealthReviewItem>,
    pub stale_threshold_hours: i64,
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
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unresolved_blocker_count: i64,
}

fn parse_string_array(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn parse_acceptance_criteria_array(raw: &str) -> Vec<AcceptanceCriterionItem> {
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

fn parse_any_json(raw: &str) -> AnyJson {
    AnyJson(serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({})))
}

fn task_to_response(t: &Task) -> TaskResponse {
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
        created_at: t.created_at.clone(),
        updated_at: t.updated_at.clone(),
        closed_at: t.closed_at.clone(),
        close_reason: t.close_reason.clone(),
        merge_commit_sha: t.merge_commit_sha.clone(),
    }
}

fn task_to_list_item(t: &Task) -> TaskListItem {
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
        created_at: base.created_at,
        updated_at: base.updated_at,
        closed_at: base.closed_at,
        close_reason: base.close_reason,
        merge_commit_sha: base.merge_commit_sha,
        unresolved_blocker_count: t.unresolved_blocker_count,
    }
}

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = task_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new work item (task, feature, or bug) under an epic.
    #[tool(
        description = "Create a new work item (task, feature, or bug) under an epic. Accepts epic_id as UUID or short_id."
    )]
    pub async fn task_create(
        &self,
        Parameters(p): Parameters<TaskCreateParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        // Validate fields.
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e))),
        };
        let description = p.description.as_deref().unwrap_or("");
        if let Err(e) = validate_description(description) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let design = p.design.as_deref().unwrap_or("");
        if let Err(e) = validate_design(design) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let issue_type = p.issue_type.as_deref().unwrap_or("task");
        if let Err(e) = validate_issue_type(issue_type) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let priority = p.priority.unwrap_or(0);
        if let Err(e) = validate_priority(priority) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let owner = match validate_owner(p.owner.as_deref().unwrap_or("")) {
            Ok(o) => o,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e))),
        };
        if let Some(ref labels) = p.labels {
            if let Err(e) = validate_labels(labels) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(ref ac) = p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }

        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        // Resolve parent epic (optional).
        let epic_id = if let Some(epic_ref) = p.epic_id.as_deref() {
            let epic_repo =
                EpicRepository::new(self.state.db().clone(), self.state.events().clone());
            let Some(epic) = epic_repo
                .resolve_in_project(&project_id, epic_ref)
                .await
                .ok()
                .flatten()
            else {
                return Json(ErrorOr::Error(ErrorResponse::new(format!(
                    "epic not found: {epic_ref}"
                ))));
            };
            Some(epic.id)
        } else {
            None
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let task = match repo
            .create_in_project(
                &project_id,
                epic_id.as_deref(),
                &title,
                description,
                design,
                issue_type,
                priority,
                &owner,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        };

        // Apply labels / ac if provided.
        let has_labels = p.labels.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
        let has_ac = p
            .acceptance_criteria
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        if has_labels || has_ac {
            let labels_json = p
                .labels
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
                .unwrap_or_else(|| task.labels.clone());
            let ac_json = p
                .acceptance_criteria
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
                .unwrap_or_else(|| task.acceptance_criteria.clone());

            let updated = repo
                .update(
                    &task.id,
                    &task.title,
                    &task.description,
                    &task.design,
                    task.priority,
                    &task.owner,
                    &labels_json,
                    &ac_json,
                )
                .await;
            if let Ok(t) = updated {
                return Json(ErrorOr::Ok(task_to_response(&t)));
            }
        }

        Json(ErrorOr::Ok(task_to_response(&task)))
    }

    /// Update allowed fields of a work item.
    #[tool(
        description = "Update allowed fields of a work item (title, description, acceptance_criteria, design, priority, owner, labels, epic_id). Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_update(
        &self,
        Parameters(p): Parameters<TaskUpdateParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        // Validate provided fields.
        if let Some(ref t) = p.title {
            if let Err(e) = validate_title(t) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(ref d) = p.description {
            if let Err(e) = validate_description(d) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(ref d) = p.design {
            if let Err(e) = validate_design(d) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(prio) = p.priority {
            if let Err(e) = validate_priority(prio) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(ref o) = p.owner {
            if let Err(e) = validate_owner(o) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(ref add) = p.labels_add {
            if let Err(e) = validate_labels(add) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(ref ac) = p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };

        // Resolve new parent epic if provided.
        let epic_id: Option<String> = if let Some(ref par) = p.epic_id {
            let epic_repo =
                EpicRepository::new(self.state.db().clone(), self.state.events().clone());
            let Some(epic) = epic_repo
                .resolve_in_project(&project_id, par)
                .await
                .ok()
                .flatten()
            else {
                return Json(ErrorOr::Error(ErrorResponse::new(format!(
                    "epic not found: {par}"
                ))));
            };
            Some(epic.id)
        } else {
            task.epic_id.clone()
        };

        // Apply partial field overrides.
        let title = p.title.as_deref().unwrap_or(&task.title);
        let description = p.description.as_deref().unwrap_or(&task.description);
        let design = p.design.as_deref().unwrap_or(&task.design);
        let priority = p.priority.unwrap_or(task.priority);
        let owner = p.owner.as_deref().unwrap_or(&task.owner);

        // Merge label changes.
        let labels_json = if p.labels_add.is_some() || p.labels_remove.is_some() {
            let mut current: Vec<String> = serde_json::from_str(&task.labels).unwrap_or_default();
            if let Some(add) = &p.labels_add {
                for lbl in add {
                    if !current.contains(lbl) {
                        current.push(lbl.clone());
                    }
                }
            }
            if let Some(remove) = &p.labels_remove {
                current.retain(|l| !remove.contains(l));
            }
            if let Err(e) = validate_labels_count(current.len()) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
            serde_json::to_string(&current).unwrap_or_else(|_| "[]".into())
        } else {
            task.labels.clone()
        };

        let ac_json = p
            .acceptance_criteria
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| task.acceptance_criteria.clone());

        // If parent changed, move the task first.
        if epic_id != task.epic_id {
            if let Err(e) = repo.move_to_epic(&task.id, epic_id.as_deref()).await {
                return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
            }
        }

        let updated = match repo
            .update(
                &task.id,
                title,
                description,
                design,
                priority,
                owner,
                &labels_json,
                &ac_json,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        };

        // Apply memory_refs changes if requested.
        if p.memory_refs_add.is_some() || p.memory_refs_remove.is_some() {
            let mut refs: Vec<String> =
                serde_json::from_str(&updated.memory_refs).unwrap_or_default();
            if let Some(add) = &p.memory_refs_add {
                for r in add {
                    if !refs.contains(r) {
                        refs.push(r.clone());
                    }
                }
            }
            if let Some(remove) = &p.memory_refs_remove {
                refs.retain(|r| !remove.contains(r));
            }
            let refs_json = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".into());
            match repo.update_memory_refs(&updated.id, &refs_json).await {
                Ok(t) => return Json(ErrorOr::Ok(task_to_response(&t))),
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            }
        }

        Json(ErrorOr::Ok(task_to_response(&updated)))
    }

    /// Show details of a work item. Accepts task UUID or short_id.
    #[tool(
        description = "Show details of a work item including recent activity and blockers. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_show(
        &self,
        Parameters(p): Parameters<TaskShowParams>,
    ) -> Json<ErrorOr<TaskShowResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_result = if let Some(project) = &p.project {
            let project_id = match self.require_project_id(project).await {
                Ok(id) => id,
                Err(e) => return Json(ErrorOr::Error(e)),
            };
            repo.resolve_in_project(&project_id, &p.id).await
        } else {
            repo.resolve(&p.id).await
        };
        match task_result {
            Ok(Some(t)) => {
                let session_count = session_repo.count_for_task(&t.id).await.unwrap_or(0);
                let active_session = session_repo
                    .active_for_task(&t.id)
                    .await
                    .ok()
                    .flatten()
                    .map(|s| SessionRecordResponse {
                        id: s.id,
                        project_id: s.project_id,
                        task_id: s.task_id,
                        model_id: s.model_id,
                        agent_type: s.agent_type,
                        started_at: s.started_at,
                        ended_at: s.ended_at,
                        status: s.status,
                        tokens_in: s.tokens_in,
                        tokens_out: s.tokens_out,
                        worktree_path: s.worktree_path,
                        goose_session_id: s.goose_session_id,
                        continuation_of: s.continuation_of,
                    });
                Json(ErrorOr::Ok(TaskShowResponse {
                    task: task_to_response(&t),
                    session_count,
                    active_session,
                }))
            }
            Ok(None) => Json(ErrorOr::Error(not_found(&p.id))),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List work items with optional filters and offset-based pagination.
    #[tool(
        description = "List work items with optional filters and offset-based pagination. Returns {tasks[], total_count, limit, offset, has_more}."
    )]
    pub async fn task_list(
        &self,
        Parameters(p): Parameters<TaskListParams>,
    ) -> Json<TaskListResponse> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(_) => {
                let limit = validate_limit(p.limit.unwrap_or(25));
                let offset = validate_offset(p.offset.unwrap_or(0));
                return Json(TaskListResponse {
                    tasks: vec![],
                    total_count: 0,
                    limit,
                    offset,
                    has_more: false,
                });
            }
        };
        let sort = p.sort.as_deref().unwrap_or("priority");
        if let Err(e) = validate_sort(
            sort,
            &[
                "priority",
                "created",
                "created_desc",
                "updated",
                "updated_desc",
                "closed",
            ],
        ) {
            let limit = validate_limit(p.limit.unwrap_or(25));
            let offset = validate_offset(p.offset.unwrap_or(0));
            tracing::error!(error = %e, "task_list: invalid sort");
            return Json(TaskListResponse {
                tasks: vec![],
                total_count: 0,
                limit,
                offset,
                has_more: false,
            });
        }
        if let Some(prio) = p.priority {
            if let Err(e) = validate_priority(prio) {
                let limit = validate_limit(p.limit.unwrap_or(25));
                let offset = validate_offset(p.offset.unwrap_or(0));
                tracing::error!(error = %e, "task_list: invalid priority");
                return Json(TaskListResponse {
                    tasks: vec![],
                    total_count: 0,
                    limit,
                    offset,
                    has_more: false,
                });
            }
        }

        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let query = ListQuery {
            project_id: Some(project_id),
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority,
            label: p.label,
            text: p.text,
            parent: None,
            sort: sort.to_owned(),
            limit,
            offset,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_filtered(query).await {
            Ok(result) => {
                let tasks = result.tasks.iter().map(task_to_list_item).collect();
                Json(TaskListResponse {
                    has_more: offset + limit < result.total_count,
                    tasks,
                    total_count: result.total_count,
                    limit,
                    offset,
                })
            }
            Err(e) => {
                tracing::error!(error = %e, "task_list failed");
                Json(TaskListResponse {
                    tasks: vec![],
                    total_count: 0,
                    limit,
                    offset,
                    has_more: false,
                })
            }
        }
    }

    /// Count work items matching the filter, with optional grouping.
    #[tool(description = "Count work items matching the filter, with optional grouping.")]
    pub async fn task_count(
        &self,
        Parameters(p): Parameters<TaskCountParams>,
    ) -> Json<ErrorOr<TaskCountSuccess>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Some(ref gb) = p.group_by {
            if let Err(e) = validate_sort(gb, &["status", "priority", "issue_type", "epic"]) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(prio) = p.priority {
            if let Err(e) = validate_priority(prio) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }

        let query = CountQuery {
            project_id: Some(project_id),
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority,
            label: p.label,
            text: p.text,
            parent: None,
            group_by: p.group_by,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.count_grouped(query).await {
            Ok(v) => match serde_json::from_value::<TaskCountSuccess>(v) {
                Ok(parsed) => Json(ErrorOr::Ok(parsed)),
                Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            },
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Add a blocker relationship between two tasks. Rejects epics and circular dependencies.
    #[tool(
        description = "Add a blocker relationship: task 'id' is blocked by task 'blocking_id'. Both accept task ID (full UUID or short_id, e.g., 'k7m2'). Epics cannot participate in blocker relationships — both tasks must be non-epic (task, feature, or bug)."
    )]
    pub async fn task_blockers_add(
        &self,
        Parameters(p): Parameters<TaskBlockersAddParams>,
    ) -> Json<ErrorOr<OkResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_id = match self.resolve_task_not_epic(&project_id, &p.id).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let blocking_id = match self
            .resolve_task_not_epic(&project_id, &p.blocking_id)
            .await
        {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        match repo.add_blocker(&task_id, &blocking_id).await {
            Ok(()) => Json(ErrorOr::Ok(OkResponse { ok: true })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Remove a blocker relationship between two tasks.
    #[tool(
        description = "Remove a blocker relationship. Both task IDs accept (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blockers_remove(
        &self,
        Parameters(p): Parameters<TaskBlockersRemoveParams>,
    ) -> Json<ErrorOr<OkResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        let Some(blocker) = repo
            .resolve_in_project(&project_id, &p.blocking_id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.blocking_id)));
        };
        match repo.remove_blocker(&task.id, &blocker.id).await {
            Ok(()) => Json(ErrorOr::Ok(OkResponse { ok: true })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List tasks that block the given task.
    #[tool(
        description = "List tasks that block the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blockers_list(
        &self,
        Parameters(p): Parameters<TaskBlockersListParams>,
    ) -> Json<ErrorOr<TaskBlockersListResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        match repo.list_blockers(&task.id).await {
            Ok(refs) => {
                let blockers = refs
                    .iter()
                    .map(|b| TaskBlockerItemResponse {
                        blocking_task_id: b.task_id.clone(),
                        blocking_task_short_id: b.short_id.clone(),
                        blocking_task_title: b.title.clone(),
                        blocking_task_status: b.status.clone(),
                        resolved: b.status == "closed",
                    })
                    .collect();
                Json(ErrorOr::Ok(TaskBlockersListResponse { blockers }))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List tasks that are blocked by the given task.
    #[tool(
        description = "List task IDs that are blocked by the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blocked_list(
        &self,
        Parameters(p): Parameters<TaskBlockedListParams>,
    ) -> Json<ErrorOr<TaskBlockedListResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_result = if let Some(project) = &p.project {
            let project_id = match self.require_project_id(project).await {
                Ok(id) => id,
                Err(e) => return Json(ErrorOr::Error(e)),
            };
            repo.resolve_in_project(&project_id, &p.id).await
        } else {
            repo.resolve(&p.id).await
        };
        let Some(task) = task_result.ok().flatten() else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        match repo.list_blocked_by(&task.id).await {
            Ok(refs) => {
                let tasks = refs
                    .iter()
                    .map(|b| TaskBlockedItemResponse {
                        task_id: b.task_id.clone(),
                        short_id: b.short_id.clone(),
                        title: b.title.clone(),
                        status: b.status.clone(),
                    })
                    .collect();
                Json(ErrorOr::Ok(TaskBlockedListResponse { tasks }))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List work items ready to start (open status with no blocking dependencies).
    #[tool(
        description = "List work items ready to start (open status with no blocking dependencies)"
    )]
    pub async fn task_ready(
        &self,
        Parameters(p): Parameters<TaskReadyParams>,
    ) -> Json<ErrorOr<TaskReadyResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Some(ref o) = p.owner {
            if let Err(e) = validate_owner(o) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }
        if let Some(pmax) = p.priority_max {
            if let Err(e) = validate_priority(pmax) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let query = ReadyQuery {
            project_id: Some(project_id),
            issue_type: None,
            label: p.label,
            owner: p.owner,
            priority_max: p.priority_max,
            limit: validate_limit(p.limit.unwrap_or(25)),
        };
        match repo.list_ready(query).await {
            Ok(tasks) => Json(ErrorOr::Ok(TaskReadyResponse {
                tasks: tasks.iter().map(task_to_response).collect(),
            })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Transition a work item through the state machine.
    #[tool(
        description = "Transition a work item to a new status (e.g., open, in_progress, review, approve, close). Accepts task ID (full UUID or short_id, e.g., 'k7m2'). For user_override action, use target_status to specify the destination column."
    )]
    pub async fn task_transition(
        &self,
        Parameters(p): Parameters<TaskTransitionParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let actor_id = p.actor_id.as_deref().unwrap_or("");
        if let Err(e) = validate_actor_id(actor_id) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        if let Err(e) = validate_actor_role(actor_role) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref r) = p.reason {
            if let Err(e) = validate_reason(r) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };

        let action = match TransitionAction::parse(&p.action) {
            Ok(a) => a,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        };

        let target_override = if let Some(ref ts) = p.target_status {
            match TaskStatus::parse(ts) {
                Ok(s) => Some(s),
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            }
        } else {
            None
        };

        let reason = p.reason.as_deref();

        match repo
            .transition(
                &task.id,
                action,
                actor_id,
                actor_role,
                reason,
                target_override,
            )
            .await
        {
            Ok(updated) => Json(ErrorOr::Ok(task_to_response(&updated))),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Claim the next available work item and transition it to in_progress.
    #[tool(
        description = "Claim the next available work item (highest priority, oldest) and transition it to in_progress"
    )]
    pub async fn task_claim(
        &self,
        Parameters(p): Parameters<TaskClaimParams>,
    ) -> Json<ErrorOr<TaskClaimSuccess>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Some(ref o) = p.owner {
            if let Err(e) = validate_owner(o) {
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let query = ReadyQuery {
            project_id: Some(project_id),
            issue_type: None,
            label: p.label,
            owner: p.owner,
            priority_max: p.priority_max,
            limit: 1,
        };
        let actor_id = p.session_id.as_deref().unwrap_or("");
        match repo.claim(query, actor_id, "coordinator").await {
            Ok(Some(task)) => Json(ErrorOr::Ok(TaskClaimSuccess::Task(task_to_response(&task)))),
            Ok(None) => Json(ErrorOr::Ok(TaskClaimSuccess::NoTask { task: None })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Add a comment to a work item. Creates an activity_log entry with event_type='comment'.
    #[tool(
        description = "Add a comment to a work item. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_comment_add(
        &self,
        Parameters(p): Parameters<TaskCommentAddParams>,
    ) -> Json<ErrorOr<ActivityEntryResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Err(e) = validate_body(&p.body) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let actor_id = p.actor_id.as_deref().unwrap_or("");
        if let Err(e) = validate_actor_id(actor_id) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        if let Err(e) = validate_actor_role(actor_role) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };

        let payload = serde_json::json!({ "body": p.body }).to_string();

        match repo
            .log_activity(Some(&task.id), actor_id, actor_role, "comment", &payload)
            .await
        {
            Ok(entry) => Json(ErrorOr::Ok(ActivityEntryResponse {
                id: entry.id,
                task_id: entry.task_id,
                actor_id: entry.actor_id,
                actor_role: entry.actor_role,
                event_type: entry.event_type,
                payload: parse_any_json(&entry.payload),
                created_at: entry.created_at,
            })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Query the activity log with optional filters.
    #[tool(
        description = "List task activity log entries filtered by task_id, event_type, and/or time range."
    )]
    pub async fn task_activity_list(
        &self,
        Parameters(p): Parameters<TaskActivityListParams>,
    ) -> Json<ErrorOr<TaskActivityListResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        // If an id was supplied, resolve it to a full UUID.
        let task_id = if let Some(ref id) = p.id {
            match repo.resolve_in_project(&project_id, id).await {
                Ok(Some(t)) => Some(t.id),
                Ok(None) => return Json(ErrorOr::Error(not_found(id))),
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            }
        } else {
            None
        };

        let q = ActivityQuery {
            project_id: Some(project_id),
            task_id,
            event_type: p.event_type,
            from_time: p.from_time,
            to_time: p.to_time,
            limit: validate_limit(p.limit.unwrap_or(50)),
            offset: validate_offset(p.offset.unwrap_or(0)),
        };

        match repo.query_activity(q).await {
            Ok(entries) => {
                let items: Vec<ActivityEntryResponse> = entries
                    .iter()
                    .map(|e| ActivityEntryResponse {
                        id: e.id.clone(),
                        task_id: e.task_id.clone(),
                        actor_id: e.actor_id.clone(),
                        actor_role: e.actor_role.clone(),
                        event_type: e.event_type.clone(),
                        payload: parse_any_json(&e.payload),
                        created_at: e.created_at.clone(),
                    })
                    .collect();
                Json(ErrorOr::Ok(TaskActivityListResponse {
                    count: items.len(),
                    entries: items,
                }))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Board health report: epic progress, stale tasks, and review queue.
    #[tool(
        description = "Returns aggregate health report (total notes, broken links, orphan notes, stale notes by folder)."
    )]
    pub async fn board_health(
        &self,
        Parameters(p): Parameters<BoardHealthParams>,
    ) -> Json<ErrorOr<BoardHealthResponse>> {
        if let Err(e) = self.require_project_id(&p.project).await {
            return Json(ErrorOr::Error(e));
        }
        let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.board_health(stale_hours).await {
            Ok(report) => match serde_json::from_value::<BoardHealthResponse>(report) {
                Ok(parsed) => Json(ErrorOr::Ok(parsed)),
                Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            },
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Heal stale tasks and recover orphaned states.
    #[tool(
        description = "Trigger board reconciliation: heal stale tasks, recover stuck sessions, trigger overdue reviews, disable dead models, and reconcile phases. Returns action counts."
    )]
    pub async fn board_reconcile(
        &self,
        Parameters(p): Parameters<BoardReconcileParams>,
    ) -> Json<ErrorOr<BoardReconcileResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(supervisor) = self.state.supervisor().await else {
            return Json(ErrorOr::Error(ErrorResponse::new(
                "supervisor actor not initialized",
            )));
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return Json(ErrorOr::Error(ErrorResponse::new(
                "coordinator actor not initialized",
            )));
        };
        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo.reconcile(stale_hours).await {
            Ok(result) => {
                let running_sessions = match session_repo.list_active_in_project(&project_id).await
                {
                    Ok(sessions) => sessions,
                    Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
                };

                let mut finalized_stale_session_ids = Vec::new();
                for session in running_sessions {
                    let has_runtime_session = match supervisor.has_session(&session.task_id).await {
                        Ok(v) => v,
                        Err(e) => {
                            return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
                        }
                    };
                    if has_runtime_session {
                        continue;
                    }
                    if session_repo
                        .update(
                            &session.id,
                            SessionStatus::Interrupted,
                            session.tokens_in,
                            session.tokens_out,
                        )
                        .await
                        .is_ok()
                    {
                        finalized_stale_session_ids.push(session.id);
                    }
                }

                let recovery_triggered = if finalized_stale_session_ids.is_empty() {
                    false
                } else {
                    coordinator
                        .trigger_dispatch_for_project(&project_id)
                        .await
                        .is_ok()
                };

                // ── batch-* worktree cleanup (ADR-016) ───────────────────
                let mut stale_batch_worktrees: Vec<String> = Vec::new();
                let project_repo =
                    ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
                if let Ok(Some(project)) = project_repo.get(&project_id).await {
                    let project_path = std::path::PathBuf::from(&project.path);
                    let worktrees_dir = project_path.join(".djinn").join("worktrees");

                    // Collect active worktree paths from live supervisor sessions.
                    let active_worktree_paths: std::collections::HashSet<String> =
                        match supervisor.get_status().await {
                            Ok(status) => status
                                .running_sessions
                                .into_iter()
                                .filter_map(|s| s.worktree_path)
                                .collect(),
                            Err(_) => std::collections::HashSet::new(),
                        };

                    if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
                        let batch_dirs: Vec<std::path::PathBuf> = entries
                            .filter_map(|e| e.ok())
                            .filter(|e| {
                                e.file_name()
                                    .to_str()
                                    .map(|n| n.starts_with("batch-"))
                                    .unwrap_or(false)
                                    && e.path().is_dir()
                            })
                            .map(|e| e.path())
                            .collect();

                        if !batch_dirs.is_empty() {
                            if let Ok(git) = self.state.git_actor(&project_path).await {
                                for batch_dir in batch_dirs {
                                    let batch_str = batch_dir.display().to_string();
                                    if active_worktree_paths.contains(&batch_str) {
                                        continue;
                                    }
                                    tracing::info!(
                                        project_id = %project_id,
                                        worktree = %batch_dir.display(),
                                        "board_reconcile: removing stale batch-* worktree"
                                    );
                                    if let Err(e) = git.remove_worktree(&batch_dir).await {
                                        tracing::warn!(
                                            project_id = %project_id,
                                            worktree = %batch_dir.display(),
                                            error = %e,
                                            "board_reconcile: failed to remove stale batch worktree"
                                        );
                                    } else {
                                        stale_batch_worktrees.push(
                                            batch_dir
                                                .file_name()
                                                .unwrap_or_default()
                                                .to_string_lossy()
                                                .into_owned(),
                                        );
                                    }
                                }
                                if !stale_batch_worktrees.is_empty() {
                                    let _ = git
                                        .run_command(vec!["worktree".into(), "prune".into()])
                                        .await;
                                }
                            }
                        }
                    }
                }

                let mut parsed = match serde_json::from_value::<BoardReconcileResponse>(
                    serde_json::json!({
                        "healed_tasks": result.get("healed_tasks").cloned().unwrap_or(serde_json::json!(0)),
                        "healed_task_ids": result.get("healed_task_ids").cloned().unwrap_or(serde_json::json!([])),
                        "recovered_tasks": result.get("recovered_tasks").cloned().unwrap_or(serde_json::json!(0)),
                        "reviews_triggered": result.get("reviews_triggered").cloned().unwrap_or(serde_json::json!(0)),
                        "stale_sessions_finalized": finalized_stale_session_ids.len(),
                        "stale_session_ids": finalized_stale_session_ids,
                        "recovery_triggered": recovery_triggered,
                        "stale_batch_worktrees_removed": stale_batch_worktrees.len(),
                        "stale_batch_worktrees": stale_batch_worktrees,
                    }),
                ) {
                    Ok(v) => v,
                    Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
                };

                parsed.stale_sessions_finalized = parsed.stale_session_ids.len();
                parsed.stale_batch_worktrees_removed = parsed.stale_batch_worktrees.len();

                Json(ErrorOr::Ok(parsed))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List memory note permalinks associated with a task.
    #[tool(
        description = "List memory note permalinks associated with a task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_memory_refs(
        &self,
        Parameters(p): Parameters<TaskMemoryRefsParams>,
    ) -> Json<ErrorOr<TaskMemoryRefsResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        Json(ErrorOr::Ok(TaskMemoryRefsResponse {
            id: task.id,
            short_id: task.short_id,
            memory_refs: parse_string_array(&task.memory_refs),
        }))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    async fn require_project_id(
        &self,
        project: &str,
    ) -> std::result::Result<String, ErrorResponse> {
        self.resolve_project_id(project)
            .await
            .map_err(|e| ErrorResponse::new(e))
    }

    /// Resolve a task UUID/short_id to its UUID, rejecting epics with a clear error.
    async fn resolve_task_not_epic(
        &self,
        project_id: &str,
        id: &str,
    ) -> std::result::Result<String, ErrorResponse> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        if let Ok(Some(task)) = repo.resolve_in_project(project_id, id).await {
            return Ok(task.id);
        }
        // Not found in tasks — check if it's an epic to give a clearer error.
        if let Ok(Some(_)) =
            EpicRepository::new(self.state.db().clone(), self.state.events().clone())
                .resolve_in_project(project_id, id)
                .await
        {
            return Err(ErrorResponse::new(format!(
                "epics cannot participate in blocker relationships: {id}"
            )));
        }
        Err(ErrorResponse::new(format!("task not found: {id}")))
    }
}

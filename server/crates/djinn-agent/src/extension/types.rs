use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct IncomingToolCall {
    pub name: String,
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub(super) struct TaskListParams {
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    #[serde(alias = "q")]
    pub text: Option<String>,
    pub label: Option<String>,
    pub parent: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskShowParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskActivityListParams {
    pub id: String,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub actor_role: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskUpdateParams {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub labels_add: Option<Vec<String>>,
    pub labels_remove: Option<Vec<String>>,
    pub acceptance_criteria: Option<Vec<serde_json::Value>>,
    pub memory_refs_add: Option<Vec<String>>,
    pub memory_refs_remove: Option<Vec<String>>,
    #[serde(default)]
    pub blocked_by_add: Vec<String>,
    #[serde(default)]
    pub blocked_by_remove: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct TaskUpdateAcParams {
    pub id: String,
    pub acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct TaskCreateParams {
    pub epic_id: String,
    pub title: String,
    pub issue_type: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub status: Option<String>,
    pub acceptance_criteria: Option<Vec<serde_json::Value>>,
    pub blocked_by: Option<Vec<String>>,
    pub memory_refs: Option<Vec<String>>,
    /// Specialist role name to route this task (e.g. "rust-expert").
    pub agent_type: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct EpicShowParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct EpicUpdateParams {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub memory_refs_add: Option<Vec<String>>,
    pub memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct EpicTasksParams {
    pub id: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskCommentAddParams {
    pub id: String,
    pub body: String,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryReadParams {
    pub identifier: String,
}

#[derive(Deserialize)]
pub(super) struct MemorySearchParams {
    pub query: String,
    pub folder: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    pub limit: Option<i64>,
    pub task_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryListParams {
    pub folder: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    pub depth: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct MemoryBuildContextParams {
    pub url: String,
    /// Link traversal depth (default 1). Currently unused at the dispatch layer.
    pub _depth: Option<i64>,
    pub max_related: Option<i64>,
    pub budget: Option<i64>,
    pub task_id: Option<String>,
    pub min_confidence: Option<f64>,
}

#[derive(Deserialize)]
pub(super) struct MemoryWriteParams {
    pub title: String,
    pub content: String,
    #[serde(rename = "type")]
    pub note_type: String,
    pub tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct MemoryEditParams {
    pub identifier: String,
    pub operation: String,
    pub content: String,
    pub find_text: Option<String>,
    pub section: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryBrokenLinksLocalParams {
    pub folder: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryOrphansLocalParams {
    pub folder: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AgentAmendPromptParams {
    pub agent_id: String,
    pub amendment: String,
    pub metrics_snapshot: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ShellParams {
    pub command: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct WriteParams {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub(super) struct EditParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Deserialize)]
pub(super) struct ApplyPatchParams {
    pub patch: String,
}

#[derive(Deserialize)]
pub(super) struct ReadParams {
    #[serde(alias = "path")]
    pub file_path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

// ── Lead-only tool params ───────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct TaskTransitionParams {
    pub id: String,
    pub action: String,
    pub reason: Option<String>,
    pub target_status: Option<String>,
    /// Required when action = "force_close". UUIDs or short IDs of replacement
    /// tasks the Lead created before closing this one.
    pub replacement_task_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct TaskDeleteBranchParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskArchiveActivityParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskResetCountersParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskKillSessionParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct LspParams {
    pub operation: String,
    pub file_path: String,
    pub line: Option<u32>,
    pub character: Option<u32>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name_filter: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CodeGraphParams {
    pub operation: String,
    pub project_path: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub kind_filter: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct CiJobLogParams {
    pub job_id: u64,
    pub step: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GithubSearchParams {
    pub query: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

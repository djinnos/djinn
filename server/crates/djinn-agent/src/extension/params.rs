use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct RequestPmParams {
    pub(super) id: String,
    pub(super) reason: String,
    pub(super) suggested_breakdown: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RequestArchitectParams {
    pub(super) id: String,
    pub(super) reason: String,
}

#[derive(Deserialize)]
pub(super) struct TaskListParams {
    pub(super) status: Option<String>,
    pub(super) issue_type: Option<String>,
    pub(super) priority: Option<i64>,
    #[serde(alias = "q")]
    pub(super) text: Option<String>,
    pub(super) label: Option<String>,
    pub(super) parent: Option<String>,
    pub(super) sort: Option<String>,
    pub(super) limit: Option<i64>,
    pub(super) offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskShowParams {
    pub(super) id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskActivityListParams {
    pub(super) id: String,
    #[serde(default)]
    pub(super) event_type: Option<String>,
    #[serde(default)]
    pub(super) actor_role: Option<String>,
    #[serde(default)]
    pub(super) limit: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskUpdateParams {
    pub(super) id: String,
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    pub(super) design: Option<String>,
    pub(super) priority: Option<i64>,
    pub(super) owner: Option<String>,
    pub(super) labels_add: Option<Vec<String>>,
    pub(super) labels_remove: Option<Vec<String>>,
    pub(super) acceptance_criteria: Option<Vec<serde_json::Value>>,
    pub(super) memory_refs_add: Option<Vec<String>>,
    pub(super) memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct TaskUpdateAcParams {
    pub(super) id: String,
    pub(super) acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct TaskCreateParams {
    pub(super) epic_id: String,
    pub(super) title: String,
    pub(super) issue_type: Option<String>,
    pub(super) description: Option<String>,
    pub(super) design: Option<String>,
    pub(super) priority: Option<i64>,
    pub(super) owner: Option<String>,
    pub(super) status: Option<String>,
    pub(super) acceptance_criteria: Option<Vec<String>>,
    pub(super) blocked_by: Option<Vec<String>>,
    pub(super) memory_refs: Option<Vec<String>>,
    /// Specialist role name to route this task (e.g. "rust-expert").
    pub(super) agent_type: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct EpicShowParams {
    pub(super) id: String,
}

#[derive(Deserialize)]
pub(super) struct EpicUpdateParams {
    pub(super) id: String,
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    pub(super) status: Option<String>,
    pub(super) memory_refs_add: Option<Vec<String>>,
    pub(super) memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct EpicTasksParams {
    pub(super) id: String,
    pub(super) limit: Option<i64>,
    pub(super) offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskCommentAddParams {
    pub(super) id: String,
    pub(super) body: String,
    pub(super) actor_id: Option<String>,
    pub(super) actor_role: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryReadParams {
    pub(super) project: Option<String>,
    pub(super) identifier: String,
}

#[derive(Deserialize)]
pub(super) struct MemorySearchParams {
    pub(super) project: Option<String>,
    pub(super) query: String,
    pub(super) folder: Option<String>,
    #[serde(rename = "type")]
    pub(super) note_type: Option<String>,
    pub(super) limit: Option<i64>,
    pub(super) task_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryListParams {
    pub(super) project: Option<String>,
    pub(super) folder: Option<String>,
    #[serde(rename = "type")]
    pub(super) note_type: Option<String>,
    pub(super) depth: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct MemoryBuildContextParams {
    pub(super) project: Option<String>,
    pub(super) url: String,
    /// Link traversal depth (default 1). Currently unused at the dispatch layer.
    pub(super) _depth: Option<i64>,
    pub(super) max_related: Option<i64>,
    pub(super) budget: Option<i64>,
    pub(super) task_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RoleMetricsParams {
    pub(super) project: Option<String>,
    pub(super) role_id: Option<String>,
    pub(super) window_days: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RoleAmendPromptParams {
    pub(super) project: Option<String>,
    pub(super) role_id: String,
    pub(super) amendment: String,
    pub(super) metrics_snapshot: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ShellParams {
    pub(super) command: String,
    pub(super) timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct WriteParams {
    pub(super) path: String,
    pub(super) content: String,
}

#[derive(Deserialize)]
pub(super) struct EditParams {
    pub(super) path: String,
    pub(super) old_text: String,
    pub(super) new_text: String,
}

#[derive(Deserialize)]
pub(super) struct ApplyPatchParams {
    pub(super) patch: String,
}

#[derive(Deserialize)]
pub(super) struct ReadParams {
    #[serde(alias = "path")]
    pub(super) file_path: String,
    pub(super) offset: Option<usize>,
    pub(super) limit: Option<usize>,
}

#[expect(dead_code)]
#[derive(Deserialize)]
pub(super) struct LspParams {
    pub(super) operation: String,
    pub(super) file_path: String,
    pub(super) line: Option<u32>,
    pub(super) character: Option<u32>,
}

#[expect(dead_code)]
#[derive(Deserialize)]
pub(super) struct TaskTransitionParams {
    pub(super) id: String,
    pub(super) action: String,
    pub(super) reason: Option<String>,
    pub(super) target_status: Option<String>,
    /// Required when action = "force_close". UUIDs or short IDs of replacement
    /// tasks the Lead created before closing this one.
    pub(super) replacement_task_ids: Option<Vec<String>>,
}

#[expect(dead_code)]
#[derive(Deserialize)]
pub(super) struct TaskDeleteBranchParams {
    pub(super) id: String,
}

#[expect(dead_code)]
#[derive(Deserialize)]
pub(super) struct TaskArchiveActivityParams {
    pub(super) id: String,
}

#[expect(dead_code)]
#[derive(Deserialize)]
pub(super) struct TaskResetCountersParams {
    pub(super) id: String,
}

#[expect(dead_code)]
#[derive(Deserialize)]
pub(super) struct TaskKillSessionParams {
    pub(super) id: String,
}

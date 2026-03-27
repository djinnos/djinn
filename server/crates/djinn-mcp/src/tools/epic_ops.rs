use crate::tools::validation::{
    validate_color, validate_description, validate_emoji, validate_limit, validate_offset,
    validate_owner, validate_sort, validate_title,
};
use djinn_core::models::{Epic, Task};
use djinn_db::{EpicRepository, EpicTaskCounts, ListQuery, TaskRepository};
use serde::{Deserialize, Serialize};

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

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicModel {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub description: String,
    pub emoji: String,
    pub color: String,
    pub status: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub memory_refs: Vec<String>,
}

impl From<&Epic> for EpicModel {
    fn from(e: &Epic) -> Self {
        Self {
            id: e.id.clone(),
            short_id: e.short_id.clone(),
            title: e.title.clone(),
            description: e.description.clone(),
            emoji: e.emoji.clone(),
            color: e.color.clone(),
            status: e.status.clone(),
            owner: e.owner.clone(),
            created_at: e.created_at.clone(),
            updated_at: e.updated_at.clone(),
            closed_at: e.closed_at.clone(),
            memory_refs: parse_string_array(&e.memory_refs),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicWithCountsModel {
    #[serde(flatten)]
    pub epic: EpicModel,
    pub task_count: i64,
    pub open_count: i64,
    pub in_progress_count: i64,
    pub closed_count: i64,
}

impl EpicWithCountsModel {
    fn from_parts(epic: &Epic, counts: &EpicTaskCounts) -> Self {
        Self {
            epic: EpicModel::from(epic),
            task_count: counts.task_count,
            open_count: counts.open_count,
            in_progress_count: counts.in_progress_count,
            closed_count: counts.closed_count,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicTaskModel {
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
    pub verification_failure_count: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
}

impl From<&Task> for EpicTaskModel {
    fn from(t: &Task) -> Self {
        Self {
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
            verification_failure_count: t.verification_failure_count,
            created_at: t.created_at.clone(),
            updated_at: t.updated_at.clone(),
            closed_at: t.closed_at.clone(),
            close_reason: t.close_reason.clone(),
            merge_commit_sha: t.merge_commit_sha.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicShowRequest {
    pub project: String,
    pub id: String,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicShowResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub epic: Option<EpicWithCountsModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicUpdateRequest {
    pub project: String,
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub owner: Option<String>,
    pub memory_refs: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicUpdateDeltaRequest {
    pub project: String,
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub owner: Option<String>,
    pub memory_refs_add: Option<Vec<String>>,
    pub memory_refs_remove: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub epic: Option<EpicModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicTasksRequest {
    pub project: String,
    pub epic_id: String,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct EpicTasksResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<EpicTaskModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn epic_not_found_error(id: &str) -> String {
    format!("epic not found: {id}")
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

pub async fn epic_show(
    repo: &EpicRepository,
    project_id: &str,
    request: EpicShowRequest,
) -> EpicShowResponse {
    let Some(epic) = repo
        .resolve_in_project(project_id, &request.id)
        .await
        .ok()
        .flatten()
    else {
        return EpicShowResponse {
            epic: None,
            error: Some(epic_not_found_error(&request.id)),
        };
    };

    let counts = match repo.task_counts(&epic.id).await {
        Ok(counts) => counts,
        Err(error) => {
            return EpicShowResponse {
                epic: None,
                error: Some(error.to_string()),
            };
        }
    };

    EpicShowResponse {
        epic: Some(EpicWithCountsModel::from_parts(&epic, &counts)),
        error: None,
    }
}

pub async fn epic_update(
    repo: &EpicRepository,
    project_id: &str,
    request: EpicUpdateRequest,
) -> EpicSingleResponse {
    let Some(epic) = repo
        .resolve_in_project(project_id, &request.id)
        .await
        .ok()
        .flatten()
    else {
        return EpicSingleResponse {
            epic: None,
            error: Some(epic_not_found_error(&request.id)),
        };
    };

    let title = if let Some(ref title) = request.title {
        match validate_title(title) {
            Ok(value) => value,
            Err(error) => {
                return EpicSingleResponse {
                    epic: None,
                    error: Some(error),
                };
            }
        }
    } else {
        epic.title.clone()
    };

    let description = request.description.as_deref().unwrap_or(&epic.description);
    if let Err(error) = validate_description(description) {
        return EpicSingleResponse {
            epic: None,
            error: Some(error),
        };
    }

    let emoji = request.emoji.as_deref().unwrap_or(&epic.emoji);
    if let Err(error) = validate_emoji(emoji) {
        return EpicSingleResponse {
            epic: None,
            error: Some(error),
        };
    }

    let color = request.color.as_deref().unwrap_or(&epic.color);
    if let Err(error) = validate_color(color) {
        return EpicSingleResponse {
            epic: None,
            error: Some(error),
        };
    }

    let owner = if let Some(ref owner) = request.owner {
        match validate_owner(owner) {
            Ok(value) => value,
            Err(error) => {
                return EpicSingleResponse {
                    epic: None,
                    error: Some(error),
                };
            }
        }
    } else {
        epic.owner.clone()
    };

    let memory_refs_str = if let Some(ref refs) = request.memory_refs {
        serde_json::to_string(refs).unwrap_or_else(|_| "[]".to_string())
    } else {
        epic.memory_refs.clone()
    };

    match repo
        .update(
            &epic.id,
            djinn_db::EpicUpdateInput {
                title: &title,
                description,
                emoji,
                color,
                owner: &owner,
                memory_refs: Some(&memory_refs_str),
                status: None,
            },
        )
        .await
    {
        Ok(updated) => EpicSingleResponse {
            epic: Some(EpicModel::from(&updated)),
            error: None,
        },
        Err(error) => EpicSingleResponse {
            epic: None,
            error: Some(error.to_string()),
        },
    }
}

pub async fn epic_update_with_delta(
    repo: &EpicRepository,
    project_id: &str,
    request: EpicUpdateDeltaRequest,
) -> EpicSingleResponse {
    let Some(epic) = repo
        .resolve_in_project(project_id, &request.id)
        .await
        .ok()
        .flatten()
    else {
        return EpicSingleResponse {
            epic: None,
            error: Some(epic_not_found_error(&request.id)),
        };
    };

    let mut memory_refs: Vec<String> = serde_json::from_str(&epic.memory_refs).unwrap_or_default();
    if let Some(add) = &request.memory_refs_add {
        for item in add {
            if !memory_refs.contains(item) {
                memory_refs.push(item.clone());
            }
        }
    }
    if let Some(remove) = &request.memory_refs_remove {
        memory_refs.retain(|item| !remove.contains(item));
    }

    epic_update(
        repo,
        project_id,
        EpicUpdateRequest {
            project: request.project,
            id: request.id,
            title: request.title,
            description: request.description,
            emoji: request.emoji,
            color: request.color,
            owner: request.owner,
            memory_refs: Some(memory_refs),
        },
    )
    .await
}

pub async fn epic_tasks(
    epic_repo: &EpicRepository,
    task_repo: &TaskRepository,
    project_id: &str,
    request: EpicTasksRequest,
) -> EpicTasksResponse {
    let Some(epic) = epic_repo
        .resolve_in_project(project_id, &request.epic_id)
        .await
        .ok()
        .flatten()
    else {
        return EpicTasksResponse {
            tasks: None,
            total_count: None,
            limit: None,
            offset: None,
            has_more: None,
            error: Some(epic_not_found_error(&request.epic_id)),
        };
    };

    let sort = request.sort.as_deref().unwrap_or("priority");
    if let Err(error) = validate_sort(
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
        return EpicTasksResponse {
            tasks: None,
            total_count: None,
            limit: None,
            offset: None,
            has_more: None,
            error: Some(error),
        };
    }

    let limit = validate_limit(request.limit.unwrap_or(25));
    let offset = validate_offset(request.offset.unwrap_or(0));

    let query = ListQuery {
        project_id: Some(project_id.to_string()),
        parent: Some(epic.id),
        status: request.status,
        issue_type: request.issue_type,
        sort: sort.to_owned(),
        limit,
        offset,
        ..Default::default()
    };

    match task_repo.list_filtered(query).await {
        Ok(result) => EpicTasksResponse {
            tasks: Some(result.tasks.iter().map(EpicTaskModel::from).collect()),
            total_count: Some(result.total_count),
            limit: Some(limit),
            offset: Some(offset),
            has_more: Some(offset + limit < result.total_count),
            error: None,
        },
        Err(error) => EpicTasksResponse {
            tasks: None,
            total_count: None,
            limit: None,
            offset: None,
            has_more: None,
            error: Some(error.to_string()),
        },
    }
}

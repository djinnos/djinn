use djinn_db::{
    EpicCountQuery, EpicRepository, NoteRepository, ProjectRepository, TaskRepository,
};

use crate::server::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ProjectChatContext {
    pub(super) project_context: Option<String>,
}

pub(super) async fn build_project_chat_context(
    state: &AppState,
    project_ref: Option<&str>,
) -> ProjectChatContext {
    let Some(project_ref) = project_ref else {
        return ProjectChatContext::default();
    };

    let project_context = build_project_context_block(state, project_ref).await;

    ProjectChatContext { project_context }
}

fn normalize_brief_excerpt(content: &str, max_chars: usize) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    compact.chars().take(max_chars).collect::<String>()
}

pub(super) async fn build_project_context_block(
    state: &AppState,
    project_ref: &str,
) -> Option<String> {
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let project_id = match project_repo.resolve(project_ref).await {
        Ok(Some(id)) => id,
        Ok(None) => return None,
        Err(_) => return None,
    };

    let project = match project_repo.get(&project_id).await {
        Ok(Some(project)) => project,
        Ok(None) => return None,
        Err(_) => return None,
    };

    let epic_repo = EpicRepository::new(state.db().clone(), state.event_bus());
    let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());

    let open_epics = epic_repo
        .count_grouped(EpicCountQuery {
            project_id: Some(project_id.clone()),
            status: Some("open".to_string()),
            group_by: None,
        })
        .await
        .ok()
        .and_then(|v| {
            v.get("total_count")
                .and_then(|n| n.as_i64())
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let open_tasks = task_repo
        .count_grouped(djinn_db::CountQuery {
            project_id: Some(project_id.clone()),
            status: Some("open".to_string()),
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            group_by: None,
        })
        .await
        .ok()
        .and_then(|v| {
            v.get("total_count")
                .and_then(|n| n.as_i64())
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let brief = note_repo
        .get_by_permalink(&project_id, "brief")
        .await
        .ok()
        .flatten()
        .map(|note| normalize_brief_excerpt(&note.content, 200))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "No brief yet — suggest /init-project".to_string());

    let project_path_display =
        djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
            .display()
            .to_string();
    Some(format!(
        "## Current Project\n**Name**: {}  **Path**: {}\n**Open epics**: {}  **Open tasks**: {}\n**Brief**: {}",
        project.name, project_path_display, open_epics, open_tasks, brief
    ))
}

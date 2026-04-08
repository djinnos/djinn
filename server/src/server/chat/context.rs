use std::collections::BTreeSet;
use std::path::Path;

use djinn_db::{
    EpicCountQuery, EpicRepository, NoteRepository, ProjectRepository, RepoMapCacheKey,
    RepoMapCacheRepository, TaskRepository,
};

use crate::repo_map::persist_repo_map_note;
use crate::server::AppState;

pub(super) const REPO_MAP_SYSTEM_HEADER: &str = "## Repository Map";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ProjectChatContext {
    pub(super) project_context: Option<String>,
    pub(super) repo_map_context: Option<String>,
}

pub(super) fn format_repo_map_block(rendered: &str, permalink: Option<&str>) -> String {
    match permalink {
        Some(permalink) => {
            format!("{REPO_MAP_SYSTEM_HEADER}\nSource note: memory://{permalink}\n{rendered}")
        }
        None => format!("{REPO_MAP_SYSTEM_HEADER}\n{rendered}"),
    }
}

async fn repo_commit_sha(state: &AppState, repo_path: &Path) -> Option<String> {
    let git = state.git_actor(repo_path).await.ok()?;
    let head = git.head_commit().await.ok()?;
    Some(head.sha)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct RepoMapCompanionContext {
    pub(super) companion_note_ids: Vec<String>,
}

pub(super) async fn build_project_chat_context(
    state: &AppState,
    project_ref: Option<&str>,
) -> ProjectChatContext {
    let Some(project_ref) = project_ref else {
        return ProjectChatContext::default();
    };

    let project_context = build_project_context_block(state, project_ref).await;
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let companion_context = match project_repo.resolve(project_ref).await {
        Ok(Some(project_id)) => repo_map_companion_context(state, &project_id).await,
        _ => RepoMapCompanionContext::default(),
    };
    let repo_map_context =
        build_repo_map_context_block(state, project_ref, &companion_context.companion_note_ids)
            .await;

    ProjectChatContext {
        project_context,
        repo_map_context,
    }
}

pub(super) fn unique_companion_note_ids<I>(companion_note_ids: I) -> Vec<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    companion_note_ids
        .into_iter()
        .map(|id| id.as_ref().trim().to_string())
        .filter(|id| !id.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(super) async fn reinforce_repo_map_companion_notes(
    note_repo: &NoteRepository,
    repo_map_note_id: Option<&str>,
    companion_note_ids: &[String],
) {
    let Some(repo_map_note_id) = repo_map_note_id else {
        return;
    };
    if companion_note_ids.is_empty() {
        return;
    }

    let _ = note_repo
        .record_repo_map_co_access(repo_map_note_id, companion_note_ids.iter().cloned())
        .await;
}

pub(super) async fn repo_map_companion_context(
    state: &AppState,
    project_id: &str,
) -> RepoMapCompanionContext {
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
    let companion_note_ids = note_repo
        .get_by_permalink(project_id, "brief")
        .await
        .ok()
        .flatten()
        .map(|note| vec![note.id])
        .unwrap_or_default();

    RepoMapCompanionContext {
        companion_note_ids: unique_companion_note_ids(companion_note_ids),
    }
}

pub(super) async fn build_repo_map_context_block(
    state: &AppState,
    project_ref: &str,
    companion_note_ids: &[String],
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

    let commit_sha = repo_commit_sha(state, Path::new(&project.path)).await?;
    let repo_map_repo = RepoMapCacheRepository::new(state.db().clone());
    let cached = repo_map_repo
        .get(RepoMapCacheKey {
            project_id: &project.id,
            project_path: &project.path,
            worktree_path: None,
            commit_sha: &commit_sha,
        })
        .await
        .ok()
        .flatten()?;

    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
    let note = persist_repo_map_note(
        &note_repo,
        &project.id,
        &commit_sha,
        &crate::repo_map::RenderedRepoMap {
            content: cached.rendered_map.clone(),
            token_estimate: cached.token_estimate as usize,
            included_entries: cached.included_entries as usize,
        },
    )
    .await
    .ok();

    reinforce_repo_map_companion_notes(
        &note_repo,
        note.as_ref().map(|note| note.id.as_str()),
        companion_note_ids,
    )
    .await;

    Some(format_repo_map_block(
        &cached.rendered_map,
        note.as_ref().map(|note| note.permalink.as_str()),
    ))
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

    Some(format!(
        "## Current Project\n**Name**: {}  **Path**: {}\n**Open epics**: {}  **Open tasks**: {}\n**Brief**: {}",
        project.name, project.path, open_epics, open_tasks, brief
    ))
}

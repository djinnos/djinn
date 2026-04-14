use std::path::{Path, PathBuf};

use djinn_db::{NoteRepository, ProjectRepository, is_singleton};

use crate::server::DjinnMcpServer;
use crate::tools::memory_tools::lifecycle::{
    detect_emit_and_schedule_contradictions, schedule_summary_regeneration,
};
use crate::tools::memory_tools::{MemoryNoteResponse, WriteParams};

fn actual_file_path_for_response(
    note: &djinn_core::models::Note,
    worktree_root: Option<&Path>,
) -> String {
    match worktree_root {
        Some(root) => root
            .join(".djinn")
            .join(format!("{}.md", note.permalink))
            .to_string_lossy()
            .to_string(),
        None => note.file_path.clone(),
    }
}

pub(super) fn note_repository(
    server: &DjinnMcpServer,
    worktree_root: Option<PathBuf>,
) -> NoteRepository {
    NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_worktree_root(worktree_root)
        .with_embedding_provider(server.state.embedding_provider())
        .with_vector_store(server.state.vector_store())
}

pub(super) async fn maybe_update_singleton_note(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    project_id: &str,
    params: &WriteParams,
    tags_json: &str,
) -> Option<MemoryNoteResponse> {
    if is_singleton(&params.note_type)
        && let Some(existing) = repo
            .get_by_permalink(project_id, &params.note_type)
            .await
            .ok()
            .flatten()
    {
        return Some(
            match repo
                .update(&existing.id, &params.title, &params.content, tags_json)
                .await
            {
                Ok(note) => {
                    schedule_summary_regeneration(server, &note.id);
                    MemoryNoteResponse::from_note(&note)
                        .with_file_path(actual_file_path_for_response(&note, repo.worktree_root()))
                }
                Err(error) => MemoryNoteResponse::error(error.to_string()),
            },
        );
    }

    None
}

pub(super) async fn create_note(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    project_id: &str,
    params: &WriteParams,
    tags_json: &str,
) -> MemoryNoteResponse {
    let canonical_project_path = if is_singleton(&params.note_type) {
        let project_repo =
            ProjectRepository::new(server.state.db().clone(), server.state.event_bus());
        project_repo
            .get_path(project_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| params.project.clone())
    } else {
        params.project.clone()
    };

    let scope_paths_json = params
        .scope_paths
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".to_string());

    let create_result = if params.scope_paths.is_some() {
        repo.create_with_scope(
            project_id,
            Path::new(&canonical_project_path),
            &params.title,
            &params.content,
            &params.note_type,
            params.status.as_deref(),
            tags_json,
            &scope_paths_json,
        )
        .await
    } else {
        repo.create_with_status(
            project_id,
            Path::new(&canonical_project_path),
            &params.title,
            &params.content,
            &params.note_type,
            params.status.as_deref(),
            tags_json,
        )
        .await
    };

    match create_result {
        Ok(note) => {
            schedule_summary_regeneration(server, &note.id);
            detect_emit_and_schedule_contradictions(server, repo, &note).await;
            MemoryNoteResponse::from_note(&note)
                .with_file_path(actual_file_path_for_response(&note, repo.worktree_root()))
        }
        Err(error) => MemoryNoteResponse::error(error.to_string()),
    }
}

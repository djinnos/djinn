use super::*;

use rmcp::{Json, handler::server::wrapper::Parameters};

pub(super) async fn memory_move(
    server: &DjinnMcpServer,
    Parameters(p): Parameters<MoveParams>,
) -> Json<MemoryNoteResponse> {
    memory_move_with_worktree(server, Parameters(p), None).await
}

pub(super) async fn memory_move_with_worktree(
    server: &DjinnMcpServer,
    Parameters(p): Parameters<MoveParams>,
    worktree_root: Option<std::path::PathBuf>,
) -> Json<MemoryNoteResponse> {
    let Some(project_id) = server.project_id_for_path(&p.project).await else {
        return Json(MemoryNoteResponse::error(format!(
            "project not found: {}",
            p.project
        )));
    };

    let project_repo = ProjectRepository::new(server.state.db().clone(), server.state.event_bus());
    let canonical_project_path = match project_repo.get(&project_id).await {
        Ok(Some(project)) => djinn_core::paths::project_dir(
            &project.github_owner,
            &project.github_repo,
        ),
        _ => Path::new(&p.project).to_path_buf(),
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_worktree_root(worktree_root)
        .with_embedding_provider(server.state.embedding_provider())
        .with_vector_store(server.state.vector_store());

    let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
        return Json(MemoryNoteResponse::error(format!(
            "note not found: {}",
            p.identifier
        )));
    };

    let new_title = p.title.as_deref().unwrap_or(&note.title);
    let moved_title = p.title.as_deref().unwrap_or(&note.title);

    match repo
        .move_note(&note.id, &canonical_project_path, moved_title, &p.note_type)
        .await
    {
        Ok(mut moved) => {
            if p.title.is_some() {
                moved.title = new_title.to_string();
            }
            Json(MemoryNoteResponse::from_note(&moved))
        }
        Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
    }
}

use super::*;

use djinn_db::{
    NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation, NoteRevisionReason,
};
use rmcp::{Json, handler::server::wrapper::Parameters};

pub(super) async fn memory_delete(
    server: &DjinnMcpServer,
    Parameters(p): Parameters<DeleteParams>,
) -> Json<MemoryDeleteResponse> {
    let Some(project_id) = server.project_id_for_path(&p.project).await else {
        return Json(MemoryDeleteResponse {
            ok: false,
            error: Some(format!("project not found: {}", p.project)),
        });
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_embedding_provider(server.state.embedding_provider())
        .with_vector_store(server.state.vector_store());

    let Some((attribution, provenance)) = super::write_services::trusted_revision_context() else {
        return Json(MemoryDeleteResponse {
            ok: false,
            error: Some("authenticated revision caller required".to_owned()),
        });
    };

    let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
        return Json(MemoryDeleteResponse {
            ok: false,
            error: Some(format!("note not found: {}", p.identifier)),
        });
    };

    match repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id,
            note_id: Some(note.id),
            event_kind: NoteRevisionEventKind::Deleted,
            desired: NoteRevisionDesiredState::Delete,
            attribution,
            provenance,
            reason: NoteRevisionReason::new(p.reason).expect("validated MCP mutation reason"),
        })
        .await
    {
        Ok(_) => Json(MemoryDeleteResponse {
            ok: true,
            error: None,
        }),
        Err(e) => Json(MemoryDeleteResponse {
            ok: false,
            error: Some(e.to_string()),
        }),
    }
}

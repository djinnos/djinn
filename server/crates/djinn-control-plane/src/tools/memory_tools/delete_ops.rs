use super::*;
use djinn_db::{NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation};
use rmcp::{Json, handler::server::wrapper::Parameters};

pub(super) async fn memory_delete(
    server: &DjinnMcpServer,
    Parameters(p): Parameters<DeleteParams>,
) -> Json<MemoryDeleteResponse> {
    let (reason, attribution, provenance) = match revision_context(&p.reason) {
        Ok(context) => context,
        Err(error) => {
            return Json(MemoryDeleteResponse {
                ok: false,
                error: Some(error),
            });
        }
    };
    let Some(project_id) = server.project_id_for_path(&p.project).await else {
        return Json(MemoryDeleteResponse {
            ok: false,
            error: Some(format!("project not found: {}", p.project)),
        });
    };
    let repo = super::write_services::note_repository(server);
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
            reason,
        })
        .await
    {
        Ok(_) => Json(MemoryDeleteResponse {
            ok: true,
            error: None,
        }),
        Err(error) => Json(MemoryDeleteResponse {
            ok: false,
            error: Some(error.to_string()),
        }),
    }
}

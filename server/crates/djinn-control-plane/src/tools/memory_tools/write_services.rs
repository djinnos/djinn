use crate::server::DjinnMcpServer;
use crate::tools::memory_tools::lifecycle::{
    detect_emit_and_schedule_contradictions, schedule_summary_regeneration,
};
use crate::tools::memory_tools::{MemoryNoteResponse, WriteParams};
use djinn_db::{
    NoteRepository, NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEventKind,
    NoteRevisionMutation, NoteRevisionReason, TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance, folder_for_type_with_status, is_singleton,
    permalink_for_with_status,
};

pub(super) fn note_repository(server: &DjinnMcpServer) -> NoteRepository {
    NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_embedding_provider(server.state.embedding_provider())
        .with_vector_store(server.state.vector_store())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_update_singleton_note(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    project_id: &str,
    params: &WriteParams,
    _tags_json: &str,
    reason: NoteRevisionReason,
    attribution: TrustedNoteRevisionAttribution,
    provenance: TrustedNoteRevisionProvenance,
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
                .mutate_with_revision(NoteRevisionMutation {
                    project_id: project_id.to_owned(),
                    note_id: Some(existing.id.clone()),
                    event_kind: NoteRevisionEventKind::Updated,
                    desired: NoteRevisionDesiredState::Existing {
                        content: params.content.clone(),
                        confidence: existing.confidence,
                    },
                    attribution,
                    provenance,
                    reason,
                })
                .await
            {
                Ok(result) => {
                    let note = result.note.expect("existing mutation returns note");
                    if result.changed {
                        schedule_summary_regeneration(server, &note.id);
                    }
                    MemoryNoteResponse::from_note(&note)
                }
                Err(error) => MemoryNoteResponse::error(error.to_string()),
            },
        );
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn create_note(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    project_id: &str,
    params: &WriteParams,
    tags_json: &str,
    reason: NoteRevisionReason,
    attribution: TrustedNoteRevisionAttribution,
    provenance: TrustedNoteRevisionProvenance,
) -> MemoryNoteResponse {
    let scope_paths = params
        .scope_paths
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".to_string());
    let status = params.status.clone().unwrap_or_else(|| "active".to_owned());
    match repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: Some(uuid::Uuid::now_v7().to_string()),
            event_kind: NoteRevisionEventKind::Created,
            desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                title: params.title.clone(),
                permalink: permalink_for_with_status(
                    &params.note_type,
                    &params.title,
                    params.status.as_deref(),
                ),
                content: params.content.clone(),
                note_type: params.note_type.clone(),
                folder: folder_for_type_with_status(&params.note_type, params.status.as_deref())
                    .to_owned(),
                status,
                tags: tags_json.to_owned(),
                retrieval_anchor: params.retrieval_anchor.clone(),
                scope_paths,
                confidence: 0.5,
            }),
            attribution,
            provenance,
            reason,
        })
        .await
    {
        Ok(result) => {
            let note = result.note.expect("created mutation returns note");
            if result.changed {
                schedule_summary_regeneration(server, &note.id);
                detect_emit_and_schedule_contradictions(server, repo, &note).await;
            }
            MemoryNoteResponse::from_note(&note)
        }
        Err(error) => MemoryNoteResponse::error(error.to_string()),
    }
}

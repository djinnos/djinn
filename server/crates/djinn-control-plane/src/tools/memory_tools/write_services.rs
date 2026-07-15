use djinn_db::{
    NoteRepository, NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEventKind,
    NoteRevisionMutation, NoteRevisionReason, TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance, folder_for_type_with_status, is_singleton,
    permalink_for_with_status,
};

use crate::server::DjinnMcpServer;
use crate::tools::memory_tools::lifecycle::{
    detect_emit_and_schedule_contradictions, schedule_summary_regeneration,
};
use crate::tools::memory_tools::{MemoryNoteResponse, WriteParams};

pub(super) fn note_repository(server: &DjinnMcpServer) -> NoteRepository {
    NoteRepository::new(server.state.db().clone(), server.state.event_bus())
        .with_embedding_provider(server.state.embedding_provider())
        .with_vector_store(server.state.vector_store())
}

pub(super) fn trusted_revision_context() -> Option<(
    TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance,
)> {
    let context = djinn_core::auth_context::current_revision_caller_context()?;
    Some((
        TrustedNoteRevisionAttribution::try_from(&context).ok()?,
        TrustedNoteRevisionProvenance::try_from(&context).ok()?,
    ))
}

pub(super) async fn maybe_update_singleton_note(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    project_id: &str,
    params: &WriteParams,
    _tags_json: &str,
) -> Option<MemoryNoteResponse> {
    // Singleton overwrite is still a live memory_write mutation. Acquire the
    // server-owned caller values before looking up or changing the existing
    // row so this early path cannot bypass the revision boundary.
    let Some((attribution, provenance)) = trusted_revision_context() else {
        return Some(MemoryNoteResponse::error(
            "authenticated revision caller required".to_owned(),
        ));
    };

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
                    reason: NoteRevisionReason::new(params.reason.clone())
                        .expect("validated MCP mutation reason"),
                })
                .await
            {
                Ok(result) => {
                    let note = result.note.expect("existing mutation returns note");
                    let note = match params.retrieval_anchor.as_deref() {
                        Some(anchor) => {
                            match repo.update_retrieval_anchor(&note.id, Some(anchor)).await {
                                Ok(note) => note,
                                Err(error) => {
                                    return Some(MemoryNoteResponse::error(error.to_string()));
                                }
                            }
                        }
                        None => note,
                    };
                    schedule_summary_regeneration(server, &note.id);
                    // No on-disk file anymore — file_path is the empty string.
                    MemoryNoteResponse::from_note(&note)
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
    let scope_paths_json = params
        .scope_paths
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".to_string());

    let Some((attribution, provenance)) = trusted_revision_context() else {
        return MemoryNoteResponse::error("authenticated revision caller required".to_owned());
    };
    let create_result = repo
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
                status: params.status.clone().unwrap_or_else(|| "active".to_owned()),
                tags: tags_json.to_owned(),
                retrieval_anchor: params.retrieval_anchor.clone(),
                scope_paths: scope_paths_json,
                confidence: 0.5,
            }),
            attribution,
            provenance,
            reason: NoteRevisionReason::new(params.reason.clone())
                .expect("validated MCP mutation reason"),
        })
        .await
        .map(|result| result.note.expect("created mutation returns note"));

    match create_result {
        Ok(note) => {
            schedule_summary_regeneration(server, &note.id);
            detect_emit_and_schedule_contradictions(server, repo, &note).await;
            MemoryNoteResponse::from_note(&note)
        }
        Err(error) => MemoryNoteResponse::error(error.to_string()),
    }
}

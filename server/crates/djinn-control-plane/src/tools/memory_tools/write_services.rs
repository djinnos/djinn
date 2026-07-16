use djinn_db::{
    NoteRepository, NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEventKind,
    NoteRevisionMutation, NoteRevisionReason, NoteRevisionUpdateState,
    TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance, folder_for_type_with_status,
    is_singleton, permalink_for_with_status,
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

pub(super) fn revision_identity(
    reason: &str,
) -> Result<
    (
        NoteRevisionReason,
        TrustedNoteRevisionAttribution,
        TrustedNoteRevisionProvenance,
    ),
    String,
> {
    let context = djinn_core::auth_context::current_revision_caller_context()
        .ok_or_else(|| "authenticated revision caller required".to_owned())?;
    Ok((
        NoteRevisionReason::new(reason).map_err(|error| error.to_string())?,
        TrustedNoteRevisionAttribution::try_from(&context).map_err(|error| error.to_string())?,
        TrustedNoteRevisionProvenance::try_from(&context).map_err(|error| error.to_string())?,
    ))
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
        let (reason, attribution, provenance) = match revision_identity(&params.reason) {
            Ok(identity) => identity,
            Err(error) => return Some(MemoryNoteResponse::error(error)),
        };
        return Some(
            match repo
                .mutate_with_revision(NoteRevisionMutation {
                    project_id: project_id.to_owned(),
                    note_id: Some(existing.id.clone()),
                    event_kind: NoteRevisionEventKind::Updated,
                    desired: NoteRevisionDesiredState::ExistingWithMetadata(
                        NoteRevisionUpdateState {
                            title: params.title.clone(),
                            permalink: existing.permalink.clone(),
                            content: params.content.clone(),
                            note_type: existing.note_type.clone(),
                            folder: existing.folder.clone(),
                            tags: tags_json.to_owned(),
                            retrieval_anchor: params
                                .retrieval_anchor
                                .clone()
                                .or(existing.retrieval_anchor.clone()),
                            confidence: existing.confidence,
                        },
                    ),
                    attribution,
                    provenance,
                    reason,
                })
                .await
            {
                Ok(result) => {
                    let note = result.note.expect("updated mutation returns note");
                    schedule_summary_regeneration(server, &note.id);
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
    let (reason, attribution, provenance) = match revision_identity(&params.reason) {
        Ok(identity) => identity,
        Err(error) => return MemoryNoteResponse::error(error),
    };
    let status = djinn_memory::note_status::normalize(params.status.as_deref());
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
                status,
                tags: tags_json.to_owned(),
                retrieval_anchor: params.retrieval_anchor.clone(),
                scope_paths: scope_paths_json,
                confidence: 0.5,
            }),
            attribution,
            provenance,
            reason,
        })
        .await;

    match create_result {
        Ok(result) => {
            let note = result.note.expect("create mutation returns note");
            schedule_summary_regeneration(server, &note.id);
            detect_emit_and_schedule_contradictions(server, repo, &note).await;
            MemoryNoteResponse::from_note(&note)
        }
        Err(error) => MemoryNoteResponse::error(error.to_string()),
    }
}

use super::*;

use crate::tools::memory_tools::contradiction::ContradictionAnalysisInput;
use crate::tools::memory_tools::summaries::NoteSummaryService;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::Note;
use djinn_db::folder_for_type;

pub(super) fn schedule_summary_regeneration(server: &DjinnMcpServer, note_id: &str) {
    let db = server.state.db().clone();
    let note_id = note_id.to_string();
    tokio::spawn(async move {
        let service = NoteSummaryService::new(db.clone());
        match djinn_provider::resolve_memory_provider(&db).await {
            Ok(_) => service.generate_for_note_ids(&[note_id]).await,
            Err(_) => service.apply_fallback_for_note_id(&note_id).await,
        }
    });
}

pub(super) async fn detect_emit_and_schedule_contradictions(
    server: &DjinnMcpServer,
    repo: &NoteRepository,
    note: &Note,
) {
    let folder = folder_for_type(&note.note_type);
    let Ok(candidates) = repo
        .detect_contradiction_candidates(&note.id, &note.note_type, folder, &note.content)
        .await
    else {
        return;
    };

    if candidates.is_empty() {
        return;
    }

    server
        .state
        .event_bus()
        .send(DjinnEventEnvelope::contradiction_candidates(
            note,
            &candidates,
        ));

    let input = ContradictionAnalysisInput {
        note_id: note.id.clone(),
        note_title: note.title.clone(),
        note_summary: note
            .abstract_
            .clone()
            .unwrap_or_else(|| note.content.chars().take(500).collect()),
        candidates,
    };
    let _ = server.contradiction_analysis_tx.try_send(input);
}

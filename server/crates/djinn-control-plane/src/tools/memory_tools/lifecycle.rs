use super::*;

use crate::tools::memory_tools::contradiction::ContradictionAnalysisInput;
use crate::tools::memory_tools::summaries::NoteSummaryService;
use djinn_db::folder_for_type;
use djinn_memory::Note;

pub(crate) fn schedule_summary_regeneration(server: &DjinnMcpServer, note_id: &str) {
    let db = server.state.db().clone();
    let note_id = note_id.to_string();
    let user_id = djinn_core::auth_context::current_user_id();
    tokio::spawn(async move {
        let service = NoteSummaryService::new_for_user(db.clone(), user_id.clone());
        match djinn_provider::resolve_memory_provider_for_user(&db, user_id.as_deref()).await {
            Ok(_) => service.generate_for_note_ids(&[note_id]).await,
            Err(_) => service.apply_fallback_for_note_id(&note_id).await,
        }
    });
}

pub(crate) async fn detect_emit_and_schedule_contradictions(
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
        .send(djinn_memory::events::contradiction_candidates(
            note,
            &candidates,
        ));

    let input = ContradictionAnalysisInput {
        user_id: djinn_core::auth_context::current_user_id(),
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

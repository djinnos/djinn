use crate::server::chat::DJINN_CHAT_SYSTEM_PROMPT;
use crate::server::chat::context::{
    REPO_MAP_SYSTEM_HEADER, format_repo_map_block, reinforce_repo_map_companion_notes,
    repo_map_companion_context, unique_companion_note_ids,
};
use crate::server::chat::prompt::layout::compose_system_prompt;
use crate::test_helpers;
use djinn_db::NoteRepository;

#[test]
fn unique_companion_note_ids_deduplicates_and_drops_empty_values() {
    let ids = unique_companion_note_ids(["note-b", "", "note-a", "note-b", "  note-a  "]);
    assert_eq!(ids, vec!["note-a".to_string(), "note-b".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reinforce_repo_map_companion_notes_records_one_association_per_unique_pair() {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let note_repo = NoteRepository::new(db.clone(), test_helpers::test_events());

    let repo_map_note = note_repo
        .upsert_db_note_by_permalink(
            &project.id,
            "reference/repo-maps/repository-map-deadbeef",
            "Repository Map deadbeef",
            "src/lib.rs",
            "repo_map",
            "[]",
        )
        .await
        .expect("repo map note persists");
    let companion = note_repo
        .upsert_db_note_by_permalink(
            &project.id,
            "references/companion-note",
            "companion note",
            "body",
            "reference",
            "[]",
        )
        .await
        .expect("companion note persists");

    reinforce_repo_map_companion_notes(
        &note_repo,
        Some(&repo_map_note.id),
        &[companion.id.clone(), companion.id.clone()],
    )
    .await;

    let associations = note_repo
        .get_associations_for_note(&repo_map_note.id)
        .await
        .expect("associations load");
    assert_eq!(associations.len(), 1);
    assert_eq!(associations[0].co_access_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reinforce_repo_map_companion_notes_is_noop_without_companions_or_repo_map_note() {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let note_repo = NoteRepository::new(db.clone(), test_helpers::test_events());

    let repo_map_note = note_repo
        .upsert_db_note_by_permalink(
            &project.id,
            "reference/repo-maps/repository-map-deadbeef",
            "Repository Map deadbeef",
            "src/lib.rs",
            "repo_map",
            "[]",
        )
        .await
        .expect("repo map note persists");

    reinforce_repo_map_companion_notes(&note_repo, Some(&repo_map_note.id), &[]).await;
    reinforce_repo_map_companion_notes(&note_repo, None, &["note-1".to_string()]).await;

    let associations = note_repo
        .get_associations_for_note(&repo_map_note.id)
        .await
        .expect("associations load");
    assert!(associations.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_map_companion_context_uses_brief_note_when_present() {
    let state = test_helpers::test_app_state_in_memory().await;
    let project = test_helpers::create_test_project(state.db()).await;
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());

    let brief = note_repo
        .upsert_db_note_by_permalink(
            &project.id,
            "brief",
            "brief",
            "project brief body",
            "reference",
            "[]",
        )
        .await
        .expect("brief note persists");

    let context = repo_map_companion_context(&state, &project.id).await;
    assert_eq!(context.companion_note_ids, vec![brief.id]);
}

#[test]
fn system_prompt_repo_map_block_is_unchanged_when_reinforcement_is_unavailable() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let prompt = compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, None, Some(&repo_map), None);
    assert!(prompt.contains(REPO_MAP_SYSTEM_HEADER));
    assert!(prompt.contains("src/lib.rs"));
}

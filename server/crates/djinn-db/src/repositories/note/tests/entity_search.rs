//! Tests for `NoteSearchParams.entity_types` — the unified search surface
//! that interleaves note and proposal rows.

use super::*;
use crate::repositories::note::NoteSearchParams;
use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};
use crate::repositories::test_support::{event_bus_for, make_project};

async fn setup() -> (
    tempfile::TempDir,
    NoteRepository,
    ProposalRepository,
    String,
) {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    db.ensure_initialized().await.unwrap();
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db, djinn_core::events::EventBus::noop());
    (tmp, note_repo, proposal_repo, project.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_entity_types_none_returns_both() {
    let (_tmp, note_repo, proposal_repo, project_id) = setup().await;

    // Create a note with a unique sentinel word.
    note_repo
        .create(
            &project_id,
            "Sentinel Note",
            "unique_zqk_sentinel content for unified search test",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    // Create a proposal with the same sentinel word.
    proposal_repo
        .create(ProposalCreateInput {
            title: "Sentinel Proposal",
            body: "unique_zqk_sentinel content for unified search test",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    let results = note_repo
        .search(NoteSearchParams {
            project_id: &project_id,
            query: "unique_zqk_sentinel",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .unwrap();

    assert!(
        results.iter().any(|r| r.entity == "note"),
        "entity_types=None should include note rows"
    );
    assert!(
        results.iter().any(|r| r.entity == "proposal"),
        "entity_types=None should include proposal rows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_entity_types_note_returns_only_notes() {
    let (_tmp, note_repo, proposal_repo, project_id) = setup().await;

    note_repo
        .create(
            &project_id,
            "Sentinel Note",
            "unique_fmq_sentinel content for notes-only test",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    proposal_repo
        .create(ProposalCreateInput {
            title: "Sentinel Proposal",
            body: "unique_fmq_sentinel content for notes-only test",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    let entity_types = ["note".to_string()];
    let results = note_repo
        .search(NoteSearchParams {
            project_id: &project_id,
            query: "unique_fmq_sentinel",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: Some(&entity_types),
        })
        .await
        .unwrap();

    assert!(
        results.iter().all(|r| r.entity == "note"),
        "entity_types=[\"note\"] should return only note rows, got: {:?}",
        results.iter().map(|r| &r.entity).collect::<Vec<_>>()
    );
    assert!(
        !results.is_empty(),
        "should find the note with the sentinel word"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_entity_types_proposal_returns_only_proposals() {
    let (_tmp, note_repo, proposal_repo, project_id) = setup().await;

    note_repo
        .create(
            &project_id,
            "Sentinel Note",
            "unique_hnb_sentinel content for proposals-only test",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    proposal_repo
        .create(ProposalCreateInput {
            title: "Sentinel Proposal",
            body: "unique_hnb_sentinel content for proposals-only test",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    let entity_types = ["proposal".to_string()];
    let results = note_repo
        .search(NoteSearchParams {
            project_id: &project_id,
            query: "unique_hnb_sentinel",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: Some(&entity_types),
        })
        .await
        .unwrap();

    assert!(
        results.iter().all(|r| r.entity == "proposal"),
        "entity_types=[\"proposal\"] should return only proposal rows, got: {:?}",
        results.iter().map(|r| &r.entity).collect::<Vec<_>>()
    );
    assert!(
        !results.is_empty(),
        "should find the proposal with the sentinel word"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_entity_types_empty_returns_no_results() {
    let (_tmp, note_repo, _proposal_repo, project_id) = setup().await;

    note_repo
        .create(
            &project_id,
            "Sentinel Note",
            "unique_kxl_sentinel content for empty entity_types test",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let entity_types: [String; 0] = [];
    let results = note_repo
        .search(NoteSearchParams {
            project_id: &project_id,
            query: "unique_kxl_sentinel",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: Some(&entity_types),
        })
        .await
        .unwrap();

    assert!(
        results.is_empty(),
        "entity_types=[] (empty) should return no results"
    );
}

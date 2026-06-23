//! Tests for proposal nodes and heterogeneous typed edges in `memory_graph` (0pfy).
//!
//! Verifies that `NoteRepository::graph()` includes:
//! * Proposal nodes from `proposals JOIN proposal_targets` with `entity_type="proposal"`.
//! * Heterogeneous typed edges from `memory_entity_associations` with
//!   `source_entity_type`/`target_entity_type` populated.
//! * No note rows are created for proposals.

use tokio::sync::broadcast;

use super::*;
use crate::repositories::note::{MemoryEntityKind, MemoryEntityRef, NoteRepository};
use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};
use crate::repositories::test_support::{event_bus_for, make_project};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_includes_proposal_nodes_and_heterogeneous_typed_edges() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    // 1. Seed a note.
    let note = note_repo
        .create(&project.id, "Source Note", "content", "reference", "[]")
        .await
        .unwrap();

    // 2. Seed a proposal linked to the project.
    let proposal = proposal_repo
        .create(ProposalCreateInput {
            title: "Derived Proposal",
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    // Link proposal to project via proposal_targets.
    sqlx::query(
        "INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, 'primary')",
    )
    .bind(&proposal.id)
    .bind(&project.id)
    .execute(db.pool())
    .await
    .unwrap();

    // 3. Create a heterogeneous typed edge: proposal → note, derived_from.
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal.id),
            MemoryEntityRef::note(&note.id),
            MemoryEntityKind::DerivedFrom,
            0.85,
        )
        .await
        .unwrap();

    // 4. Call graph().
    let graph = note_repo.graph(&project.id).await.unwrap();

    // 5. Assert both nodes present.
    let node_map: std::collections::HashMap<String, GraphNode> =
        graph.nodes.into_iter().map(|n| (n.id.clone(), n)).collect();

    assert!(
        node_map.contains_key(&note.id),
        "graph should contain the note node"
    );
    assert!(
        node_map.contains_key(&proposal.id),
        "graph should contain the proposal node"
    );

    let note_node = node_map.get(&note.id).unwrap();
    assert_eq!(note_node.entity_type, "note");
    assert_eq!(note_node.note_type, "reference");

    let proposal_node = node_map.get(&proposal.id).unwrap();
    assert_eq!(proposal_node.entity_type, "proposal");
    assert_eq!(proposal_node.note_type, "proposal");
    assert_eq!(proposal_node.folder, "");
    assert_eq!(proposal_node.permalink, proposal.short_id);

    // 6. Assert the derived_from edge is present with correct entity types.
    let derived_edges: Vec<&TypedEdge> = graph
        .typed_edges
        .iter()
        .filter(|e| {
            e.kind == "derived_from" && e.source_id == proposal.id && e.target_id == note.id
        })
        .collect();
    assert_eq!(
        derived_edges.len(),
        1,
        "expected exactly one derived_from edge from proposal to note"
    );
    let edge = derived_edges[0];
    assert_eq!(edge.source_entity_type, "proposal");
    assert_eq!(edge.target_entity_type, "note");
    assert!((edge.weight - 0.85).abs() < 1e-12);

    // 7. Assert no notes row exists for the proposal id.
    let note_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE id = $1")
        .bind(&proposal.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(note_count, 0, "no note row should exist for proposal id");
}

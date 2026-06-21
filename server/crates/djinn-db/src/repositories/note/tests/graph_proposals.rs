use super::*;
use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_includes_proposal_nodes_and_heterogeneous_typed_edges() {
    // Seed a graph with one note and one proposal, linked by a
    // `derived_from` typed edge through the heterogeneous substrate.
    // The graph must contain both nodes and the edge, without creating
    // a note row for the proposal.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let note = note_repo
        .create(&project.id, "Source note", "content", "reference", "[]")
        .await
        .unwrap();
    let proposal = proposal_repo
        .create(ProposalCreateInput {
            title: "Derived proposal",
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap();
    proposal_repo
        .add_target(&proposal.id, &project.id, "primary")
        .await
        .unwrap();

    // Link proposal → note via the heterogeneous substrate.
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal.id),
            MemoryEntityRef::note(&note.id),
            MemoryEntityKind::DerivedFrom,
            0.85,
        )
        .await
        .unwrap();

    let graph = note_repo.graph(&project.id).await.unwrap();

    // ── Nodes ──
    // One note node + one proposal node.
    let node_ids: std::collections::HashSet<_> = graph.nodes.iter().map(|n| &n.id).collect();
    assert!(
        node_ids.contains(&note.id),
        "graph must contain the note node"
    );
    assert!(
        node_ids.contains(&proposal.id),
        "graph must contain the proposal node"
    );

    // The proposal node must carry the `proposal` entity type so clients
    // can distinguish it from notes.
    let proposal_node = graph
        .nodes
        .iter()
        .find(|n| n.id == proposal.id)
        .expect("proposal node present");
    assert_eq!(proposal_node.entity_type, "proposal");
    assert_eq!(proposal_node.title, "Derived proposal");

    // The note node keeps its existing shape.
    let note_node = graph
        .nodes
        .iter()
        .find(|n| n.id == note.id)
        .expect("note node present");
    assert_eq!(note_node.entity_type, "note");
    assert_eq!(note_node.note_type, "reference");

    // ── Typed edges ──
    // The heterogeneous `derived_from` edge must appear in typed_edges.
    let derived_edges: Vec<_> = graph
        .typed_edges
        .iter()
        .filter(|e| e.kind == "derived_from")
        .collect();
    assert_eq!(
        derived_edges.len(),
        1,
        "expected exactly one derived_from typed edge"
    );
    let edge = &derived_edges[0];
    assert_eq!(edge.source_id, proposal.id);
    assert_eq!(edge.target_id, note.id);
    assert!((edge.weight - 0.85).abs() < 1e-12);

    // ── No proposal body leaked into notes ──
    let note_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE id = $1")
        .bind(&proposal.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(note_count, 0, "proposal must not create a note row");
}

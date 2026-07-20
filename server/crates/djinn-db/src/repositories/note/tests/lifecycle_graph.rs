//! Lifecycle graph selection fixtures.

use std::collections::HashSet;

use djinn_memory::GraphOptions;
use tokio::sync::broadcast;

use super::*;
use crate::repositories::note::{
    MemoryEntityKind, MemoryEntityRef, NoteAssociationKind, NoteRepository,
};
use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};
use crate::repositories::test_support::{event_bus_for, make_project};

fn assert_endpoint_closed(graph: &GraphResponse) {
    let ids: HashSet<&str> = graph.nodes.iter().map(|node| node.id.as_str()).collect();
    assert!(graph.edges.iter().all(|edge| ids.contains(edge.source_id.as_str())
        && ids.contains(edge.target_id.as_str())));
    assert!(graph.typed_edges.iter().all(|edge| ids.contains(edge.source_id.as_str())
        && ids.contains(edge.target_id.as_str())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_graph_caps_after_selecting_nodes_and_closes_every_edge_layer() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    // Create the target before the active source so the wikilink is resolved.
    let archived = repo.create(&project.id, "Archived Exact", "body", "reference", "[]").await.unwrap();
    let deprecated = repo.create(&project.id, "Deprecated Null", "body", "reference", "[]").await.unwrap();
    let active = repo.create(&project.id, "Active Source", "See [[Archived Exact]]", "reference", "[]").await.unwrap();
    let tie_a = repo.create(&project.id, "Archived Tie A", "body", "reference", "[]").await.unwrap();
    let tie_b = repo.create(&project.id, "Archived Tie B", "body", "reference", "[]").await.unwrap();

    for (id, status, changed_at) in [
        (&archived.id, "archived", Some("2026-01-03T00:00:00.000Z")),
        (&deprecated.id, "deprecated", None),
        (&tie_a.id, "archived", Some("2026-01-02T00:00:00.000Z")),
        (&tie_b.id, "deprecated", Some("2026-01-02T00:00:00.000Z")),
    ] {
        sqlx::query("UPDATE notes SET status = $1, lifecycle_changed_at = $2 WHERE id = $3")
            .bind(status).bind(changed_at).bind(id).execute(db.pool()).await.unwrap();
    }
    repo.upsert_typed_association(&active.id, &archived.id, NoteAssociationKind::Supersedes, 0.8).await.unwrap();
    repo.upsert_typed_association(&archived.id, &deprecated.id, NoteAssociationKind::Contradicts, 0.7).await.unwrap();

    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let proposal = proposal_repo.create(ProposalCreateInput {
        title: "Lifecycle proposal", body: "", acceptance_criteria: None, status: None, body_format: None,
    }).await.unwrap();
    sqlx::query("INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, 'primary')")
        .bind(&proposal.id).bind(&project.id).execute(db.pool()).await.unwrap();
    repo.upsert_typed_entity_association(
        MemoryEntityRef::proposal(&proposal.id), MemoryEntityRef::note(&archived.id),
        MemoryEntityKind::DerivedFrom, 0.9,
    ).await.unwrap();

    // Default compatibility output excludes inactive notes and their proposal edge.
    let default_graph = repo.graph(&project.id).await.unwrap();
    assert!(default_graph.lifecycle_summary.is_none());
    assert!(default_graph.nodes.iter().all(|node| node.entity_type == "proposal" || node.status == "active"));
    assert_endpoint_closed(&default_graph);

    // Limit zero admits no inactive node despite preserving active/proposal context.
    let zero = repo.graph_with_options(&project.id, GraphOptions {
        statuses: Some(vec!["active".into(), "archived".into(), "deprecated".into()]), lifecycle_limit: Some(0),
    }).await.unwrap();
    assert_eq!(zero.lifecycle_summary.unwrap().inactive_total, 4);
    assert!(zero.nodes.iter().all(|node| node.entity_type != "note" || node.status == "active"));
    assert_endpoint_closed(&zero);

    // A cap includes the newest timestamp, then stable id ordering for equal timestamps.
    let capped = repo.graph_with_options(&project.id, GraphOptions {
        statuses: Some(vec!["active".into(), "archived".into(), "deprecated".into()]), lifecycle_limit: Some(3),
    }).await.unwrap();
    let summary = capped.lifecycle_summary.as_ref().unwrap();
    assert_eq!((summary.inactive_total, summary.inactive_returned, summary.inactive_omitted), (4, 3, 1));
    let inactive: Vec<_> = capped.nodes.iter().filter(|node| node.entity_type == "note" && node.status != "active").collect();
    assert_eq!(inactive[0].id, archived.id);
    assert_eq!(inactive[1].id, std::cmp::min(tie_a.id.clone(), tie_b.id.clone()));
    assert_eq!(inactive[2].id, std::cmp::max(tie_a.id.clone(), tie_b.id.clone()));
    assert!(capped.edges.iter().any(|edge| edge.source_id == active.id && edge.target_id == archived.id));
    assert!(capped.typed_edges.iter().any(|edge| edge.kind == "supersedes"));
    assert!(capped.typed_edges.iter().any(|edge| edge.kind == "derived_from" && edge.source_id == proposal.id));
    assert!(!capped.typed_edges.iter().any(|edge| edge.kind == "contradicts"));
    assert_endpoint_closed(&capped);

    // Historical rows retain null and sort after exact transition timestamps.
    let all = repo.graph_with_options(&project.id, GraphOptions {
        statuses: Some(vec!["active".into(), "archived".into(), "deprecated".into()]), lifecycle_limit: Some(1000),
    }).await.unwrap();
    let all_inactive: Vec<_> = all.nodes.iter().filter(|node| node.entity_type == "note" && node.status != "active").collect();
    assert_eq!(all_inactive.last().unwrap().id, deprecated.id);
    assert_eq!(all_inactive.last().unwrap().lifecycle_changed_at, None);
    assert!(all.typed_edges.iter().any(|edge| edge.kind == "contradicts"));
    assert_endpoint_closed(&all);
}

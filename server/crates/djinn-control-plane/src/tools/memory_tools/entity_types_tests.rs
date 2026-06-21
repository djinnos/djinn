//! Tests for the unified entity surface: `SearchParams.entity_types`,
//! `MemorySearchResultItem.entity`, and `MemoryBuildContextResponse.proposals`.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, NoteRepository, ProjectRepository, ProposalRepository};
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::ops;
    use crate::tools::memory_tools::{BuildContextParams, SearchParams};

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    fn test_mcp_state(db: Database, tx: &broadcast::Sender<DjinnEventEnvelope>) -> McpState {
        McpState::new(
            db,
            event_bus_for(tx),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        )
    }

    /// Set up an in-memory DB with a project, one note, and one proposal,
    /// both containing the same unique sentinel word so a query for that word
    /// matches both entities.
    async fn setup_with_note_and_proposal() -> (DjinnMcpServer, String, String, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), event_bus.clone());
        let note = note_repo
            .create(
                &project.id,
                "Sentinel Note",
                "unique_v7y_sentinel content for unified entity search test",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = proposal_repo
            .create(djinn_db::ProposalCreateInput {
                title: "Sentinel Proposal",
                body: "unique_v7y_sentinel content for unified entity search test",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));
        (server, project.slug(), note.permalink, proposal.short_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_entity_types_default_returns_both_notes_and_proposals() {
        let (server, project, _note_permalink, _proposal_short_id) =
            setup_with_note_and_proposal().await;

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.clone(),
                query: "unique_v7y_sentinel".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: None,
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        assert!(
            response.results.iter().any(|r| r.entity == "note"),
            "default (entity_types=None) should include note rows"
        );
        assert!(
            response.results.iter().any(|r| r.entity == "proposal"),
            "default (entity_types=None) should include proposal rows"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_entity_types_note_only_excludes_proposals() {
        let (server, project, _note_permalink, _proposal_short_id) =
            setup_with_note_and_proposal().await;

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.clone(),
                query: "unique_v7y_sentinel".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: Some(vec!["note".to_string()]),
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            !response.results.is_empty(),
            "should find the note with the sentinel word"
        );
        assert!(
            response.results.iter().all(|r| r.entity == "note"),
            "entity_types=[\"note\"] should return only note rows, got: {:?}",
            response
                .results
                .iter()
                .map(|r| &r.entity)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_entity_types_proposal_only_excludes_notes() {
        let (server, project, _note_permalink, _proposal_short_id) =
            setup_with_note_and_proposal().await;

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.clone(),
                query: "unique_v7y_sentinel".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: Some(vec!["proposal".to_string()]),
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            !response.results.is_empty(),
            "should find the proposal with the sentinel word"
        );
        assert!(
            response.results.iter().all(|r| r.entity == "proposal"),
            "entity_types=[\"proposal\"] should return only proposal rows, got: {:?}",
            response
                .results
                .iter()
                .map(|r| &r.entity)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_entity_types_empty_returns_no_results() {
        let (server, project, _note_permalink, _proposal_short_id) =
            setup_with_note_and_proposal().await;

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.clone(),
                query: "unique_v7y_sentinel".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: Some(vec![]),
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            response.results.is_empty(),
            "entity_types=[] should return no results"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_entity_field_is_proposal_for_proposal_rows() {
        let (server, project, _note_permalink, _proposal_short_id) =
            setup_with_note_and_proposal().await;

        // Default search returns both note and proposal rows.
        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.clone(),
                query: "unique_v7y_sentinel".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: None,
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(!response.results.is_empty());

        let note_results: Vec<_> = response
            .results
            .iter()
            .filter(|r| r.entity == "note")
            .collect();
        let proposal_results: Vec<_> = response
            .results
            .iter()
            .filter(|r| r.entity == "proposal")
            .collect();

        assert!(
            !note_results.is_empty(),
            "should have at least one note result with entity=\"note\""
        );
        assert!(
            !proposal_results.is_empty(),
            "should have at least one proposal result with entity=\"proposal\""
        );
    }

    // ── memory_build_context proposal surface tests ────────────────────────────

    /// Set up a seed note with distinctive content and a graduated proposal
    /// targeting the same project whose body overlaps the seed.
    async fn setup_for_build_context() -> (DjinnMcpServer, String, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), event_bus.clone());

        // Seed note with distinctive content.
        let seed = note_repo
            .create(
                &project.id,
                "Widget Refactor Plan",
                "refactor the widget_j3k module to support streaming output and batched writes",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Proposal with overlapping body text and accepted status.
        let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = proposal_repo
            .create(djinn_db::ProposalCreateInput {
                title: "Widget Refactor Proposal",
                body: "The widget_j3k module needs streaming output support and batched writes for performance",
                acceptance_criteria: Some(
                    r#"["streaming output works", "batched writes reduce latency"]"#,
                ),
                status: Some("approved"),
                body_format: None,
            })
            .await
            .unwrap();

        // Wire the proposal to this project via proposal_targets so the
        // title-body match path in build_context can find it.
        sqlx::query(
            "INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, 'primary')",
        )
        .bind(&proposal.id)
        .bind(&project.id)
        .execute(db.pool())
        .await
        .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));
        (server, project.slug(), seed.permalink)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_build_context_includes_relevant_proposals() {
        let (server, project, seed_permalink) = setup_for_build_context().await;

        let response = ops::memory_build_context(
            &server,
            BuildContextParams {
                project: project.clone(),
                url: format!("memory://{seed_permalink}"),
                depth: None,
                max_related: Some(20),
                budget: Some(8192),
                task_id: None,
                min_confidence: Some(0.0),
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            !response.proposals.is_empty(),
            "build_context should surface relevant proposals, got empty"
        );
        let overview = &response.proposals[0];
        assert_eq!(overview.title, "Widget Refactor Proposal");
        assert_eq!(overview.body_format, "markdown");
        assert_eq!(overview.status, "approved");
        assert_eq!(
            overview.acceptance_criteria,
            vec![
                "streaming output works".to_string(),
                "batched writes reduce latency".to_string()
            ]
        );
        assert!(
            overview.score.is_some(),
            "proposal overview should carry a score"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_build_context_does_not_duplicate_proposal_body_as_note() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), event_bus.clone());

        let seed = note_repo
            .create(
                &project.id,
                "Gadget Refactor Plan",
                "refactor the gadget_m1n module to support streaming output and batched writes",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        let proposal_repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = proposal_repo
            .create(djinn_db::ProposalCreateInput {
                title: "Gadget Refactor Proposal",
                body: "The gadget_m1n module needs streaming output support and batched writes for performance",
                acceptance_criteria: None,
                status: Some("approved"),
                body_format: None,
            })
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, 'primary')",
        )
        .bind(&proposal.id)
        .bind(&project.id)
        .execute(db.pool())
        .await
        .unwrap();

        // Count notes before build_context.
        let notes_before = note_repo
            .list(&project.id, None)
            .await
            .unwrap_or_default()
            .len();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = ops::memory_build_context(
            &server,
            BuildContextParams {
                project: project.slug(),
                url: format!("memory://{}", seed.permalink),
                depth: None,
                max_related: Some(20),
                budget: Some(8192),
                task_id: None,
                min_confidence: Some(0.0),
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);

        // Count notes after — should be unchanged (no proposal body duplicated).
        let note_repo_after =
            NoteRepository::new(server.state.db().clone(), server.state.event_bus());
        let notes_after = note_repo_after
            .list(&project.id, None)
            .await
            .unwrap_or_default()
            .len();

        assert_eq!(
            notes_before, notes_after,
            "build_context should NOT create new notes (proposal body is not duplicated as a note)"
        );
    }
}

#[cfg(test)]
mod tests {

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Project;
    use djinn_db::{Database, NoteRepository};
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::AssociationsParams;
    use rmcp::handler::server::wrapper::Parameters;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, _path: &std::path::Path) -> Project {
        use djinn_db::ProjectRepository;
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-project", "test", "test-project")
            .await
            .unwrap()
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

    async fn make_note(
        repo: &NoteRepository,
        project: &Project,
        _tmp: &tempfile::TempDir,
        title: &str,
    ) -> djinn_memory::Note {
        repo.create(&project.id, title, "content", "reference", "[]")
            .await
            .unwrap()
    }

    // ── helpers ────────────────────────────────────────────────────────────────

    async fn call_associations(
        server: &DjinnMcpServer,
        project: &str,
        identifier: &str,
        min_weight: Option<f64>,
        limit: Option<i64>,
    ) -> crate::tools::memory_tools::MemoryAssociationsResponse {
        server
            .memory_associations(Parameters(AssociationsParams {
                project: project.to_string(),
                identifier: identifier.to_string(),
                min_weight,
                limit,
            }))
            .await
            .0
    }

    // ── tests ──────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn returns_empty_array_for_note_with_no_associations() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let note = make_note(&repo, &project, &tmp, "Lonely Note").await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp = call_associations(&server, &project.slug(), &note.permalink, None, None).await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(resp.associations.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn returns_associations_in_both_directions() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let note_a = make_note(&repo, &project, &tmp, "Note A").await;
        let note_b = make_note(&repo, &project, &tmp, "Note B").await;
        let note_c = make_note(&repo, &project, &tmp, "Note C").await;

        // note_a–note_b: note_a could be note_a_id or note_b_id depending on UUID ordering
        repo.upsert_association(&note_a.id, &note_b.id, 1)
            .await
            .unwrap();
        // note_c–note_a: another direction
        repo.upsert_association(&note_c.id, &note_a.id, 1)
            .await
            .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp = call_associations(&server, &project.slug(), &note_a.permalink, None, None).await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(resp.associations.len(), 2, "expected both directions");

        let permalinks: Vec<&str> = resp
            .associations
            .iter()
            .map(|a| a.note_permalink.as_str())
            .collect();
        assert!(
            permalinks.contains(&note_b.permalink.as_str()),
            "missing note_b"
        );
        assert!(
            permalinks.contains(&note_c.permalink.as_str()),
            "missing note_c"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sorted_by_weight_descending() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let seed = make_note(&repo, &project, &tmp, "Seed").await;
        let heavy = make_note(&repo, &project, &tmp, "Heavy").await;
        let light = make_note(&repo, &project, &tmp, "Light").await;

        // Build up heavy association
        for _ in 0..5 {
            repo.upsert_association(&seed.id, &heavy.id, 1)
                .await
                .unwrap();
        }
        // Single light association
        repo.upsert_association(&seed.id, &light.id, 1)
            .await
            .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp = call_associations(&server, &project.slug(), &seed.permalink, None, None).await;

        assert!(resp.error.is_none());
        assert_eq!(resp.associations.len(), 2);
        // First result must have higher weight
        assert!(
            resp.associations[0].weight >= resp.associations[1].weight,
            "results not sorted descending: {:?}",
            resp.associations
                .iter()
                .map(|a| a.weight)
                .collect::<Vec<_>>()
        );
        assert_eq!(resp.associations[0].note_permalink, heavy.permalink);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn min_weight_filter_excludes_below_threshold() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let seed = make_note(&repo, &project, &tmp, "Seed").await;
        let strong = make_note(&repo, &project, &tmp, "Strong").await;
        let weak = make_note(&repo, &project, &tmp, "Weak").await;

        // Boost strong association significantly
        for _ in 0..400 {
            repo.upsert_association(&seed.id, &strong.id, 1)
                .await
                .unwrap();
        }
        // Single weak association (weight = 0.01)
        repo.upsert_association(&seed.id, &weak.id, 1)
            .await
            .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // With high min_weight, only the strong association should appear
        let resp =
            call_associations(&server, &project.slug(), &seed.permalink, Some(0.5), None).await;

        assert!(resp.error.is_none());
        assert_eq!(resp.associations.len(), 1);
        assert_eq!(resp.associations[0].note_permalink, strong.permalink);
        assert!(resp.associations[0].weight >= 0.5);
    }

    // ── Note ↔ Proposal heterogeneous association tests ───────────────────────
    //
    // These tests exercise the Wave 3 extension: `memory_associations` can
    // start from a note or a proposal and traverse typed entity edges from
    // the `memory_entity_associations` substrate (qb9o) in addition to the
    // legacy note↔note associations.

    use djinn_db::{MemoryEntityKind, MemoryEntityRef, ProposalRepository};

    async fn make_proposal(repo: &ProposalRepository, title: &str) -> djinn_core::models::Proposal {
        repo.create(djinn_db::ProposalCreateInput {
            title,
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap()
    }

    /// A note seed reaches a linked proposal through a typed
    /// `derived_from` edge written on the heterogeneous substrate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn note_seed_reaches_linked_proposal() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let proposal_repo =
            ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let note = make_note(&repo, &project, &tmp, "Source Note").await;
        let proposal = make_proposal(&proposal_repo, "Linked Proposal").await;

        // Write a proposal → note derived_from edge.
        repo.upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal.id),
            MemoryEntityRef::note(&note.id),
            MemoryEntityKind::DerivedFrom,
            0.9,
        )
        .await
        .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp = call_associations(&server, &project.slug(), &note.permalink, None, None).await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        // The seed resolved to a note.
        assert_eq!(resp.seed_entity_type, "note");
        // Exactly one association: the linked proposal.
        assert_eq!(resp.associations.len(), 1, "expected the linked proposal");
        let entry = &resp.associations[0];
        assert_eq!(entry.entity_type, "proposal");
        assert_eq!(entry.entity_id, proposal.id);
        assert_eq!(entry.entity_title, "Linked Proposal");
        assert_eq!(entry.entity_permalink, proposal.short_id);
        assert_eq!(entry.kind, "derived_from");
        assert!((entry.weight - 0.9).abs() < 1e-12);
    }

    /// A proposal seed reaches its `derived_from` note through a typed
    /// edge traversed in the reverse direction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_seed_reaches_derived_from_note() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let proposal_repo =
            ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let note = make_note(&repo, &project, &tmp, "Origin Note").await;
        let proposal = make_proposal(&proposal_repo, "Derived Proposal").await;

        // Write a proposal → note derived_from edge.
        repo.upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal.id),
            MemoryEntityRef::note(&note.id),
            MemoryEntityKind::DerivedFrom,
            0.75,
        )
        .await
        .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Query from the proposal side using its short_id as identifier.
        let resp =
            call_associations(&server, &project.slug(), &proposal.short_id, None, None).await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        // The seed resolved to a proposal.
        assert_eq!(resp.seed_entity_type, "proposal");
        // Exactly one association: the derived_from note.
        assert_eq!(resp.associations.len(), 1, "expected the derived_from note");
        let entry = &resp.associations[0];
        assert_eq!(entry.entity_type, "note");
        assert_eq!(entry.entity_permalink, note.permalink);
        assert_eq!(entry.entity_title, "Origin Note");
        assert_eq!(entry.kind, "derived_from");
        assert!((entry.weight - 0.75).abs() < 1e-12);
        // Legacy note fields are also populated for backward compat.
        assert_eq!(entry.note_permalink, note.permalink);
        assert_eq!(entry.note_title, "Origin Note");
    }

    /// A note seed that has BOTH a legacy co_access note association and a
    /// typed proposal edge returns both. The legacy note-only fields on the
    /// note association entry are preserved for backward compatibility.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn note_seed_returns_both_legacy_and_typed_associations() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let proposal_repo =
            ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let seed = make_note(&repo, &project, &tmp, "Seed Note").await;
        let other_note = make_note(&repo, &project, &tmp, "Other Note").await;
        let proposal = make_proposal(&proposal_repo, "Related Proposal").await;

        // Legacy co_access edge (note↔note).
        repo.upsert_association(&seed.id, &other_note.id, 1)
            .await
            .unwrap();
        // Typed proposal edge (proposal → note builds_on).
        repo.upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal.id),
            MemoryEntityRef::note(&seed.id),
            MemoryEntityKind::BuildsOn,
            0.8,
        )
        .await
        .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp = call_associations(&server, &project.slug(), &seed.permalink, None, None).await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(resp.associations.len(), 2, "expected both associations");

        // Find the note and proposal entries.
        let note_entry = resp
            .associations
            .iter()
            .find(|a| a.entity_type == "note")
            .expect("expected a note association entry");
        let proposal_entry = resp
            .associations
            .iter()
            .find(|a| a.entity_type == "proposal")
            .expect("expected a proposal association entry");

        // The legacy note entry has backward-compatible fields populated.
        assert_eq!(note_entry.note_permalink, other_note.permalink);
        assert_eq!(note_entry.note_title, "Other Note");
        assert_eq!(note_entry.entity_permalink, other_note.permalink);

        // The typed proposal entry has proposal metadata.
        assert_eq!(proposal_entry.entity_id, proposal.id);
        assert_eq!(proposal_entry.entity_permalink, proposal.short_id);
        assert_eq!(proposal_entry.kind, "builds_on");
    }

    /// A proposal seed with no associations returns an empty array and
    /// resolves as a proposal seed type.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_seed_with_no_associations_returns_empty() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let proposal_repo =
            ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let proposal = make_proposal(&proposal_repo, "Lonely Proposal").await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp =
            call_associations(&server, &project.slug(), &proposal.short_id, None, None).await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(resp.seed_entity_type, "proposal");
        assert_eq!(resp.associations.len(), 0);
    }
}

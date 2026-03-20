#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Project;
    use djinn_db::{Database, NoteRepository};
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRuntimeOps, StubSlotPoolOps, StubSyncOps,
    };
    use crate::tools::memory_tools::AssociationsParams;
    use rmcp::handler::server::wrapper::Parameters;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, path: &std::path::Path) -> Project {
        use djinn_db::ProjectRepository;
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-project", path.to_str().unwrap())
            .await
            .unwrap()
    }

    fn test_mcp_state(db: Database, tx: &broadcast::Sender<DjinnEventEnvelope>) -> McpState {
        McpState::new(
            db,
            event_bus_for(tx),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            "test-user".into(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
        )
    }

    async fn make_note(
        repo: &NoteRepository,
        project: &Project,
        tmp: &tempfile::TempDir,
        title: &str,
    ) -> djinn_core::models::Note {
        repo.create(&project.id, tmp.path(), title, "content", "reference", "[]")
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
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let note = make_note(&repo, &project, &tmp, "Lonely Note").await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let resp = call_associations(
            &server,
            tmp.path().to_str().unwrap(),
            &note.permalink,
            None,
            None,
        )
        .await;

        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(resp.associations.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn returns_associations_in_both_directions() {
        let tmp = tempfile::tempdir().unwrap();
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

        let resp = call_associations(
            &server,
            tmp.path().to_str().unwrap(),
            &note_a.permalink,
            None,
            None,
        )
        .await;

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
        let tmp = tempfile::tempdir().unwrap();
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

        let resp = call_associations(
            &server,
            tmp.path().to_str().unwrap(),
            &seed.permalink,
            None,
            None,
        )
        .await;

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
        let tmp = tempfile::tempdir().unwrap();
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
        let resp = call_associations(
            &server,
            tmp.path().to_str().unwrap(),
            &seed.permalink,
            Some(0.5),
            None,
        )
        .await;

        assert!(resp.error.is_none());
        assert_eq!(resp.associations.len(), 1);
        assert_eq!(resp.associations[0].note_permalink, strong.permalink);
        assert!(resp.associations[0].weight >= 0.5);
    }
}

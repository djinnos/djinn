// Integration tests for extracted shared memory ops and MCP adapters.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRuntimeOps, StubSlotPoolOps, StubSyncOps,
    };
    use crate::tools::memory_tools::ops;
    use crate::tools::memory_tools::{BuildContextParams, ListParams, ReadParams, SearchParams};

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
            "test-user".into(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
        )
    }

    async fn setup_server() -> (DjinnMcpServer, tempfile::TempDir, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), event_bus);
        let primary = note_repo
            .create(
                &project.id,
                tmp.path(),
                "Seed Note",
                "Seed note content with links to [[Related Note]] and architecture context.",
                "adr",
                "[]",
            )
            .await
            .unwrap();
        let related = note_repo
            .create(
                &project.id,
                tmp.path(),
                "Related Note",
                "Related architecture context note.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let _ = related;
        let folder_note = note_repo
            .create(
                &project.id,
                tmp.path(),
                "Folder Note",
                "Folder wildcard note.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));
        (server, tmp, primary.permalink, folder_note.folder)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_read_ops_updates_access_tracking() {
        let (server, tmp, permalink, _folder) = setup_server().await;
        let project_id =
            ProjectRepository::new(server.state.db().clone(), server.state.event_bus())
                .resolve(tmp.path().to_str().unwrap())
                .await
                .unwrap()
                .expect("project id");
        let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
        let before = repo
            .get_by_permalink(&project_id, &permalink)
            .await
            .unwrap()
            .expect("seed note before read");

        let response = ops::memory_read(
            &server,
            ReadParams {
                project: tmp.path().to_str().unwrap().to_string(),
                identifier: permalink,
            },
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        assert!(response.id.is_some());
        let after = repo
            .get(response.id.as_deref().unwrap())
            .await
            .unwrap()
            .expect("seed note after read");
        assert_eq!(after.access_count, before.access_count + 1);
        assert_eq!(response.title.as_deref(), Some(before.title.as_str()));
        assert_eq!(response.content.as_deref(), Some(before.content.as_str()));
        assert_eq!(server.recorded_note_ids().await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_ops_applies_task_fallback_and_success_shape() {
        let (server, tmp, _permalink, _folder) = setup_server().await;

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: tmp.path().to_str().unwrap().to_string(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
            },
            Some("task-123"),
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        assert!(!response.results.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_build_context_ops_preserves_prefix_and_wildcard_behavior() {
        let (server, tmp, permalink, folder) = setup_server().await;

        let single = ops::memory_build_context(
            &server,
            BuildContextParams {
                project: tmp.path().to_str().unwrap().to_string(),
                url: format!("memory://{permalink}"),
                depth: None,
                max_related: Some(10),
                budget: Some(4096),
                task_id: None,
            },
            None,
        )
        .await;
        assert!(
            single.error.is_none(),
            "unexpected error: {:?}",
            single.error
        );
        assert_eq!(single.primary.len(), 1);

        let wildcard = ops::memory_build_context(
            &server,
            BuildContextParams {
                project: tmp.path().to_str().unwrap().to_string(),
                url: format!("memory://{}/*", folder),
                depth: None,
                max_related: Some(10),
                budget: Some(4096),
                task_id: None,
            },
            None,
        )
        .await;
        assert!(
            wildcard.error.is_none(),
            "unexpected error: {:?}",
            wildcard.error
        );
        assert!(!wildcard.primary.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_memory_adapters_delegate_to_shared_ops() {
        let (server, tmp, permalink, _folder) = setup_server().await;

        let search = server
            .memory_search(rmcp::handler::server::wrapper::Parameters(SearchParams {
                project: tmp.path().to_str().unwrap().to_string(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
            }))
            .await
            .0;
        assert!(search.error.is_none());
        assert!(!search.results.is_empty());

        let list = server
            .memory_list(rmcp::handler::server::wrapper::Parameters(ListParams {
                project: tmp.path().to_str().unwrap().to_string(),
                folder: None,
                note_type: None,
                depth: Some(1),
            }))
            .await
            .0;
        assert!(list.error.is_none());
        assert!(!list.notes.is_empty());

        let read = server
            .memory_read(rmcp::handler::server::wrapper::Parameters(ReadParams {
                project: tmp.path().to_str().unwrap().to_string(),
                identifier: permalink,
            }))
            .await
            .0;
        assert!(read.error.is_none());
        assert!(read.id.is_some());
    }
}

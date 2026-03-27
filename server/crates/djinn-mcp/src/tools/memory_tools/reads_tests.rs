// Integration tests for memory read tools (memory_history, etc.).
#[cfg(test)]
mod tests {
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::handler::server::wrapper::Parameters;
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRuntimeOps, StubSlotPoolOps, StubSyncOps,
    };
    use crate::tools::memory_tools::HistoryParams;

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
            Some(std::sync::Arc::new(StubCoordinatorOps)),
            Some(std::sync::Arc::new(StubSlotPoolOps)),
            std::sync::Arc::new(StubLspOps),
            std::sync::Arc::new(StubSyncOps),
            std::sync::Arc::new(StubRuntimeOps),
            std::sync::Arc::new(StubGitOps),
        )
    }

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_returns_error_for_db_only_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let state = test_mcp_state(db.clone(), &tx);
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Create a DB-only note (storage='db')
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note = repo
            .create_db_note(
                &project.id,
                "DB Pattern Note",
                "db note content for testing",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // Verify the note is stored in DB only
        assert_eq!(note.storage, "db");
        assert!(note.file_path.is_empty());

        // Call memory_history - should return error for DB-only notes
        let response = server
            .memory_history(Parameters(HistoryParams {
                project: tmp.path().to_str().unwrap().to_string(),
                permalink: note.permalink.clone(),
                limit: Some(10),
            }))
            .await
            .0;

        // Should have empty history and an error message
        assert!(
            response.error.is_some(),
            "memory_history should return error for DB-only notes"
        );
        let error = response.error.unwrap();
        assert!(
            error.contains("database only") || error.contains("git history unavailable"),
            "Error should indicate git history unavailable for DB-only notes: got {}",
            error
        );
        assert!(
            response.history.is_empty(),
            "history should be empty for DB-only notes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_returns_error_for_missing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let state = test_mcp_state(db.clone(), &tx);
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Call memory_history with non-existent permalink
        let response = server
            .memory_history(Parameters(HistoryParams {
                project: tmp.path().to_str().unwrap().to_string(),
                permalink: "non-existent-note".to_string(),
                limit: Some(10),
            }))
            .await
            .0;

        // Should have empty history and a "not found" error
        assert!(
            response.error.is_some(),
            "memory_history should return error for missing note"
        );
        let error = response.error.unwrap();
        assert!(
            error.contains("not found"),
            "Error should indicate note not found: got {}",
            error
        );
        assert!(
            response.history.is_empty(),
            "history should be empty for missing notes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_works_for_file_backed_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let state = test_mcp_state(db.clone(), &tx);
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Create a file-backed note
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "File Backed Note",
                "content for file-backed note testing",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Verify the note is file-backed
        assert_eq!(note.storage, "file");
        assert!(!note.file_path.is_empty());

        // Call memory_history - for file-backed notes in a test environment without git,
        // we expect an empty history (not an error about DB-only storage)
        let response = server
            .memory_history(Parameters(HistoryParams {
                project: tmp.path().to_str().unwrap().to_string(),
                permalink: note.permalink.clone(),
                limit: Some(10),
            }))
            .await
            .0;

        // Should NOT have the "DB-only" error
        if let Some(ref error) = response.error {
            assert!(
                !error.contains("database only"),
                "Should not get DB-only error for file-backed note: got {}",
                error
            );
        }

        // Either success with empty history (no git) or success with entries (if git exists)
        // The key point is we didn't get a DB-only error and didn't panic
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_returns_error_for_invalid_project() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let state = test_mcp_state(db.clone(), &tx);
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Call memory_history with non-existent project
        let response = server
            .memory_history(Parameters(HistoryParams {
                project: "/non/existent/project".to_string(),
                permalink: "some-note".to_string(),
                limit: Some(10),
            }))
            .await
            .0;

        // Should have empty history and a "project not found" error
        assert!(
            response.error.is_some(),
            "memory_history should return error for invalid project"
        );
        let error = response.error.unwrap();
        assert!(
            error.contains("project not found"),
            "Error should indicate project not found: got {}",
            error
        );
        assert!(
            response.history.is_empty(),
            "history should be empty for invalid project"
        );
    }
}

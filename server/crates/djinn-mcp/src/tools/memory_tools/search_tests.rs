// Tests for memory_diff and memory_history functionality
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps, StubSyncOps,
    };
    use crate::tools::memory_tools::{DiffParams, HistoryParams};

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
            Arc::new(StubRepoGraphOps),
        )
    }

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_diff_returns_error_for_db_only_notes() {
        let tmp = workspace_tempdir();
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

        // Create a DB-only note (no file on disk)
        let db_note = note_repo
            .create_db_note(&project.id, "DB Only Note", "db note body", "pattern", "[]")
            .await
            .unwrap();
        assert_eq!(db_note.storage, "db");

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_diff(rmcp::handler::server::wrapper::Parameters(DiffParams {
                project: project.path.clone(),
                permalink: db_note.permalink.clone(),
                sha: None,
            }))
            .await
            .0;

        // Should return error for DB-only notes
        assert!(response.error.is_some(), "Expected error for DB-only note");
        let error_msg = response.error.unwrap();
        assert!(
            error_msg.contains("database only"),
            "Error should mention 'database only': {}",
            error_msg
        );
        assert!(
            error_msg.contains("storage='db'"),
            "Error should show storage='db': {}",
            error_msg
        );
        assert!(
            response.diff.is_empty(),
            "Diff should be empty for DB-only note"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_diff_returns_error_for_project_not_found() {
        let _tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_diff(rmcp::handler::server::wrapper::Parameters(DiffParams {
                project: "/nonexistent/project".to_string(),
                permalink: "test/note".to_string(),
                sha: None,
            }))
            .await
            .0;

        assert!(response.error.is_some());
        assert!(
            response
                .error
                .as_ref()
                .unwrap()
                .contains("project not found")
        );
        assert!(response.diff.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_diff_returns_error_for_note_not_found() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_diff(rmcp::handler::server::wrapper::Parameters(DiffParams {
                project: project.path.clone(),
                permalink: "nonexistent/note".to_string(),
                sha: None,
            }))
            .await
            .0;

        assert!(response.error.is_some());
        assert!(response.error.as_ref().unwrap().contains("note not found"));
        assert!(response.diff.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_diff_preserves_behavior_for_file_backed_notes() {
        let tmp = workspace_tempdir();
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

        // Create a file-backed note
        let file_note = note_repo
            .create(
                &project.id,
                tmp.path(),
                "File Backed Note",
                "file note body content",
                "adr",
                "[]",
            )
            .await
            .unwrap();
        assert_eq!(file_note.storage, "file");

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_diff(rmcp::handler::server::wrapper::Parameters(DiffParams {
                project: project.path.clone(),
                permalink: file_note.permalink.clone(),
                sha: None,
            }))
            .await
            .0;

        // For file-backed notes, the git diff is attempted
        // In test environment without git history, this may return empty diff
        // but should not return an error
        assert!(
            response.error.is_none(),
            "File-backed notes should not return error, got: {:?}",
            response.error
        );
        // Diff may be empty in test environment, but no error should occur
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_returns_error_for_db_only_notes() {
        let tmp = workspace_tempdir();
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

        let db_note = note_repo
            .create_db_note(
                &project.id,
                "DB Only History Note",
                "db note body",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        assert_eq!(db_note.storage, "db");

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_history(rmcp::handler::server::wrapper::Parameters(HistoryParams {
                project: project.path.clone(),
                permalink: db_note.permalink.clone(),
                limit: None,
            }))
            .await
            .0;

        assert!(response.error.is_some(), "Expected error for DB-only note");
        let error_msg = response.error.unwrap();
        assert!(
            error_msg.contains("database only"),
            "Error should mention 'database only': {}",
            error_msg
        );
        assert!(
            error_msg.contains("storage='db'"),
            "Error should show storage='db': {}",
            error_msg
        );
        assert!(
            response.history.is_empty(),
            "History should be empty for DB-only note"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_preserves_behavior_for_file_backed_notes() {
        let tmp = workspace_tempdir();
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

        let file_note = note_repo
            .create(
                &project.id,
                tmp.path(),
                "File Backed History Note",
                "file note body content",
                "adr",
                "[]",
            )
            .await
            .unwrap();
        assert_eq!(file_note.storage, "file");

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_history(rmcp::handler::server::wrapper::Parameters(HistoryParams {
                project: project.path.clone(),
                permalink: file_note.permalink.clone(),
                limit: None,
            }))
            .await
            .0;

        assert!(
            response.error.is_none(),
            "File-backed notes should not return error, got: {:?}",
            response.error
        );
    }
}

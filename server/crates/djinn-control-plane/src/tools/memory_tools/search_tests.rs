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
        StubSlotPoolOps,
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

    // memory_diff is a back-compat shim under the db-only knowledge-base
    // cut-over: it always returns an empty diff with an explanatory error
    // string. The pre-cut-over storage-routing tests are gone; this single
    // contract test confirms the shape of the response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_diff_returns_empty_diff_with_explanatory_error() {
        let _tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), event_bus);
        let note = note_repo
            .create(
                &project.id,
                "Any Note",
                "body",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = server
            .memory_diff(rmcp::handler::server::wrapper::Parameters(DiffParams {
                project: project.slug(),
                permalink: note.permalink.clone(),
                sha: None,
            }))
            .await
            .0;

        assert!(response.error.is_some(), "memory_diff should now return error");
        assert!(response.diff.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_history_returns_error_for_db_only_notes() {
        let _tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-project", "test", "test-project")
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
                project: project.slug(),
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

    // memory_history previously had a "file-backed-vs-db" branching
    // assertion. With db-only KB storage every note returns an error; we
    // only assert the db-only error path.
}

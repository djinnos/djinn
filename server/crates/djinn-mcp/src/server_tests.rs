#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::{Json, handler::server::wrapper::Parameters};
    use tokio::time::sleep;

    use crate::{
        server::{DjinnMcpServer, SessionEndHookSessionManager},
        state::stubs::test_mcp_state,
        tools::memory_tools::{EditParams, ReadParams, WriteParams},
    };

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    async fn wait_for_summaries_change(
        repo: &NoteRepository,
        note_id: &str,
        previous_overview: Option<String>,
    ) -> djinn_core::models::Note {
        for _ in 0..40 {
            let note = repo.get(note_id).await.unwrap().unwrap();
            if note
                .abstract_
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
                && note
                    .overview
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty())
                && note.overview != previous_overview
            {
                return note;
            }
            sleep(Duration::from_millis(25)).await;
        }
        repo.get(note_id).await.unwrap().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_and_edit_regenerate_summaries_without_blocking_ack() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Summary Note".to_string(),
                content: "Sentence one. Sentence two.\n\nMore context follows here.".to_string(),
                note_type: "reference".to_string(),
                tags: None,
            }))
            .await;

        assert!(created.error.is_none());
        let note_id = created.id.clone().expect("memory_write returns note id");
        let created_note = repo.get(&note_id).await.unwrap().unwrap();
        assert!(created_note.abstract_.is_none());
        assert!(created_note.overview.is_none());

        let generated = wait_for_summaries_change(&repo, &note_id, None).await;
        assert!(
            generated
                .abstract_
                .as_deref()
                .is_some_and(|v| v.contains("Sentence one"))
        );
        assert!(
            generated
                .overview
                .as_deref()
                .is_some_and(|v| v.contains("Sentence one"))
        );

        let previous_overview = generated.overview.clone();

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: tmp.path().to_str().unwrap().to_string(),
                identifier: note_id.clone(),
                operation: "append".to_string(),
                content: "Fresh closing details.".to_string(),
                find_text: None,
                section: None,
                note_type: None,
            }))
            .await;

        assert!(edited.error.is_none());
        let regenerated = wait_for_summaries_change(&repo, &note_id, previous_overview).await;
        assert!(
            regenerated
                .overview
                .as_deref()
                .is_some_and(|v| v.contains("Fresh closing details."))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_access_backfills_missing_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let legacy = repo
            .create(
                &project.id,
                tmp.path(),
                "Legacy Note",
                "Legacy note body. It has enough content for summaries.\n\nSecond paragraph here.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let server = DjinnMcpServer::new(state);

        let Json(response) = server
            .memory_read(Parameters(ReadParams {
                project: tmp.path().to_str().unwrap().to_string(),
                identifier: legacy.permalink.clone(),
            }))
            .await;

        assert!(response.error.is_none());
        let updated = wait_for_summaries_change(&repo, &legacy.id, None).await;
        assert!(
            updated
                .abstract_
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
        );
        assert!(
            updated
                .overview
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
        );
        assert_ne!(updated.abstract_.as_deref(), Some(""));
        assert_ne!(updated.overview.as_deref(), Some(""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_session_flushes_reads_from_same_session_server() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note_a = repo
            .create(
                &project.id,
                tmp.path(),
                "Note A",
                "alpha",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let note_b = repo
            .create(&project.id, tmp.path(), "Note B", "beta", "reference", "[]")
            .await
            .unwrap();

        let manager = Arc::new(SessionEndHookSessionManager::new(state));
        let (session_id, _transport) =
            rmcp::transport::streamable_http_server::SessionManager::create_session(&*manager)
                .await
                .unwrap();

        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&note_a.id).await;
        server.record_memory_read(&note_b.id).await;
        assert_eq!(
            server.recorded_note_ids().await,
            vec![note_a.id.clone(), note_b.id.clone()]
        );

        rmcp::transport::streamable_http_server::SessionManager::close_session(
            &*manager,
            &session_id,
        )
        .await
        .unwrap();

        let associations = repo.get_associations_for_note(&note_a.id).await.unwrap();
        assert_eq!(associations.len(), 1);
        let assoc = &associations[0];
        let pair = [assoc.note_a_id.as_str(), assoc.note_b_id.as_str()];
        assert!(pair.contains(&note_a.id.as_str()));
        assert!(pair.contains(&note_b.id.as_str()));
        assert!(manager.server_for_session(&session_id).await.is_none());
    }
}

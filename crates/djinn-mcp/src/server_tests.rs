#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository, repositories::note};
    use rmcp::transport::streamable_http_server::SessionManager;

    use crate::{server::SessionEndHookSessionManager, state::stubs::test_mcp_state};

    async fn make_note(
        repo: &NoteRepository,
        project_id: &str,
        path: &std::path::Path,
        title: &str,
    ) -> djinn_core::models::Note {
        repo.create(project_id, path, title, title, "reference", "[]")
            .await
            .unwrap()
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
        let (session_id, _transport) = manager.create_session().await.unwrap();

        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&note_a.id).await;
        server.record_memory_read(&note_b.id).await;
        assert_eq!(
            server.recorded_note_ids().await,
            vec![note_a.id.clone(), note_b.id.clone()]
        );

        manager.close_session(&session_id).await.unwrap();

        let associations = repo.get_associations_for_note(&note_a.id).await.unwrap();
        assert_eq!(associations.len(), 1);
        let assoc = &associations[0];
        let pair = [assoc.note_a_id.as_str(), assoc.note_b_id.as_str()];
        assert!(pair.contains(&note_a.id.as_str()));
        assert!(pair.contains(&note_b.id.as_str()));
        assert!(manager.server_for_session(&session_id).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_session_boosts_low_confidence_note_when_coaccessed_with_high_confidence_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note_a = make_note(&repo, &project.id, tmp.path(), "Note A").await;
        let note_b = make_note(&repo, &project.id, tmp.path(), "Note B").await;

        sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
            .bind(0.9_f64)
            .bind(&note_a.id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
            .bind(0.4_f64)
            .bind(&note_b.id)
            .execute(db.pool())
            .await
            .unwrap();

        let manager = Arc::new(SessionEndHookSessionManager::new(state));
        let (session_id, _transport) = manager.create_session().await.unwrap();
        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&note_a.id).await;
        server.record_memory_read(&note_b.id).await;

        manager.close_session(&session_id).await.unwrap();

        let boosted: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
            .bind(&note_b.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let expected = note::bayesian_update(0.4, note::CO_ACCESS_HIGH);
        assert!((boosted - expected).abs() < 1e-9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_session_skips_boost_when_both_notes_are_already_high_confidence() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note_a = make_note(&repo, &project.id, tmp.path(), "Note A").await;
        let note_c = make_note(&repo, &project.id, tmp.path(), "Note C").await;

        for (id, confidence) in [(&note_a.id, 0.9_f64), (&note_c.id, 0.85_f64)] {
            sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
                .bind(confidence)
                .bind(id)
                .execute(db.pool())
                .await
                .unwrap();
        }

        let manager = Arc::new(SessionEndHookSessionManager::new(state));
        let (session_id, _transport) = manager.create_session().await.unwrap();
        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&note_a.id).await;
        server.record_memory_read(&note_c.id).await;

        manager.close_session(&session_id).await.unwrap();

        let confidence_a: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
            .bind(&note_a.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let confidence_c: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
            .bind(&note_c.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!((confidence_a - 0.9).abs() < 1e-9);
        assert!((confidence_c - 0.85).abs() < 1e-9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_session_with_single_note_does_not_change_confidence() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note = make_note(&repo, &project.id, tmp.path(), "Solo").await;

        sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
            .bind(0.4_f64)
            .bind(&note.id)
            .execute(db.pool())
            .await
            .unwrap();

        let manager = Arc::new(SessionEndHookSessionManager::new(state));
        let (session_id, _transport) = manager.create_session().await.unwrap();
        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&note.id).await;

        manager.close_session(&session_id).await.unwrap();

        let confidence: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
            .bind(&note.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!((confidence - 0.4).abs() < 1e-9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_session_with_one_high_confidence_note_boosts_all_low_confidence_partners_once() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let high = make_note(&repo, &project.id, tmp.path(), "High").await;
        let low_a = make_note(&repo, &project.id, tmp.path(), "Low A").await;
        let low_b = make_note(&repo, &project.id, tmp.path(), "Low B").await;

        for (id, confidence) in [(&high.id, 0.9_f64), (&low_a.id, 0.4_f64), (&low_b.id, 0.3_f64)] {
            sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
                .bind(confidence)
                .bind(id)
                .execute(db.pool())
                .await
                .unwrap();
        }

        let manager = Arc::new(SessionEndHookSessionManager::new(state));
        let (session_id, _transport) = manager.create_session().await.unwrap();
        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&high.id).await;
        server.record_memory_read(&low_a.id).await;
        server.record_memory_read(&low_b.id).await;
        server.record_memory_read(&low_a.id).await;

        manager.close_session(&session_id).await.unwrap();

        let confidence_low_a: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
            .bind(&low_a.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let confidence_low_b: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
            .bind(&low_b.id)
            .fetch_one(db.pool())
            .await
            .unwrap();

        assert!((confidence_low_a - note::bayesian_update(0.4, note::CO_ACCESS_HIGH)).abs() < 1e-9);
        assert!((confidence_low_b - note::bayesian_update(0.3, note::CO_ACCESS_HIGH)).abs() < 1e-9);
    }
}

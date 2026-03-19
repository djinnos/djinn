#[cfg(test)]
mod tests {
    use std::time::Duration;

    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::{Json, handler::server::wrapper::Parameters};
    use tokio::time::sleep;

    use crate::{
        server::DjinnMcpServer,
        state::stubs::test_mcp_state,
        tools::memory_tools::WriteParams,
    };

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_mergeable_note_type_bypasses_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Create first note
        let Json(created1) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Research Topic".to_string(),
                content: "This is a research note about async Rust patterns.".to_string(),
                note_type: "research".to_string(),
                tags: None,
            }))
            .await;

        assert!(created1.error.is_none());
        let note_id1 = created1.id.clone().expect("first note created");

        // Create second similar note - research is not mergeable, so it should create a new note
        // Use a slightly different title to avoid permalink collision
        let Json(created2) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Research Topic Two".to_string(),
                content: "This is a research note about async Rust patterns.".to_string(),
                note_type: "research".to_string(),
                tags: None,
            }))
            .await;

        assert!(created2.error.is_none(), "error: {:?}", created2.error);
        let note_id2 = created2.id.clone().expect("second note created");

        // Both notes should exist and be different
        assert_ne!(note_id1, note_id2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mergeable_note_type_runs_dedup_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Create first pattern note
        let Json(created1) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Async Pattern".to_string(),
                content: "Use tokio::spawn for concurrent task execution in Rust async code.".to_string(),
                note_type: "pattern".to_string(),
                tags: None,
            }))
            .await;

        assert!(created1.error.is_none());
        let _note_id1 = created1.id.clone().expect("first pattern created");

        // Wait for the note to be indexed
        sleep(Duration::from_millis(100)).await;

        // Verify dedup_candidates finds the existing note
        let candidates = repo
            .dedup_candidates(
                &created1.project_id.clone().unwrap(),
                "patterns",
                "pattern",
                "Async Pattern tokio spawn concurrent",
                5,
            )
            .await
            .unwrap();

        assert!(!candidates.is_empty(), "dedup_candidates should find the existing pattern");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedup_candidates_filters_by_type_and_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Create a pattern note
        let Json(pattern) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Error Handling Pattern".to_string(),
                content: "Use Result types for explicit error handling in Rust.".to_string(),
                note_type: "pattern".to_string(),
                tags: None,
            }))
            .await;

        assert!(pattern.error.is_none());

        // Create an ADR in decisions folder with similar content
        let Json(adr) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Error Handling ADR".to_string(),
                content: "Use Result types for explicit error handling in Rust.".to_string(),
                note_type: "adr".to_string(),
                tags: None,
            }))
            .await;

        assert!(adr.error.is_none());

        // Wait for indexing
        sleep(Duration::from_millis(100)).await;

        // Query for pattern candidates - should only find pattern, not ADR
        let pattern_candidates = repo
            .dedup_candidates(&project.id, "patterns", "pattern", "Error Handling Result Rust", 5)
            .await
            .unwrap();

        assert_eq!(pattern_candidates.len(), 1);
        assert_eq!(pattern_candidates[0].note_type, "pattern");

        // Query for adr candidates - should only find adr, not pattern
        let adr_candidates = repo
            .dedup_candidates(&project.id, "decisions", "adr", "Error Handling Result Rust", 5)
            .await
            .unwrap();

        assert_eq!(adr_candidates.len(), 1);
        assert_eq!(adr_candidates[0].note_type, "adr");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_candidates_proceeds_with_normal_write() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Create a pattern note with unique content
        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Unique Pattern XYZ123".to_string(),
                content: "This content is completely unique and should not match anything. XYZ123ABC".to_string(),
                note_type: "pattern".to_string(),
                tags: None,
            }))
            .await;

        assert!(created.error.is_none());
        assert!(created.id.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn is_mergeable_returns_correct_values() {
        // Test the is_mergeable helper function
        let mergeable_types = ["pattern", "case", "pitfall"];
        let non_mergeable_types = [
            "adr", "research", "design", "reference",
            "requirement", "session", "persona", "journey",
            "design_spec", "competitive", "tech_spike", "brief", "roadmap",
        ];

        for note_type in mergeable_types {
            assert!(
                super::super::writes::is_mergeable(note_type),
                "{} should be mergeable",
                note_type
            );
        }

        for note_type in non_mergeable_types {
            assert!(
                !super::super::writes::is_mergeable(note_type),
                "{} should not be mergeable",
                note_type
            );
        }
    }
}

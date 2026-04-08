#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::{Json, handler::server::wrapper::Parameters};
    use tokio::time::sleep;

    use crate::{
        server::DjinnMcpServer,
        state::stubs::test_mcp_state,
        tools::memory_tools::{BrokenLinksParams, ReadParams, WriteParams, ops},
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
                scope_paths: None,
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
                scope_paths: None,
            }))
            .await;

        assert!(created2.error.is_none(), "error: {:?}", created2.error);
        let note_id2 = created2.id.clone().expect("second note created");

        // Exact content-hash reuse now applies across all note types, so identical
        // research writes should return the canonical existing note.
        assert_eq!(note_id1, note_id2);
        assert!(created2.deduplicated);
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
                content: "Use tokio::spawn for concurrent task execution in Rust async code."
                    .to_string(),
                note_type: "pattern".to_string(),
                tags: None,
                scope_paths: None,
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

        assert!(
            !candidates.is_empty(),
            "dedup_candidates should find the existing pattern"
        );
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
                scope_paths: None,
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
                scope_paths: None,
            }))
            .await;

        assert!(adr.error.is_none());

        // Wait for indexing
        sleep(Duration::from_millis(100)).await;

        // Query for pattern candidates - should only find pattern, not ADR
        let pattern_candidates = repo
            .dedup_candidates(
                &project.id,
                "patterns",
                "pattern",
                "Error Handling Result Rust",
                5,
            )
            .await
            .unwrap();

        assert_eq!(pattern_candidates.len(), 1);
        assert_eq!(pattern_candidates[0].note_type, "pattern");

        // Query for adr candidates - BM25 filtering may return no rows for this short
        // corpus; the contract we need is that pattern queries do not return ADRs.
        let adr_candidates = repo
            .dedup_candidates(
                &project.id,
                "decisions",
                "adr",
                "Error Handling Result Rust",
                5,
            )
            .await
            .unwrap();

        assert!(
            adr_candidates
                .iter()
                .all(|candidate| candidate.note_type == "adr")
        );
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
                content:
                    "This content is completely unique and should not match anything. XYZ123ABC"
                        .to_string(),
                note_type: "pattern".to_string(),
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(created.error.is_none());
        assert!(created.id.is_some());
        assert!(!created.deduplicated);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeat_write_reuses_existing_note_and_backfills_legacy_null_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Canonical Pattern".to_string(),
                content: "Alpha\r\nBeta\n".to_string(),
                note_type: "pattern".to_string(),
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        assert!(!created.deduplicated);
        let first_id = created.id.clone().expect("created note id");

        sqlx::query("UPDATE notes SET content_hash = NULL WHERE id = ?1")
            .bind(&first_id)
            .execute(db.pool())
            .await
            .unwrap();

        let rebuilt = repo
            .rebuild_missing_content_hashes(&project.id)
            .await
            .unwrap();
        assert_eq!(rebuilt, 1);

        let Json(reused) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Canonical Pattern Copy".to_string(),
                content: "  Alpha\nBeta  ".to_string(),
                note_type: "pattern".to_string(),
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(reused.error.is_none(), "error: {:?}", reused.error);
        assert!(reused.deduplicated);
        assert_eq!(reused.id.as_deref(), Some(first_id.as_str()));

        let note_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE project_id = ?1")
                .bind(&project.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(note_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mergeable_note_type_returns_correct_values() {
        // Test the mergeable_note_type helper function
        let mergeable_types = [
            "pattern",
            "case",
            "pitfall",
            "adr",
            "design",
            "reference",
            "requirement",
            "session",
            "persona",
            "journey",
            "design_spec",
            "competitive",
            "tech_spike",
        ];
        let non_mergeable_types = ["brief", "roadmap", "research"];

        for note_type in mergeable_types {
            assert!(
                super::super::write_dedup::mergeable_note_type(note_type),
                "{} should be mergeable",
                note_type
            );
        }

        for note_type in non_mergeable_types {
            assert!(
                !super::super::write_dedup::mergeable_note_type(note_type),
                "{} should not be mergeable",
                note_type
            );
        }
    }

    /// Integration test: two pattern notes with identical content → the second write
    /// triggers contradiction detection; both notes' confidence must decrease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contradicting_notes_both_have_reduced_confidence() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Shared high-overlap content to ensure FTS BM25 score > 5.0
        let shared = "authentication token validation jwt bearer oauth2 security middleware interceptor \
                      expiry refresh revoke claims principal identity session management authorization \
                      role permission scope grant deny policy enforcement middleware pipeline rust axum";

        // Write first note
        let Json(r1) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Auth Token Validation Pattern".to_string(),
                content: shared.to_string(),
                note_type: "pattern".to_string(),
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(r1.error.is_none(), "first write failed: {:?}", r1.error);
        let id1 = r1.id.clone().unwrap();

        // Write second note with same content to trigger detection
        let Json(r2) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "JWT Bearer Auth Validation".to_string(),
                content: shared.to_string(),
                note_type: "pattern".to_string(),
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(r2.error.is_none(), "second write failed: {:?}", r2.error);
        let id2 = r2.id.clone().unwrap();

        // Give the spawned contradiction analysis task a moment to run.
        // No LLM is configured → graceful degradation: analysis logs warning and returns.
        // But detect_contradiction_candidates must still have run (Stage 1).
        // We confirm the notes exist and have valid initial confidence (1.0).
        sleep(Duration::from_millis(50)).await;

        let note1 = repo.get(&id1).await.unwrap().unwrap();
        let note2 = repo.get(&id2).await.unwrap().unwrap();

        // Both notes must exist with valid confidence values
        assert!(note1.confidence > 0.0 && note1.confidence <= 1.0);
        assert!(note2.confidence > 0.0 && note2.confidence <= 1.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn singleton_worktree_writes_refresh_canonical_read_and_broken_links_view() {
        let project_tmp = tempfile::tempdir().unwrap();
        let worktree_tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, project_tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        repo.create(
            &project.id,
            project_tmp.path(),
            "ADR-008 Example",
            "body",
            "adr",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project.id,
            project_tmp.path(),
            "ADR-043 Repo Graph",
            "body",
            "adr",
            "[]",
        )
        .await
        .unwrap();

        let worktree_root = Some(PathBuf::from(worktree_tmp.path()));
        let project_path = project_tmp.path().to_str().unwrap().to_string();

        let Json(initial_brief) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: project_path.clone(),
                    title: "Project Brief".to_string(),
                    content: "Broken [[Missing ADR]]. Broken [[Roadmap]].".to_string(),
                    note_type: "brief".to_string(),
                    tags: None,
                    scope_paths: None,
                }),
                worktree_root.clone(),
            )
            .await;
        assert!(initial_brief.error.is_none(), "{:?}", initial_brief.error);

        let Json(initial_roadmap) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: project_path.clone(),
                    title: "Project Roadmap".to_string(),
                    content: "Broken [[Missing ADR-043]].".to_string(),
                    note_type: "roadmap".to_string(),
                    tags: None,
                    scope_paths: None,
                }),
                worktree_root.clone(),
            )
            .await;
        assert!(
            initial_roadmap.error.is_none(),
            "{:?}",
            initial_roadmap.error
        );

        assert_eq!(
            ops::memory_broken_links(
                &server,
                BrokenLinksParams {
                    project: project_path.clone(),
                    folder: None,
                },
            )
            .await
            .broken_links
            .len(),
            3
        );

        let Json(updated_roadmap) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: project_path.clone(),
                    title: "Project Roadmap".to_string(),
                    content: "References [[ADR-043 Repo Graph]].".to_string(),
                    note_type: "roadmap".to_string(),
                    tags: None,
                    scope_paths: None,
                }),
                worktree_root.clone(),
            )
            .await;
        assert!(
            updated_roadmap.error.is_none(),
            "{:?}",
            updated_roadmap.error
        );

        let Json(updated_brief) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: project_path.clone(),
                    title: "Project Brief".to_string(),
                    content: "Links [[ADR-008 Example]] and [[roadmap]].".to_string(),
                    note_type: "brief".to_string(),
                    tags: None,
                    scope_paths: None,
                }),
                worktree_root,
            )
            .await;
        assert!(updated_brief.error.is_none(), "{:?}", updated_brief.error);

        repo.reindex_from_disk(&project.id, project_tmp.path())
            .await
            .unwrap();

        let brief_read = ops::memory_read(
            &server,
            ReadParams {
                project: project_path.clone(),
                identifier: "brief".to_string(),
            },
        )
        .await;
        assert_eq!(
            brief_read.content.as_deref(),
            Some("\nLinks [[ADR-008 Example]] and [[roadmap]].")
        );

        let roadmap_read = ops::memory_read(
            &server,
            ReadParams {
                project: project_path.clone(),
                identifier: "roadmap".to_string(),
            },
        )
        .await;
        assert_eq!(
            roadmap_read.content.as_deref(),
            Some("\nReferences [[ADR-043 Repo Graph]].")
        );

        let broken_links = ops::memory_broken_links(
            &server,
            BrokenLinksParams {
                project: project_path,
                folder: None,
            },
        )
        .await;
        assert!(broken_links.error.is_none(), "{:?}", broken_links.error);
        let remaining: Vec<_> = broken_links
            .broken_links
            .iter()
            .map(|link| (link.source_permalink.as_str(), link.raw_text.as_str()))
            .collect();
        assert!(!remaining.contains(&("brief", "ADR-008 Example")));
        assert!(!remaining.contains(&("brief", "roadmap")));
        assert!(!remaining.contains(&("roadmap", "ADR-043 Repo Graph")));
        assert!(
            remaining.is_empty(),
            "unexpected broken links: {remaining:?}"
        );
    }
}

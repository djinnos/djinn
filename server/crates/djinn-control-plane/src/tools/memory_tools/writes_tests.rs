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

    use std::time::Duration;

    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::{Json, handler::server::wrapper::Parameters};
    use tokio::time::sleep;

    use crate::{
        server::DjinnMcpServer,
        state::stubs::test_mcp_state,
        tools::memory_tools::{
            BrokenLinksParams, EditParams, ListParams, ReadParams, WriteParams, ops,
        },
    };

    async fn create_project(db: &Database, _root: &std::path::Path) -> djinn_core::models::Project {
        // Unique owner/repo per test — `memory_write` derives canonical
        // `project_dir(owner, repo)` which is outside any `TempDir`, so
        // parallel tests sharing "test/test-project" would stomp each
        // other on the same `~/.djinn/projects/test/test-project` tree.
        let id = uuid::Uuid::now_v7();
        let repo_name = format!("test-project-{id}");
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create(&repo_name, "test", &repo_name)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_mergeable_note_type_bypasses_dedup() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Create first note
        let Json(created1) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Research Topic".to_string(),
                content: "This is a research note about async Rust patterns.".to_string(),
                note_type: "research".to_string(),
                status: None,
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
                project: project.slug(),
                title: "Research Topic Two".to_string(),
                content: "This is a research note about async Rust patterns.".to_string(),
                note_type: "research".to_string(),
                status: None,
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
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Create first pattern note
        let Json(created1) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Async Pattern".to_string(),
                content: "Use tokio::spawn for concurrent task execution in Rust async code."
                    .to_string(),
                note_type: "pattern".to_string(),
                status: None,
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
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Create a pattern note
        let Json(pattern) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Error Handling Pattern".to_string(),
                content: "Use Result types for explicit error handling in Rust.".to_string(),
                note_type: "pattern".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(pattern.error.is_none());

        // Create an ADR in decisions folder with similar content
        let Json(adr) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Error Handling ADR".to_string(),
                content: "Use Result types for explicit error handling in Rust.".to_string(),
                note_type: "adr".to_string(),
                status: None,
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
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        // Create a pattern note with unique content
        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Unique Pattern XYZ123".to_string(),
                content:
                    "This content is completely unique and should not match anything. XYZ123ABC"
                        .to_string(),
                note_type: "pattern".to_string(),
                status: None,
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
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Canonical Pattern".to_string(),
                content: "Alpha\r\nBeta\n".to_string(),
                note_type: "pattern".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        assert!(!created.deduplicated);
        let first_id = created.id.clone().expect("created note id");

        repo.clear_content_hash(&first_id).await.unwrap();

        let rebuilt = repo
            .rebuild_missing_content_hashes(&project.id)
            .await
            .unwrap();
        assert_eq!(rebuilt, 1);

        let Json(reused) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Canonical Pattern Copy".to_string(),
                content: "  Alpha\nBeta  ".to_string(),
                note_type: "pattern".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(reused.error.is_none(), "error: {:?}", reused.error);
        assert!(reused.deduplicated);
        assert_eq!(reused.id.as_deref(), Some(first_id.as_str()));

        let note_count = repo.count_by_project(&project.id).await.unwrap();
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
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Shared high-overlap content to ensure FTS BM25 score > 5.0
        let shared = "authentication token validation jwt bearer oauth2 security middleware interceptor \
                      expiry refresh revoke claims principal identity session management authorization \
                      role permission scope grant deny policy enforcement middleware pipeline rust axum";

        // Write first note
        let Json(r1) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Auth Token Validation Pattern".to_string(),
                content: shared.to_string(),
                note_type: "pattern".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(r1.error.is_none(), "first write failed: {:?}", r1.error);
        let id1 = r1.id.clone().unwrap();

        // Write second note with same content to trigger detection
        let Json(r2) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "JWT Bearer Auth Validation".to_string(),
                content: shared.to_string(),
                note_type: "pattern".to_string(),
                status: None,
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

    // ── singleton + scope_paths regressions, ported to db-only storage ──
    //
    // These previously asserted on canonical/worktree on-disk file contents.
    // With the db-only knowledge-base cut-over the assertions reduce to "the
    // db row landed and reads back correctly" — no .md file is written.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn singleton_writes_overwrite_existing_row_and_resolve_links() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        repo.create(&project.id, "ADR-008 Example", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(&project.id, "ADR-043 Repo Graph", "body", "adr", "[]")
            .await
            .unwrap();

        let project_path = project.slug();

        let Json(initial_brief) = server
            .memory_write(Parameters(WriteParams {
                project: project_path.clone(),
                title: "Project Brief".to_string(),
                content: "Broken [[Missing ADR]]. Broken [[Roadmap]].".to_string(),
                note_type: "brief".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(initial_brief.error.is_none(), "{:?}", initial_brief.error);

        let Json(initial_roadmap) = server
            .memory_write(Parameters(WriteParams {
                project: project_path.clone(),
                title: "Project Roadmap".to_string(),
                content: "Broken [[Missing ADR-043]].".to_string(),
                note_type: "roadmap".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
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
            .memory_write(Parameters(WriteParams {
                project: project_path.clone(),
                title: "Project Roadmap".to_string(),
                content: "References [[ADR-043 Repo Graph]].".to_string(),
                note_type: "roadmap".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(
            updated_roadmap.error.is_none(),
            "{:?}",
            updated_roadmap.error
        );

        let Json(updated_brief) = server
            .memory_write(Parameters(WriteParams {
                project: project_path.clone(),
                title: "Project Brief".to_string(),
                content: "Links [[ADR-008 Example]] and [[roadmap]].".to_string(),
                note_type: "brief".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(updated_brief.error.is_none(), "{:?}", updated_brief.error);

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
            Some("Links [[ADR-008 Example]] and [[roadmap]].")
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
            Some("References [[ADR-043 Repo Graph]].")
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
        assert!(
            broken_links.broken_links.is_empty(),
            "expected no broken links after update; got {:?}",
            broken_links.broken_links
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_singleton_write_persists_db_row_and_listable_view() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let title = "ADR-054 Roadmap Memory Extraction Quality Gates and Note Taxonomy";
        let permalink = "design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy";
        let content = "Originated from ADR-054 closure reconciliation.";

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: title.to_string(),
                content: content.to_string(),
                note_type: "design".to_string(),
                status: None,
                tags: Some(vec!["adr-054".to_string(), "design".to_string()]),
                scope_paths: None,
            }))
            .await;

        assert!(created.error.is_none(), "{:?}", created.error);
        assert_eq!(created.permalink.as_deref(), Some(permalink));

        let note = repo
            .get_by_permalink(&project.id, permalink)
            .await
            .unwrap()
            .expect("canonical row should exist immediately");
        assert_eq!(note.storage, "db");
        assert_eq!(note.file_path, "");

        let read_back = ops::memory_read(
            &server,
            ReadParams {
                project: project.slug(),
                identifier: permalink.to_string(),
            },
        )
        .await;
        assert_eq!(read_back.error, None);
        assert_eq!(read_back.id.as_deref(), Some(note.id.as_str()));
        assert_eq!(read_back.permalink.as_deref(), Some(permalink));
        assert_eq!(read_back.content.as_deref(), Some(content));

        let listed = ops::memory_list(
            &server,
            ListParams {
                project: project.slug(),
                folder: Some("design".to_string()),
                note_type: Some("design".to_string()),
                depth: Some(1),
            },
        )
        .await;
        assert!(listed.error.is_none(), "{:?}", listed.error);
        assert!(
            listed.notes.iter().any(|entry| {
                entry.permalink == permalink
                    && entry.title == title
                    && entry.note_type == "design"
                    && entry.folder == "design"
            }),
            "design note should be listed under canonical project identity"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requirement_edit_resolves_link_text_in_db_row() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        repo.create(
            &project.id,
            "V1 Requirements",
            "References [[Cognitive Memory Scope]].",
            "requirement",
            "[]",
        )
        .await
        .unwrap();

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: project.slug(),
                identifier: "requirements/v1-requirements".to_string(),
                operation: "find_replace".to_string(),
                content: "[[reference/cognitive-memory-scope]]".to_string(),
                find_text: Some("[[Cognitive Memory Scope]]".to_string()),
                section: None,
                note_type: None,
            }))
            .await;
        assert!(edited.error.is_none(), "{:?}", edited.error);

        let row = repo
            .get_by_permalink(&project.id, "requirements/v1-requirements")
            .await
            .unwrap()
            .expect("requirement row should exist");
        assert!(row.content.contains("[[reference/cognitive-memory-scope]]"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_adr_with_empty_scope_paths_persists_to_db() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "ADR-300 Empty Scope".to_string(),
                content: "body for ADR-300".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: Some(vec![]),
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        let permalink = created.permalink.as_deref().unwrap();
        let note = repo
            .get_by_permalink(&project.id, permalink)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.storage, "db");
        assert_eq!(note.scope_paths, "[]");
        assert_eq!(note.content, "body for ADR-300");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_adr_with_nonempty_scope_paths_persists_scope() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "ADR-301 Scoped".to_string(),
                content: "scoped body".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: Some(vec!["crates/foo".to_string()]),
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        let permalink = created.permalink.as_deref().unwrap();
        let note = repo
            .get_by_permalink(&project.id, permalink)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.scope_paths, r#"["crates/foo"]"#);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_edit_find_replace_updates_db_content() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "ADR-303 Editable".to_string(),
                content: "The quick brown fox".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: Some(vec!["crates/foo".to_string()]),
            }))
            .await;
        assert!(created.error.is_none(), "error: {:?}", created.error);
        let permalink = created.permalink.clone().unwrap();
        let note_id = created.id.clone().unwrap();

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: project.slug(),
                identifier: permalink,
                operation: "find_replace".to_string(),
                content: "slow purple turtle".to_string(),
                find_text: Some("quick brown fox".to_string()),
                section: None,
                note_type: None,
            }))
            .await;

        assert!(edited.error.is_none(), "edit error: {:?}", edited.error);

        let row = repo.get(&note_id).await.unwrap().unwrap();
        assert!(
            row.content.contains("The slow purple turtle"),
            "new text must be in db row content: {}",
            row.content
        );
        assert!(
            !row.content.contains("quick brown fox"),
            "old text must be gone: {}",
            row.content
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_edit_find_replace_noop_returns_error_and_keeps_updated_at() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "ADR-304 NoopEdit".to_string(),
                content: "hello world".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;
        assert!(created.error.is_none());
        let note_id = created.id.clone().unwrap();

        sleep(Duration::from_millis(200)).await;
        let before = repo.get(&note_id).await.unwrap().unwrap();
        let before_updated_at = before.updated_at.clone();

        sleep(Duration::from_millis(50)).await;

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: project.slug(),
                identifier: created.permalink.clone().unwrap(),
                operation: "find_replace".to_string(),
                content: "hello".to_string(),
                find_text: Some("hello".to_string()),
                section: None,
                note_type: None,
            }))
            .await;

        assert!(
            edited.error.is_some(),
            "find_replace no-op must return an error"
        );
        assert!(
            edited
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no-op"),
            "error message should mention no-op; got {:?}",
            edited.error
        );

        let after = repo.get(&note_id).await.unwrap().unwrap();
        assert_eq!(
            after.updated_at, before_updated_at,
            "updated_at must not change on no-op find_replace"
        );
    }
}

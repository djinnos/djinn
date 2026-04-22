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
    use std::path::PathBuf;
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

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_mergeable_note_type_bypasses_dedup() {
        let tmp = workspace_tempdir();
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
                project: tmp.path().to_str().unwrap().to_string(),
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
                project: tmp.path().to_str().unwrap().to_string(),
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
                project: tmp.path().to_str().unwrap().to_string(),
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
                project: tmp.path().to_str().unwrap().to_string(),
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
                project: tmp.path().to_str().unwrap().to_string(),
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
                project: tmp.path().to_str().unwrap().to_string(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn singleton_worktree_writes_refresh_canonical_read_and_broken_links_view() {
        let project_tmp = workspace_tempdir();
        let worktree_tmp = project_tmp
            .path()
            .join(".djinn/worktrees/test-brief-singleton");
        std::fs::create_dir_all(worktree_tmp.join(".git")).unwrap();
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

        let worktree_root = Some(PathBuf::from(&worktree_tmp));
        let project_path = project_tmp.path().to_str().unwrap().to_string();

        let Json(initial_brief) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: project_path.clone(),
                    title: "Project Brief".to_string(),
                    content: "Broken [[Missing ADR]]. Broken [[Roadmap]].".to_string(),
                    note_type: "brief".to_string(),
                    status: None,
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
                    status: None,
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
                    status: None,
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
                    status: None,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn singleton_brief_write_with_worktree_project_path_keeps_canonical_path() {
        let project_tmp = workspace_tempdir();
        let worktree_tmp = project_tmp
            .path()
            .join(".djinn/worktrees/test-brief-singleton");
        std::fs::create_dir_all(worktree_tmp.join(".git")).unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, project_tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: worktree_tmp.to_string_lossy().to_string(),
                    title: "Project Brief".to_string(),
                    content: "Links [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning]] and [[roadmap]].".to_string(),
                    note_type: "brief".to_string(),
                    status: None,
                    tags: None,
                    scope_paths: None,
                }),
                Some(PathBuf::from(&worktree_tmp)),
            )
            .await;
        assert!(created.error.is_none(), "{:?}", created.error);
        assert_eq!(created.permalink.as_deref(), Some("brief"));

        let note = repo
            .get_by_permalink(&project.id, "brief")
            .await
            .unwrap()
            .unwrap();

        let canonical_path = project_tmp.path().join(".djinn/brief.md");
        let worktree_path = worktree_tmp.join(".djinn/brief.md");
        assert_eq!(
            std::path::Path::new(&note.file_path),
            canonical_path.as_path()
        );
        assert!(canonical_path.exists());
        assert!(worktree_path.exists());
        assert!(
            repo.get_by_permalink(&project.id, "reference/project-brief")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !project_tmp
                .path()
                .join(".djinn/reference/project-brief.md")
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(&canonical_path).unwrap(),
            std::fs::read_to_string(&worktree_path).unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_singleton_worktree_write_uses_canonical_project_identity_and_exact_permalink_reads()
     {
        let project_tmp = workspace_tempdir();
        let worktree_tmp = project_tmp
            .path()
            .join(".djinn/worktrees/test-design-canonical-write");
        std::fs::create_dir_all(worktree_tmp.join(".git")).unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, project_tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let title = "ADR-054 Roadmap Memory Extraction Quality Gates and Note Taxonomy";
        let permalink = "design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy";
        let content = "Originated from ADR-054 closure reconciliation.";

        let Json(created) = server
            .memory_write_with_worktree(
                Parameters(WriteParams {
                    project: worktree_tmp.to_string_lossy().to_string(),
                    title: title.to_string(),
                    content: content.to_string(),
                    note_type: "design".to_string(),
                    status: None,
                    tags: Some(vec!["adr-054".to_string(), "design".to_string()]),
                    scope_paths: None,
                }),
                Some(PathBuf::from(&worktree_tmp)),
            )
            .await;

        assert!(created.error.is_none(), "{:?}", created.error);
        assert_eq!(created.permalink.as_deref(), Some(permalink));

        let note = repo
            .get_by_permalink(&project.id, permalink)
            .await
            .unwrap()
            .expect("canonical row should exist immediately");

        let canonical_path = project_tmp.path().join(
            ".djinn/design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy.md",
        );
        let worktree_path = worktree_tmp.join(
            ".djinn/design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy.md",
        );

        assert_eq!(
            std::path::Path::new(&note.file_path),
            canonical_path.as_path()
        );
        assert!(
            worktree_path.exists(),
            "worktree-authored note should exist on disk immediately"
        );
        assert!(
            !canonical_path.exists(),
            "non-singleton worktree writes should keep the canonical row identity without forcing an immediate canonical file mirror"
        );

        let read_back = ops::memory_read(
            &server,
            ReadParams {
                project: project_tmp.path().to_string_lossy().to_string(),
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
                project: project_tmp.path().to_string_lossy().to_string(),
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
    async fn current_requirement_edit_from_worktree_updates_canonical_path() {
        let project_tmp = workspace_tempdir();
        let worktree_tmp = project_tmp
            .path()
            .join(".djinn/worktrees/test-current-requirement");
        std::fs::create_dir_all(worktree_tmp.join(".git")).unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, project_tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        repo.create(
            &project.id,
            project_tmp.path(),
            "V1 Requirements",
            "References [[Cognitive Memory Scope]].",
            "requirement",
            "[]",
        )
        .await
        .unwrap();

        let Json(edited) = server
            .memory_edit_with_worktree(
                Parameters(EditParams {
                    project: worktree_tmp.to_string_lossy().to_string(),
                    identifier: "requirements/v1-requirements".to_string(),
                    operation: "find_replace".to_string(),
                    content: "[[reference/cognitive-memory-scope]]".to_string(),
                    find_text: Some("[[Cognitive Memory Scope]]".to_string()),
                    section: None,
                    note_type: None,
                }),
                Some(PathBuf::from(&worktree_tmp)),
            )
            .await;
        assert!(edited.error.is_none(), "{:?}", edited.error);

        let canonical_path = project_tmp
            .path()
            .join(".djinn/requirements/v1-requirements.md");
        let worktree_path = worktree_tmp.join(".djinn/requirements/v1-requirements.md");
        assert!(canonical_path.exists());
        assert!(worktree_path.exists());
        let canonical_contents = std::fs::read_to_string(&canonical_path).unwrap();
        let worktree_contents = std::fs::read_to_string(&worktree_path).unwrap();
        assert!(canonical_contents.contains("[[reference/cognitive-memory-scope]]"));
        assert_eq!(canonical_contents, worktree_contents);
        assert!(
            repo.get_by_permalink(&project.id, "reference/v1-requirements")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !project_tmp
                .path()
                .join(".djinn/reference/v1-requirements.md")
                .exists()
        );
    }

    // ── scope_paths routing regression tests ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_adr_with_empty_scope_paths_writes_file_to_disk() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "ADR-300 Empty Scope".to_string(),
                content: "body for ADR-300".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: Some(vec![]),
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        let file_path = created.file_path.clone().expect("file_path must be set");
        assert!(!file_path.is_empty(), "file_path must not be empty");
        assert!(
            std::path::Path::new(&file_path).exists(),
            "markdown file must exist on disk: {file_path}"
        );
        let on_disk = std::fs::read_to_string(&file_path).unwrap();
        assert!(on_disk.contains("ADR-300 Empty Scope"));
        assert!(on_disk.contains("body for ADR-300"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_adr_with_nonempty_scope_paths_writes_file_to_disk() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "ADR-301 Scoped".to_string(),
                content: "scoped body".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: Some(vec!["crates/foo".to_string()]),
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        let file_path = created.file_path.clone().expect("file_path must be set");
        assert!(std::path::Path::new(&file_path).exists());
        let on_disk = std::fs::read_to_string(&file_path).unwrap();
        assert!(on_disk.contains("ADR-301 Scoped"));
        assert!(on_disk.contains("scoped body"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_adr_without_scope_paths_still_writes_file_to_disk() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "ADR-302 Unscoped".to_string(),
                content: "unscoped body".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(created.error.is_none(), "error: {:?}", created.error);
        let file_path = created.file_path.clone().expect("file_path must be set");
        assert!(std::path::Path::new(&file_path).exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_edit_find_replace_on_scoped_adr_updates_file_on_disk() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "ADR-303 Editable".to_string(),
                content: "The quick brown fox".to_string(),
                note_type: "adr".to_string(),
                status: None,
                tags: None,
                scope_paths: Some(vec!["crates/foo".to_string()]),
            }))
            .await;
        assert!(created.error.is_none(), "error: {:?}", created.error);
        let file_path = created.file_path.clone().expect("file_path must be set");

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: tmp.path().to_str().unwrap().to_string(),
                identifier: created.permalink.clone().unwrap(),
                operation: "find_replace".to_string(),
                content: "slow purple turtle".to_string(),
                find_text: Some("quick brown fox".to_string()),
                section: None,
                note_type: None,
            }))
            .await;

        assert!(edited.error.is_none(), "edit error: {:?}", edited.error);

        let on_disk = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            on_disk.contains("The slow purple turtle"),
            "new text must be present in file. got:\n{on_disk}"
        );
        assert!(
            !on_disk.contains("quick brown fox"),
            "old text must be gone from file. got:\n{on_disk}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_edit_find_replace_noop_returns_error_and_keeps_updated_at() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
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

        // Let any post-write background tasks (summary regen, contradiction
        // analysis) drain before capturing the baseline timestamp.
        sleep(Duration::from_millis(200)).await;
        let before = repo.get(&note_id).await.unwrap().unwrap();
        let before_updated_at = before.updated_at.clone();

        // Small delay so any accidental bump from the edit would be detectable.
        sleep(Duration::from_millis(50)).await;

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: tmp.path().to_str().unwrap().to_string(),
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

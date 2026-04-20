// Integration tests for memory_build_context budget pruning semantics
//
// Tests verify:
// 1. budget=500 returns fewer related items than budget=4096
// 2. Seed notes are present in both budget runs and never pruned
// 3. Pruning order removes lowest-ranked related notes first (highest-ranked survive tight budget)

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
    use std::path::Path;
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Project;
    use djinn_db::Database;
    use djinn_db::NoteRepository;
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps, StubSyncOps,
    };
    use crate::tools::memory_tools::BuildContextParams;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, path: &Path) -> Project {
        use djinn_db::ProjectRepository;
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-project", path.to_str().unwrap())
            .await
            .unwrap()
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
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
            Arc::new(djinn_provider::oauth::codex::CodexPendingStore::new()),
        )
    }

    /// Create a seed note and 20 related notes with deterministic content
    /// that allows for stable ranking.
    async fn setup_ranking_test_data(
        tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> String {
        let repo = NoteRepository::new(db.clone(), event_bus_for(tx));

        // Create seed note with unique content
        let seed = repo
            .create(
                &project.id,
                tmp.path(),
                "Seed Note",
                "This is the central seed note about database architecture and system design patterns.",
                "adr",
                "[\"seed\",\"core\"]",
            )
            .await
            .unwrap();

        // Create 20 related notes with deterministic content
        // Each note links to the seed via wikilink in its content
        // We vary the content length and keyword density to create stable ranking
        for i in 1..=20 {
            let title = format!("Related Note {:02}", i);
            // Content varies by index to create stable ranking
            // Lower index notes have more keywords matching seed, higher rank
            let keyword_repeats = 21 - i; // Note 01 has 20 repeats, Note 20 has 1 repeat
            let keywords = "database architecture system design patterns ".repeat(keyword_repeats);
            let content = format!(
                "This note discusses {}and references [[Seed Note]] for details.",
                keywords
            );

            repo.create(
                &project.id,
                tmp.path(),
                &title,
                &content,
                "reference",
                &format!("[\"related\",\"rank{:02}\"]", i),
            )
            .await
            .unwrap();
        }

        // Also create notes that are NOT linked (for L0 discovery testing)
        for i in 1..=10 {
            let title = format!("Unlinked Note {:02}", i);
            // These don't link to seed, so they appear in L0 via FTS
            let keywords = "architecture system design patterns".repeat(i);
            let content = format!("Independent note about {} unrelated concepts.", keywords);

            repo.create(
                &project.id,
                tmp.path(),
                &title,
                &content,
                "research",
                "[\"unlinked\"]",
            )
            .await
            .unwrap();
        }

        seed.permalink
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_budget_tight_returns_fewer_items() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let seed_permalink = setup_ranking_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Call with tight budget (500)
        let tight_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(500),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        // Call with loose budget (4096)
        let loose_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        let tight = tight_result.0;
        let loose = loose_result.0;

        // Assert no error
        assert!(
            tight.error.is_none(),
            "tight budget should not error: {:?}",
            tight.error
        );
        assert!(
            loose.error.is_none(),
            "loose budget should not error: {:?}",
            loose.error
        );

        // Calculate total related items
        let tight_total = tight.related_l1.len() + tight.related_l0.len();
        let loose_total = loose.related_l1.len() + loose.related_l0.len();

        // Tight budget should return fewer items
        assert!(
            tight_total < loose_total,
            "budget=500 should return fewer items ({} < {})",
            tight_total,
            loose_total
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_seed_never_pruned() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let seed_permalink = setup_ranking_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Test with extremely tight budget (100)
        let tight_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(100),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        // Test with loose budget (4096)
        let loose_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        let tight = tight_result.0;
        let loose = loose_result.0;

        // Seed should be present in both results
        assert!(
            !tight.primary.is_empty(),
            "seed should be present with tight budget"
        );
        assert!(
            !loose.primary.is_empty(),
            "seed should be present with loose budget"
        );

        // Seed ID should match in both results
        assert_eq!(
            tight.primary[0].permalink, loose.primary[0].permalink,
            "seed permalink should match"
        );

        // Both should have exactly one primary (the seed)
        assert_eq!(
            tight.primary.len(),
            1,
            "tight budget should have exactly 1 primary"
        );
        assert_eq!(
            loose.primary.len(),
            1,
            "loose budget should have exactly 1 primary"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_pruning_removes_lowest_ranked_first() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let seed_permalink = setup_ranking_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Call with loose budget to get "ground truth" ranking
        let loose_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        // Call with medium budget
        let medium_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(1500),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        // Call with tight budget
        let tight_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(500),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        let loose = loose_result.0;
        let medium = medium_result.0;
        let tight = tight_result.0;

        // Collect all related permalinks from each result
        let loose_related: std::collections::HashSet<String> = loose
            .related_l1
            .iter()
            .map(|n| n.permalink.clone())
            .chain(loose.related_l0.iter().map(|n| n.permalink.clone()))
            .collect();

        let medium_related: std::collections::HashSet<String> = medium
            .related_l1
            .iter()
            .map(|n| n.permalink.clone())
            .chain(medium.related_l0.iter().map(|n| n.permalink.clone()))
            .collect();

        let tight_related: std::collections::HashSet<String> = tight
            .related_l1
            .iter()
            .map(|n| n.permalink.clone())
            .chain(tight.related_l0.iter().map(|n| n.permalink.clone()))
            .collect();

        // Tight should be subset of medium, medium should be subset of loose
        // This verifies that pruning removes from the tail (lowest ranked)
        for permalink in &tight_related {
            assert!(
                medium_related.contains(permalink),
                "tight budget item {} should also be in medium budget",
                permalink
            );
        }

        for permalink in &medium_related {
            assert!(
                loose_related.contains(permalink),
                "medium budget item {} should also be in loose budget",
                permalink
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_default_budget_is_4096() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let seed_permalink = setup_ranking_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Call without specifying budget (should default to 4096)
        let default_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: None,
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        // Call with explicit 4096 budget
        let explicit_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                },
            ))
            .await;

        let default = default_result.0;
        let explicit = explicit_result.0;

        // Should return same number of items
        let default_total = default.related_l1.len() + default.related_l0.len();
        let explicit_total = explicit.related_l1.len() + explicit.related_l0.len();

        assert_eq!(
            default_total, explicit_total,
            "default budget should return same count as explicit 4096"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_task_id_parameter_accepted() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let seed_permalink = setup_ranking_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Call with task_id parameter
        let result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(10),
                    budget: Some(4096),
                    task_id: Some("test-task-123".to_string()),
                    min_confidence: None,
                },
            ))
            .await;

        let response = result.0;

        // Should not error
        assert!(
            response.error.is_none(),
            "task_id parameter should be accepted: {:?}",
            response.error
        );

        // Should return seed
        assert!(!response.primary.is_empty(), "should return primary note");
    }

    /// Helper: create a seed note and related notes with controlled confidence values.
    /// Returns (seed_permalink, low_confidence_title, stale_citation_title, normal_title).
    async fn setup_confidence_test_data(
        tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> (String, String, String, String) {
        let repo = NoteRepository::new(db.clone(), event_bus_for(tx));

        // Create seed note
        let seed = repo
            .create(
                &project.id,
                tmp.path(),
                "Confidence Seed",
                "This seed note covers database architecture patterns for confidence testing.",
                "adr",
                "[\"seed\"]",
            )
            .await
            .unwrap();

        // Create a note with very low confidence (below default 0.1 threshold)
        let low_conf = repo
            .create(
                &project.id,
                tmp.path(),
                "Low Confidence Note",
                "database architecture patterns low confidence content that references [[Confidence Seed]].",
                "reference",
                "[\"low\"]",
            )
            .await
            .unwrap();
        repo.set_confidence(&low_conf.id, 0.05).await.unwrap();

        // Create a note with stale-citation confidence (0.3 - at the STALE_CITATION threshold)
        let stale = repo
            .create(
                &project.id,
                tmp.path(),
                "Stale Citation Note",
                "database architecture patterns stale citation content that references [[Confidence Seed]].",
                "reference",
                "[\"stale\"]",
            )
            .await
            .unwrap();
        repo.set_confidence(&stale.id, 0.3).await.unwrap();

        // Create a normal high-confidence note
        let normal = repo
            .create(
                &project.id,
                tmp.path(),
                "Normal Confidence Note",
                "database architecture patterns normal confidence content that references [[Confidence Seed]].",
                "reference",
                "[\"normal\"]",
            )
            .await
            .unwrap();
        // confidence defaults to 1.0, no need to set

        (seed.permalink, low_conf.title, stale.title, normal.title)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_default_min_confidence_filters_low_confidence_notes() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (seed_permalink, low_conf_title, _stale_title, _normal_title) =
            setup_confidence_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Default min_confidence (0.1) should exclude notes with confidence < 0.1
        let result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None, // uses default 0.1
                },
            ))
            .await;

        let response = result.0;
        assert!(
            response.error.is_none(),
            "should not error: {:?}",
            response.error
        );

        // Collect all related note titles
        let all_related_titles: Vec<String> = response
            .related_l1
            .iter()
            .map(|n| n.title.clone())
            .chain(response.related_l0.iter().map(|n| n.title.clone()))
            .collect();

        // Low confidence note (0.05) should be filtered out
        assert!(
            !all_related_titles.contains(&low_conf_title),
            "low confidence note should be filtered out with default min_confidence=0.1, but found in: {:?}",
            all_related_titles
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_min_confidence_zero_includes_all_notes() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (seed_permalink, low_conf_title, _stale_title, _normal_title) =
            setup_confidence_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // min_confidence=0.0 should include all notes
        let result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: Some(0.0),
                },
            ))
            .await;

        let response = result.0;
        assert!(
            response.error.is_none(),
            "should not error: {:?}",
            response.error
        );

        // Collect all related note titles
        let all_related_titles: Vec<String> = response
            .related_l1
            .iter()
            .map(|n| n.title.clone())
            .chain(response.related_l0.iter().map(|n| n.title.clone()))
            .collect();

        // Low confidence note should now be included
        assert!(
            all_related_titles.contains(&low_conf_title),
            "low confidence note should be included with min_confidence=0.0, titles found: {:?}",
            all_related_titles
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_superseded_notes_are_annotated() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (seed_permalink, _low_conf_title, stale_title, normal_title) =
            setup_confidence_test_data(&tmp, &db, &tx, &project).await;

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Use min_confidence=0.0 to include all notes so we can check annotations
        let result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: tmp.path().to_str().unwrap().to_string(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: Some(0.0),
                },
            ))
            .await;

        let response = result.0;
        assert!(
            response.error.is_none(),
            "should not error: {:?}",
            response.error
        );

        // Find the stale citation note in the results
        let stale_l1 = response.related_l1.iter().find(|n| n.title == stale_title);
        let stale_l0 = response.related_l0.iter().find(|n| n.title == stale_title);

        let stale_note = stale_l1.is_some() || stale_l0.is_some();
        assert!(
            stale_note,
            "stale citation note should appear in results with min_confidence=0.0"
        );

        // Verify superseded annotation
        if let Some(note) = stale_l1 {
            assert!(
                note.superseded,
                "stale citation L1 note should be marked superseded"
            );
            assert!(
                note.overview_text.starts_with("[SUPERSEDED]"),
                "stale citation L1 note overview should have [SUPERSEDED] prefix, got: {}",
                note.overview_text
            );
        }
        if let Some(note) = stale_l0 {
            assert!(
                note.superseded,
                "stale citation L0 note should be marked superseded"
            );
            assert!(
                note.abstract_text.starts_with("[SUPERSEDED]"),
                "stale citation L0 note abstract should have [SUPERSEDED] prefix, got: {}",
                note.abstract_text
            );
        }

        // Normal confidence note should NOT be superseded
        let normal_l1 = response.related_l1.iter().find(|n| n.title == normal_title);
        let normal_l0 = response.related_l0.iter().find(|n| n.title == normal_title);

        if let Some(note) = normal_l1 {
            assert!(
                !note.superseded,
                "normal confidence L1 note should not be marked superseded"
            );
        }
        if let Some(note) = normal_l0 {
            assert!(
                !note.superseded,
                "normal confidence L0 note should not be marked superseded"
            );
        }
    }
}

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
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::BuildContextParams;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, _path: &Path) -> Project {
        use djinn_db::ProjectRepository;
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-project", "test", "test-project")
            .await
            .unwrap()
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

    /// Create a seed note and 20 related notes with deterministic content
    /// that allows for stable ranking.
    async fn setup_ranking_test_data(
        _tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> String {
        let repo = NoteRepository::new(db.clone(), event_bus_for(tx));

        // Create seed note with unique content
        let seed = repo
            .create(
                &project.id,
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

            repo.create(&project.id, &title, &content, "research", "[\"unlinked\"]")
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
                    project: project.id.clone(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(500),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        // Call with loose budget (4096)
        let loose_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
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
                    project: project.id.clone(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(100),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        // Test with loose budget (4096)
        let loose_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
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
                    project: project.id.clone(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        // Call with medium budget
        let medium_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(1500),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        // Call with tight budget
        let tight_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(500),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
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
                    project: project.id.clone(),
                    url: seed_permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: None,
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        // Call with explicit 4096 budget
        let explicit_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(4096),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
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
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(10),
                    budget: Some(4096),
                    task_id: Some("test-task-123".to_string()),
                    min_confidence: None,
                    edge_kinds: None,
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
        _tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> (String, String, String, String) {
        let repo = NoteRepository::new(db.clone(), event_bus_for(tx));

        // Create seed note
        let seed = repo
            .create(
                &project.id,
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
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None, // uses default 0.1
                    edge_kinds: None,
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
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: Some(0.0),
                    edge_kinds: None,
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
                    project: project.id.clone(),
                    url: seed_permalink,
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: Some(0.0),
                    edge_kinds: None,
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

    // ── Embedding-related build_context regression tests (68o7) ─────────────

    /// `memory_build_context` includes `embedding_related` edges in graph
    /// expansion by default (when `edge_kinds` is `None`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_embedding_related_edges_included_by_default() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        // Seed note.
        let seed = repo
            .create(
                &project.id,
                "BC Seed",
                "build context seed for embedding edge test about architecture",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Neighbor: connected to seed only via embedding_related.
        // Content intentionally unrelated to seed's FTS query so the
        // neighbor's appearance in results is purely graph-driven.
        let neighbor = repo
            .create(
                &project.id,
                "BC Embed Neighbor",
                "quantum entanglement physics experiment results data",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Seed the embedding_related edge.
        repo.upsert_provenance_association(
            &seed.id,
            &neighbor.id,
            &djinn_db::NoteAssociationProvenanceUpsert {
                kind: djinn_db::NoteAssociationKind::EmbeddingRelated,
                source: djinn_db::NoteAssociationSource::EmbeddingSimilarity,
                weight: 0.30,
                confidence: Some(0.85),
                algorithm_version: Some("test-v1".to_owned()),
                embedding_model: Some("test-model".to_owned()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Default: edge_kinds=None → all kinds, including embedding_related.
        let result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed.permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        let response = result.0;
        assert!(
            response.error.is_none(),
            "build_context should not error: {:?}",
            response.error
        );

        // The seed should always be in primary.
        assert_eq!(response.primary.len(), 1);
        assert_eq!(response.primary[0].id, seed.id);

        // Check if the embedding neighbor appears in related notes
        // (either L1 or L0). With embedding_related edges included by
        // default and the neighbor's content intentionally unrelated to
        // the seed's FTS query, the neighbor can only be discovered
        // through graph expansion — so this assertion is not masked by FTS.
        let all_related_ids: Vec<&str> = response
            .related_l1
            .iter()
            .map(|n| n.id.as_str())
            .chain(response.related_l0.iter().map(|n| n.id.as_str()))
            .collect();

        assert!(
            all_related_ids.contains(&neighbor.id.as_str()),
            "embedding-related neighbor must appear in related notes when edge_kinds is None (default); got: {all_related_ids:?}"
        );
    }

    /// `memory_build_context` with `edge_kinds` that exclude
    /// `embedding_related` should reduce the graph proximity contribution
    /// for notes connected only via embedding edges. Because
    /// `build_context`'s `run_rrf_discovery` calls `temporal_scores_all`
    /// (which returns every active note regardless of `edge_kinds`), we
    /// cannot assert presence/absence. Instead, we compare each neighbor's
    /// **fused score** across different `edge_kinds` calls — the score
    /// difference isolates the graph signal contribution.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_respects_edge_kinds_filter_for_embedding_related() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let seed = repo
            .create(
                &project.id,
                "Filter Seed",
                "filter seed for edge kinds test about system design",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Connected only via embedding_related. Content is intentionally
        // unrelated to the seed's FTS query so FTS cannot mask the graph
        // signal.
        let embed_neighbor = repo
            .create(
                &project.id,
                "Filter Embed Neighbor",
                "quantum physics entanglement research unrelated content",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Connected via co_access. Content intentionally unrelated to seed's
        // FTS query to isolate graph expansion behavior.
        let co_neighbor = repo
            .create(
                &project.id,
                "Filter Co Neighbor",
                "biology genetics mutation research unrelated",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Seed embedding_related edge.
        repo.upsert_provenance_association(
            &seed.id,
            &embed_neighbor.id,
            &djinn_db::NoteAssociationProvenanceUpsert {
                kind: djinn_db::NoteAssociationKind::EmbeddingRelated,
                source: djinn_db::NoteAssociationSource::EmbeddingSimilarity,
                weight: 0.30,
                confidence: Some(0.85),
                algorithm_version: Some("test-v1".to_owned()),
                embedding_model: Some("test-model".to_owned()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();

        // Seed co_access edge with a weight above MIN_ASSOCIATION_WEIGHT (0.05)
        // but LOW enough that embed_neighbor's embedding_related graph score
        // (HOP_DECAY * 0.5 * 0.30 = 0.105) is HIGHER than co_neighbor's
        // co_access score (HOP_DECAY * 0.10 = 0.070). This ensures
        // embed_neighbor ranks ABOVE co_neighbor in graph_scores when both
        // are present, so its actual rank (1) differs from its missing rank
        // (2) when filtered out — making the score comparison meaningful.
        repo.upsert_association_min_weight(&seed.id, &co_neighbor.id, 0.10)
            .await
            .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        // Helper: fetch a note's score (from L1 or L0) for a given
        // build_context response.
        let score_of = |resp: &crate::tools::memory_tools::types::MemoryBuildContextResponse,
                        id: &str|
         -> Option<f32> {
            resp.related_l1
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.score.unwrap_or(0.0))
                .or_else(|| {
                    resp.related_l0
                        .iter()
                        .find(|n| n.id == id)
                        .map(|n| n.score.unwrap_or(0.0))
                })
        };

        // ── co_access-only filter ────────────────────────────────────────────
        let co_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed.permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: Some(vec!["co_access".to_string()]),
                },
            ))
            .await;

        let co_resp = &co_result.0;
        assert!(
            co_resp.error.is_none(),
            "co_access-only filter should not error: {:?}",
            co_resp.error
        );

        let co_embed_score = score_of(co_resp, &embed_neighbor.id)
            .expect("embed_neighbor must appear in co_access results (temporal signal)");
        let co_co_score = score_of(co_resp, &co_neighbor.id)
            .expect("co_neighbor must appear in co_access results (temporal signal)");

        // ── embedding_related-only filter ────────────────────────────────────
        let embed_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed.permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: Some(vec!["embedding_related".to_string()]),
                },
            ))
            .await;

        let embed_resp = &embed_result.0;
        assert!(
            embed_resp.error.is_none(),
            "embedding_only filter should not error: {:?}",
            embed_resp.error
        );

        let embed_embed_score = score_of(embed_resp, &embed_neighbor.id)
            .expect("embed_neighbor must appear in embedding_related results");
        let embed_co_score = score_of(embed_resp, &co_neighbor.id)
            .expect("co_neighbor must appear in embedding_related results");

        // ── Cross-call score comparison ──────────────────────────────────────
        //
        // The only difference between the two calls is which edges survive
        // the `edge_kinds` filter. All other RRF signals (FTS, temporal,
        // task) are identical. So the score difference for each neighbor
        // isolates the graph proximity contribution:
        //
        // embed_neighbor:
        //   - With embedding_related filter: gets graph rank 1 → higher score
        //   - With co_access filter: no graph edge → lower score (missing_rank)
        //
        // co_neighbor:
        //   - With co_access filter: gets graph rank 1 → higher score
        //   - With embedding_related filter: no graph edge → lower score
        assert!(
            embed_embed_score > co_embed_score,
            "embed_neighbor must score higher with edge_kinds=[\"embedding_related\"] \
             ({embed_embed_score}) than with [\"co_access\"] ({co_embed_score}) — the \
             graph proximity boost is present only when embedding_related is allowed"
        );

        assert!(
            embed_co_score < co_co_score,
            "co_neighbor must score higher with edge_kinds=[\"co_access\"] \
             ({co_co_score}) than with [\"embedding_related\"] ({embed_co_score}) — the \
             graph proximity boost is present only when co_access is allowed"
        );

        // ── Default (all kinds) includes embedding_related ───────────────────
        let default_result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed.permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        let default_resp = &default_result.0;
        assert!(
            default_resp.error.is_none(),
            "default filter should not error: {:?}",
            default_resp.error
        );

        let default_embed_score = score_of(default_resp, &embed_neighbor.id)
            .expect("embed_neighbor must appear in default results");

        // With edge_kinds=None (all kinds), embed_neighbor gets its
        // embedding_related graph contribution, so its score must be
        // higher than the co_access-only call (where the edge is filtered).
        assert!(
            default_embed_score > co_embed_score,
            "embed_neighbor must score higher with default edge_kinds=None \
             ({default_embed_score}) than with [\"co_access\"] ({co_embed_score}) — \
             embedding_related edges are included by default"
        );
    }

    /// Explicit wikilinks remain the strongest retrieval signal in
    /// `memory_build_context` — notes connected via wikilinks rank higher
    /// than those connected only via `embedding_related` machine edges.
    /// Co-access (Hebbian) behavior is not demoted or elevated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_wikilink_precedence_over_embedding_related() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        // Seed note.
        let seed = repo
            .create(
                &project.id,
                "Precedence BC Seed",
                "precedence test seed about database architecture and system patterns",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Wikilinked note: explicitly links to seed via [[Precedence BC Seed]].
        // Shares FTS terms ("database", "architecture") with the seed.
        let wikilinked = repo
            .create(
                &project.id,
                "Precedence Wikilinked",
                "This note discusses database architecture precedence. See [[Precedence BC Seed]] for the canonical source.",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Ensure L1 discovery exercises explicit wikilink precedence without
        // issuing raw SQL from the control-plane test crate.
        repo.upsert_wikilink_edge(&wikilinked.id, &seed.id, "Precedence BC Seed", None)
            .await
            .unwrap();

        let wikilink_pairs = repo
            .wikilink_pairs_for_notes(&[wikilinked.id.clone(), seed.id.clone()])
            .await
            .unwrap();
        let expected_pair = if wikilinked.id <= seed.id {
            (wikilinked.id.clone(), seed.id.clone())
        } else {
            (seed.id.clone(), wikilinked.id.clone())
        };
        assert!(
            wikilink_pairs.contains(&expected_pair),
            "explicit wikilink edge must be seeded before L1 assertion"
        );

        // Embedding-only note: connected to seed via embedding_related only.
        // Content intentionally unrelated to seed's FTS query so it does
        // NOT appear through text matching — only through graph expansion.
        let embedding_note = repo
            .create(
                &project.id,
                "Precedence Embedding Only",
                "quantum entanglement physics experiment data analysis",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Co-access note: connected via co_access only. Shares some FTS
        // overlap with the seed ("architecture") so it's a candidate.
        // Verifies co-access (Hebbian) behavior is not demoted.
        let co_access_note = repo
            .create(
                &project.id,
                "Precedence Co Access",
                "architecture system design patterns for distributed computing",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Seed embedding_related edge.
        repo.upsert_provenance_association(
            &seed.id,
            &embedding_note.id,
            &djinn_db::NoteAssociationProvenanceUpsert {
                kind: djinn_db::NoteAssociationKind::EmbeddingRelated,
                source: djinn_db::NoteAssociationSource::EmbeddingSimilarity,
                weight: 0.30,
                confidence: Some(0.85),
                algorithm_version: Some("test-v1".to_owned()),
                embedding_model: Some("test-model".to_owned()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();

        // Seed co_access (Hebbian) edge to the co-access note.
        repo.upsert_association(&seed.id, &co_access_note.id, 3)
            .await
            .unwrap();

        let state = test_mcp_state(db, &tx);
        let server = DjinnMcpServer::new(state);

        let result = server
            .memory_build_context(rmcp::handler::server::wrapper::Parameters(
                BuildContextParams {
                    project: project.id.clone(),
                    url: seed.permalink.clone(),
                    depth: None,
                    max_related: Some(20),
                    budget: Some(8192),
                    task_id: None,
                    min_confidence: None,
                    edge_kinds: None,
                },
            ))
            .await;

        let response = result.0;
        assert!(
            response.error.is_none(),
            "build_context should not error: {:?}",
            response.error
        );

        // The seed is always in primary.
        assert_eq!(response.primary.len(), 1);
        assert_eq!(response.primary[0].id, seed.id);

        // The wikilinked note must be in L1 (direct wikilink neighbor).
        // It has an explicit [[Precedence BC Seed]] wikilink and shares
        // FTS terms with the seed, so it should be both a direct neighbor
        // and a high-ranking RRF candidate.
        let l1_ids: Vec<&str> = response.related_l1.iter().map(|n| n.id.as_str()).collect();
        assert!(
            l1_ids.contains(&wikilinked.id.as_str()),
            "wikilinked note must be in L1 (direct wikilink neighbor); got L1: {l1_ids:?}"
        );

        // Collect all related IDs (L1 + L0) for rank comparison.
        let all_related: Vec<&str> = response
            .related_l1
            .iter()
            .map(|n| n.id.as_str())
            .chain(response.related_l0.iter().map(|n| n.id.as_str()))
            .collect();

        // The embedding note (quantum content, no FTS overlap) should not
        // outrank the wikilinked note. Since the embedding note's content
        // is unrelated to the seed's FTS query, it may not even appear in
        // results. If it does appear (via graph proximity + temporal), its
        // rank must be lower than the wikilinked note's.
        if let (Some(wl_rank), Some(em_rank)) = (
            all_related.iter().position(|&id| id == wikilinked.id),
            all_related.iter().position(|&id| id == embedding_note.id),
        ) {
            assert!(
                wl_rank < em_rank,
                "wikilinked note (rank={wl_rank}) must outrank embedding-only note (rank={em_rank})"
            );
        }

        // Co-access (Hebbian) behavior: the co_access_note, connected via
        // co_access with shared FTS terms ("architecture"), should appear
        // in results — verifying co-access edges are not demoted by the
        // presence of embedding edges.
        assert!(
            all_related.contains(&co_access_note.id.as_str()),
            "co_access note must appear in related results (Hebbian behavior preserved); got: {all_related:?}"
        );

        // Wikilinked note must outrank co-access note too — explicit
        // wikilinks are the strongest signal.
        if let (Some(wl_rank), Some(co_rank)) = (
            all_related.iter().position(|&id| id == wikilinked.id),
            all_related.iter().position(|&id| id == co_access_note.id),
        ) {
            assert!(
                wl_rank < co_rank,
                "wikilinked note (rank={wl_rank}) must outrank co_access note (rank={co_rank})"
            );
        }
    }
}

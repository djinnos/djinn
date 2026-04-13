// Integration tests for extracted shared memory ops and MCP adapters.
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use tokio::sync::broadcast;

    use crate::bridge::{RuntimeOps, SemanticQueryEmbedding};
    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps, StubSyncOps,
    };
    use crate::tools::memory_tools::ops;
    use crate::tools::memory_tools::{
        BrokenLinksParams, BuildContextParams, ListParams, OrphansParams, ReadParams, SearchParams,
    };

    struct SemanticRuntimeOps {
        embedding: Vec<f32>,
    }

    struct FailingSemanticRuntimeOps;

    #[async_trait::async_trait]
    impl RuntimeOps for SemanticRuntimeOps {
        async fn apply_settings(
            &self,
            _: &djinn_core::models::DjinnSettings,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn embed_memory_query(
            &self,
            _: &str,
        ) -> Result<Option<SemanticQueryEmbedding>, String> {
            Ok(Some(SemanticQueryEmbedding {
                values: self.embedding.clone(),
            }))
        }

        async fn reset_runtime_settings(&self) {}
        async fn persist_model_health_state(&self) {}
        async fn purge_worktrees(&self) {}
    }

    #[async_trait::async_trait]
    impl RuntimeOps for FailingSemanticRuntimeOps {
        async fn apply_settings(
            &self,
            _: &djinn_core::models::DjinnSettings,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn embed_memory_query(
            &self,
            _: &str,
        ) -> Result<Option<SemanticQueryEmbedding>, String> {
            Err("embedding model unavailable".to_string())
        }

        async fn reset_runtime_settings(&self) {}
        async fn persist_model_health_state(&self) {}
        async fn purge_worktrees(&self) {}
    }

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
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        )
    }

    struct SetupResult {
        server: DjinnMcpServer,
        _tmp: tempfile::TempDir,
        project: String,
        permalink: String,
        folder: String,
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

    async fn setup_server() -> SetupResult {
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
        let primary = note_repo
            .create(
                &project.id,
                tmp.path(),
                "Seed Note",
                "Seed note content with links to [[Related Note]] and architecture context.",
                "adr",
                "[]",
            )
            .await
            .unwrap();
        let related = note_repo
            .create(
                &project.id,
                tmp.path(),
                "Related Note",
                "Related architecture context note.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let _ = related;
        let folder_note = note_repo
            .create(
                &project.id,
                tmp.path(),
                "Folder Note",
                "Folder wildcard note.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));
        SetupResult {
            server,
            _tmp: tmp,
            project: project.path,
            permalink: primary.permalink,
            folder: folder_note.folder,
        }
    }

    async fn access_count_for(server: &DjinnMcpServer, project: &str, permalink: &str) -> i64 {
        let project_id =
            ProjectRepository::new(server.state.db().clone(), server.state.event_bus())
                .resolve(project)
                .await
                .unwrap()
                .expect("project id");
        NoteRepository::new(server.state.db().clone(), server.state.event_bus())
            .get_by_permalink(&project_id, permalink)
            .await
            .unwrap()
            .expect("note")
            .access_count
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_read_ops_increments_access_count_for_repeated_file_backed_reads() {
        let setup = setup_server().await;
        let before = access_count_for(&setup.server, &setup.project, &setup.permalink).await;

        for _ in 0..2 {
            let response = ops::memory_read(
                &setup.server,
                ReadParams {
                    project: setup.project.clone(),
                    identifier: setup.permalink.clone(),
                },
            )
            .await;

            assert!(
                response.error.is_none(),
                "unexpected error: {:?}",
                response.error
            );
            assert!(response.id.is_some());
        }

        let after = access_count_for(&setup.server, &setup.project, &setup.permalink).await;
        assert_eq!(after, before + 2);
        assert_eq!(setup.server.recorded_note_ids().await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_read_ops_increments_access_count_for_repeated_db_backed_reads() {
        let setup = setup_server().await;
        let project_id = ProjectRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        )
        .resolve(&setup.project)
        .await
        .unwrap()
        .expect("project id");
        let repo = NoteRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        );
        let note = repo
            .create_db_note(&project_id, "DB Read Note", "db note body", "pattern", "[]")
            .await
            .unwrap();
        assert_eq!(note.storage, "db");
        assert!(!Path::new(&note.file_path).exists());

        let before = access_count_for(&setup.server, &setup.project, &note.permalink).await;

        for _ in 0..2 {
            let response = ops::memory_read(
                &setup.server,
                ReadParams {
                    project: setup.project.clone(),
                    identifier: note.permalink.clone(),
                },
            )
            .await;

            assert!(
                response.error.is_none(),
                "unexpected error: {:?}",
                response.error
            );
            assert_eq!(response.id.as_deref(), Some(note.id.as_str()));
        }

        let after = access_count_for(&setup.server, &setup.project, &note.permalink).await;
        assert_eq!(after, before + 2);
        assert_eq!(setup.server.recorded_note_ids().await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_read_ops_not_found_does_not_mutate_access_count() {
        let setup = setup_server().await;
        let before = access_count_for(&setup.server, &setup.project, &setup.permalink).await;

        let response = ops::memory_read(
            &setup.server,
            ReadParams {
                project: setup.project.clone(),
                identifier: "missing-note".to_string(),
            },
        )
        .await;

        assert_eq!(
            response.error.as_deref(),
            Some("note not found: missing-note")
        );
        assert!(response.id.is_none());

        let after = access_count_for(&setup.server, &setup.project, &setup.permalink).await;
        assert_eq!(after, before);
        assert!(setup.server.recorded_note_ids().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_ops_applies_task_fallback_and_success_shape() {
        let setup = setup_server().await;

        let response = ops::memory_search(
            &setup.server,
            SearchParams {
                project: setup.project.clone(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
            },
            Some("task-123"),
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        assert!(!response.results.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_ops_merges_semantic_candidates_with_lexical_results() {
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
        let repo = NoteRepository::new(db.clone(), event_bus.clone());

        let lexical = repo
            .create(
                &project.id,
                tmp.path(),
                "Lexical Match",
                "architecture planning context",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let semantic = repo
            .create_db_note(
                &project.id,
                "Semantic Match",
                "dispatch slot registry",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let embedding = vec![0.25_f32; 768];
        repo.upsert_embedding(djinn_db::UpsertNoteEmbedding {
            note_id: &semantic.id,
            content_hash: "semantic-hash",
            model_version: "nomic-embed-text-v1.5",
            embedding: &embedding,
        })
        .await
        .unwrap();

        let server = DjinnMcpServer::new(McpState::new(
            db,
            event_bus_for(&tx),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            "test-user".into(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(SemanticRuntimeOps {
                embedding: embedding.clone(),
            }),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        ));

        let semantic_candidates = repo
            .semantic_candidate_scores(&project.id, &embedding, None, None, 10)
            .await
            .unwrap();

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.path.clone(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        let ids: Vec<&str> = response
            .results
            .iter()
            .map(|result| result.id.as_str())
            .collect();
        assert!(ids.contains(&lexical.id.as_str()));

        if semantic_candidates.iter().any(|(id, _)| id == &semantic.id) {
            assert!(ids.contains(&semantic.id.as_str()));
            assert_eq!(
                ids.iter().filter(|&&id| id == semantic.id.as_str()).count(),
                1,
                "merged semantic+lexical results should be deduplicated"
            );
        } else {
            assert!(
                !ids.contains(&semantic.id.as_str()),
                "semantic-only match should be absent when semantic candidate retrieval returns no match"
            );
        }

        assert_eq!(
            ids.iter().filter(|&&id| id == lexical.id.as_str()).count(),
            1,
            "lexical matches should also remain deduplicated"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_ops_falls_back_to_fts_when_query_embedding_fails() {
        let setup = setup_server().await;
        let failing_server = DjinnMcpServer::new(McpState::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            "test-user".into(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubSyncOps),
            Arc::new(FailingSemanticRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        ));

        let response = ops::memory_search(
            &failing_server,
            SearchParams {
                project: setup.project,
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            !response.results.is_empty(),
            "fts fallback should still return lexical matches"
        );
        assert!(
            response
                .results
                .iter()
                .any(|result| result.title == "Seed Note" || result.title == "Related Note")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_build_context_ops_preserves_prefix_and_wildcard_behavior() {
        let setup = setup_server().await;

        let single = ops::memory_build_context(
            &setup.server,
            BuildContextParams {
                project: setup.project.clone(),
                url: format!("memory://{}", setup.permalink),
                depth: None,
                max_related: Some(10),
                budget: Some(4096),
                task_id: None,
                min_confidence: None,
            },
            None,
        )
        .await;
        assert!(
            single.error.is_none(),
            "unexpected error: {:?}",
            single.error
        );
        assert_eq!(single.primary.len(), 1);

        let wildcard = ops::memory_build_context(
            &setup.server,
            BuildContextParams {
                project: setup.project.clone(),
                url: format!("memory://{}/*", setup.folder),
                depth: None,
                max_related: Some(10),
                budget: Some(4096),
                task_id: None,
                min_confidence: None,
            },
            None,
        )
        .await;
        assert!(
            wildcard.error.is_none(),
            "unexpected error: {:?}",
            wildcard.error
        );
        assert!(!wildcard.primary.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_memory_adapters_delegate_to_shared_ops() {
        let setup = setup_server().await;

        let search = setup
            .server
            .memory_search(rmcp::handler::server::wrapper::Parameters(SearchParams {
                project: setup.project.clone(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
            }))
            .await
            .0;
        assert!(search.error.is_none());
        assert!(!search.results.is_empty());

        let list = setup
            .server
            .memory_list(rmcp::handler::server::wrapper::Parameters(ListParams {
                project: setup.project.clone(),
                folder: None,
                note_type: None,
                depth: Some(1),
            }))
            .await
            .0;
        assert!(list.error.is_none());
        assert!(!list.notes.is_empty());

        let read = setup
            .server
            .memory_read(rmcp::handler::server::wrapper::Parameters(ReadParams {
                project: setup.project,
                identifier: setup.permalink,
            }))
            .await
            .0;
        assert!(read.error.is_none());
        assert!(read.id.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_detail_ops_treat_empty_folder_as_project_wide_filter() {
        let setup = setup_server().await;
        let project_id = ProjectRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        )
        .resolve(&setup.project)
        .await
        .unwrap()
        .expect("project id");
        let repo = NoteRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        );

        repo.create(
            &project_id,
            setup._tmp.path(),
            "Broken Source",
            "See [[Missing Memory Target]].",
            "research",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project_id,
            setup._tmp.path(),
            "Standalone Orphan",
            "no inbound links",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

        let health = repo.health(&project_id).await.unwrap();

        let broken_links = ops::memory_broken_links(
            &setup.server,
            BrokenLinksParams {
                project: setup.project.clone(),
                folder: Some(String::new()),
            },
        )
        .await;
        assert!(broken_links.error.is_none(), "{:?}", broken_links.error);
        assert_eq!(
            broken_links.broken_links.len() as i64,
            health.broken_link_count
        );

        let orphans = ops::memory_orphans(
            &setup.server,
            OrphansParams {
                project: setup.project.clone(),
                folder: Some(String::new()),
            },
        )
        .await;
        assert!(orphans.error.is_none(), "{:?}", orphans.error);
        assert_eq!(orphans.orphans.len() as i64, health.orphan_note_count);
    }
}

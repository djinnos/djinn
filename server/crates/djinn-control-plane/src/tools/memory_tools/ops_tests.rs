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
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::ops;
    use crate::tools::memory_tools::{
        BrokenLinksParams, BuildContextParams, HealthParams, ListParams, OrphansParams, ReadParams,
        SearchParams,
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
        async fn apply_environment_config(
            &self,
            _: &str,
            _: &djinn_stack::environment::EnvironmentConfig,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_mirror_refresh(&self, _: &str) {}
        async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_graph_warm(&self, _: &str) {}
        async fn apply_user_model_change(&self) {}
        async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_taskrun_jobs(&self) -> Result<Vec<crate::bridge::TaskrunJobRef>, String> {
            Ok(Vec::new())
        }
        async fn cleanup_task_branches(&self, _: &str) {}
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
        async fn apply_environment_config(
            &self,
            _: &str,
            _: &djinn_stack::environment::EnvironmentConfig,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_mirror_refresh(&self, _: &str) {}
        async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_graph_warm(&self, _: &str) {}
        async fn apply_user_model_change(&self) {}
        async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_taskrun_jobs(&self) -> Result<Vec<crate::bridge::TaskrunJobRef>, String> {
            Ok(Vec::new())
        }
        async fn cleanup_task_branches(&self, _: &str) {}
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
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let note_repo = NoteRepository::new(db.clone(), event_bus);
        let primary = note_repo
            .create(
                &project.id,
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
            project: project.slug(),
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

        // Use a string with no shared tokens with seeded content — MySQL
        // fulltext would happily fuzzy-match a query like "missing-note"
        // against any row containing the word "note" (unlike the SQLite
        // FTS5 tokenizer we previously relied on).
        let probe = "xyzzynonexistentidentifier";
        let response = ops::memory_read(
            &setup.server,
            ReadParams {
                project: setup.project.clone(),
                identifier: probe.to_string(),
            },
        )
        .await;

        assert_eq!(
            response.error.as_deref(),
            Some(format!("note not found: {probe}").as_str())
        );
        assert!(response.id.is_none());

        let after = access_count_for(&setup.server, &setup.project, &setup.permalink).await;
        assert_eq!(after, before);
        assert!(setup.server.recorded_note_ids().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_read_ops_prefers_exact_design_permalink_over_competing_case_search_match() {
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

        let design_permalink =
            "design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy";
        let design = repo
            .create_db_note_with_permalink(
                &project_id,
                design_permalink,
                "ADR-054 Roadmap Memory Extraction Quality Gates and Note Taxonomy",
                "Canonical design note for ADR-054 closure reconciliation.",
                "design",
                "[]",
            )
            .await
            .unwrap();
        let stale_case = repo
            .create(
                &project_id,
                "ADR-054 roadmap memory extraction quality gates and note taxonomy",
                "Superseded case note mentioning ADR-054 roadmap extraction quality gates taxonomy.",
                "case",
                "[]",
            )
            .await
            .unwrap();

        let response = ops::memory_read(
            &setup.server,
            ReadParams {
                project: setup.project.clone(),
                identifier: format!("memory://{design_permalink}.md"),
            },
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        assert_eq!(response.id.as_deref(), Some(design.id.as_str()));
        assert_eq!(response.permalink.as_deref(), Some(design_permalink));
        assert_ne!(response.id.as_deref(), Some(stale_case.id.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_list_ops_normalizes_design_folder_filter_before_listing() {
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

        let design = repo
            .create_db_note_with_permalink(
                &project_id,
                "design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy",
                "ADR-054 Roadmap Memory Extraction Quality Gates and Note Taxonomy",
                "Canonical design note visible to folder listing.",
                "design",
                "[]",
            )
            .await
            .unwrap();

        let listed = ops::memory_list(
            &setup.server,
            ListParams {
                project: setup.project.clone(),
                folder: Some("memory://design/".to_string()),
                note_type: Some("design".to_string()),
                status: None,
                depth: Some(1),
            },
        )
        .await;

        assert!(
            listed.error.is_none(),
            "unexpected error: {:?}",
            listed.error
        );
        assert!(
            listed
                .notes
                .iter()
                .any(|note| note.id == design.id && note.permalink == design.permalink)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_list_ops_honors_explicit_archived_status_filter() {
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

        let active = repo
            .create_db_note(
                &project_id,
                "Lifecycle Active Note",
                "active lifecycle list note",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let archived = repo
            .create_db_note(
                &project_id,
                "Lifecycle Archived Note",
                "archived lifecycle list note",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        repo.update_status(&archived.id, djinn_memory::note_status::ARCHIVED)
            .await
            .unwrap();

        let default_list = ops::memory_list(
            &setup.server,
            ListParams {
                project: setup.project.clone(),
                folder: None,
                note_type: Some("reference".to_string()),
                status: None,
                depth: Some(0),
            },
        )
        .await;
        assert!(default_list.error.is_none(), "{:?}", default_list.error);
        assert!(default_list.notes.iter().any(|note| note.id == active.id));
        assert!(default_list.notes.iter().all(|note| note.id != archived.id));

        let archived_list = ops::memory_list(
            &setup.server,
            ListParams {
                project: setup.project.clone(),
                folder: None,
                note_type: Some("reference".to_string()),
                status: Some("archived".to_string()),
                depth: Some(0),
            },
        )
        .await;
        assert!(archived_list.error.is_none(), "{:?}", archived_list.error);
        assert!(
            archived_list
                .notes
                .iter()
                .any(|note| note.id == archived.id)
        );
        assert!(archived_list.notes.iter().all(|note| note.id != active.id));
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
                entity_types: None,
                edge_kinds: None,
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

        for result in &response.results {
            let access_count =
                access_count_for(&setup.server, &setup.project, &result.permalink).await;
            assert_eq!(
                access_count, 1,
                "returned search results should count as accessed retrievals"
            );
        }

        let recorded = setup.server.recorded_note_ids().await;
        let returned_ids: Vec<String> = response
            .results
            .iter()
            .map(|result| result.id.clone())
            .collect();
        assert_eq!(recorded, returned_ids);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_ops_flushes_co_access_for_returned_results_only() {
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
        let hidden = repo
            .create(
                &project_id,
                "Hidden Note",
                "completely unrelated content",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        let response = ops::memory_search(
            &setup.server,
            SearchParams {
                project: setup.project.clone(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: None,
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(
            response.results.len() >= 2,
            "seed setup should return multiple results"
        );

        setup.server.flush_co_access_batch().await;

        let associations = repo
            .get_associations_for_note(&response.results[0].id)
            .await
            .unwrap();
        assert!(
            associations.iter().any(|association| {
                let pair = [
                    association.note_a_id.as_str(),
                    association.note_b_id.as_str(),
                ];
                pair.contains(&response.results[0].id.as_str())
                    && pair.contains(&response.results[1].id.as_str())
            }),
            "returned search results should become co-access associated"
        );

        assert_eq!(
            access_count_for(&setup.server, &setup.project, &hidden.permalink).await,
            0,
            "notes not returned from search should not be touched"
        );
        let hidden_associations = repo.get_associations_for_note(&hidden.id).await.unwrap();
        assert!(
            hidden_associations.is_empty(),
            "notes excluded from search results should not be co-access associated"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_ops_merges_semantic_candidates_with_lexical_results() {
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
        let repo = NoteRepository::new(db.clone(), event_bus.clone());

        let lexical = repo
            .create(
                &project.id,
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
            branch: "main",
        })
        .await
        .unwrap();

        let server = DjinnMcpServer::new(McpState::new(
            db,
            event_bus_for(&tx),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(SemanticRuntimeOps {
                embedding: embedding.clone(),
            }),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        ));

        let semantic_candidates = repo
            .semantic_candidate_scores(&project.id, &embedding, None, None, None, 10)
            .await
            .unwrap();

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.slug(),
                query: "architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: None,
                edge_kinds: None,
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
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
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
                entity_types: None,
                edge_kinds: None,
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
                edge_kinds: None,
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
                edge_kinds: None,
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
                entity_types: None,
                edge_kinds: None,
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
                status: None,
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
            "Broken Source",
            "See [[Missing Memory Target]].",
            "research",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project_id,
            "Standalone Orphan",
            "no inbound links",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

        let health = repo.health(&project_id).await.unwrap();
        assert_eq!(health.low_confidence_note_count, 0);
        assert_eq!(health.stale_note_count, 0);
        // Verify split orphan/isolation metrics from HealthReport
        assert_eq!(
            health.orphan_note_count, health.authored_orphan_count,
            "orphan_note_count must be a backward-compatible alias for authored_orphan_count"
        );
        // No machine-minted edges exist in this test, so authored orphans
        // are fully isolated.
        assert_eq!(health.machine_connected_orphan_count, 0);

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_health_response_exposes_split_orphan_and_isolation_metrics() {
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

        // Create a note with a broken wikilink and an orphan
        repo.create(
            &project_id,
            "Broken Source",
            "See [[Missing Memory Target]].",
            "research",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project_id,
            "Standalone Orphan",
            "no inbound links",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

        // Exercise the MCP-level memory_health op
        let response = ops::memory_health(
            &setup.server,
            HealthParams {
                project: Some(setup.project.clone()),
            },
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        assert!(response.total_notes.is_some());
        assert!(response.broken_link_count.is_some());
        assert!(response.orphan_note_count.is_some());
        // New split metrics must be present on success
        assert!(response.authored_orphan_count.is_some());
        assert!(response.isolated_count.is_some());
        assert!(response.isolated_pct.is_some());
        assert!(response.machine_connected_orphan_count.is_some());

        // orphan_note_count is a backward-compatible alias
        assert_eq!(response.orphan_note_count, response.authored_orphan_count);

        // No machine-minted edges in this test environment
        assert_eq!(response.machine_connected_orphan_count.unwrap(), 0);

        // isolated_pct should be a valid percentage
        let pct = response.isolated_pct.unwrap();
        assert!(
            (0.0..=100.0).contains(&pct),
            "isolated_pct out of range: {pct}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_health_response_returns_none_on_error_paths() {
        let setup = setup_server().await;

        // Missing project parameter
        let no_project = ops::memory_health(&setup.server, HealthParams { project: None }).await;
        assert!(no_project.error.is_some());
        assert!(no_project.authored_orphan_count.is_none());
        assert!(no_project.isolated_count.is_none());
        assert!(no_project.isolated_pct.is_none());
        assert!(no_project.machine_connected_orphan_count.is_none());

        // Unknown project
        let bad_project = ops::memory_health(
            &setup.server,
            HealthParams {
                project: Some("nonexistent-project".to_string()),
            },
        )
        .await;
        assert!(bad_project.error.is_some());
        assert!(bad_project.authored_orphan_count.is_none());
        assert!(bad_project.isolated_count.is_none());
        assert!(bad_project.isolated_pct.is_none());
        assert!(bad_project.machine_connected_orphan_count.is_none());
    }

    // ── Embedding-related health and retrieval regression tests (68o7) ──────

    /// `memory_health` reports authored-orphan debt separately from graph
    /// isolation when `embedding_related` machine-minted edges exist.
    ///
    /// Verifies:
    /// - authored_orphan_count includes notes that have only machine edges
    /// - machine_connected_orphan_count counts authored orphans rescued by
    ///   threshold-qualified embedding_related edges
    /// - isolated_count is lower because embedding edges reduce isolation
    /// - orphan_note_count == authored_orphan_count (backward-compat alias)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_health_reports_split_metrics_with_embedding_edges() {
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

        // Hub: receives no inbound wikilink or authored association → authored orphan.
        let hub = repo
            .create(
                &project_id,
                "Embed Hub",
                "hub content for embedding test",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Connected only by machine edge: authored orphan, but not isolated.
        let machine_note = repo
            .create(
                &project_id,
                "Machine Connected",
                "no wikilinks or authored edges",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // Truly isolated: no edges at all.
        repo.create(
            &project_id,
            "Fully Isolated",
            "no edges whatsoever",
            "research",
            "[]",
        )
        .await
        .unwrap();

        // Seed a threshold-qualified embedding_related edge.
        let confidence_above = 0.78_f64 + 0.05;
        repo.upsert_provenance_association(
            &hub.id,
            &machine_note.id,
            &djinn_db::NoteAssociationProvenanceUpsert {
                kind: djinn_db::NoteAssociationKind::EmbeddingRelated,
                source: djinn_db::NoteAssociationSource::EmbeddingSimilarity,
                weight: 0.20,
                confidence: Some(confidence_above),
                algorithm_version: Some("test-v1".to_owned()),
                embedding_model: Some("test-model".to_owned()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();

        let response = ops::memory_health(
            &setup.server,
            HealthParams {
                project: Some(setup.project.clone()),
            },
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);

        // The 3 newly created notes + 3 from setup_server (Seed Note,
        // Related Note, Folder Note) have no inbound wikilinks/authored
        // associations → all 6 active non-singleton notes are authored orphans.
        // (Seed Note and Related Note each have a wikilink resolving to them
        // through note_links; let's verify the actual count by asserting
        // the relationship rather than a hard number.)
        let authored = response.authored_orphan_count.unwrap();
        let compat = response.orphan_note_count.unwrap();
        assert_eq!(
            authored, compat,
            "orphan_note_count must equal authored_orphan_count"
        );

        // Hub and MachineConnected are both authored orphans connected by
        // the embedding_related edge → machine_connected_orphan_count >= 2.
        let machine_connected = response.machine_connected_orphan_count.unwrap();
        assert!(
            machine_connected >= 2,
            "Hub and MachineConnected should be machine-connected orphans, got {machine_connected}"
        );

        // isolated_count must be strictly less than authored_orphan_count
        // because the embedding edge reduces isolation for Hub + MachineConnected.
        let isolated = response.isolated_count.unwrap();
        assert!(
            isolated < authored,
            "isolated_count ({isolated}) < authored_orphan_count ({authored})"
        );

        // isolated_pct is a valid percentage.
        let pct = response.isolated_pct.unwrap();
        assert!(
            (0.0..=100.0).contains(&pct),
            "isolated_pct out of range: {pct}"
        );
    }

    /// `memory_orphans()` must still return authored-orphan notes that are
    /// connected only by machine-minted `embedding_related` edges.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_orphans_includes_notes_connected_only_by_embedding_related_edges() {
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

        let anchor = repo
            .create(&project_id, "Anchor Note", "anchor body", "adr", "[]")
            .await
            .unwrap();
        let orphan = repo
            .create(
                &project_id,
                "Embed Orphan",
                "no wikilinks or authored links",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // Give "Embed Orphan" a machine-minted embedding_related edge from
        // Anchor — above threshold so it reduces isolation, but it must NOT
        // hide authored-orphan debt.
        repo.upsert_provenance_association(
            &anchor.id,
            &orphan.id,
            &djinn_db::NoteAssociationProvenanceUpsert {
                kind: djinn_db::NoteAssociationKind::EmbeddingRelated,
                source: djinn_db::NoteAssociationSource::EmbeddingSimilarity,
                weight: 0.25,
                confidence: Some(0.85),
                algorithm_version: Some("test-v1".to_owned()),
                embedding_model: Some("test-model".to_owned()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();

        let response = ops::memory_orphans(
            &setup.server,
            OrphansParams {
                project: setup.project.clone(),
                folder: None,
            },
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);

        let titles: Vec<&str> = response.orphans.iter().map(|o| o.title.as_str()).collect();
        assert!(
            titles.contains(&"Embed Orphan"),
            "note connected only by embedding_related edge must still appear as orphan: {titles:?}"
        );
    }

    /// Explicit wikilinks remain the strongest retrieval signal — notes
    /// connected via wikilinks rank above those connected only via
    /// `embedding_related` machine edges.  Co-access (Hebbian) behavior
    /// is not demoted or elevated by the presence of embedding edges.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_search_wikilink_precedence_over_embedding_related() {
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
        let repo = NoteRepository::new(db.clone(), event_bus.clone());

        // Seed: target of the wikilink. Contains query terms.
        let seed = repo
            .create(
                &project.id,
                "Precedence Seed",
                "Precedence testing seed note about architecture and system design.",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Wikilinked note: explicitly links to seed via [[Precedence Seed]].
        // Also shares FTS terms ("architecture") with the query.
        let wikilinked = repo
            .create(
                &project.id,
                "Wikilinked Note",
                "This note is about precedence testing for architecture. See [[Precedence Seed]] for details.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Embedding-only note: connected to seed via embedding_related only.
        // Content intentionally FTS-unrelated to the query to eliminate
        // FTS masking of the graph signal.
        let embedding_only = repo
            .create(
                &project.id,
                "Embedding Only Note",
                "quantum entanglement physics experiment data results",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Co-access note: connected via co_access (Hebbian) to seed.
        // Shares FTS terms ("architecture") with the query so it's a
        // candidate. Verifies co-access behavior is not demoted.
        let co_access_note = repo
            .create(
                &project.id,
                "Co Access Note",
                "architecture system patterns distributed computing design",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Seed embedding_related edge between seed and embedding_only.
        repo.upsert_provenance_association(
            &seed.id,
            &embedding_only.id,
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

        // Seed co_access (Hebbian) edge between seed and co_access_note.
        // This is a separate note from the embedding_only note.
        repo.upsert_association(&seed.id, &co_access_note.id, 3)
            .await
            .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        let response = ops::memory_search(
            &server,
            SearchParams {
                project: project.slug(),
                query: "precedence testing architecture".to_string(),
                folder: None,
                note_type: None,
                limit: Some(10),
                entity_types: None,
                edge_kinds: None,
            },
            None,
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);

        // The seed and wikilinked must both appear (they share FTS terms
        // and have strong graph connections).
        let wikilinked_pos = response
            .results
            .iter()
            .position(|r| r.id == wikilinked.id)
            .expect("wikilinked note must appear in search results");

        // The wikilinked note (explicit wikilink + FTS overlap) must rank
        // at or above any embedding_only note that appears.
        let embedding_pos = response
            .results
            .iter()
            .position(|r| r.id == embedding_only.id);

        if let Some(em_pos) = embedding_pos {
            assert!(
                wikilinked_pos < em_pos,
                "wikilinked note (pos={wikilinked_pos}) must outrank embedding-only note \
                 (pos={em_pos}) — wikilink graph signal is the strongest"
            );
        }

        // Co-access (Hebbian) behavior: the co_access_note, connected
        // via co_access with FTS overlap, must appear in results — proving
        // co-access edges are not demoted by the presence of embedding edges.
        let co_access_pos = response
            .results
            .iter()
            .position(|r| r.id == co_access_note.id)
            .expect("co_access note must appear in results (Hebbian behavior preserved)");

        // Wikilinked note must outrank co_access note — explicit wikilinks
        // are the strongest signal.
        assert!(
            wikilinked_pos < co_access_pos,
            "wikilinked note (pos={wikilinked_pos}) must outrank co_access note \
             (pos={co_access_pos}) — explicit wikilinks are strongest"
        );
    }
}

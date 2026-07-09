// Integration tests for embedding_related edge-kind filtering in memory_build_context.
//
// Validates that:
// 1. Graph expansion includes embedding_related edges by default (no edge_kinds filter).
// 2. edge_kinds=["embedding_related"] includes/boosts the embedding neighbor.
// 3. edge_kinds=["co_access"] (excluding embedding) removes that graph boost.
//
// Uses fused score comparisons across calls for deterministic assertions that do
// not depend on temporal discovery absence.

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
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Project;
    use djinn_db::{
        Database, NoteAssociationKind, NoteAssociationProvenanceUpsert, NoteAssociationSource,
        NoteRepository,
    };
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::{BuildContextParams, ops};

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, _path: &std::path::Path) -> Project {
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

    /// Seed note + one embedding_related neighbor + one co_access control neighbor.
    ///
    /// Returns `(server, project_id, seed_permalink, embed_neighbor_id, coaccess_neighbor_id)`.
    async fn setup_filter_data(
        _tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> (DjinnMcpServer, String, String, String, String) {
        let repo = NoteRepository::new(db.clone(), event_bus_for(tx));

        // Seed note — primary target for build_context.
        let seed = repo
            .create(
                &project.id,
                "Architecture Seed",
                "Core note on database architecture patterns and system design.",
                "adr",
                "[\"seed\",\"architecture\"]",
            )
            .await
            .unwrap();

        // Embedding-related neighbor — connected via embedding_related edge.
        let embed_nb = repo
            .create(
                &project.id,
                "Embedding Neighbor",
                "Related note discovered through embedding similarity analysis.",
                "reference",
                "[\"embedding\",\"neighbor\"]",
            )
            .await
            .unwrap();

        // Co-access control neighbor — connected via co_access edge.
        let coaccess_nb = repo
            .create(
                &project.id,
                "Coaccess Neighbor",
                "Note that co-occurs frequently in user sessions with the seed.",
                "reference",
                "[\"coaccess\",\"neighbor\"]",
            )
            .await
            .unwrap();

        // Add embedding_related association between seed and embed_nb.
        repo.upsert_provenance_association(
            &seed.id,
            &embed_nb.id,
            &NoteAssociationProvenanceUpsert {
                kind: NoteAssociationKind::EmbeddingRelated,
                source: NoteAssociationSource::EmbeddingSimilarity,
                weight: 0.3,
                confidence: Some(0.3),
                algorithm_version: Some("v1".to_string()),
                embedding_model: Some("test-model".to_string()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();

        // Add co_access association between seed and coaccess_nb.
        repo.upsert_provenance_association(
            &seed.id,
            &coaccess_nb.id,
            &NoteAssociationProvenanceUpsert {
                kind: NoteAssociationKind::CoAccess,
                source: NoteAssociationSource::SessionCoAccess,
                weight: 0.25,
                confidence: None,
                algorithm_version: None,
                embedding_model: None,
                embedding_dim: None,
            },
        )
        .await
        .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db.clone(), tx));

        (
            server,
            project.id.clone(),
            seed.permalink.clone(),
            embed_nb.id.clone(),
            coaccess_nb.id.clone(),
        )
    }

    /// Helper: call `memory_build_context` with the given `edge_kinds`.
    async fn call_build_context(
        server: &DjinnMcpServer,
        project_id: &str,
        seed_permalink: &str,
        edge_kinds: Option<Vec<String>>,
    ) -> crate::tools::memory_tools::MemoryBuildContextResponse {
        ops::memory_build_context(
            server,
            BuildContextParams {
                project: project_id.to_string(),
                url: format!("memory://{}", seed_permalink),
                depth: None,
                max_related: Some(20),
                budget: Some(8192),
                task_id: None,
                min_confidence: None,
                edge_kinds,
            },
            None,
        )
        .await
    }

    /// Extract the fused score for a given note ID from a build_context response.
    /// Searches both L1 and L0 tiers.
    fn score_for_note(
        resp: &crate::tools::memory_tools::MemoryBuildContextResponse,
        note_id: &str,
    ) -> Option<f32> {
        for n in &resp.related_l1 {
            if n.id == note_id {
                return n.score;
            }
        }
        for n in &resp.related_l0 {
            if n.id == note_id {
                return n.score;
            }
        }
        None
    }

    /// AC 1: Default graph expansion (no edge_kinds filter) includes the
    /// embedding_related neighbor with a positive score.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_default_includes_embedding_related_neighbor() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, embed_nb_id, _coaccess_nb_id) =
            setup_filter_data(&tmp, &db, &tx, &project).await;

        let resp = call_build_context(&server, &project_id, &seed_permalink, None).await;
        assert!(
            resp.error.is_none(),
            "build_context should not error: {:?}",
            resp.error
        );

        // The embedding_related neighbor must appear with a positive fused score
        // when graph expansion is enabled by default (no edge_kinds filter).
        let score = score_for_note(&resp, &embed_nb_id);
        assert!(
            score.is_some(),
            "embedding_related neighbor should appear in default build_context results"
        );
        assert!(
            score.unwrap() > 0.0,
            "embedding_related neighbor should have a positive fused score, got {:?}",
            score
        );
    }

    /// AC 2: edge_kinds=["embedding_related"] includes/boosts the embedding
    /// neighbor; edge_kinds=["co_access"] (excluding embedding) removes that
    /// graph boost.
    ///
    /// The embedding neighbor's fused score should be higher when
    /// edge_kinds=["embedding_related"] than when edge_kinds=["co_access"],
    /// because the embedding_related graph proximity signal contributes only
    /// in the former case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_embedding_filter_boosts_embedding_neighbor_over_coaccess_filter() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, embed_nb_id, coaccess_nb_id) =
            setup_filter_data(&tmp, &db, &tx, &project).await;

        // Call with edge_kinds=["embedding_related"] — embedding graph boost active.
        let embed_only = call_build_context(
            &server,
            &project_id,
            &seed_permalink,
            Some(vec!["embedding_related".to_string()]),
        )
        .await;
        assert!(
            embed_only.error.is_none(),
            "embed_only call should not error: {:?}",
            embed_only.error
        );

        // Call with edge_kinds=["co_access"] — co_access graph boost active, embedding inactive.
        let coaccess_only = call_build_context(
            &server,
            &project_id,
            &seed_permalink,
            Some(vec!["co_access".to_string()]),
        )
        .await;
        assert!(
            coaccess_only.error.is_none(),
            "coaccess_only call should not error: {:?}",
            coaccess_only.error
        );

        // The embedding neighbor's fused score must be higher under
        // edge_kinds=["embedding_related"] than under ["co_access"].
        let embed_score_with_filter = score_for_note(&embed_only, &embed_nb_id);
        let embed_score_without = score_for_note(&coaccess_only, &embed_nb_id);

        assert!(
            embed_score_with_filter.is_some(),
            "embedding neighbor should appear under embedding_related filter"
        );
        assert!(
            embed_score_without.is_some(),
            "embedding neighbor may appear under co_access filter (temporal/FTS), but with lower score"
        );
        assert!(
            embed_score_with_filter.unwrap() > embed_score_without.unwrap(),
            "embedding neighbor score with embedding_related filter ({:?}) \
             must exceed score with co_access filter ({:?})",
            embed_score_with_filter,
            embed_score_without
        );

        // Conversely, the co_access neighbor's fused score should be higher
        // under edge_kinds=["co_access"] than under ["embedding_related"].
        let coaccess_score_with_filter = score_for_note(&coaccess_only, &coaccess_nb_id);
        let coaccess_score_without = score_for_note(&embed_only, &coaccess_nb_id);

        assert!(
            coaccess_score_with_filter.is_some(),
            "co_access neighbor should appear under co_access filter"
        );
        assert!(
            coaccess_score_without.is_some(),
            "co_access neighbor may appear under embedding_related filter (temporal/FTS), but with lower score"
        );
        assert!(
            coaccess_score_with_filter.unwrap() > coaccess_score_without.unwrap(),
            "co_access neighbor score with co_access filter ({:?}) \
             must exceed score with embedding_related filter ({:?})",
            coaccess_score_with_filter,
            coaccess_score_without
        );
    }

    /// AC 2 (cont.): Default (no filter) should produce a score for the
    /// embedding neighbor that is at least as high as when edge_kinds excludes
    /// embedding, because all edges participate by default.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_default_embedding_score_at_least_as_high_as_coaccess_only() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, embed_nb_id, _coaccess_nb_id) =
            setup_filter_data(&tmp, &db, &tx, &project).await;

        // Default (all edges active).
        let default_resp = call_build_context(&server, &project_id, &seed_permalink, None).await;
        assert!(
            default_resp.error.is_none(),
            "default call should not error: {:?}",
            default_resp.error
        );

        // co_access only (embedding graph contribution removed).
        let coaccess_only = call_build_context(
            &server,
            &project_id,
            &seed_permalink,
            Some(vec!["co_access".to_string()]),
        )
        .await;
        assert!(
            coaccess_only.error.is_none(),
            "coaccess_only call should not error: {:?}",
            coaccess_only.error
        );

        let default_score = score_for_note(&default_resp, &embed_nb_id);
        let coaccess_score = score_for_note(&coaccess_only, &embed_nb_id);

        assert!(
            default_score.is_some(),
            "embedding neighbor should appear in default build_context results"
        );
        assert!(
            coaccess_score.is_some(),
            "embedding neighbor should appear in co_access-only results via temporal/FTS"
        );
        assert!(
            default_score.unwrap() >= coaccess_score.unwrap(),
            "default score ({:?}) for embedding neighbor should be >= co_access-only score ({:?}), \
             because default includes the embedding_related graph boost",
            default_score,
            coaccess_score
        );
    }
}

// Deterministic precedence tests for wikilink, co_access, and embedding_related edges.
//
// Validates that:
// 1. An explicit wikilink candidate outranks a comparable embedding_related candidate.
// 2. A co_access-only candidate participates with a positive score (non-no-op).
// 3. Precedence assertions control / equalize non-graph (FTS, temporal) signals.
//
// Uses `memory_build_context` with the seed note as the retrieval anchor so graph
// proximity is computed from the seed (explicit wikilinks → HOP_DECAY 0.7,
// embedding_related → HOP_DECAY*0.5*weight, co_access → HOP_DECAY*weight).
// All neighbor notes share near-identical content so FTS/temporal signals are
// equalized; only graph proximity differs.

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

    /// Shared neighbor content base so FTS signals are equalized.
    const SHARED_CONTENT: &str = "Architecture design patterns database systems core principles \
        engineering framework. Shared domain coverage for deterministic retrieval ranking tests \
        across graph edge kinds. Covers database architecture, system design, and engineering \
        patterns with broad topical overlap.";

    /// Seed a note graph: one seed + three neighbors (wikilink, embedding_related, co_access).
    ///
    /// The seed's content is updated to include a `[[Title]]` wikilink to the
    /// wikilink neighbor, which triggers `note_links` indexing through the
    /// standard `update()` path. Embedding and co-access edges are wired via
    /// `upsert_provenance_association`.
    ///
    /// Returns `(server, project_id, seed_permalink, wikilink_nb_id, embed_nb_id, coaccess_nb_id)`.
    async fn setup_precedence_data(
        _tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> (DjinnMcpServer, String, String, String, String, String) {
        let repo = NoteRepository::new(db.clone(), event_bus_for(tx));

        // Seed note — retrieval anchor for build_context.
        let seed = repo
            .create(
                &project.id,
                "Precedence Seed",
                "Initial seed content placeholder for precedence test.",
                "adr",
                "[\"seed\",\"precedence\"]",
            )
            .await
            .unwrap();

        // Three neighbors with near-identical content to equalize FTS/temporal.
        let wikilink_nb = repo
            .create(
                &project.id,
                "Design Alpha",
                SHARED_CONTENT,
                "reference",
                "[\"alpha\"]",
            )
            .await
            .unwrap();

        let embed_nb = repo
            .create(
                &project.id,
                "Design Beta",
                SHARED_CONTENT,
                "reference",
                "[\"beta\"]",
            )
            .await
            .unwrap();

        let coaccess_nb = repo
            .create(
                &project.id,
                "Design Gamma",
                SHARED_CONTENT,
                "reference",
                "[\"gamma\"]",
            )
            .await
            .unwrap();

        // Update seed content with a wikilink to the alpha neighbor.
        // `update()` re-indexes `note_links` via `index_links_for_note`,
        // so `Design Alpha` becomes a direct wikilink neighbor.
        let seed_content = format!(
            "Architecture design patterns database systems core principles engineering framework. \
             See [[Design Alpha]] for related analysis. {}",
            SHARED_CONTENT,
        );
        repo.update(&seed.id, "Precedence Seed", &seed_content, "[]")
            .await
            .unwrap();

        // Wire embedding_related association: seed ↔ embed_nb (weight 0.3).
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

        // Wire co_access association: seed ↔ coaccess_nb (weight 0.25).
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
            wikilink_nb.id.clone(),
            embed_nb.id.clone(),
            coaccess_nb.id.clone(),
        )
    }

    /// Call `memory_build_context` for the seed with default edge kinds.
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
    /// Searches both L1 and L0 tiers (scores come from the same RRF pipeline
    /// and are directly comparable across tiers).
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

    /// AC 1: An explicit wikilink candidate outranks a comparable
    /// `embedding_related` machine-associated candidate.
    ///
    /// All three neighbors share near-identical content so FTS and temporal
    /// signals are equalized. The only differentiator is graph proximity:
    /// wikilink edges use HOP_DECAY (0.7) while embedding_related uses
    /// HOP_DECAY * 0.5 * weight (≈0.105 at weight 0.3).  The wikilink
    /// neighbor must therefore receive a higher fused score.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_wikilink_outranks_comparable_embedding_related() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, wikilink_nb_id, embed_nb_id, _coaccess_nb_id) =
            setup_precedence_data(&tmp, &db, &tx, &project).await;

        let resp = call_build_context(&server, &project_id, &seed_permalink, None).await;
        assert!(
            resp.error.is_none(),
            "build_context should not error: {:?}",
            resp.error
        );

        let wl_score = score_for_note(&resp, &wikilink_nb_id);
        let em_score = score_for_note(&resp, &embed_nb_id);

        assert!(
            wl_score.is_some(),
            "wikilink neighbor must appear in build_context results"
        );
        assert!(
            em_score.is_some(),
            "embedding_related neighbor must appear in build_context results (graph expansion by default)"
        );
        assert!(
            wl_score.unwrap() > em_score.unwrap(),
            "wikilink candidate score ({:?}) must exceed embedding_related score ({:?}); \
             all non-graph signals are equalized by design",
            wl_score,
            em_score,
        );
    }

    /// AC 2: A co_access-only candidate participates through the same
    /// retrieval surface with a positive score, proving co-access behavior
    /// is not demoted or broken by the presence of embedding_related edges.
    ///
    /// The co_access neighbor has weight 0.25 → graph proximity
    /// HOP_DECAY * 0.25 = 0.175, which is between the embedding_related
    /// score (0.105) and the wikilink score (0.7).  It must appear with a
    /// positive fused score.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_coaccess_participates_with_positive_score() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, _wikilink_nb_id, _embed_nb_id, coaccess_nb_id) =
            setup_precedence_data(&tmp, &db, &tx, &project).await;

        let resp = call_build_context(&server, &project_id, &seed_permalink, None).await;
        assert!(
            resp.error.is_none(),
            "build_context should not error: {:?}",
            resp.error
        );

        let ca_score = score_for_note(&resp, &coaccess_nb_id);
        assert!(
            ca_score.is_some(),
            "co_access neighbor must appear in build_context results"
        );
        assert!(
            ca_score.unwrap() > 0.0,
            "co_access candidate must have a positive fused score, got {:?}",
            ca_score
        );
    }

    /// AC 1 + AC 2 combined with explicit edge_kinds filter.
    ///
    /// When `edge_kinds` includes both `embedding_related` and `co_access`,
    /// the precedence ordering must still hold: wikilink > co_access >
    /// embedding_related (matching the per-kind multiplier design).
    /// This proves embedding edges do not elevate above wikilink precedence
    /// even when both graph edge kinds are explicitly enabled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn precedence_ordering_preserved_with_explicit_edge_kinds() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, wikilink_nb_id, embed_nb_id, coaccess_nb_id) =
            setup_precedence_data(&tmp, &db, &tx, &project).await;

        // All edge kinds enabled via explicit filter (same as default but
        // exercises the filter path).
        let resp = call_build_context(
            &server,
            &project_id,
            &seed_permalink,
            Some(vec![
                "embedding_related".to_string(),
                "co_access".to_string(),
            ]),
        )
        .await;
        assert!(
            resp.error.is_none(),
            "build_context with edge_kinds should not error: {:?}",
            resp.error
        );

        let wl_score = score_for_note(&resp, &wikilink_nb_id);
        let em_score = score_for_note(&resp, &embed_nb_id);
        let ca_score = score_for_note(&resp, &coaccess_nb_id);

        // Wikilink direct neighbors always participate (note_links are
        // edge-kind-filter-independent).  Assert presence and ordering.
        assert!(
            wl_score.is_some(),
            "wikilink neighbor must appear regardless of edge_kinds filter"
        );
        assert!(
            em_score.is_some(),
            "embedding_related neighbor must appear with embedding_related in edge_kinds"
        );
        assert!(
            ca_score.is_some(),
            "co_access neighbor must appear with co_access in edge_kinds"
        );

        assert!(
            wl_score.unwrap() > em_score.unwrap(),
            "wikilink ({:?}) must outrank embedding_related ({:?}) under explicit edge_kinds",
            wl_score,
            em_score,
        );
        assert!(
            ca_score.unwrap() > em_score.unwrap(),
            "co_access ({:?}) must outrank embedding_related ({:?}) because \
             HOP_DECAY*weight (0.175) > HOP_DECAY*0.5*weight (0.105)",
            ca_score,
            em_score,
        );
    }
}

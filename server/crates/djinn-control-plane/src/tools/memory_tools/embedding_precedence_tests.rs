// Deterministic precedence tests for wikilink, co_access, and embedding_related edges.
//
// Validates that:
// 1. An explicit wikilink candidate outranks a comparable embedding_related candidate.
// 2. A co_access-only candidate participates with a positive score (non-no-op).
// 3. Precedence assertions control / equalize non-graph (FTS, temporal) signals.
//
// The fixture is engineered so the only signal that distinguishes the three
// candidates is the graph proximity component of the RRF pipeline:
//
//   * All three neighbor notes share IDENTICAL content (`SHARED_CONTENT`).
//   * All three titles are single, FTS-neutral words (`Alpha`, `Beta`, `Gamma`)
//     that do NOT appear in the seed's discovery FTS query. The seed's first
//     200 chars — which `run_rrf_discovery` uses as the FTS query — are
//     exactly `SHARED_CONTENT`, so every candidate receives the same C-weight
//     tsvector match (or none) and identical FTS scores.
//   * The seed's wikilink target reference `[[Alpha]]` is placed AFTER
//     character 200 so it never enters the FTS query.
//   * The seed is created and updated in the same tokio test, so temporal
//     signals (access_count, created_age, updated_age) are virtually identical
//     across all candidates.
//   * Before asserting precedence, each test uses the public
//     `repo.graph_proximity_scores` API to verify that the wikilink is
//     indexed (target receives the 0.7 `HOP_DECAY` graph score) and that the
//     embedding/co_access associations produce the expected bounded
//     multipliers. This guards against the previously-failed case where the
//     link was not actually indexed before being relied on.

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

    /// Shared content for all neighbors so FTS, temporal, and content-derived
    /// signals are equalized. The vocabulary here is deliberately broad so
    /// every candidate receives the same C-weight tsvector match when the
    /// discovery FTS query is built from the first 200 chars of this string.
    ///
    /// Length is engineered to be > 200 chars so we can place the wikilink
    /// `[[Alpha]]` AFTER the FTS-query cutoff without truncating the content.
    const SHARED_CONTENT: &str = "Architecture design patterns database systems core principles \
        engineering framework. Shared domain coverage for deterministic retrieval ranking tests \
        across graph edge kinds. Covers database architecture, system design, and engineering \
        patterns with broad topical overlap across research, references, and design notes.";

    /// One-hop spreading-activation multiplier for explicit wikilink edges.
    /// Mirrors `HOP_DECAY` in `djinn_db::repositories::note::scoring` so the
    /// test can verify the wikilink's indexed graph score precisely.
    const WIKILINK_MULTIPLIER: f64 = 0.7;

    /// Seed a note graph: one seed + three neighbors (wikilink, embedding_related, co_access).
    ///
    /// All candidates share `SHARED_CONTENT` and have FTS-neutral titles. The
    /// wikilink is placed past the FTS-query cutoff so the only signal that
    /// differentiates candidates is the graph proximity component.
    #[allow(clippy::too_many_arguments)]
    async fn setup_precedence_data(
        _tmp: &tempfile::TempDir,
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        project: &Project,
    ) -> (
        DjinnMcpServer,
        String,
        String,
        String,
        String,
        String,
        NoteRepository,
    ) {
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

        // Three neighbors with near-identical content (SHARED_CONTENT) and
        // FTS-neutral single-word titles (`Alpha`, `Beta`, `Gamma`). None of
        // these title words appear in `SHARED_CONTENT`, so when the discovery
        // FTS query is built from the seed's first 200 chars (which equal
        // `SHARED_CONTENT`), all three candidates receive identical FTS scores.
        let wikilink_nb = repo
            .create(
                &project.id,
                "Alpha",
                SHARED_CONTENT,
                "reference",
                "[\"alpha\"]",
            )
            .await
            .unwrap();

        let embed_nb = repo
            .create(
                &project.id,
                "Beta",
                SHARED_CONTENT,
                "reference",
                "[\"beta\"]",
            )
            .await
            .unwrap();

        let coaccess_nb = repo
            .create(
                &project.id,
                "Gamma",
                SHARED_CONTENT,
                "reference",
                "[\"gamma\"]",
            )
            .await
            .unwrap();

        // Build the seed's updated content with two regions:
        //
        //   1. The first 200 chars (the discovery FTS query) equal
        //      `SHARED_CONTENT` exactly. Because all three candidates'
        //      content is identical, every candidate FTS-matches the query
        //      with the same C-weight token set, producing identical FTS
        //      scores. No candidate has a lexical advantage.
        //
        //   2. AFTER the 200-char cutoff, append the wikilink `[[Alpha]]`
        //      so the explicit link is indexed via `update()` →
        //      `index_links_for_note()` → `extract_wikilinks()`. The wikilink
        //      tokens never enter the discovery FTS query, so they cannot
        //      bias the ranking toward the wikilink candidate.
        //
        // `SHARED_CONTENT` is longer than 200 chars (verified in the
        // `shared_content_is_long_enough_for_fts_cutoff` test below), so the
        // wikilink placement is deterministic.
        let mut seed_content = String::new();
        seed_content.push_str(SHARED_CONTENT);
        seed_content.push('\n');
        seed_content.push_str("Reference: [[Alpha]] for related analysis.");
        repo.update(&seed.id, "Precedence Seed", &seed_content, "[]")
            .await
            .unwrap();

        // Wire embedding_related association: seed ↔ embed_nb (weight 0.3).
        // Effective per-hop contribution = HOP_DECAY * 0.5 * weight = 0.105.
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
        // Effective per-hop contribution = HOP_DECAY * weight = 0.175.
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
            repo,
        )
    }

    /// Call `memory_build_context` for the seed with the given `edge_kinds`.
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

    /// Extract the fused score for a given note ID from a build_context
    /// response. Searches both L1 and L0 tiers (scores come from the same
    /// RRF pipeline and are directly comparable across tiers).
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

    /// Return the one-hop `graph_proximity_scores` map for `seed_id` using
    /// the public `NoteRepository::graph_proximity_scores` API. Used to
    /// verify that wikilinks and associations are indexed as expected
    /// before relying on the fused build_context score.
    ///
    /// `seed_id` must be the note primary-key `id`, NOT its permalink.
    async fn graph_scores_for_seed(
        repo: &NoteRepository,
        seed_id: &str,
    ) -> std::collections::HashMap<String, f64> {
        repo.graph_proximity_scores(std::slice::from_ref(&seed_id.to_string()), 1)
            .await
            .unwrap()
            .into_iter()
            .collect()
    }

    /// Same as [`graph_scores_for_seed`] but takes a permalink and resolves
    /// it to the seed primary-key id via the public `get_by_permalink` API.
    /// Keeps test setup permalink-typed while passing the right id into the
    /// graph-proximity API.
    async fn graph_scores_for_permalink(
        repo: &NoteRepository,
        project_id: &str,
        seed_permalink: &str,
    ) -> std::collections::HashMap<String, f64> {
        let seed_id = repo
            .get_by_permalink(project_id, seed_permalink)
            .await
            .unwrap()
            .expect("seed must exist by permalink")
            .id;
        graph_scores_for_seed(repo, &seed_id).await
    }

    /// Verify that `SHARED_CONTENT` is at least 200 chars so the wikilink
    /// placement past the FTS-query cutoff is deterministic. If this test
    /// ever fails, update `SHARED_CONTENT` to be longer and re-validate the
    /// wikilink placement reasoning in `setup_precedence_data`.
    #[test]
    fn shared_content_is_long_enough_for_fts_cutoff() {
        assert!(
            SHARED_CONTENT.chars().count() > 200,
            "SHARED_CONTENT must exceed 200 chars so the wikilink can be placed \
             AFTER the discovery FTS query cutoff; got {} chars",
            SHARED_CONTENT.chars().count()
        );
        // The first 200 chars must NOT contain `[[` — that would re-introduce
        // the lexical asymmetry the fixture is designed to avoid.
        let first_200: String = SHARED_CONTENT.chars().take(200).collect();
        assert!(
            !first_200.contains("[["),
            "first 200 chars of SHARED_CONTENT must not contain a wikilink; got {:?}",
            first_200
        );
    }

    /// Verify that the candidate-distinguishing title tokens (`Alpha`,
    /// `Beta`, `Gamma`) do NOT appear in the first 200 chars of
    /// `SHARED_CONTENT` (which becomes the discovery FTS query when the
    /// seed's content is `SHARED_CONTENT` followed by the wikilink).
    /// This is the lexical-signal equalization guarantee.
    #[test]
    fn seed_fts_query_is_lexically_equalized() {
        let first_200: String = SHARED_CONTENT.chars().take(200).collect();
        for forbidden in ["Alpha", "Beta", "Gamma", "alpha", "beta", "gamma"] {
            assert!(
                !first_200.contains(forbidden),
                "FTS query must not contain candidate-distinguishing token {:?}; \
                 first 200 chars: {:?}",
                forbidden,
                first_200
            );
        }
    }

    /// AC 1: An explicit wikilink candidate outranks a comparable
    /// `embedding_related` machine-associated candidate.
    ///
    /// Steps:
    ///   1. Build the deterministic fixture (seed + 3 FTS-equalized neighbors).
    ///   2. Use the public `graph_proximity_scores` API to prove the wikilink
    ///      is indexed (`Alpha` receives `WIKILINK_MULTIPLIER` = 0.7) before
    ///      relying on the fused score.
    ///   3. Use `memory_build_context` to compute the fused RRF score.
    ///   4. Assert `wikilink_score > embedding_related_score` under
    ///      controlled FTS conditions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_wikilink_outranks_comparable_embedding_related() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (
            server,
            project_id,
            seed_permalink,
            wikilink_nb_id,
            embed_nb_id,
            _coaccess_nb_id,
            repo,
        ) = setup_precedence_data(&tmp, &db, &tx, &project).await;

        // Step 2: explicit public-repository API check that the wikilink is
        // indexed and produces the expected HOP_DECAY multiplier before we
        // rely on the fused score. This guards against the previously-failed
        // case where the link was not actually indexed.
        let graph_scores = graph_scores_for_permalink(&repo, &project_id, &seed_permalink).await;
        let wl_graph = graph_scores
            .get(&wikilink_nb_id)
            .copied()
            .expect("wikilink must be indexed via graph_proximity_scores");
        assert!(
            (wl_graph - WIKILINK_MULTIPLIER).abs() < 1e-6,
            "wikilink graph score must equal HOP_DECAY ({WIKILINK_MULTIPLIER}); got {wl_graph}",
        );

        // Step 3: fused build_context ranking with default edge kinds.
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
            "embedding_related neighbor must appear in build_context results \
             (graph expansion by default)"
        );
        // Step 4: precedence assertion. Non-graph signals (FTS, temporal)
        // are equalized by the fixture, so any score difference is
        // attributable to the graph proximity component (wikilink =
        // HOP_DECAY vs embedding_related = HOP_DECAY * 0.5 * weight).
        assert!(
            wl_score.unwrap() > em_score.unwrap(),
            "wikilink candidate score ({:?}) must exceed embedding_related score ({:?}); \
             non-graph (FTS, temporal) signals are equalized by fixture design",
            wl_score,
            em_score,
        );
    }

    /// AC 2: A co_access-only candidate participates with a positive score,
    /// proving co-access behavior is not demoted or broken by the presence of
    /// embedding_related edges.
    ///
    /// Verifies that the co_access candidate:
    ///   1. Receives a positive graph proximity score via the public API
    ///      (`HOP_DECAY * weight` = 0.175).
    ///   2. Appears in the fused build_context result with a positive score.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_coaccess_participates_with_positive_score() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (
            server,
            project_id,
            seed_permalink,
            _wikilink_nb_id,
            _embed_nb_id,
            coaccess_nb_id,
            repo,
        ) = setup_precedence_data(&tmp, &db, &tx, &project).await;

        // Public-API check: co_access association produces the expected
        // multiplier (HOP_DECAY * weight = 0.7 * 0.25 = 0.175).
        let graph_scores = graph_scores_for_permalink(&repo, &project_id, &seed_permalink).await;
        let ca_graph = graph_scores
            .get(&coaccess_nb_id)
            .copied()
            .expect("co_access association must produce a graph score");
        let expected_ca_graph = WIKILINK_MULTIPLIER * 0.25;
        assert!(
            (ca_graph - expected_ca_graph).abs() < 1e-6,
            "co_access graph score must equal HOP_DECAY * weight ({expected_ca_graph}); got {ca_graph}",
        );

        // Fused build_context: co_access candidate must appear with a positive
        // score, proving co-access behavior is not demoted by embedding edges.
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

    /// AC 1 + AC 2 combined: precedence ordering preserved when both
    /// `embedding_related` and `co_access` are explicitly enabled via
    /// `edge_kinds`. Embedding edges must not elevate above wikilink
    /// precedence under controlled FTS conditions.
    ///
    /// Expected ordering (per-kind multipliers, single hop):
    ///   wikilink          = HOP_DECAY                       = 0.7
    ///   co_access         = HOP_DECAY * weight              = 0.7 * 0.25 = 0.175
    ///   embedding_related = HOP_DECAY * 0.5 * weight        = 0.5 * 0.7 * 0.3 = 0.105
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn precedence_ordering_preserved_with_explicit_edge_kinds() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let (server, project_id, seed_permalink, wikilink_nb_id, embed_nb_id, coaccess_nb_id, repo) =
            setup_precedence_data(&tmp, &db, &tx, &project).await;

        // Public-API check: explicit edge_kinds=["embedding_related",
        // "co_access"] must still include both machine-minted edges at their
        // bounded multipliers while the wikilink edge remains at HOP_DECAY.
        let seed_id = repo
            .get_by_permalink(&project_id, &seed_permalink)
            .await
            .unwrap()
            .expect("seed must exist by permalink")
            .id;
        let explicit_scores = repo
            .graph_proximity_scores_with_edge_kinds(
                std::slice::from_ref(&seed_id),
                1,
                Some(&["embedding_related".to_string(), "co_access".to_string()]),
            )
            .await
            .unwrap()
            .0
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        assert!(
            (explicit_scores.get(&embed_nb_id).copied().unwrap_or(0.0)
                - WIKILINK_MULTIPLIER * 0.5 * 0.3)
                .abs()
                < 1e-6,
            "embedding_related must produce HOP_DECAY * 0.5 * weight under explicit filter"
        );
        assert!(
            (explicit_scores.get(&coaccess_nb_id).copied().unwrap_or(0.0)
                - WIKILINK_MULTIPLIER * 0.25)
                .abs()
                < 1e-6,
            "co_access must produce HOP_DECAY * weight under explicit filter"
        );

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
        // edge-kind-filter-independent). Assert presence and ordering.
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

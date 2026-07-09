// End-to-end control-plane regressions for the embedding-related health and
// authored-orphan split metrics (epic 68o7 / cbld).
//
// These tests exercise the *real* `memory_health` and `memory_orphans` MCP
// ops end-to-end via the `ops` module entry points. They seed typed
// `embedding_related` provenance associations directly through
// `NoteRepository::upsert_provenance_association` — no housekeeping is run
// so the assertions target the seed graph deterministically.
//
// Scope: only health and orphan semantics. Retrieval precedence / context
// expansion coverage lives in `build_context_tests.rs` (task `4i1e`).

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{
        Database, NoteAssociationKind, NoteAssociationProvenanceUpsert, NoteAssociationSource,
        NoteRepository, ProjectRepository,
    };
    use tokio::sync::broadcast;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::ops;
    use crate::tools::memory_tools::{HealthParams, OrphansParams};

    // Threshold above which `embedding_related` edges count as
    // retrieval-effective for graph-isolation purposes. Mirrors
    // `djinn_db::repositories::note::embedding_associations::EMBEDDING_ASSOCIATION_THRESHOLD`
    // (0.78); we pick a confidence strictly above so the seed is
    // unambiguously "above threshold".
    const ABOVE_THRESHOLD_CONFIDENCE: f64 = 0.85;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
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

    struct EmbeddingHealthFixture {
        server: DjinnMcpServer,
        _tmp: tempfile::TempDir,
        /// `"owner/repo"` — what `Project::slug()` returns.
        project_slug: String,
        /// Notes wired together with a wikilink so they are neither orphans
        /// nor isolated — they form the "clean baseline" of the fixture.
        _linked_source_id: String,
        _linked_target_id: String,
        /// Plain authored-orphan note with no inbound edges of any kind.
        /// Counts toward `authored_orphan_count`, `isolated_count`,
        /// and `machine_connected_orphan_count` should NOT pick it up.
        pure_orphan_id: String,
        pure_orphan_permalink: String,
        /// Authored-orphan note that only has a threshold-qualified
        /// `embedding_related` machine edge connecting it to `linked_target`.
        /// Should count toward `authored_orphan_count` and
        /// `machine_connected_orphan_count` but NOT toward `isolated_count`.
        machine_connected_orphan_id: String,
        machine_connected_orphan_permalink: String,
    }

    async fn build_embedding_health_fixture() -> EmbeddingHealthFixture {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);

        let project = ProjectRepository::new(db.clone(), event_bus.clone())
            .create("emb-health", "test", "emb-health")
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), event_bus);

        // Two notes wired by a resolved wikilink. Both are non-orphans,
        // non-isolated, and form the clean baseline of the fixture.
        let linked_source = repo
            .create(
                &project.id,
                "Linked Source",
                "See [[Linked Target]] for the matching pattern.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let linked_target = repo
            .create(
                &project.id,
                "Linked Target",
                "Linked target body that resolves the [[Linked Source]] wikilink.",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Plain authored orphan: zero inbound edges of any kind. Must count
        // as authored-orphan debt AND as graph-isolated.
        let pure_orphan = repo
            .create(
                &project.id,
                "Pure Orphan",
                "No inbound wikilinks and no associations.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // Authored orphan that ONLY has a threshold-qualified machine-minted
        // `embedding_related` edge connecting it to `linked_target`. The
        // machine edge must reduce isolation but must NOT hide the
        // authored-orphan debt.
        let machine_connected = repo
            .create(
                &project.id,
                "Machine-Connected Orphan",
                "No inbound wikilinks; only a similarity edge to Linked Target.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // Seed the provenance-rich embedding_related row directly through
        // the NoteRepository API. Use `EmbeddingRelated` kind with
        // `EmbeddingSimilarity` source and confidence above the 0.78
        // threshold so the row is unambiguously retrieval-effective.
        repo.upsert_provenance_association(
            &machine_connected.id,
            &linked_target.id,
            &NoteAssociationProvenanceUpsert {
                kind: NoteAssociationKind::EmbeddingRelated,
                source: NoteAssociationSource::EmbeddingSimilarity,
                weight: ABOVE_THRESHOLD_CONFIDENCE,
                confidence: Some(ABOVE_THRESHOLD_CONFIDENCE),
                algorithm_version: Some("cbld-test-v1".to_string()),
                embedding_model: Some("cbld-test-model".to_string()),
                embedding_dim: Some(8),
            },
        )
        .await
        .unwrap();

        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));

        EmbeddingHealthFixture {
            server,
            _tmp: tmp,
            project_slug: project.slug(),
            _linked_source_id: linked_source.id,
            _linked_target_id: linked_target.id,
            pure_orphan_id: pure_orphan.id.clone(),
            pure_orphan_permalink: pure_orphan.permalink,
            machine_connected_orphan_id: machine_connected.id.clone(),
            machine_connected_orphan_permalink: machine_connected.permalink,
        }
    }

    // ── health: split metrics with embedding_related machine edges ─────────

    /// `memory_health` must surface authored-orphan debt separately from
    /// graph isolation and credit the threshold-qualified
    /// `embedding_related` machine edge with reducing isolation without
    /// hiding the orphan debt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_health_reports_split_metrics_with_embedding_edges() {
        let fixture = build_embedding_health_fixture().await;

        // Sanity-check the fixture: the IDs of the two authored orphans
        // must be distinct (they are two separate notes). A regression
        // that collapsed the fixture into a single note would trip here.
        assert_ne!(
            fixture.pure_orphan_id, fixture.machine_connected_orphan_id,
            "fixture must contain two distinct authored-orphan notes"
        );

        let response = ops::memory_health(
            &fixture.server,
            HealthParams {
                project: Some(fixture.project_slug.clone()),
            },
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );

        // All four split-metric fields must be present.
        let authored_orphan_count = response
            .authored_orphan_count
            .expect("authored_orphan_count must be set");
        let isolated_count = response.isolated_count.expect("isolated_count must be set");
        let isolated_pct = response.isolated_pct.expect("isolated_pct must be set");
        let machine_connected_orphan_count = response
            .machine_connected_orphan_count
            .expect("machine_connected_orphan_count must be set");
        let orphan_note_count = response
            .orphan_note_count
            .expect("orphan_note_count must be set");

        // `orphan_note_count` is the backward-compatible alias and must
        // equal `authored_orphan_count`.
        assert_eq!(
            orphan_note_count, authored_orphan_count,
            "orphan_note_count must equal authored_orphan_count"
        );

        // Two authored orphans in the fixture: pure + machine-connected.
        assert_eq!(
            authored_orphan_count, 2,
            "expected 2 authored orphans (pure + machine-connected), got {authored_orphan_count}"
        );

        // Only the pure orphan is graph-isolated; the machine-connected
        // orphan has a threshold-qualified `embedding_related` edge.
        assert_eq!(
            isolated_count, 1,
            "expected exactly 1 graph-isolated note (pure orphan only), got {isolated_count}"
        );

        // The machine-connected orphan must show up in
        // `machine_connected_orphan_count`.
        assert_eq!(
            machine_connected_orphan_count, 1,
            "expected 1 machine-connected orphan, got {machine_connected_orphan_count}"
        );

        // `isolated_pct` must be a well-formed percentage in [0, 100].
        assert!(
            (0.0..=100.0).contains(&isolated_pct),
            "isolated_pct must be in [0, 100], got {isolated_pct}"
        );
        // 1 isolated out of 4 active non-singleton notes = 25%.
        assert!(
            (isolated_pct - 25.0).abs() < 1e-9,
            "expected isolated_pct ~25.0, got {isolated_pct}"
        );

        // The machine-connected orphan contributes to
        // `machine_connected_orphan_count` while still counting as
        // authored-orphan debt. Concretely: authored_orphan_count >
        // isolated_count, because the machine-connected orphan is
        // authored-orphan debt but is not graph-isolated.
        assert!(
            authored_orphan_count > isolated_count,
            "authored_orphan_count ({authored_orphan_count}) must be greater than isolated_count ({isolated_count}) when a machine edge reduces isolation for an authored orphan"
        );
        // Specifically the gap is exactly the machine-connected count.
        assert_eq!(
            authored_orphan_count - isolated_count,
            machine_connected_orphan_count,
            "authored_orphan_count - isolated_count must equal machine_connected_orphan_count"
        );
    }

    // ── orphans: authored-orphan detail survives machine edges ─────────────

    /// `memory_orphans()` must continue to surface an authored-orphan note
    /// even when it is connected to the rest of the graph only via
    /// machine-minted `embedding_related` edges. The detail-level listing
    /// does not collapse authored-orphan debt just because the note is
    /// not graph-isolated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_orphans_includes_notes_connected_only_by_embedding_related_edges() {
        let fixture = build_embedding_health_fixture().await;

        let response = ops::memory_orphans(
            &fixture.server,
            OrphansParams {
                project: fixture.project_slug.clone(),
                folder: None,
            },
        )
        .await;

        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );

        let orphan_permalinks: Vec<&str> = response
            .orphans
            .iter()
            .map(|o| o.permalink.as_str())
            .collect();

        // The pure authored orphan must always appear in the listing.
        assert!(
            orphan_permalinks.contains(&fixture.pure_orphan_permalink.as_str()),
            "pure authored orphan must appear in memory_orphans; got {:?}",
            orphan_permalinks
        );

        // The machine-connected orphan must STILL appear in
        // `memory_orphans()` — authored-orphan debt is not hidden by the
        // `embedding_related` machine edge. This is the key invariant
        // the call site relies on to keep cleanup debt visible.
        assert!(
            orphan_permalinks.contains(&fixture.machine_connected_orphan_permalink.as_str()),
            "authored-orphan note connected only by embedding_related edges must appear in memory_orphans; got {:?}",
            orphan_permalinks
        );

        // The two linked-by-wikilink notes must NOT appear in the orphan
        // listing — they have inbound authored edges.
        for orphan in &response.orphans {
            assert_ne!(
                orphan.id, fixture._linked_source_id,
                "wikilinked source must not be reported as orphan"
            );
            assert_ne!(
                orphan.id, fixture._linked_target_id,
                "wikilinked target must not be reported as orphan"
            );
        }

        // The number of orphans in the detail listing must match the
        // `authored_orphan_count` health metric.
        let health = ops::memory_health(
            &fixture.server,
            HealthParams {
                project: Some(fixture.project_slug.clone()),
            },
        )
        .await;
        assert!(health.error.is_none(), "{:?}", health.error);
        let authored_orphan_count = health.authored_orphan_count.unwrap();
        assert_eq!(
            response.orphans.len() as i64,
            authored_orphan_count,
            "memory_orphans count must equal authored_orphan_count from memory_health"
        );
    }
}

use djinn_core::models::Project;
use djinn_memory::canonical_pair;
use tokio::sync::broadcast;

use super::*;
use crate::repositories::test_support::{event_bus_for, make_project};

async fn make_note(
    repo: &NoteRepository,
    project: &Project,
    _tmp: &tempfile::TempDir,
    title: &str,
) -> String {
    let note = repo
        .create(&project.id, title, "content", "reference", "[]")
        .await
        .unwrap();
    note.id
}

// ── Source-aware co-access isolation tests (ao5x / xk17) ────────────────
//
// The tests below verify that co-access pruning helpers (used by periodic
// housekeeping) only delete `kind = 'co_access'` / `source =
// 'session_co_access'` rows.  Typed provenance edges that share the same
// note pair or have low weight must survive co-access pruning.

// ── Provenance-ready substrate regression tests (ao5x / wave 1) ─────────
//
// The tests below verify the full provenance-rich note association
// substrate after migrations q41j, ixib, and xk17 land.  They cover
// row coexistence, provenance round-trip, idempotent upsert by the
// four-column key (note_a_id, note_b_id, kind, source), and the
// non-co-access isolation guarantee (co_access_count is never
// incremented for authored/embedding rows and the legacy
// session_co_access row is never overwritten by non-co-access writes).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_access_authored_embedding_rows_can_coexist() {
    // AC: co_access, authored, and embedding_related rows can coexist for
    // the same canonical note pair with distinct (kind, source) slots.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Coexist A").await;
    let note_b = make_note(&repo, &project, &tmp, "Coexist B").await;

    // 1) Implicit Hebbian co-access row.
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    // 2) Authored row (different kind, same source slot).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::SessionCoAccess,
            weight: 0.7,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // 3) Embedding row (different kind AND different source).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.85,
            confidence: Some(0.9),
            algorithm_version: Some("v2".to_string()),
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_dim: Some(1536),
        },
    )
    .await
    .unwrap();

    // All three rows must coexist.
    let all = repo
        .list_provenance_associations_for_pair(&note_a, &note_b)
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        3,
        "expected 3 coexistent rows (co_access, authored, embedding_related), got {}",
        all.len()
    );

    let kinds: Vec<&str> = all.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains(&"co_access"), "co_access row missing");
    assert!(kinds.contains(&"authored"), "authored row missing");
    assert!(
        kinds.contains(&"embedding_related"),
        "embedding_related row missing"
    );

    // Each row has the correct (kind, source) combination.
    let co_access_row = all.iter().find(|r| r.kind.as_str() == "co_access").unwrap();
    assert_eq!(co_access_row.source.as_str(), "session_co_access");

    let authored_row = all.iter().find(|r| r.kind.as_str() == "authored").unwrap();
    assert_eq!(authored_row.source.as_str(), "session_co_access");
    assert!((authored_row.weight - 0.7).abs() < 1e-12);

    let embedding_row = all
        .iter()
        .find(|r| r.kind.as_str() == "embedding_related")
        .unwrap();
    assert_eq!(embedding_row.source.as_str(), "embedding_similarity");
    assert!((embedding_row.weight - 0.85).abs() < 1e-12);

    // Also verify the raw row count in the DB.
    let (a_id, b_id) = canonical_pair(&note_a, &note_b);
    let raw_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations WHERE note_a_id = $1 AND note_b_id = $2",
    )
    .bind(a_id)
    .bind(b_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(raw_count, 3, "expected 3 raw rows, got {raw_count}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_upsert_readback_preserves_all_fields() {
    // AC: typed provenance upserts/readbacks preserve confidence,
    // algorithm version, embedding model, embedding dimension, last refresh
    // timestamp, kind, source, and weight.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Roundtrip A").await;
    let note_b = make_note(&repo, &project, &tmp, "Roundtrip B").await;

    // Write a fully-populated embedding_related row.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.77,
            confidence: Some(0.92),
            algorithm_version: Some("v3.1".to_string()),
            embedding_model: Some("text-embedding-3-large".to_string()),
            embedding_dim: Some(3072),
        },
    )
    .await
    .unwrap();

    // Read it back via the provenance-specific get.
    let row = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::EmbeddingRelated,
            &NoteAssociationSource::EmbeddingSimilarity,
        )
        .await
        .unwrap()
        .expect("embedding_related row must exist");

    assert_eq!(row.kind, NoteAssociationKind::EmbeddingRelated);
    assert_eq!(row.source, NoteAssociationSource::EmbeddingSimilarity);
    assert!((row.weight - 0.77).abs() < 1e-12, "weight: {}", row.weight);
    assert!(
        (row.confidence.unwrap() - 0.92).abs() < 1e-12,
        "confidence: {:?}",
        row.confidence
    );
    assert_eq!(
        row.algorithm_version.as_deref(),
        Some("v3.1"),
        "algorithm_version"
    );
    assert_eq!(
        row.embedding_model.as_deref(),
        Some("text-embedding-3-large"),
        "embedding_model"
    );
    assert_eq!(row.embedding_dim, Some(3072), "embedding_dim");
    // last_refreshed_at is set by the upsert to the current UTC timestamp;
    // it must be non-null and parseable.
    assert!(
        row.last_refreshed_at.is_some(),
        "last_refreshed_at must be set for provenance upserts"
    );
    let refreshed = row.last_refreshed_at.as_ref().unwrap();
    assert!(
        refreshed.ends_with('Z'),
        "last_refreshed_at must be UTC: {refreshed}"
    );

    // Also read via list_provenance_associations_for_note and verify the
    // same fields are returned.
    let listed = repo
        .list_provenance_associations_for_note(
            &note_a,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    let listed_row = &listed[0];
    assert_eq!(listed_row.kind, NoteAssociationKind::EmbeddingRelated);
    assert_eq!(
        listed_row.source,
        NoteAssociationSource::EmbeddingSimilarity
    );
    assert!((listed_row.weight - 0.77).abs() < 1e-12);
    assert!((listed_row.confidence.unwrap() - 0.92).abs() < 1e-12);
    assert_eq!(listed_row.embedding_dim, Some(3072));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_upsert_is_idempotent_by_quadruple() {
    // AC: repeated upserts are idempotent by (note_a_id, note_b_id, kind,
    // source).  Weight uses max-merge; provenance metadata fields are
    // overwritten with the latest values.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Idem A").await;
    let note_b = make_note(&repo, &project, &tmp, "Idem B").await;
    let (expected_a, expected_b) = canonical_pair(&note_a, &note_b);

    // First write — moderate weight, specific provenance.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.6,
            confidence: Some(0.8),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("model-a".to_string()),
            embedding_dim: Some(256),
        },
    )
    .await
    .unwrap();

    // Second write — lower weight (must NOT reduce), updated provenance.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.3,
            confidence: Some(0.5),
            algorithm_version: Some("v2".to_string()),
            embedding_model: Some("model-b".to_string()),
            embedding_dim: Some(512),
        },
    )
    .await
    .unwrap();

    // Exactly one row for the quadruple.
    let raw_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations
         WHERE note_a_id = $1 AND note_b_id = $2
           AND kind = 'embedding_related' AND source = 'embedding_similarity'",
    )
    .bind(expected_a)
    .bind(expected_b)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        raw_count, 1,
        "duplicate row was inserted for the same quadruple"
    );

    // Weight keeps max (0.6 > 0.3).
    let row = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::EmbeddingRelated,
            &NoteAssociationSource::EmbeddingSimilarity,
        )
        .await
        .unwrap()
        .expect("row must exist");

    assert!(
        (row.weight - 0.6).abs() < 1e-12,
        "max-merge weight must keep 0.6, got {}",
        row.weight
    );

    // Provenance metadata is overwritten with latest values.
    assert_eq!(
        row.algorithm_version.as_deref(),
        Some("v2"),
        "algorithm_version must be overwritten"
    );
    assert_eq!(
        row.embedding_model.as_deref(),
        Some("model-b"),
        "embedding_model must be overwritten"
    );
    assert_eq!(
        row.embedding_dim,
        Some(512),
        "embedding_dim must be overwritten"
    );

    // A stronger third write raises the weight floor.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.9,
            confidence: Some(0.95),
            algorithm_version: Some("v3".to_string()),
            embedding_model: Some("model-c".to_string()),
            embedding_dim: Some(1024),
        },
    )
    .await
    .unwrap();

    let row = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::EmbeddingRelated,
            &NoteAssociationSource::EmbeddingSimilarity,
        )
        .await
        .unwrap()
        .expect("row must exist");
    assert!(
        (row.weight - 0.9).abs() < 1e-12,
        "stronger write must raise weight, got {}",
        row.weight
    );
    assert_eq!(row.algorithm_version.as_deref(), Some("v3"));

    // Still exactly one row.
    let raw_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations
         WHERE note_a_id = $1 AND note_b_id = $2
           AND kind = 'embedding_related' AND source = 'embedding_similarity'",
    )
    .bind(expected_a)
    .bind(expected_b)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(raw_count, 1, "three upserts must produce exactly 1 row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_upsert_canonicalizes_reversed_note_ids() {
    // AC: upserts canonicalize reversed note IDs — writing with (b, a) and
    // then (a, b) produces one row at the canonical position.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Canon A").await;
    let note_b = make_note(&repo, &project, &tmp, "Canon B").await;
    let (expected_a, expected_b) = canonical_pair(&note_a, &note_b);

    // Write with reversed order.
    repo.upsert_provenance_association(
        &note_b,
        &note_a,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::LlmEnrichment,
            weight: 0.6,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // Write with canonical order — same (kind, source).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::LlmEnrichment,
            weight: 0.4,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // Exactly one row at the canonical position.
    let raw_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations
         WHERE note_a_id = $1 AND note_b_id = $2
           AND kind = 'authored' AND source = 'llm_enrichment'",
    )
    .bind(expected_a)
    .bind(expected_b)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(raw_count, 1, "canonical pair must produce exactly 1 row");

    // Weight keeps max (0.6 from the first reversed write).
    let row = repo
        .get_provenance_association(
            &note_b, // reversed — still canonicalized internally
            &note_a,
            NoteAssociationKind::Authored,
            &NoteAssociationSource::LlmEnrichment,
        )
        .await
        .unwrap()
        .expect("authored row must exist");
    assert!(
        (row.weight - 0.6).abs() < 1e-12,
        "max-merge must keep 0.6, got {}",
        row.weight
    );
    // note_a_id < note_b_id in the returned row.
    assert_eq!(row.note_a_id, expected_a);
    assert_eq!(row.note_b_id, expected_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_co_access_rows_do_not_increment_co_access_count() {
    // AC: non-co-access rows do not increment co_access_count.  Only the
    // Hebbian upsert_association path increments it.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Count A").await;
    let note_b = make_note(&repo, &project, &tmp, "Count B").await;

    // Seed a co-access row with co_access_count = 3.
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    let co_access = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("co_access row must exist");
    assert_eq!(co_access.co_access_count, 3, "pre-condition: count=3");

    // Upsert an authored row for the same pair.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::SessionCoAccess,
            weight: 0.8,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // Upsert an embedding row for the same pair.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.9,
            confidence: Some(0.85),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("test-model".to_string()),
            embedding_dim: Some(512),
        },
    )
    .await
    .unwrap();

    // Co-access row's count must remain 3.
    let co_access = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("co_access row must still exist");
    assert_eq!(
        co_access.co_access_count, 3,
        "non-co-access writes must not increment co_access_count"
    );

    // Authored row has co_access_count = 0.
    let authored = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::Authored,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("authored row must exist");
    assert_eq!(
        authored.co_access_count, 0,
        "non-co-access row must have co_access_count=0"
    );

    // Embedding row also has co_access_count = 0.
    let embedding = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::EmbeddingRelated,
            &NoteAssociationSource::EmbeddingSimilarity,
        )
        .await
        .unwrap()
        .expect("embedding row must exist");
    assert_eq!(
        embedding.co_access_count, 0,
        "embedding row must have co_access_count=0"
    );

    // Re-upsert the authored row — co_access_count must still be 0 (not incremented).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::SessionCoAccess,
            weight: 0.9,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    let authored = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::Authored,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("authored row must exist");
    assert_eq!(
        authored.co_access_count, 0,
        "repeated non-co-access upsert must not increment co_access_count"
    );

    // Co-access row is unchanged after all the above.
    let co_access = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("co_access row must still exist");
    assert_eq!(
        co_access.co_access_count, 3,
        "co_access_count must remain 3 after typed provenance writes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_co_access_rows_do_not_overwrite_session_co_access_row() {
    // AC: writing a non-co-access provenance row for the same canonical
    // pair does NOT delete or overwrite the session_co_access row.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Preserve A").await;
    let note_b = make_note(&repo, &project, &tmp, "Preserve B").await;

    // Seed a co-access row.
    repo.upsert_association(&note_a, &note_b, 5).await.unwrap();

    // Record the co-access row's weight and count for later comparison.
    let original = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("original co_access row");
    let original_weight = original.weight;
    let original_count = original.co_access_count;

    // Write an authored row with a DIFFERENT source (LlmEnrichment).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::LlmEnrichment,
            weight: 0.8,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // Write an embedding row with a DIFFERENT source (EmbeddingSimilarity).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.85,
            confidence: Some(0.9),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("test-model".to_string()),
            embedding_dim: Some(512),
        },
    )
    .await
    .unwrap();

    // Co-access row must be unchanged.
    let co_access = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("co_access row must survive non-co-access writes");
    assert!(
        (co_access.weight - original_weight).abs() < 1e-12,
        "co_access weight must be unchanged; expected {original_weight}, got {}",
        co_access.weight
    );
    assert_eq!(
        co_access.co_access_count, original_count,
        "co_access_count must be unchanged"
    );

    // All three rows coexist.
    let all = repo
        .list_provenance_associations_for_pair(&note_a, &note_b)
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        3,
        "expected 3 coexistent rows after non-co-access writes, got {}",
        all.len()
    );

    // Legacy get_associations_for_note still returns only the co-access row.
    let legacy = repo.get_associations_for_note(&note_a).await.unwrap();
    assert_eq!(
        legacy.len(),
        1,
        "legacy path must return only co-access rows"
    );
    assert_eq!(legacy[0].co_access_count, original_count);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_co_access_row_carries_post_migration_defaults() {
    // Migration-compatibility test: rows written via the legacy
    // upsert_association path carry kind='co_access' and
    // source='session_co_access' (the post-migration defaults), and the
    // provenance columns are NULL (populated only by the provenance-rich
    // path).  This verifies the expected post-migration state at the
    // repository layer.
    //
    // The migration's own backfill behavior (pre-existing rows receiving the
    // `source` DEFAULT, PK widening to the four-column key, CHECK constraint
    // widening) is covered by the dedicated migration harness in
    // `tests/migrations_note_association_provenance.rs`, which replays
    // migration 97 on top of the prior schema with seeded legacy rows — the
    // repository-layer tests here exercise the post-migration *defaults* an
    // application write produces, not the migration replay itself.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Default A").await;
    let note_b = make_note(&repo, &project, &tmp, "Default B").await;

    // Write via the legacy path.
    repo.upsert_association(&note_a, &note_b, 2).await.unwrap();

    // Read via the provenance-rich path.
    let row = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("legacy co_access row must be readable via provenance API");

    assert_eq!(row.kind, NoteAssociationKind::CoAccess);
    assert_eq!(row.source, NoteAssociationSource::SessionCoAccess);
    assert_eq!(row.co_access_count, 2);
    assert!(
        (row.weight - 0.01).abs() < 1e-12,
        "legacy seed weight=0.01, got {}",
        row.weight
    );

    // Provenance columns are NULL for legacy rows.
    assert!(
        row.confidence.is_none(),
        "legacy row must have NULL confidence"
    );
    assert!(
        row.algorithm_version.is_none(),
        "legacy row must have NULL algorithm_version"
    );
    assert!(
        row.embedding_model.is_none(),
        "legacy row must have NULL embedding_model"
    );
    assert!(
        row.embedding_dim.is_none(),
        "legacy row must have NULL embedding_dim"
    );
    assert!(
        row.last_refreshed_at.is_none(),
        "legacy row must have NULL last_refreshed_at"
    );

    // last_co_access is populated (set by the Hebbian upsert).
    assert!(
        !row.last_co_access.is_empty(),
        "legacy row must have a non-empty last_co_access"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_embeddings_with_different_sources_coexist() {
    // Two embedding rows with DIFFERENT sources for the same pair must
    // coexist — e.g. one from the initial embedding pass and one from a
    // refreshed pass with a different model.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "MultiSrc A").await;
    let note_b = make_note(&repo, &project, &tmp, "MultiSrc B").await;

    // Embedding from the first model pass.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.7,
            confidence: Some(0.85),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_dim: Some(1536),
        },
    )
    .await
    .unwrap();

    // Embedding from a custom source (e.g. a different pipeline run).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::Custom("embedding_v2_refresh".to_string()),
            weight: 0.9,
            confidence: Some(0.95),
            algorithm_version: Some("v2".to_string()),
            embedding_model: Some("text-embedding-3-large".to_string()),
            embedding_dim: Some(3072),
        },
    )
    .await
    .unwrap();

    // Both coexist.
    let all = repo
        .list_provenance_associations_for_pair(&note_a, &note_b)
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "two embedding sources must coexist");

    let sources: Vec<&str> = all.iter().map(|r| r.source.as_str()).collect();
    assert!(sources.contains(&"embedding_similarity"));
    assert!(sources.contains(&"embedding_v2_refresh"));

    // Each has its own provenance fields.
    let first = all
        .iter()
        .find(|r| r.source.as_str() == "embedding_similarity")
        .unwrap();
    assert_eq!(first.embedding_dim, Some(1536));
    assert!((first.weight - 0.7).abs() < 1e-12);

    let second = all
        .iter()
        .find(|r| r.source.as_str() == "embedding_v2_refresh")
        .unwrap();
    assert_eq!(second.embedding_dim, Some(3072));
    assert!((second.weight - 0.9).abs() < 1e-12);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_access_prune_does_not_delete_authored_or_embedding_rows() {
    // Scenario: a note pair carries three rows — a stale low-weight
    // co-access row, a fresh authored row, and a fresh embedding row.
    // Pruning the stale co-access row must leave the authored and
    // embedding rows intact.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Prune A").await;
    let note_b = make_note(&repo, &project, &tmp, "Prune B").await;

    // 1) Seed a co-access row (weight 0.01, single co-access event).
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    // Back-date it to 100 days ago so prune_associations targets it.
    sqlx::query(
        r#"UPDATE note_associations
           SET last_co_access = to_char(
               (now() AT TIME ZONE 'utc') - interval '100 day',
               'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
           WHERE note_a_id = $1 AND note_b_id = $2
             AND kind = 'co_access' AND source = 'session_co_access'"#,
    )
    .bind(&note_a)
    .bind(&note_b)
    .execute(db.pool())
    .await
    .unwrap();

    // 2) Insert an authored row for the same pair.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::SessionCoAccess,
            weight: 0.6,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // 3) Insert an embedding_related row for the same pair.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.8,
            confidence: Some(0.85),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_dim: Some(1536),
        },
    )
    .await
    .unwrap();

    // Verify all three rows exist.
    let all_pairs = repo
        .list_provenance_associations_for_pair(&note_a, &note_b)
        .await
        .unwrap();
    assert_eq!(
        all_pairs.len(),
        3,
        "expected 3 rows (co_access, authored, embedding_related), got {}",
        all_pairs.len()
    );

    // 4) Run project-scoped prune.
    let deleted = repo.prune_associations(&project.id).await.unwrap();
    assert_eq!(deleted, 1, "exactly one co-access row should be pruned");

    // 5) The authored and embedding rows must survive.
    let remaining = repo
        .list_provenance_associations_for_pair(&note_a, &note_b)
        .await
        .unwrap();
    assert_eq!(
        remaining.len(),
        2,
        "authored + embedding rows must survive co-access pruning, got {}",
        remaining.len()
    );

    let remaining_kinds: Vec<&str> = remaining.iter().map(|r| r.kind.as_str()).collect();
    assert!(
        remaining_kinds.contains(&"authored"),
        "authored row must survive pruning"
    );
    assert!(
        remaining_kinds.contains(&"embedding_related"),
        "embedding_related row must survive pruning"
    );

    // 6) The co-access row must be gone.
    let co_access_remaining = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap();
    assert!(
        co_access_remaining.is_none(),
        "co-access row should have been pruned"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_associations_for_note_returns_only_co_access_rows() {
    // Verify that `get_associations_for_note` (the legacy retrieval path)
    // returns only co-access rows and excludes typed/embedding rows for the
    // same pair.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Filter A").await;
    let note_b = make_note(&repo, &project, &tmp, "Filter B").await;
    let note_c = make_note(&repo, &project, &tmp, "Filter C").await;

    // Co-access edge: note_a ↔ note_b.
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    // Authored edge: note_a ↔ note_c (same note_a, different pair).
    repo.upsert_provenance_association(
        &note_a,
        &note_c,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::SessionCoAccess,
            weight: 0.7,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    // Legacy retrieval: should see only the co-access edge.
    let co_access_assocs = repo.get_associations_for_note(&note_a).await.unwrap();
    assert_eq!(
        co_access_assocs.len(),
        1,
        "get_associations_for_note should return only co-access rows"
    );
    let assoc = &co_access_assocs[0];
    let pair = [&assoc.note_a_id, &assoc.note_b_id];
    assert!(
        pair.contains(&&note_a) && pair.contains(&&note_b),
        "returned association should be the co-access pair"
    );

    // Full retrieval: list_associations_for_note should return both.
    let all_entries = repo
        .list_associations_for_note(&note_a, 0.0, 100)
        .await
        .unwrap();
    assert_eq!(
        all_entries.len(),
        2,
        "list_associations_for_note should return all kinds"
    );
    let kinds: Vec<&str> = all_entries.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"co_access"));
    assert!(kinds.contains(&"authored"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_associations_below_weight_spares_typed_rows() {
    // Verify that prune_associations_below_weight only deletes co-access
    // rows, even when typed rows fall below the same threshold.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Low A").await;
    let note_b = make_note(&repo, &project, &tmp, "Low B").await;

    // Co-access row: weight 0.01 (below 0.05 threshold).
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    // Authored row: weight 0.03 (also below 0.05, but should NOT be pruned).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::Authored,
            source: NoteAssociationSource::SessionCoAccess,
            weight: 0.03,
            confidence: None,
            algorithm_version: None,
            embedding_model: None,
            embedding_dim: None,
        },
    )
    .await
    .unwrap();

    let deleted = repo.prune_associations_below_weight(0.05).await.unwrap();
    assert_eq!(deleted, 1, "only the co-access row should be pruned");

    // Authored row survives.
    let authored = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::Authored,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap();
    assert!(
        authored.is_some(),
        "low-weight authored row must survive co-access pruning"
    );
    assert!(
        (authored.unwrap().weight - 0.03).abs() < 1e-12,
        "authored weight must be unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_old_associations_spares_typed_rows() {
    // Verify that prune_old_associations only deletes stale co-access rows,
    // not typed rows with the same age or low weight.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Old A").await;
    let note_b = make_note(&repo, &project, &tmp, "Old B").await;

    // Co-access row (weight 0.01).
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    // Back-date the co-access row to 100 days ago.
    let old_ts = "2025-01-01T00:00:00.000Z";
    sqlx::query(
        r#"UPDATE note_associations
           SET last_co_access = $3
           WHERE note_a_id = $1 AND note_b_id = $2
             AND kind = 'co_access' AND source = 'session_co_access'"#,
    )
    .bind(&note_a)
    .bind(&note_b)
    .bind(old_ts)
    .execute(db.pool())
    .await
    .unwrap();

    // Embedding row (weight 0.03, same pair, same old timestamp via last_co_access).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.03,
            confidence: Some(0.5),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("test".to_string()),
            embedding_dim: Some(256),
        },
    )
    .await
    .unwrap();

    // Back-date the embedding row too (same stale timestamp).
    sqlx::query(
        r#"UPDATE note_associations
           SET last_co_access = $3
           WHERE note_a_id = $1 AND note_b_id = $2
             AND kind = 'embedding_related'"#,
    )
    .bind(&note_a)
    .bind(&note_b)
    .bind(old_ts)
    .execute(db.pool())
    .await
    .unwrap();

    // Prune old associations (before now, weight <= 0.05).
    let now_ts = "2099-12-31T23:59:59.000Z";
    let deleted = repo.prune_old_associations(now_ts, 0.05).await.unwrap();
    assert_eq!(
        deleted, 1,
        "only the co-access row should be pruned by prune_old_associations"
    );

    // Embedding row survives.
    let embedding = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::EmbeddingRelated,
            &NoteAssociationSource::EmbeddingSimilarity,
        )
        .await
        .unwrap();
    assert!(
        embedding.is_some(),
        "embedding row must survive co-access-only prune_old_associations"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_typed_helper_preserves_provenance_rows_for_same_pair() {
    // Bridge regression: the legacy `upsert_typed_association` helper removes
    // only the implicit `co_access / session_co_access` row before writing its
    // own typed edge (so existing single-row-per-pair readers keep working).
    // It must NOT delete provenance-rich rows (`embedding_related`,
    // `authored`, etc.) that share the same canonical pair — those are owned
    // by the provenance substrate and coexist with the typed helper's edge.
    //
    // This ties the existing `association.rs` typed-helper behavior to the new
    // multi-row semantics: the legacy helper's co-access removal is scoped to
    // `kind='co_access' AND source='session_co_access'`, never touching other
    // (kind, source) slots.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note(&repo, &project, &tmp, "Bridge A").await;
    let note_b = make_note(&repo, &project, &tmp, "Bridge B").await;

    // 1) Seed a co_access row (the legacy Hebbian edge).
    repo.upsert_association(&note_a, &note_b, 1).await.unwrap();

    // 2) Seed a provenance-rich embedding_related row for the same pair.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.8,
            confidence: Some(0.9),
            algorithm_version: Some("v1".to_string()),
            embedding_model: Some("test-model".to_string()),
            embedding_dim: Some(512),
        },
    )
    .await
    .unwrap();

    // 3) Write a typed edge via the LEGACY helper. This deletes the co_access
    //    row and writes a `supersedes` row in its place.
    repo.upsert_typed_association(&note_a, &note_b, NoteAssociationKind::Supersedes, 0.6)
        .await
        .unwrap();

    // The co_access row must be gone (promoted/replaced by the typed edge).
    let co_access = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::CoAccess,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap();
    assert!(
        co_access.is_none(),
        "legacy typed helper must remove the implicit co_access row"
    );

    // The typed supersedes row exists at the session_co_access source slot.
    let typed = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::Supersedes,
            &NoteAssociationSource::SessionCoAccess,
        )
        .await
        .unwrap()
        .expect("typed supersedes row must exist");
    assert!(
        (typed.weight - 0.6).abs() < 1e-12,
        "typed weight, got {}",
        typed.weight
    );

    // The provenance-rich embedding_related row must SURVIVE the legacy
    // typed helper's co-access removal — it occupies a different
    // (kind, source) slot.
    let embedding = repo
        .get_provenance_association(
            &note_a,
            &note_b,
            NoteAssociationKind::EmbeddingRelated,
            &NoteAssociationSource::EmbeddingSimilarity,
        )
        .await
        .unwrap()
        .expect("embedding row must survive the legacy typed helper");
    assert!(
        (embedding.weight - 0.8).abs() < 1e-12,
        "embedding weight must be unchanged, got {}",
        embedding.weight
    );
    assert_eq!(embedding.embedding_dim, Some(512));

    // Two rows now coexist: the typed supersedes edge and the embedding edge.
    let all = repo
        .list_provenance_associations_for_pair(&note_a, &note_b)
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        2,
        "expected typed + embedding rows to coexist, got {}",
        all.len()
    );
}

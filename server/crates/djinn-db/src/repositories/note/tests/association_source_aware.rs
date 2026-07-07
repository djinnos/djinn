use djinn_core::models::Project;
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

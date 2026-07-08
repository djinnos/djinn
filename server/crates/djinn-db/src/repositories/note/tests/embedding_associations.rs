//! Regression and performance tests for the embedding association
//! refresh/prune algorithm (`embedding_associations.rs`).
//!
//! Covers threshold filtering, top-k cap, weight formula, provenance
//! metadata round-trip, prune-on-archive, prune-on-model/dim mismatch,
//! prune-below-threshold, prune-top-k-overflow, idempotence, and bounded
//! complexity.
//!
//! Uses a [`MockNoteVectorStore`] to inject controlled cosine similarities
//! into the `query_embedding_candidates` path so the full
//! `refresh_embedding_associations` algorithm can be tested end-to-end
//! against the in-memory Postgres.
//!
//! Test-only: Instant::now is used for timing assertions in the bounded
//! complexity test.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;

use djinn_core::events::DjinnEventEnvelope;
use tokio::sync::broadcast;

use super::*;
use crate::database::Database;
use crate::repositories::note::embedding_associations::{
    EMBEDDING_ASSOCIATION_ALGORITHM_VERSION, EMBEDDING_ASSOCIATION_CANDIDATE_POOL,
    EMBEDDING_ASSOCIATION_THRESHOLD, EMBEDDING_ASSOCIATION_TOP_K,
};
use crate::repositories::note::embeddings::{
    EmbeddingQueryContext, NoteEmbeddingMatch, NoteVectorBackend, NoteVectorStore,
    UpsertNoteEmbedding,
};
use crate::repositories::note::{
    NoteAssociationKind, NoteAssociationProvenanceUpsert, NoteAssociationSource, NoteRepository,
};
use crate::repositories::test_support::{event_bus_for, make_project};

// ── Mock vector store ──────────────────────────────────────────────────

/// A `NoteVectorStore` implementation that returns pre-configured
/// `NoteEmbeddingMatch` results keyed by the first element of the
/// query embedding (packed as a note-id string via a side-channel).
///
/// In practice, [`NoteRepository::query_embedding_candidates`] reads
/// the query note's vector from `note_embeddings`, passes it to
/// `query_similar_embeddings`, and converts the raw distance to cosine
/// similarity based on the backend.  We set `backend = Qdrant` so the
/// conversion is `similarity = -distance`.
struct MockNoteVectorStore {
    /// `note_id` → list of (candidate_note_id, raw_distance).
    /// `query_similar_embeddings` ignores the actual query vector; it
    /// looks up the vector-store matches by a note-id key that we
    /// pack into the first float of the query embedding.
    matches: std::sync::Mutex<HashMap<String, Vec<NoteEmbeddingMatch>>>,
}

impl MockNoteVectorStore {
    fn new() -> Self {
        Self {
            matches: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Configure the mock to return `candidates` when
    /// `query_similar_embeddings` is called on behalf of `note_id`.
    ///
    /// `candidates` is a list of `(candidate_note_id, cosine_similarity)`.
    /// Because the backend is Qdrant (`similarity = -distance`), the
    /// mock stores `distance = -cosine_similarity`.
    fn set_candidates(&self, note_id: &str, candidates: Vec<(&str, f64)>) {
        let matches = candidates
            .into_iter()
            .map(|(nid, cosine)| NoteEmbeddingMatch {
                note_id: nid.to_string(),
                distance: -cosine, // Qdrant convention
            })
            .collect();
        self.matches
            .lock()
            .unwrap()
            .insert(note_id.to_string(), matches);
    }
}

#[async_trait::async_trait]
impl NoteVectorStore for MockNoteVectorStore {
    fn backend(&self) -> NoteVectorBackend {
        NoteVectorBackend::Qdrant
    }

    async fn can_index(&self, _repo: &NoteRepository) -> crate::error::DbResult<bool> {
        Ok(true)
    }

    async fn upsert_embedding(
        &self,
        repo: &NoteRepository,
        input: UpsertNoteEmbedding<'_>,
    ) -> crate::error::DbResult<NoteEmbeddingRecord> {
        // Delegate to the same metadata insert that Noop uses.
        super::super::embeddings::NoopNoteVectorStore
            .upsert_embedding(repo, input)
            .await
    }

    async fn delete_embedding(
        &self,
        repo: &NoteRepository,
        note_id: &str,
    ) -> crate::error::DbResult<()> {
        super::super::embeddings::NoopNoteVectorStore
            .delete_embedding(repo, note_id)
            .await
    }

    async fn query_similar_embeddings(
        &self,
        _repo: &NoteRepository,
        query_embedding: &[f32],
        _query: EmbeddingQueryContext<'_>,
        limit: usize,
    ) -> crate::error::DbResult<Vec<NoteEmbeddingMatch>> {
        // The note_id is encoded as the first float of the embedding.
        // (We use `f32::to_bits()` in the test helper below.)
        let note_id = note_id_from_embedding(query_embedding);
        let guard = self.matches.lock().unwrap();
        Ok(guard
            .get(&note_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(limit)
            .collect())
    }
}

// ── Test helpers ───────────────────────────────────────────────────────

/// Encode a note-id into the first element of a dummy embedding vector.
/// The mock store reverses this to dispatch to the right candidate set.
fn embedding_for_note(note_id: &str) -> Vec<f32> {
    // Use a hash of the note_id as the first float element so we can
    // recover it in `note_id_from_embedding`.  The rest of the vector
    // is unit-scaled so the actual vector is valid for the DB.
    let hash = note_id_hash(note_id);
    let mut v = vec![0.0f32; 384];
    v[0] = f32::from_bits(hash);
    // Normalize to unit length (cosine similarity is scale-invariant
    // but we need a valid vector for the blob conversion).
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn note_id_hash(note_id: &str) -> u32 {
    // Simple FNV-1a hash (32-bit) – deterministic and cheap.
    let mut h: u32 = 0x811c_9dc5;
    for b in note_id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn note_id_from_embedding(embedding: &[f32]) -> String {
    // Reverse: the first element's bit pattern was set by
    // `embedding_for_note`.  We recover the hash and then look it up
    // in a pre-built reverse map.
    //
    // However, this is fragile.  Instead we use a simpler scheme:
    // store the note-id's first 4 bytes in the first 4 floats as
    // their byte values (normalized out).
    //
    // **Simpler approach**: embed the hash as a u32 in `v[0]` and
    // recover it.  Then we look up via a thread-local reverse map.
    // But `query_similar_embeddings` doesn't have access to the
    // note_id in a clean way...
    //
    // Actually, re-reading `query_embedding_candidates`: the
    // `query_embedding` is the raw vector from `note_embeddings`.
    // We *could* encode the note_id as the first N bytes, but that's
    // fragile.
    //
    // **Best approach**: override `query_similar_embeddings` to
    // accept `note_id` as a parameter.  But the trait doesn't have
    // it.  So we encode the note_id in the vector itself.
    //
    // We'll use a thread-local lookup table:
    // `note_id_from_embedding` recovers the hash from `v[0]` and
    // uses EMBEDDING_NOTE_ID_MAP to find the original note_id.
    let bits = embedding[0].to_bits();
    EMBEDDING_NOTE_ID_MAP.with(|map| {
        let map = map.borrow();
        map.get(&bits).cloned().unwrap_or_default()
    })
}

use std::cell::RefCell;

thread_local! {
    static EMBEDDING_NOTE_ID_MAP: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
}

/// Create a note with a controlled embedding vector.
///
/// Returns `(note_id, embedding)`.
async fn make_note_with_embedding(
    db: &Database,
    repo: &NoteRepository,
    project_id: &str,
    title: &str,
    model: &str,
    dim: i32,
) -> String {
    let note = repo
        .create(project_id, title, "content", "reference", "[]")
        .await
        .unwrap();
    let note_id = &note.id;

    let embedding = embedding_for_note(note_id);

    // Register the hash → note_id mapping for the mock store.
    let hash = note_id_hash(note_id);
    EMBEDDING_NOTE_ID_MAP.with(|map| {
        map.borrow_mut().insert(hash, note_id.clone());
    });

    // Insert embedding metadata directly.
    let content_hash = format!("hash-{note_id}");
    insert_embedding_data(db, note_id, &embedding, &content_hash, model, dim).await;

    note_id.clone()
}

/// Directly insert a note embedding into the database tables.
async fn insert_embedding_data(
    db: &Database,
    note_id: &str,
    embedding: &[f32],
    content_hash: &str,
    model: &str,
    dim: i32,
) {
    let blob = embedding_to_blob_helper(embedding);
    sqlx::query(
        r#"INSERT INTO note_embeddings (note_id, embedding, embedding_dim, updated_at)
         VALUES ($1, $2, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
         ON CONFLICT (note_id) DO UPDATE SET
             embedding = EXCLUDED.embedding,
             embedding_dim = EXCLUDED.embedding_dim,
             updated_at = EXCLUDED.updated_at"#,
    )
    .bind(note_id)
    .bind(&blob)
    .bind(dim)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(
        r#"INSERT INTO note_embedding_meta (
            note_id, content_hash, embedded_at, model_version, embedding_dim, extension_state, branch
         ) VALUES (
            $1, $2, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), $3, $4, 'ready', 'main'
         )
         ON CONFLICT (note_id) DO UPDATE SET
            content_hash = EXCLUDED.content_hash,
            embedded_at = EXCLUDED.embedded_at,
            model_version = EXCLUDED.model_version,
            embedding_dim = EXCLUDED.embedding_dim,
            extension_state = EXCLUDED.extension_state,
            branch = EXCLUDED.branch"#,
    )
    .bind(note_id)
    .bind(content_hash)
    .bind(model)
    .bind(dim)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Convert a `&[f32]` to the little-endian byte blob expected by
/// `note_embeddings.embedding`.
fn embedding_to_blob_helper(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for &v in embedding {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob
}

/// Build a repo with the mock vector store.
fn make_repo_with_mock(
    db: &Database,
    mock: Arc<MockNoteVectorStore>,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> NoteRepository {
    NoteRepository::new(db.clone(), event_bus_for(tx)).with_vector_store(Some(mock))
}

/// Count `embedding_related` / `embedding_similarity` rows for a note.
async fn count_embedding_edges(db: &Database, note_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM note_associations
         WHERE kind = 'embedding_related' AND source = 'embedding_similarity'
           AND (note_a_id = $1 OR note_b_id = $1)",
    )
    .bind(note_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

/// Archive a note (set status to 'archived').
async fn archive_note(db: &Database, note_id: &str) {
    sqlx::query("UPDATE notes SET status = 'archived' WHERE id = $1")
        .bind(note_id)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Compute the expected weight for a given cosine similarity.
fn expected_weight(cosine: f64) -> f64 {
    (0.05_f64).max(
        ((cosine - EMBEDDING_ASSOCIATION_THRESHOLD) / (1.0 - EMBEDDING_ASSOCIATION_THRESHOLD)
            * 0.30
            + 0.05)
            .min(0.35),
    )
}

/// Build a standard test repo, project, and mock for the refresh tests.
async fn setup_refresh_test(
    db: &Database,
) -> (
    tempfile::TempDir,
    djinn_core::models::Project,
    Arc<MockNoteVectorStore>,
    broadcast::Sender<DjinnEventEnvelope>,
) {
    let tmp = crate::database::test_tempdir().unwrap();
    let project = make_project(db, tmp.path()).await;
    let mock = Arc::new(MockNoteVectorStore::new());
    let (tx, _rx) = broadcast::channel(256);
    (tmp, project, mock, tx)
}

// ── Test cases ─────────────────────────────────────────────────────────

// 1. Threshold filtering — candidates below cosine 0.78 are NOT upserted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_threshold_filtering() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let note_a =
        make_note_with_embedding(&db, &repo, &project.id, "Note A", "test-model", 384).await;
    let note_b =
        make_note_with_embedding(&db, &repo, &project.id, "Note B", "test-model", 384).await;
    let note_c =
        make_note_with_embedding(&db, &repo, &project.id, "Note C", "test-model", 384).await;

    // note_a → note_b: above threshold (0.85)
    // note_a → note_c: below threshold (0.70)
    mock.set_candidates(&note_a, vec![(&note_b, 0.85), (&note_c, 0.70)]);
    // note_b needs candidates too (can be empty for this test)
    mock.set_candidates(&note_b, vec![(&note_a, 0.85)]);
    // note_c needs candidates too
    mock.set_candidates(&note_c, vec![]);

    let stats = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // note_a should have exactly 1 edge (note_b above threshold).
    assert_eq!(count_embedding_edges(&db, &note_a).await, 1);
    // note_b should also have 1 edge (the note_a ↔ note_b edge).
    assert_eq!(count_embedding_edges(&db, &note_b).await, 1);
    // note_c should have 0 edges (note_a → note_c was below threshold).
    assert_eq!(count_embedding_edges(&db, &note_c).await, 0);
    // Stats: 2 edges upserted (note_a→note_b and note_b→note_a; but
    // canonical pair means it's the same row counted once per refresh).
    // Actually, note_a generates 1 edge, note_b generates 1 edge
    // (but it's the same canonical pair, so max-merge keeps 1 row).
    // note_c generates 0.
    assert_eq!(
        stats.edges_upserted, 2,
        "stats should count each upsert call"
    );
    assert_eq!(stats.notes_scanned, 3);
}

// 2. Top-k cap — exactly 8 (or fewer) edges per note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_top_k_cap() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let note_a =
        make_note_with_embedding(&db, &repo, &project.id, "Hub Note", "test-model", 384).await;

    // Create 12 candidate notes, all above threshold.
    let mut candidate_ids = Vec::new();
    for i in 0..12 {
        let nid = make_note_with_embedding(
            &db,
            &repo,
            &project.id,
            &format!("Candidate {i}"),
            "test-model",
            384,
        )
        .await;
        candidate_ids.push(nid);
    }

    // Assign descending similarities so we can verify the top 8 survive.
    // Cosine values: 0.99, 0.98, ..., 0.88
    let candidates: Vec<(&str, f64)> = candidate_ids
        .iter()
        .enumerate()
        .map(|(i, nid)| (nid.as_str(), 0.99 - i as f64 * 0.01))
        .collect();
    mock.set_candidates(&note_a, candidates);

    // Set empty candidates for all candidate notes so their refresh
    // doesn't create extra edges.
    for nid in &candidate_ids {
        mock.set_candidates(nid, vec![]);
    }

    let stats = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // Note: the top-K enforcement CTE runs after each note's upserts.
    // It ranks ALL embedding_related edges touching the note and
    // deletes rows ranked beyond K for either endpoint.
    //
    // The CTE enforces K on both note_a_id and note_b_id partitions.
    // For note_a (hub), it keeps at most 8 edges where note_a is
    // either note_a_id or note_b_id.
    let edge_count = count_embedding_edges(&db, &note_a).await;
    assert!(
        edge_count <= EMBEDDING_ASSOCIATION_TOP_K as i64,
        "expected <= {} edges for hub note, got {edge_count}",
        EMBEDDING_ASSOCIATION_TOP_K,
    );

    // Verify the retained edges are the top-8 by cosine similarity.
    let rows = repo
        .list_provenance_associations_for_note(
            &note_a,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();

    // The retained edges should be the ones with highest confidence.
    let mut confidences: Vec<f64> = rows.iter().map(|r| r.confidence.unwrap_or(0.0)).collect();
    confidences.sort_by(|a, b| b.partial_cmp(a).unwrap());

    // Top confidence should be 0.99 (the highest we assigned).
    if !confidences.is_empty() {
        assert!(
            (confidences[0] - 0.99).abs() < 1e-6,
            "top confidence should be 0.99, got {}",
            confidences[0]
        );
    }

    // All retained confidences should be >= the first excluded one.
    // The 9th candidate (index 8) had cosine = 0.91. So all retained
    // confidences should be >= 0.91.
    for c in &confidences {
        assert!(
            *c >= 0.91 - 1e-6,
            "retained confidence {c} should be >= 0.91 (the 9th candidate's cosine)"
        );
    }

    assert!(stats.notes_scanned >= 1);
}

// 3. Weight formula correctness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_weight_formula_correctness() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    // Create a hub note and 4 candidates with specific cosine values.
    let hub =
        make_note_with_embedding(&db, &repo, &project.id, "Weight Hub", "test-model", 384).await;

    let cosine_values = [0.78, 0.80, 0.90, 1.00];
    let mut candidate_ids = Vec::new();
    for (i, &cosine) in cosine_values.iter().enumerate() {
        let nid = make_note_with_embedding(
            &db,
            &repo,
            &project.id,
            &format!("Weight Candidate {cosine:.2}"),
            "test-model",
            384,
        )
        .await;
        candidate_ids.push((nid, cosine));
        let _ = i;
    }

    let candidates: Vec<(&str, f64)> = candidate_ids
        .iter()
        .map(|(nid, cosine)| (nid.as_str(), *cosine))
        .collect();
    mock.set_candidates(&hub, candidates);

    // Empty candidates for the candidate notes.
    for (nid, _) in &candidate_ids {
        mock.set_candidates(nid, vec![]);
    }

    repo.refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // Read back the edges and verify weights.
    let rows = repo
        .list_provenance_associations_for_note(
            &hub,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        4,
        "expected 4 edges for 4 above-threshold candidates"
    );

    for row in &rows {
        let confidence = row.confidence.unwrap_or(0.0);
        let expected = expected_weight(confidence);
        assert!(
            (row.weight - expected).abs() < 1e-6,
            "weight mismatch for confidence {confidence:.2}: expected {expected:.6}, got {:.6}",
            row.weight
        );
    }

    // Verify specific boundary values.
    let row_078 = rows
        .iter()
        .find(|r| (r.confidence.unwrap() - 0.78).abs() < 1e-6);
    if let Some(row) = row_078 {
        assert!(
            (row.weight - 0.05).abs() < 1e-6,
            "cosine=0.78 should yield weight=0.05, got {}",
            row.weight
        );
    }

    let row_100 = rows
        .iter()
        .find(|r| (r.confidence.unwrap() - 1.00).abs() < 1e-6);
    if let Some(row) = row_100 {
        assert!(
            (row.weight - 0.35).abs() < 1e-6,
            "cosine=1.00 should yield weight=0.35, got {}",
            row.weight
        );
    }
}

// 4. Provenance metadata round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_provenance_metadata_round_trip() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let note_a =
        make_note_with_embedding(&db, &repo, &project.id, "Prov A", "my-embedding-v2", 768).await;
    let note_b =
        make_note_with_embedding(&db, &repo, &project.id, "Prov B", "my-embedding-v2", 768).await;

    mock.set_candidates(&note_a, vec![(&note_b, 0.92)]);
    mock.set_candidates(&note_b, vec![(&note_a, 0.92)]);

    repo.refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // Read back via list_provenance_associations_for_note.
    let rows = repo
        .list_provenance_associations_for_note(
            &note_a,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1, "expected exactly 1 embedding_related edge");
    let row = &rows[0];

    // confidence = cosine similarity
    assert!(
        (row.confidence.unwrap() - 0.92).abs() < 1e-6,
        "confidence: {:?}",
        row.confidence
    );
    // algorithm_version
    assert_eq!(
        row.algorithm_version.as_deref(),
        Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION),
        "algorithm_version"
    );
    // embedding_model = the source note's model_version
    assert_eq!(
        row.embedding_model.as_deref(),
        Some("my-embedding-v2"),
        "embedding_model"
    );
    // embedding_dim = the source note's embedding_dim
    assert_eq!(row.embedding_dim, Some(768), "embedding_dim");
    // last_refreshed_at is populated
    assert!(
        row.last_refreshed_at.is_some(),
        "last_refreshed_at must be set"
    );
    let refreshed = row.last_refreshed_at.as_ref().unwrap();
    assert!(
        refreshed.ends_with('Z'),
        "last_refreshed_at must be UTC: {refreshed}"
    );
}

// 5. Prune on archive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_embedding_edges_on_archive() {
    let db = Database::open_in_memory().unwrap();
    let tmp = crate::database::test_tempdir().unwrap();
    let project = make_project(&db, tmp.path()).await;
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note_with_embedding(&db, &repo, &project.id, "Prune A", "m", 384).await;
    let note_b = make_note_with_embedding(&db, &repo, &project.id, "Prune B", "m", 384).await;
    let note_c = make_note_with_embedding(&db, &repo, &project.id, "Prune C", "m", 384).await;

    // Seed edges: a↔b and a↔c (both embedding_related).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.8,
            confidence: Some(0.90),
            algorithm_version: Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION.to_owned()),
            embedding_model: Some("m".to_owned()),
            embedding_dim: Some(384),
        },
    )
    .await
    .unwrap();

    repo.upsert_provenance_association(
        &note_a,
        &note_c,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.7,
            confidence: Some(0.85),
            algorithm_version: Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION.to_owned()),
            embedding_model: Some("m".to_owned()),
            embedding_dim: Some(384),
        },
    )
    .await
    .unwrap();

    // Verify both edges exist.
    assert_eq!(count_embedding_edges(&db, &note_a).await, 2);
    assert_eq!(count_embedding_edges(&db, &note_b).await, 1);
    assert_eq!(count_embedding_edges(&db, &note_c).await, 1);

    // Archive note_b.
    archive_note(&db, &note_b).await;

    // Prune.
    let deleted = repo
        .prune_embedding_associations(&project.id)
        .await
        .unwrap();
    assert!(
        deleted >= 1,
        "at least 1 row should be pruned for archived note_b"
    );

    // Edge a↔b should be gone.
    assert_eq!(
        count_embedding_edges(&db, &note_b).await,
        0,
        "edges touching archived note_b should be deleted"
    );

    // Edge a↔c should survive.
    assert!(
        count_embedding_edges(&db, &note_c).await >= 1,
        "edges touching active note_c must survive"
    );
}

// 6. Prune on model/dim mismatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_on_model_dim_mismatch() {
    let db = Database::open_in_memory().unwrap();
    let tmp = crate::database::test_tempdir().unwrap();
    let project = make_project(&db, tmp.path()).await;
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note_with_embedding(&db, &repo, &project.id, "Model A", "v1", 384).await;
    let note_b = make_note_with_embedding(&db, &repo, &project.id, "Model B", "v1", 384).await;

    // Seed edge with model "v1" / dim 384.
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.8,
            confidence: Some(0.90),
            algorithm_version: Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION.to_owned()),
            embedding_model: Some("v1".to_owned()),
            embedding_dim: Some(384),
        },
    )
    .await
    .unwrap();

    assert_eq!(count_embedding_edges(&db, &note_a).await, 1);

    // Change note_b's embedding to model "v2" / dim 768.
    let new_embedding = embedding_for_note(&note_b);
    insert_embedding_data(&db, &note_b, &new_embedding, "new-hash", "v2", 768).await;

    // Prune — the stored model/dim on the edge (v1/384) no longer
    // matches note_b's current meta (v2/768).
    let deleted = repo
        .prune_embedding_associations(&project.id)
        .await
        .unwrap();
    assert!(
        deleted >= 1,
        "model/dim mismatch should trigger pruning, deleted={deleted}"
    );

    assert_eq!(
        count_embedding_edges(&db, &note_a).await,
        0,
        "edge with mismatched model/dim should be deleted"
    );
}

// 7. Prune below-threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_below_threshold_confidence() {
    let db = Database::open_in_memory().unwrap();
    let tmp = crate::database::test_tempdir().unwrap();
    let project = make_project(&db, tmp.path()).await;
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note_a = make_note_with_embedding(&db, &repo, &project.id, "Below A", "m", 384).await;
    let note_b = make_note_with_embedding(&db, &repo, &project.id, "Below B", "m", 384).await;

    // Seed an edge with confidence 0.85 (above threshold).
    repo.upsert_provenance_association(
        &note_a,
        &note_b,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.25,
            confidence: Some(0.85),
            algorithm_version: Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION.to_owned()),
            embedding_model: Some("m".to_owned()),
            embedding_dim: Some(384),
        },
    )
    .await
    .unwrap();

    assert_eq!(count_embedding_edges(&db, &note_a).await, 1);

    // Directly lower the stored confidence to below threshold (simulate
    // a re-embed that reduced cosine similarity).
    let (a_id, b_id) = djinn_memory::canonical_pair(&note_a, &note_b);
    sqlx::query(
        r#"UPDATE note_associations
           SET confidence = $3
           WHERE note_a_id = $1 AND note_b_id = $2
             AND kind = 'embedding_related' AND source = 'embedding_similarity'"#,
    )
    .bind(a_id)
    .bind(b_id)
    .bind(0.70) // below 0.78
    .execute(db.pool())
    .await
    .unwrap();

    // Prune.
    let deleted = repo
        .prune_embedding_associations(&project.id)
        .await
        .unwrap();
    assert!(
        deleted >= 1,
        "below-threshold confidence should trigger pruning"
    );

    assert_eq!(
        count_embedding_edges(&db, &note_a).await,
        0,
        "edge with confidence below threshold should be deleted"
    );
}

// 8. Prune top-k overflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_top_k_overflow() {
    let db = Database::open_in_memory().unwrap();
    let tmp = crate::database::test_tempdir().unwrap();
    let project = make_project(&db, tmp.path()).await;
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let hub = make_note_with_embedding(&db, &repo, &project.id, "Overflow Hub", "m", 384).await;

    // Directly insert 12 embedding_related edges for the hub note.
    let mut note_ids = Vec::new();
    for i in 0..12 {
        let nid = make_note_with_embedding(
            &db,
            &repo,
            &project.id,
            &format!("Overflow Leaf {i}"),
            "m",
            384,
        )
        .await;
        note_ids.push(nid);
    }

    // Insert edges with descending confidence.
    for (i, nid) in note_ids.iter().enumerate() {
        let cosine = 0.99 - i as f64 * 0.01;
        let weight = expected_weight(cosine);
        repo.upsert_provenance_association(
            &hub,
            nid,
            &NoteAssociationProvenanceUpsert {
                kind: NoteAssociationKind::EmbeddingRelated,
                source: NoteAssociationSource::EmbeddingSimilarity,
                weight,
                confidence: Some(cosine),
                algorithm_version: Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION.to_owned()),
                embedding_model: Some("m".to_owned()),
                embedding_dim: Some(384),
            },
        )
        .await
        .unwrap();
    }

    // Verify we have 12 edges before pruning.
    assert_eq!(count_embedding_edges(&db, &hub).await, 12);

    // Prune — the top-K overflow condition should trim to 8 per endpoint.
    let deleted = repo
        .prune_embedding_associations(&project.id)
        .await
        .unwrap();
    assert!(
        deleted >= 4,
        "expected at least 4 pruned for 12 → 8 overflow, deleted={deleted}"
    );

    let remaining = count_embedding_edges(&db, &hub).await;
    assert!(
        remaining <= EMBEDDING_ASSOCIATION_TOP_K as i64,
        "expected <= {} edges after overflow prune, got {remaining}",
        EMBEDDING_ASSOCIATION_TOP_K,
    );

    // The surviving edges should be the top-8 by confidence.
    let rows = repo
        .list_provenance_associations_for_note(
            &hub,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();

    let confs: Vec<f64> = rows.iter().map(|r| r.confidence.unwrap_or(0.0)).collect();
    // The top 8 cosine values were 0.99, 0.98, ..., 0.92.
    for c in &confs {
        assert!(
            *c >= 0.92 - 1e-6,
            "surviving confidence {c} should be >= 0.92"
        );
    }
}

// 9. Idempotence — re-running refresh on unchanged notes doesn't
// duplicate rows or churn weights.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_idempotence() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let note_a =
        make_note_with_embedding(&db, &repo, &project.id, "Idem A", "test-model", 384).await;
    let note_b =
        make_note_with_embedding(&db, &repo, &project.id, "Idem B", "test-model", 384).await;

    mock.set_candidates(&note_a, vec![(&note_b, 0.90)]);
    mock.set_candidates(&note_b, vec![(&note_a, 0.90)]);

    // First refresh.
    let stats1 = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();
    assert_eq!(stats1.edges_upserted, 2);

    let rows_after_1 = repo
        .list_provenance_associations_for_note(
            &note_a,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();
    assert_eq!(rows_after_1.len(), 1);
    let weight_after_1 = rows_after_1[0].weight;
    let refreshed_at_1 = rows_after_1[0].last_refreshed_at.clone();

    // Second refresh — unchanged notes.
    let stats2 = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();
    assert_eq!(
        stats2.edges_upserted, 2,
        "second refresh still calls upsert for each eligible note"
    );

    // Still exactly 1 row.
    let rows_after_2 = repo
        .list_provenance_associations_for_note(
            &note_a,
            Some(NoteAssociationKind::EmbeddingRelated),
            Some(&NoteAssociationSource::EmbeddingSimilarity),
            0.0,
            100,
        )
        .await
        .unwrap();
    assert_eq!(
        rows_after_2.len(),
        1,
        "idempotent refresh must not create duplicate rows"
    );

    // Weight unchanged (cosine is the same, no max-merge change).
    assert!(
        (rows_after_2[0].weight - weight_after_1).abs() < 1e-12,
        "weight must not churn: expected {weight_after_1}, got {}",
        rows_after_2[0].weight
    );

    // last_refreshed_at updated (or same if within the same second).
    // The important thing is it's still populated.
    assert!(
        rows_after_2[0].last_refreshed_at.is_some(),
        "last_refreshed_at must remain populated"
    );
    let _ = refreshed_at_1; // suppress unused warning
}

// 10. Bounded complexity — 700 notes, O(n) not O(n²).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_bounded_complexity_700_notes() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    // Create 700 notes with embeddings.
    let mut note_ids = Vec::with_capacity(700);
    for i in 0..700 {
        let nid = make_note_with_embedding(
            &db,
            &repo,
            &project.id,
            &format!("Bench Note {i:04}"),
            "bench-model",
            384,
        )
        .await;
        note_ids.push(nid);
    }

    // For each note, configure at most 50 candidates (the candidate pool).
    // We use the next 50 note_ids (wrapping around) to simulate a
    // nearest-neighbor result.
    for (i, nid) in note_ids.iter().enumerate() {
        let mut candidates = Vec::new();
        for j in 1..=50 {
            let idx = (i + j) % 700;
            // All above threshold to exercise the upsert path.
            let cosine = 0.80 + (j as f64 / 500.0).min(0.19);
            candidates.push((note_ids[idx].as_str(), cosine));
        }
        mock.set_candidates(nid, candidates);
    }

    let start = std::time::Instant::now();
    let stats = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    // AC: notes_scanned == 700.
    assert_eq!(stats.notes_scanned, 700, "must scan all 700 notes");

    // AC: candidates_evaluated <= 700 * 50 (O(n) not O(n²)).
    let max_candidates = 700 * EMBEDDING_ASSOCIATION_CANDIDATE_POOL;
    assert!(
        stats.candidates_evaluated <= max_candidates,
        "candidates_evaluated ({}) must be <= {max_candidates} (700 * {})",
        stats.candidates_evaluated,
        EMBEDDING_ASSOCIATION_CANDIDATE_POOL,
    );

    // AC: completes within a CI-safe bounded window (5 seconds).
    assert!(
        elapsed.as_secs() < 5,
        "700-note refresh must complete in <5s, took {:.2?}",
        elapsed
    );

    // Verify that edges were actually created.
    assert!(
        stats.edges_upserted > 0,
        "expected some edges to be upserted for 700 notes"
    );
}

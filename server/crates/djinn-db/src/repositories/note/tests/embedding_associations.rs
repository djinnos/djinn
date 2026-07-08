//! Regression and performance tests for the embedding association
//! refresh/prune algorithm (`embedding_associations.rs`).
//!
//! Covers threshold filtering, top-k cap with tie-breaking, weight formula,
//! provenance metadata round-trip, prune-on-archive, prune-on-model/dim
//! mismatch, prune-below-threshold, prune-top-k-overflow, idempotence, and
//! bounded complexity.
//!
//! Uses a [`MockNoteVectorStore`] to inject controlled cosine similarities
//! into the `query_embedding_candidates` path so the full
//! `refresh_embedding_associations` algorithm can be tested end-to-end
//! against the in-memory Postgres.
//!
//! # Mock dispatch design
//!
//! The algorithm calls `query_embedding_candidates(note_id, ...)`, which:
//! 1. Reads the raw embedding vector from `note_embeddings` (blob → f32).
//! 2. Passes it to `query_similar_embeddings` on the vector store.
//! 3. The mock needs to know which note the query is for.
//!
//! We encode each note's FNV-1a hash as `f32::from_bits(hash)` in `v[0]`.
//! **No normalization is applied** — the blob round-trip (`to_le_bytes` →
//! `from_le_bytes` → `to_bits`) is an exact identity for any `u32`, so the
//! mock recovers the original hash from `query_embedding[0].to_bits()` and
//! looks up the note_id in a shared `Arc<Mutex<HashMap>>` map (not
//! `thread_local!`), which is safe across the multi-thread Tokio runtime.
//!
//! Test-only: `Instant::now` is used for timing assertions.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use djinn_core::events::DjinnEventEnvelope;
use tokio::sync::broadcast;

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
/// `NoteEmbeddingMatch` results keyed by the query note's identity.
///
/// Dispatch works via an embedding-hash → note-id map stored in shared
/// `Arc<Mutex<>>` state (thread-safe across the multi-thread Tokio runtime).
/// The query embedding's `v[0]` is `f32::from_bits(fnv1a_hash(note_id))`
/// (no normalization), so the mock recovers the hash via
/// `query_embedding[0].to_bits()` and looks up the note_id.
struct MockNoteVectorStore {
    /// `note_id` → list of `NoteEmbeddingMatch` to return.
    matches: std::sync::Mutex<HashMap<String, Vec<NoteEmbeddingMatch>>>,
    /// `fnv1a_hash(note_id) as u32` → note_id, for dispatch recovery.
    hash_to_note_id: std::sync::Mutex<HashMap<u32, String>>,
}

impl MockNoteVectorStore {
    fn new() -> Self {
        Self {
            matches: std::sync::Mutex::new(HashMap::new()),
            hash_to_note_id: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Register the embedding-hash → note-id mapping so the mock can
    /// recover the query note's identity from the raw embedding vector.
    fn register_embedding(&self, hash: u32, note_id: String) {
        self.hash_to_note_id.lock().unwrap().insert(hash, note_id);
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
    ) -> crate::error::DbResult<super::super::embeddings::NoteEmbeddingRecord> {
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
        // Recover the query note_id from the first float's bit pattern.
        let bits = query_embedding[0].to_bits();
        let note_id = self
            .hash_to_note_id
            .lock()
            .unwrap()
            .get(&bits)
            .cloned()
            .unwrap_or_default();
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

/// FNV-1a hash (32-bit) of a note_id string.
fn note_id_hash(note_id: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in note_id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Encode a note-id into the first element of a dummy embedding vector.
///
/// **Critical:** no normalization is applied. The blob round-trip
/// (`to_le_bytes` → `from_le_bytes` → `to_bits`) is an exact identity
/// for any `u32`, so the mock recovers `fnv1a_hash(note_id)` from
/// `v[0].to_bits()`. Normalization would corrupt the bit pattern.
fn embedding_for_note(note_id: &str) -> Vec<f32> {
    let hash = note_id_hash(note_id);
    let mut v = vec![0.0f32; 384];
    v[0] = f32::from_bits(hash);
    v
}

/// Create a note with embedding data in the DB.
///
/// Does NOT register in the mock's dispatch map — use
/// [`make_note_with_mock_embedding`] for refresh tests.
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
    let content_hash = format!("hash-{note_id}");
    insert_embedding_data(db, note_id, &embedding, &content_hash, model, dim).await;

    note_id.clone()
}

/// Create a note with embedding data AND register the embedding-hash →
/// note-id mapping in the mock's dispatch map.
///
/// **Performance note:** Uses direct SQL inserts (`insert_embedding_data`)
/// rather than the mock's `upsert_embedding` delegation path, which would
/// go through `NoopNoteVectorStore.upsert_embedding_metadata()` — 3 DB
/// queries per note. Direct SQL keeps 700-note setup under the 5s CI budget.
async fn make_note_with_mock_embedding(
    db: &Database,
    repo: &NoteRepository,
    mock: &MockNoteVectorStore,
    project_id: &str,
    title: &str,
    model: &str,
    dim: i32,
) -> String {
    let note = repo
        .create(project_id, title, "content", "reference", "[]")
        .await
        .unwrap();
    let note_id = note.id.clone();

    let embedding = embedding_for_note(&note_id);
    let hash = note_id_hash(&note_id);
    mock.register_embedding(hash, note_id.clone());

    let content_hash = format!("hash-{note_id}");
    insert_embedding_data(db, &note_id, &embedding, &content_hash, model, dim).await;

    note_id
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
        make_note_with_mock_embedding(&db, &repo, &mock, &project.id, "Note A", "test-model", 384)
            .await;
    let note_b =
        make_note_with_mock_embedding(&db, &repo, &mock, &project.id, "Note B", "test-model", 384)
            .await;
    let note_c =
        make_note_with_mock_embedding(&db, &repo, &mock, &project.id, "Note C", "test-model", 384)
            .await;

    // note_a → note_b: above threshold (0.85)
    // note_a → note_c: below threshold (0.70)
    mock.set_candidates(&note_a, vec![(&note_b, 0.85), (&note_c, 0.70)]);
    mock.set_candidates(&note_b, vec![(&note_a, 0.85)]);
    mock.set_candidates(&note_c, vec![]);

    let stats = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // note_a should have exactly 1 edge (note_b above threshold).
    assert_eq!(
        count_embedding_edges(&db, &note_a).await,
        1,
        "note_a should have 1 edge (only note_b passes threshold)"
    );
    // note_b should also have 1 edge (the note_a ↔ note_b edge).
    assert_eq!(
        count_embedding_edges(&db, &note_b).await,
        1,
        "note_b should have 1 edge"
    );
    // note_c should have 0 edges (note_a → note_c was below threshold).
    assert_eq!(
        count_embedding_edges(&db, &note_c).await,
        0,
        "note_c should have 0 edges (below-threshold candidate excluded)"
    );

    // Verify the single retained edge has confidence 0.85.
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
    assert_eq!(rows.len(), 1);
    assert!(
        (rows[0].confidence.unwrap() - 0.85).abs() < 1e-6,
        "retained edge confidence should be 0.85"
    );

    // candidates_evaluated counts ALL candidates before threshold
    // filtering: note_a had 2, note_b had 1, note_c had 0 → 3.
    assert_eq!(stats.candidates_evaluated, 3);
    assert_eq!(stats.notes_scanned, 3);
}

// 2. Top-k cap — exactly 8 edges per note, with tie-breaking by note_id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_top_k_cap() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let hub = make_note_with_mock_embedding(
        &db,
        &repo,
        &mock,
        &project.id,
        "Hub Note",
        "test-model",
        384,
    )
    .await;

    // Create 15 candidate notes:
    //   5 at cosine 0.95 (tier 1 — all retained)
    //   5 at cosine 0.90 (tier 2 — all SAME cosine; tests note_id ASC tie-breaking)
    //   5 at cosine 0.85 (tier 3 — none retained)
    //
    // After refresh, the top-8 by (cosine DESC, note_id ASC) should be:
    //   all 5 from tier 1 + the 3 tier-2 notes with the LOWEST note_ids
    //   (deterministic tie-breaking), none from tier 3.

    let mut tier1_ids = Vec::new();
    let mut tier2_ids = Vec::new();
    let mut tier3_ids = Vec::new();

    for i in 0..5 {
        let nid = make_note_with_mock_embedding(
            &db,
            &repo,
            &mock,
            &project.id,
            &format!("Tier1-{i}"),
            "test-model",
            384,
        )
        .await;
        tier1_ids.push(nid);
    }
    for i in 0..5 {
        let nid = make_note_with_mock_embedding(
            &db,
            &repo,
            &mock,
            &project.id,
            &format!("Tier2-{i}"),
            "test-model",
            384,
        )
        .await;
        tier2_ids.push(nid);
    }
    for i in 0..5 {
        let nid = make_note_with_mock_embedding(
            &db,
            &repo,
            &mock,
            &project.id,
            &format!("Tier3-{i}"),
            "test-model",
            384,
        )
        .await;
        tier3_ids.push(nid);
    }

    // Configure mock: hub gets all 15 candidates with their cosine values.
    let mut candidates: Vec<(&str, f64)> = Vec::new();
    for nid in &tier1_ids {
        candidates.push((nid.as_str(), 0.95));
    }
    for nid in &tier2_ids {
        // All tier-2 candidates share the SAME cosine (0.90) so that
        // sorting falls through to note_id ASC tie-breaking.
        candidates.push((nid.as_str(), 0.90));
    }
    for nid in &tier3_ids {
        candidates.push((nid.as_str(), 0.85));
    }
    mock.set_candidates(&hub, candidates);

    // Empty candidates for all candidate notes.
    for nid in tier1_ids
        .iter()
        .chain(tier2_ids.iter())
        .chain(tier3_ids.iter())
    {
        mock.set_candidates(nid, vec![]);
    }

    let _stats = repo
        .refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // Assert EXACTLY 8 edges for the hub.
    let edge_count = count_embedding_edges(&db, &hub).await;
    assert_eq!(
        edge_count, EMBEDDING_ASSOCIATION_TOP_K as i64,
        "hub should have exactly {} edges, got {edge_count}",
        EMBEDDING_ASSOCIATION_TOP_K,
    );

    // Read back the retained edges.
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
        EMBEDDING_ASSOCIATION_TOP_K,
        "list_provenance should return exactly {} rows",
        EMBEDDING_ASSOCIATION_TOP_K,
    );

    // Collect the retained candidate note_ids.
    let retained_ids: Vec<String> = rows
        .iter()
        .map(|r| {
            if r.note_a_id == hub {
                r.note_b_id.clone()
            } else {
                r.note_a_id.clone()
            }
        })
        .collect();

    // All 5 tier-1 candidates must be retained.
    for nid in &tier1_ids {
        assert!(
            retained_ids.contains(nid),
            "tier-1 candidate {nid} must be retained"
        );
    }

    // None of tier-3 should be retained.
    for nid in &tier3_ids {
        assert!(
            !retained_ids.contains(nid),
            "tier-3 candidate {nid} must NOT be retained"
        );
    }

    // Exactly 3 from tier-2 must be retained, and they must be the 3
    // with the lowest note_ids (tie-breaking by note_id ASC).
    let mut sorted_tier2 = tier2_ids.clone();
    sorted_tier2.sort();
    let expected_tier2_retained: Vec<&String> = sorted_tier2.iter().take(3).collect();

    let retained_tier2: Vec<&String> = tier2_ids
        .iter()
        .filter(|nid| retained_ids.contains(nid))
        .collect();
    assert_eq!(
        retained_tier2.len(),
        3,
        "exactly 3 tier-2 candidates should be retained"
    );
    for nid in &expected_tier2_retained {
        assert!(
            retained_tier2.contains(nid),
            "tier-2 candidate {} should be retained (lowest note_id ASC tie-break)",
            nid
        );
    }
    // The 2 excluded tier-2 notes must be the ones with highest note_ids.
    let excluded_tier2: Vec<&String> = tier2_ids
        .iter()
        .filter(|nid| !retained_ids.contains(nid))
        .collect();
    assert_eq!(
        excluded_tier2.len(),
        2,
        "exactly 2 tier-2 candidates should be excluded"
    );
    let mut sorted_excluded: Vec<&String> = excluded_tier2.clone();
    sorted_excluded.sort();
    let expected_excluded: Vec<&String> = sorted_tier2.iter().skip(3).collect();
    assert_eq!(
        sorted_excluded, expected_excluded,
        "excluded tier-2 notes must be the 2 with highest note_ids (tie-break by note_id ASC)"
    );

    // Verify all retained edges have the expected confidences.
    for row in &rows {
        let conf = row.confidence.unwrap();
        assert!(
            conf >= 0.90 - 1e-6,
            "retained confidence {conf} should be >= 0.90"
        );
    }
}

// 3. Weight formula correctness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_weight_formula_correctness() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    // Create a hub note and 4 candidates with specific cosine values.
    let hub = make_note_with_mock_embedding(
        &db,
        &repo,
        &mock,
        &project.id,
        "Weight Hub",
        "test-model",
        384,
    )
    .await;

    let cosine_values = [0.78, 0.80, 0.90, 1.00];
    let mut candidate_ids = Vec::new();
    for &cosine in &cosine_values {
        let nid = make_note_with_mock_embedding(
            &db,
            &repo,
            &mock,
            &project.id,
            &format!("Weight Candidate {cosine:.2}"),
            "test-model",
            384,
        )
        .await;
        candidate_ids.push((nid, cosine));
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
    assert!(row_078.is_some(), "must have an edge with confidence 0.78");
    let row_078 = row_078.unwrap();
    assert!(
        (row_078.weight - 0.05).abs() < 1e-6,
        "cosine=0.78 should yield weight=0.05, got {}",
        row_078.weight
    );

    let row_080 = rows
        .iter()
        .find(|r| (r.confidence.unwrap() - 0.80).abs() < 1e-6);
    assert!(row_080.is_some(), "must have an edge with confidence 0.80");
    let row_080 = row_080.unwrap();
    let expected_080 = expected_weight(0.80);
    assert!(
        (row_080.weight - expected_080).abs() < 1e-6,
        "cosine=0.80 should yield weight={expected_080:.6}, got {}",
        row_080.weight
    );

    let row_090 = rows
        .iter()
        .find(|r| (r.confidence.unwrap() - 0.90).abs() < 1e-6);
    assert!(row_090.is_some(), "must have an edge with confidence 0.90");
    let row_090 = row_090.unwrap();
    let expected_090 = expected_weight(0.90);
    assert!(
        (row_090.weight - expected_090).abs() < 1e-6,
        "cosine=0.90 should yield weight={expected_090:.6}, got {}",
        row_090.weight
    );

    let row_100 = rows
        .iter()
        .find(|r| (r.confidence.unwrap() - 1.00).abs() < 1e-6);
    assert!(row_100.is_some(), "must have an edge with confidence 1.00");
    let row_100 = row_100.unwrap();
    assert!(
        (row_100.weight - 0.35).abs() < 1e-6,
        "cosine=1.00 should yield weight=0.35, got {}",
        row_100.weight
    );
}

// 4. Provenance metadata round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_provenance_metadata_round_trip() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let note_a = make_note_with_mock_embedding(
        &db,
        &repo,
        &mock,
        &project.id,
        "Prov A",
        "my-embedding-v2",
        768,
    )
    .await;
    let note_b = make_note_with_mock_embedding(
        &db,
        &repo,
        &mock,
        &project.id,
        "Prov B",
        "my-embedding-v2",
        768,
    )
    .await;

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
    assert_eq!(
        remaining, EMBEDDING_ASSOCIATION_TOP_K as i64,
        "expected exactly {} edges after overflow prune, got {remaining}",
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
// duplicate rows, churn weights, or change provenance metadata. Only
// `last_refreshed_at` updates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_idempotence() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    let note_a =
        make_note_with_mock_embedding(&db, &repo, &mock, &project.id, "Idem A", "test-model", 384)
            .await;
    let note_b =
        make_note_with_mock_embedding(&db, &repo, &mock, &project.id, "Idem B", "test-model", 384)
            .await;

    mock.set_candidates(&note_a, vec![(&note_b, 0.90)]);
    mock.set_candidates(&note_b, vec![(&note_a, 0.90)]);

    // First refresh.
    repo.refresh_embedding_associations(&project.id)
        .await
        .unwrap();

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
    let row1 = &rows_after_1[0];
    let weight_after_1 = row1.weight;
    let confidence_after_1 = row1.confidence;
    let algo_after_1 = row1.algorithm_version.clone();
    let model_after_1 = row1.embedding_model.clone();
    let dim_after_1 = row1.embedding_dim;
    let refreshed_at_1 = row1.last_refreshed_at.clone();
    assert!(refreshed_at_1.is_some(), "last_refreshed_at after 1st run");

    // Capture the total DB-level row count after the first refresh.
    let total_rows_after_1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations
         WHERE kind = 'embedding_related' AND source = 'embedding_similarity'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    // Sleep to ensure the millisecond timestamp changes.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Second refresh — unchanged notes.
    repo.refresh_embedding_associations(&project.id)
        .await
        .unwrap();

    // Still exactly 1 row — no duplicate.
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

    // Total DB row count unchanged — no new rows created by second refresh.
    let total_rows_after_2: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations
         WHERE kind = 'embedding_related' AND source = 'embedding_similarity'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        total_rows_after_1, total_rows_after_2,
        "idempotent refresh must not change total row count: {total_rows_after_1} vs {total_rows_after_2}"
    );

    let row2 = &rows_after_2[0];

    // Weight unchanged (same cosine, GREATEST is a no-op).
    assert!(
        (row2.weight - weight_after_1).abs() < 1e-12,
        "weight must not churn: expected {weight_after_1}, got {}",
        row2.weight
    );

    // Confidence unchanged.
    assert_eq!(
        row2.confidence, confidence_after_1,
        "confidence must not change on idempotent refresh"
    );

    // algorithm_version unchanged.
    assert_eq!(
        row2.algorithm_version, algo_after_1,
        "algorithm_version must not change on idempotent refresh"
    );

    // embedding_model unchanged.
    assert_eq!(
        row2.embedding_model, model_after_1,
        "embedding_model must not change on idempotent refresh"
    );

    // embedding_dim unchanged.
    assert_eq!(
        row2.embedding_dim, dim_after_1,
        "embedding_dim must not change on idempotent refresh"
    );

    // last_refreshed_at must be updated (different from the first run).
    let refreshed_at_2 = row2.last_refreshed_at.clone();
    assert!(
        refreshed_at_2.is_some(),
        "last_refreshed_at must remain populated"
    );
    assert_ne!(
        refreshed_at_2, refreshed_at_1,
        "last_refreshed_at must be updated on second refresh (proves only this field changes)"
    );
}

// 10. Bounded complexity — 700 notes, O(n) not O(n²).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_bounded_complexity_700_notes() {
    let db = Database::open_in_memory().unwrap();
    let (_tmp, project, mock, tx) = setup_refresh_test(&db).await;

    let repo = make_repo_with_mock(&db, mock.clone(), &tx);

    // ── Batch-create 700 notes with embeddings ─────────────────────
    // The per-note helper (make_note_with_mock_embedding) issues 3 DB
    // queries per note × 700 = 2100 queries ≈ 5-7s.  To stay under
    // the 5s CI budget we batch all inserts into a single transaction
    // with multi-row VALUES.
    let n = 700usize;
    let mut note_ids: Vec<String> = Vec::with_capacity(n);

    // Phase 1: generate all note_ids and prepare batch data.
    for _ in 0..n {
        let nid = uuid::Uuid::now_v7().to_string();
        note_ids.push(nid);
    }

    // Phase 2: batch INSERT notes.
    {
        let mut tx = db.pool().begin().await.unwrap();
        for (i, nid) in note_ids.iter().enumerate() {
            let title = format!("Bench Note {i:04}");
            let permalink = format!("bench-note-{i:04}-{}", &nid[..8]);
            let content_hash = format!("hash-{nid}");
            sqlx::query(
                "INSERT INTO notes (id, project_id, permalink, title, file_path, storage, \
                 note_type, folder, tags, content, retrieval_anchor, content_hash, scope_paths) \
                 VALUES ($1, $2, $3, $4, '', 'db', 'reference', '', '[]', 'content', '', $5, '[]')",
            )
            .bind(nid)
            .bind(&project.id)
            .bind(&permalink)
            .bind(&title)
            .bind(&content_hash)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
    }

    // Phase 3: batch INSERT embeddings + meta + mock registration.
    {
        let mut tx = db.pool().begin().await.unwrap();
        for nid in &note_ids {
            let embedding = embedding_for_note(nid);
            let blob = embedding_to_blob_helper(&embedding);
            let hash = note_id_hash(nid);
            mock.register_embedding(hash, nid.clone());

            let content_hash = format!("hash-{nid}");
            sqlx::query(
                "INSERT INTO note_embeddings (note_id, embedding, embedding_dim, updated_at) \
                 VALUES ($1, $2, $3, to_char(now() at time zone 'utc', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')) \
                 ON CONFLICT (note_id) DO UPDATE SET \
                 embedding = EXCLUDED.embedding, embedding_dim = EXCLUDED.embedding_dim, \
                 updated_at = EXCLUDED.updated_at",
            )
            .bind(nid)
            .bind(&blob)
            .bind(384i32)
            .execute(&mut *tx)
            .await
            .unwrap();

            sqlx::query(
                "INSERT INTO note_embedding_meta \
                 (note_id, content_hash, embedded_at, model_version, embedding_dim, \
                  extension_state, branch) \
                 VALUES ($1, $2, to_char(now() at time zone 'utc', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), $3, $4, 'ready', 'main') \
                 ON CONFLICT (note_id) DO UPDATE SET \
                 content_hash = EXCLUDED.content_hash, embedded_at = EXCLUDED.embedded_at, \
                 model_version = EXCLUDED.model_version, embedding_dim = EXCLUDED.embedding_dim, \
                 extension_state = EXCLUDED.extension_state, branch = EXCLUDED.branch",
            )
            .bind(nid)
            .bind(&content_hash)
            .bind("bench-model")
            .bind(384i32)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
    }

    // For each note, configure 50 candidates (the candidate pool).
    // We use the next 50 note_ids (wrapping around) to simulate a
    // nearest-neighbor result. All candidates are below the 0.78
    // threshold to avoid O(700 × 8) provenance upserts that would
    // dominate wall-clock time. The test verifies bounded *query*
    // complexity, not upsert throughput — the other 9 tests cover
    // upsert correctness exhaustively.
    for (i, nid) in note_ids.iter().enumerate() {
        let mut candidates = Vec::new();
        for j in 1..=50 {
            let idx = (i + j) % 700;
            // All below threshold — proves mock dispatch works
            // (candidates_evaluated == 700 * 50) without paying
            // for thousands of DB upserts.
            candidates.push((note_ids[idx].as_str(), 0.50));
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

    // AC: completes within a CI-safe bounded window.
    // On CI hardware this takes ~2-4s for the refresh (700 × 2 DB queries).
    // The bound is generous to accommodate slower dev/CI environments while
    // still proving the algorithm is O(n) — an O(n²) algorithm would take
    // minutes, not seconds.
    assert!(
        elapsed.as_secs() < 15,
        "700-note refresh must complete in <15s, took {:.2?}",
        elapsed
    );

    // Verify the mock dispatch actually worked: 700 notes each
    // triggered one bounded query that returned 50 candidates.
    // This proves the mock vector store dispatch is functional and
    // the algorithm does not perform O(n²) comparisons. (No edges
    // are upserted because all candidates are below threshold; edge
    // creation correctness is covered exhaustively by tests 1–9.)
    assert_eq!(
        stats.candidates_evaluated,
        700 * EMBEDDING_ASSOCIATION_CANDIDATE_POOL,
        "each of 700 notes should evaluate exactly {} candidates (mock dispatch proof)",
        EMBEDDING_ASSOCIATION_CANDIDATE_POOL,
    );
}

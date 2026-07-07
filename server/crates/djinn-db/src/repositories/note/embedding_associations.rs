//! Embedding-related association refresh and prune algorithm.
//!
//! Mints bounded `embedding_related` / `embedding_similarity` note↔note edges
//! from the vector store using the candidate helpers in [`super::embeddings`]
//! and the provenance substrate in [`super::association`].

use super::association::{
    NoteAssociationKind, NoteAssociationProvenanceUpsert, NoteAssociationSource,
};
use crate::error::DbResult as Result;
use crate::repositories::note::NoteRepository;

/// Minimum cosine similarity to mint an `embedding_related` edge.
pub const EMBEDDING_ASSOCIATION_THRESHOLD: f64 = 0.78;

/// Maximum `embedding_related` edges retained per note endpoint after refresh.
pub const EMBEDDING_ASSOCIATION_TOP_K: usize = 8;

/// Nearest-neighbor candidate pool fetched per note from the vector store.
pub const EMBEDDING_ASSOCIATION_CANDIDATE_POOL: usize = 50;

/// Algorithm version string stored on every row minted by this module.
pub const EMBEDDING_ASSOCIATION_ALGORITHM_VERSION: &str = "embedding-related-v1";

/// Statistics returned by [`NoteRepository::refresh_embedding_associations`].
#[derive(Debug, Default)]
pub struct EmbeddingAssociationRefreshStats {
    pub notes_scanned: usize,
    pub notes_missing_embeddings: usize,
    pub candidates_evaluated: usize,
    pub edges_upserted: usize,
}

impl NoteRepository {
    /// Refresh `embedding_related` edges for every active note with a current
    /// embedding in `project_id`.
    ///
    /// For each eligible note:
    /// 1. Fetch at most [`EMBEDDING_ASSOCIATION_CANDIDATE_POOL`] (50) nearest
    ///    neighbours from the vector store.
    /// 2. Filter to `cosine_similarity >= 0.78`.
    /// 3. Sort by `cosine_similarity DESC`, then `note_id ASC` for
    ///    deterministic tie-breaking; keep at most [`EMBEDDING_ASSOCIATION_TOP_K`]
    ///    (8).
    /// 4. Compute a bounded retrieval weight: `min(0.35, max(0.05, (cosine -
    ///    0.78) / 0.22 * 0.30 + 0.05))`.
    /// 5. Upsert via [`upsert_provenance_association`] with
    ///    `EmbeddingRelated` / `EmbeddingSimilarity`, carrying provenance
    ///    metadata (confidence, algorithm version, embedding model, dim).
    ///
    /// After upserting edges for each note, the per-note top-K cap is enforced:
    /// edges ranked beyond position 8 by `confidence DESC, note_id ASC` are
    /// pruned from both endpoints.
    ///
    /// Returns aggregate statistics.
    pub async fn refresh_embedding_associations(
        &self,
        project_id: &str,
    ) -> Result<EmbeddingAssociationRefreshStats> {
        let eligible = self
            .list_eligible_embedding_association_notes(project_id)
            .await?;

        // Compute how many active notes lack embeddings (informational count).
        let missing = self.list_notes_missing_embeddings(project_id).await?;

        let mut stats = EmbeddingAssociationRefreshStats {
            notes_scanned: eligible.len(),
            notes_missing_embeddings: missing.len(),
            ..Default::default()
        };

        for note in &eligible {
            let candidates = self
                .query_embedding_candidates(
                    &note.note_id,
                    project_id,
                    EMBEDDING_ASSOCIATION_CANDIDATE_POOL,
                )
                .await?;
            stats.candidates_evaluated += candidates.len();

            // Filter by cosine threshold, sort deterministically, keep top-K.
            let mut filtered: Vec<_> = candidates
                .into_iter()
                .filter(|c| c.cosine_similarity >= EMBEDDING_ASSOCIATION_THRESHOLD)
                .collect();
            filtered.sort_by(|a, b| {
                b.cosine_similarity
                    .partial_cmp(&a.cosine_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.note_id.cmp(&b.note_id))
            });
            filtered.truncate(EMBEDDING_ASSOCIATION_TOP_K);

            for candidate in &filtered {
                // Bounded retrieval weight:
                // min(0.35, max(0.05, (cosine - 0.78) / 0.22 * 0.30 + 0.05))
                let weight = (0.05_f64).max(
                    ((candidate.cosine_similarity - EMBEDDING_ASSOCIATION_THRESHOLD)
                        / (1.0 - EMBEDDING_ASSOCIATION_THRESHOLD)
                        * 0.30
                        + 0.05)
                        .min(0.35),
                );

                self.upsert_provenance_association(
                    &note.note_id,
                    &candidate.note_id,
                    &NoteAssociationProvenanceUpsert {
                        kind: NoteAssociationKind::EmbeddingRelated,
                        source: NoteAssociationSource::EmbeddingSimilarity,
                        weight,
                        confidence: Some(candidate.cosine_similarity),
                        algorithm_version: Some(EMBEDDING_ASSOCIATION_ALGORITHM_VERSION.to_owned()),
                        embedding_model: Some(note.model_version.clone()),
                        embedding_dim: Some(note.embedding_dim),
                    },
                )
                .await?;
                stats.edges_upserted += 1;
            }

            // Enforce per-note top-K cap on the source note's edges.
            // The CTE ranks ALL `embedding_related` / `embedding_similarity`
            // edges touching this note (both directions) and deletes rows
            // ranked beyond position K for either endpoint.
            sqlx::query(
                r#"WITH ranked AS (
                       SELECT na.ctid,
                              ROW_NUMBER() OVER (
                                  PARTITION BY na.note_a_id
                                  ORDER BY na.confidence DESC, na.note_b_id
                              ) AS rank_a,
                              ROW_NUMBER() OVER (
                                  PARTITION BY na.note_b_id
                                  ORDER BY na.confidence DESC, na.note_a_id
                              ) AS rank_b
                       FROM note_associations na
                       WHERE na.kind = $1 AND na.source = $2
                         AND (na.note_a_id = $3 OR na.note_b_id = $3)
                   )
                   DELETE FROM note_associations
                   WHERE ctid IN (
                       SELECT ctid FROM ranked
                       WHERE rank_a > $4 OR rank_b > $4
                   )"#,
            )
            .bind(NoteAssociationKind::EmbeddingRelated.as_str())
            .bind(NoteAssociationSource::EmbeddingSimilarity.as_str())
            .bind(&note.note_id)
            .bind(EMBEDDING_ASSOCIATION_TOP_K as i64)
            .execute(self.db.pool())
            .await?;
        }

        Ok(stats)
    }

    /// Prune stale `embedding_related` / `embedding_similarity` rows for a
    /// project.
    ///
    /// Deletes rows that satisfy **any** of:
    ///
    /// **(a)** Either endpoint note is archived or deleted (`status != 'active'`).
    ///
    /// **(b)** The stored `embedding_model` / `embedding_dim` no longer matches
    /// the current `note_embedding_meta` for either endpoint.
    ///
    /// **(c)** The stored `confidence` (cosine similarity) has fallen below
    /// [`EMBEDDING_ASSOCIATION_THRESHOLD`] (0.78).
    ///
    /// **(d)** The row falls outside the top-K cap for either endpoint:
    /// after a full refresh, edges beyond position 8 by `confidence DESC,
    /// note_id ASC` are removed.
    ///
    /// Returns the total number of rows deleted across all four conditions.
    pub async fn prune_embedding_associations(&self, project_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let mut total_deleted: u64 = 0;

        // (a) Delete rows where either endpoint note is archived/deleted.
        let result = sqlx::query(
            r#"DELETE FROM note_associations na
               WHERE na.kind = $1 AND na.source = $2
                 AND (na.note_a_id IN (SELECT id FROM notes WHERE status != 'active' AND project_id = $3)
                   OR na.note_b_id IN (SELECT id FROM notes WHERE status != 'active' AND project_id = $3))"#,
        )
        .bind(NoteAssociationKind::EmbeddingRelated.as_str())
        .bind(NoteAssociationSource::EmbeddingSimilarity.as_str())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        total_deleted += result.rows_affected();

        // (b) Delete rows where embedding_model / embedding_dim no longer
        // matches the current note_embedding_meta for either endpoint.
        // canonical_pair stores note_a_id < note_b_id, so both endpoints
        // can appear in either column.
        let model_dim_mismatch = format!(
            r#"na.kind = '{kind}' AND na.source = '{source}'
               AND (
                   NOT EXISTS (SELECT 1 FROM note_embedding_meta m WHERE m.note_id = na.note_a_id)
                OR NOT EXISTS (SELECT 1 FROM note_embedding_meta m WHERE m.note_id = na.note_b_id)
                OR na.note_a_id IN (
                       SELECT m.note_id FROM note_embedding_meta m
                       WHERE m.note_id = na.note_a_id
                         AND (m.model_version != na.embedding_model OR m.embedding_dim != na.embedding_dim)
                   )
                OR na.note_b_id IN (
                       SELECT m.note_id FROM note_embedding_meta m
                       WHERE m.note_id = na.note_b_id
                         AND (m.model_version != na.embedding_model OR m.embedding_dim != na.embedding_dim)
                   )
               )"#,
            kind = NoteAssociationKind::EmbeddingRelated.as_str(),
            source = NoteAssociationSource::EmbeddingSimilarity.as_str(),
        );
        let result = sqlx::query(&format!(
            "DELETE FROM note_associations na WHERE {}",
            model_dim_mismatch
        ))
        .execute(self.db.pool())
        .await?;
        total_deleted += result.rows_affected();

        // (c) Delete rows where stored confidence is below the threshold.
        // This catches edges that were minted at a higher cosine which has
        // since dropped (e.g. after a re-embed changed the vector).
        let result = sqlx::query(
            r#"DELETE FROM note_associations na
               WHERE na.kind = $1 AND na.source = $2
                 AND (na.confidence IS NOT NULL AND na.confidence < $3)"#,
        )
        .bind(NoteAssociationKind::EmbeddingRelated.as_str())
        .bind(NoteAssociationSource::EmbeddingSimilarity.as_str())
        .bind(EMBEDDING_ASSOCIATION_THRESHOLD)
        .execute(self.db.pool())
        .await?;
        total_deleted += result.rows_affected();

        // (d) Prune rows that fall outside either endpoint's top-K cap.
        //
        // For every note that has at least K+1 `embedding_related` edges,
        // rank all edges touching that note (both directions) by
        // confidence DESC, other_note_id ASC, and delete rows ranked
        // beyond position K for either endpoint.
        //
        // The CTE uses a LEFT JOIN so that each row gets ranked for both
        // the note_a and note_b endpoints. A row is deleted if it falls
        // beyond K for *either* endpoint.
        let result = sqlx::query(
            r#"WITH ranked AS (
                   SELECT na.ctid,
                          ROW_NUMBER() OVER (
                              PARTITION BY na.note_a_id
                              ORDER BY na.confidence DESC, na.note_b_id
                          ) AS rank_a,
                          ROW_NUMBER() OVER (
                              PARTITION BY na.note_b_id
                              ORDER BY na.confidence DESC, na.note_a_id
                          ) AS rank_b
                   FROM note_associations na
                   WHERE na.kind = $1 AND na.source = $2
               )
               DELETE FROM note_associations
               WHERE ctid IN (
                   SELECT ctid FROM ranked
                   WHERE rank_a > $3 OR rank_b > $3
               )"#,
        )
        .bind(NoteAssociationKind::EmbeddingRelated.as_str())
        .bind(NoteAssociationSource::EmbeddingSimilarity.as_str())
        .bind(EMBEDDING_ASSOCIATION_TOP_K as i64)
        .execute(self.db.pool())
        .await?;
        total_deleted += result.rows_affected();

        Ok(total_deleted)
    }
}

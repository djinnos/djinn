//! Per-symbol AST chunk store + Qdrant `code_chunks` collection.
//!
//! Storage scaffolding for Epic B of the code-graph + RAG overhaul plan
//! (`~/.claude/plans/code-graph-and-rag-overhaul.md`). PR B1 lands the
//! tables, the Qdrant vector-store handle, and the empty `chunker` /
//! `text_generator` surface. The chunker (B2), embedding pipeline (B3),
//! and hybrid retrieval (B4) land in follow-up PRs.
//!
//! Mirrors the `note` module layout so warm/repair/search code can be
//! ported one piece at a time without restructuring on each PR.

pub mod chunker;
pub mod embeddings;
pub mod text_generator;

use crate::database::Database;
use crate::error::DbResult as Result;

pub use chunker::CodeChunk;
pub use embeddings::{
    CodeChunkVectorBackend, CodeChunkVectorStore, NoopCodeChunkVectorStore, QdrantCodeChunkConfig,
    QdrantCodeChunkVectorStore,
};

/// Per-chunk state surfaced by [`CodeChunkRepository::list_repair_embedding_rows`].
/// Mirror of `NoteRepairEmbeddingRow` for the new tables. `content_hash` /
/// `model_version` are `None` when no `code_chunk_meta` row exists yet
/// (i.e. the chunk has never been embedded). Filled in by PR B3; the
/// scaffolding exists today so `repair_embeddings` has a stable shape.
#[derive(Debug, Clone)]
pub struct CodeChunkRepairEmbeddingRow {
    pub id: String,
    pub project_id: String,
    pub file_path: String,
    pub symbol_key: Option<String>,
    pub kind: String,
    pub content_hash: String,
    pub embedded_text: String,
    pub meta_content_hash: Option<String>,
    pub meta_model_version: Option<String>,
}

/// Thin repository handle for the `code_chunks` / `code_chunk_meta` tables.
///
/// PR B1 only exposes the read paths the repair tool needs. Writes land in
/// PR B3 alongside the chunker output.
#[derive(Clone)]
pub struct CodeChunkRepository {
    db: Database,
}

impl CodeChunkRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Walk every `code_chunks` row for one project, left-joined with its
    /// `code_chunk_meta` fingerprint. Until B3 lands the chunker pipeline,
    /// no rows are produced and this returns an empty vec — letting
    /// `memory_repair_embeddings` exercise the new code path safely on
    /// today's deployments.
    pub async fn list_repair_embedding_rows(
        &self,
        project_id: &str,
    ) -> Result<Vec<CodeChunkRepairEmbeddingRow>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            r#"SELECT c.id, c.project_id, c.file_path, c.symbol_key, c.kind,
                      c.content_hash, c.embedded_text,
                      m.content_hash AS "meta_content_hash?",
                      m.model_version AS "meta_model_version?"
                 FROM code_chunks c
            LEFT JOIN code_chunk_meta m ON m.id = c.id
                WHERE c.project_id = ?"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| CodeChunkRepairEmbeddingRow {
                id: r.id,
                project_id: r.project_id,
                file_path: r.file_path,
                symbol_key: r.symbol_key,
                kind: r.kind,
                content_hash: r.content_hash,
                embedded_text: r.embedded_text,
                meta_content_hash: r.meta_content_hash,
                meta_model_version: r.meta_model_version,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Migration smoke test: the `code_chunks` + `code_chunk_meta` tables
    /// from migration 18 must apply cleanly and the repair-row left-join
    /// must compile + run, even with zero rows. Until PR B3 ships the
    /// chunker pipeline, this is the only end-to-end exercise of the new
    /// schema.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_repair_embedding_rows_returns_empty_for_unknown_project() {
        let db = Database::open_in_memory().unwrap();
        let repo = CodeChunkRepository::new(db);
        let rows = repo
            .list_repair_embedding_rows("project-does-not-exist")
            .await
            .expect("query against fresh code_chunks table should succeed");
        assert!(rows.is_empty(), "expected no rows on a fresh project");
    }

    /// Confirms the `code_chunks` + `code_chunk_meta` tables actually
    /// honor the schema declared in migration 18: insert a chunk + its
    /// meta row, then read them back through the repair-row left join.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migration_18_persists_chunk_with_meta() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        sqlx::query!(
            r#"INSERT INTO code_chunks
                (id, project_id, file_path, symbol_key, kind,
                 start_line, end_line, content_hash, embedded_text)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            "chunk-1",
            "proj-1",
            "src/lib.rs",
            Some::<&str>("rust:lib::foo"),
            "function",
            1_i32,
            10_i32,
            "deadbeef",
            "Label: foo\n…",
        )
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query!(
            r#"INSERT INTO code_chunk_meta
                (id, project_id, content_hash, model_version, embedded_at)
               VALUES (?, ?, ?, ?, ?)"#,
            "chunk-1",
            "proj-1",
            "deadbeef",
            "test-model-v1",
            "2026-04-28T00:00:00Z",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let rows = CodeChunkRepository::new(db)
            .list_repair_embedding_rows("proj-1")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, "chunk-1");
        assert_eq!(row.symbol_key.as_deref(), Some("rust:lib::foo"));
        assert_eq!(row.content_hash, "deadbeef");
        assert_eq!(row.meta_content_hash.as_deref(), Some("deadbeef"));
        assert_eq!(row.meta_model_version.as_deref(), Some("test-model-v1"));
    }

    /// Bootstrap-time idempotency: building two `QdrantCodeChunkVectorStore`s
    /// with the same config returns matching collection names, so a
    /// second `initialize_code_vector_store()` call doesn't pick up a
    /// drifted handle. The actual `ensure_collection` round-trip needs a
    /// live Qdrant and is exercised in the server-level integration
    /// tests; this test pins the config plumbing.
    #[test]
    fn qdrant_code_chunk_config_is_stable_across_handles() {
        let cfg = QdrantCodeChunkConfig::default();
        let a = QdrantCodeChunkVectorStore::new(cfg.clone());
        let b = QdrantCodeChunkVectorStore::new(cfg);
        assert_eq!(a.config().collection, "code_chunks");
        assert_eq!(b.config().collection, "code_chunks");
        assert_eq!(a.config().url, b.config().url);
    }
}

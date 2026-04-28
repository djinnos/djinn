//! Chunk-and-embed pipeline (PR B3).
//!
//! Walks a [`crate::repositories::code_chunk::chunker`] output across every
//! source file in a project, diffs against `code_chunk_meta` for staleness,
//! and persists fresh embeddings into both the SQL tables (`code_chunks`
//! + `code_chunk_meta`) and the configured [`CodeChunkVectorStore`].
//!
//! Mirrors the notes-side concurrency model
//! (`note/embeddings.rs:upsert_embedding_metadata`):
//!
//! * `INSERT ... ON DUPLICATE KEY UPDATE` so overlapping warms last-writer-win
//!   without coordination.
//! * Qdrant point id derived from `(chunk_id, content_hash)` so re-warms
//!   on identical content are pure no-ops at the vector layer.
//! * Per-project in-memory `Mutex<HashSet<String>>` so a second warm tick
//!   on the same project coalesces rather than duplicating embed work.
//! * Fire-and-forget — the canonical-graph warmer spawns this on its
//!   exit branch and never blocks on completion. Errors are logged.
//!
//! Higher-level entrypoints are split:
//! * [`chunk_and_embed_files`] — pure pipeline given a pre-built list of
//!   `FileInput`s. Used by the warmer (which has the graph + symbol_ranges
//!   in hand) and by unit tests.
//! * [`try_claim_project`] / [`release_project`] — thin handles around the
//!   in-flight set so callers (including external warmers) can implement
//!   coalescing without duplicating the static `OnceLock` here.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use sqlx::Acquire;
use tokio::sync::Mutex;

use crate::database::Database;
use crate::error::DbResult as Result;

use super::chunker::{ChunkConfig, FileInput, RepoMetadata, chunk_file};
use super::embeddings::{CodeChunkVectorStore, UpsertCodeChunkEmbedding};

/// Trait implemented by an embedding provider that knows how to embed a
/// chunk's rendered text. Sliced off `NoteEmbeddingProvider` so callers
/// (warmer, repair tool) can pass the same `EmbeddingService` without
/// `djinn-db` taking a hard dep on `djinn-provider`.
#[async_trait::async_trait]
pub trait CodeChunkEmbeddingProvider: Send + Sync {
    fn model_version(&self) -> String;
    async fn embed_chunk(&self, text: &str) -> std::result::Result<EmbeddedCodeChunk, String>;
}

/// Output of one embedding call.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedCodeChunk {
    pub values: Vec<f32>,
    pub model_version: String,
}

/// Counts surfaced after a [`chunk_and_embed_files`] run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkAndEmbedReport {
    /// Number of `(file, symbol) -> chunk` outputs the chunker produced.
    pub chunks_total: usize,
    /// Chunks for which a fresh embedding was generated and persisted.
    pub chunks_embedded: usize,
    /// Chunks whose meta row already matched the freshly-rendered
    /// content_hash + model_version (skipped — pure no-op).
    pub chunks_skipped_stale_match: usize,
    /// Chunks whose embedding step failed (provider unavailable, etc.);
    /// the SQL row is still written so a future repair pass can heal.
    pub chunks_embed_failed: usize,
    /// Chunks whose vector-store upsert failed but the SQL row was still
    /// written with `extension_state="pending"`. Mirrors the notes-side
    /// "qdrant down" graceful degradation.
    pub chunks_pending: usize,
    /// Chunks that landed in the vector store with `extension_state="ready"`.
    pub chunks_ready: usize,
}

/// Process-wide in-flight set keyed by `project_id`. A `tokio::sync::Mutex`
/// rather than a `std::sync::Mutex` so contention from the spawned task
/// path doesn't risk poisoning the global state on panic.
fn inflight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Try to claim the chunk-and-embed slot for `project_id`. Returns `Some`
/// on success — the [`InflightGuard`] releases the slot on drop. Returns
/// `None` if another caller is already running the pipeline for this
/// project; the caller should treat this as a coalesced no-op.
pub async fn try_claim_project(project_id: &str) -> Option<InflightGuard> {
    let set = inflight();
    let mut guard = set.lock().await;
    if !guard.insert(project_id.to_string()) {
        return None;
    }
    Some(InflightGuard {
        project_id: project_id.to_string(),
    })
}

/// RAII handle returned by [`try_claim_project`]. Removing on drop so a
/// panic inside the pipeline still releases the slot.
pub struct InflightGuard {
    project_id: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let project_id = std::mem::take(&mut self.project_id);
        if project_id.is_empty() {
            return;
        }
        // We're in a sync drop, so spawn a tiny task to release the slot
        // on the runtime. If no runtime is present (test teardown), fall
        // through silently — the pipeline is per-process and a stuck
        // entry only matters as long as the process keeps running.
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                let mut guard = inflight().lock().await;
                guard.remove(&project_id);
            });
        }
    }
}

/// Run the chunker over `files`, embed everything that's stale, persist
/// fresh rows + vectors. Idempotent across overlapping invocations.
///
/// `vector_store` may be a [`super::NoopCodeChunkVectorStore`] when the
/// `DJINN_CODE_CHUNKS_BACKEND` env var is unset — the SQL rows still land
/// (so `repair_embeddings` can pick them up later) but nothing is sent
/// to the vector store and every chunk lands as `extension_state="pending"`.
pub async fn chunk_and_embed_files(
    db: &Database,
    embeddings: Arc<dyn CodeChunkEmbeddingProvider>,
    vector_store: Arc<dyn CodeChunkVectorStore>,
    project_id: &str,
    repo_metadata: RepoMetadata<'_>,
    files: &[FileInput<'_>],
    config: ChunkConfig,
) -> Result<ChunkAndEmbedReport> {
    db.ensure_initialized().await?;
    let model_version = embeddings.model_version();

    let mut report = ChunkAndEmbedReport::default();

    for file in files {
        let chunks = chunk_file(project_id, &repo_metadata, file, config);
        report.chunks_total += chunks.len();

        for chunk in chunks {
            // Staleness check: skip when the meta row already matches the
            // freshly-computed content hash AND the model version AND the
            // vector store is `ready`. Anything weaker re-embeds — keeping
            // parity with the notes-side rule.
            let existing = sqlx::query!(
                r#"SELECT content_hash, model_version, extension_state
                     FROM code_chunk_meta
                    WHERE id = ?"#,
                chunk.id
            )
            .fetch_optional(db.pool())
            .await?;

            let already_fresh = existing.as_ref().is_some_and(|row| {
                row.content_hash == chunk.content_hash
                    && row.model_version == model_version
                    && row.extension_state == "ready"
            });
            if already_fresh {
                report.chunks_skipped_stale_match += 1;
                continue;
            }

            // Always upsert the SQL row so the chunker's view of the
            // project is authoritative even if the embed call fails.
            upsert_chunk_row(db, &chunk).await?;

            let embedding = match embeddings.embed_chunk(&chunk.embedded_text).await {
                Ok(embedded) => embedded,
                Err(reason) => {
                    tracing::debug!(
                        chunk_id = %chunk.id,
                        project_id,
                        %reason,
                        "code-chunk embedding unavailable; skipping vector upsert"
                    );
                    // Still record meta so a repair pass can find the row.
                    upsert_chunk_meta(
                        db,
                        &chunk.id,
                        project_id,
                        &chunk.content_hash,
                        &model_version,
                        "pending",
                    )
                    .await?;
                    report.chunks_embed_failed += 1;
                    report.chunks_pending += 1;
                    continue;
                }
            };

            let upsert_input = UpsertCodeChunkEmbedding {
                chunk_id: &chunk.id,
                project_id,
                file_path: &chunk.file_path,
                symbol_key: chunk.symbol_key.as_deref(),
                kind: &chunk.kind,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                content_hash: &chunk.content_hash,
                embedded_text: &chunk.embedded_text,
                model_version: &embedding.model_version,
                embedding: &embedding.values,
            };

            let extension_state = vector_store.upsert_vector(&upsert_input).await?;
            upsert_chunk_meta(
                db,
                &chunk.id,
                project_id,
                &chunk.content_hash,
                &embedding.model_version,
                extension_state,
            )
            .await?;

            report.chunks_embedded += 1;
            match extension_state {
                "ready" => report.chunks_ready += 1,
                _ => report.chunks_pending += 1,
            }

            tokio::task::yield_now().await;
        }
    }

    tracing::info!(
        project_id,
        chunks_total = report.chunks_total,
        chunks_embedded = report.chunks_embedded,
        chunks_ready = report.chunks_ready,
        chunks_pending = report.chunks_pending,
        chunks_embed_failed = report.chunks_embed_failed,
        chunks_skipped_stale_match = report.chunks_skipped_stale_match,
        "chunk_and_embed_files: pipeline pass complete"
    );

    Ok(report)
}

async fn upsert_chunk_row(db: &Database, chunk: &super::chunker::CodeChunk) -> Result<()> {
    let mut tx = db.pool().begin().await?;
    let conn = tx.acquire().await?;
    let start_line = chunk.start_line as i32;
    let end_line = chunk.end_line as i32;
    sqlx::query!(
        r#"INSERT INTO code_chunks
            (id, project_id, file_path, symbol_key, kind,
             start_line, end_line, content_hash, embedded_text)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON DUPLICATE KEY UPDATE
             project_id = VALUES(project_id),
             file_path = VALUES(file_path),
             symbol_key = VALUES(symbol_key),
             kind = VALUES(kind),
             start_line = VALUES(start_line),
             end_line = VALUES(end_line),
             content_hash = VALUES(content_hash),
             embedded_text = VALUES(embedded_text)"#,
        chunk.id,
        chunk.project_id,
        chunk.file_path,
        chunk.symbol_key,
        chunk.kind,
        start_line,
        end_line,
        chunk.content_hash,
        chunk.embedded_text,
    )
    .execute(&mut *conn)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn upsert_chunk_meta(
    db: &Database,
    chunk_id: &str,
    project_id: &str,
    content_hash: &str,
    model_version: &str,
    extension_state: &str,
) -> Result<()> {
    // `embedded_at` is a VARCHAR (migration 18) so we just stamp it with
    // a server-side `NOW(3)`-derived ISO-8601 string via a CAST.  Avoids
    // pulling chrono into djinn-db just for this one shim — and it
    // mirrors how note_embedding_meta uses NOW(3) to stamp `embedded_at`.
    let mut tx = db.pool().begin().await?;
    let conn = tx.acquire().await?;
    sqlx::query!(
        r#"INSERT INTO code_chunk_meta
            (id, project_id, content_hash, model_version, embedded_at, extension_state)
           VALUES (?, ?, ?, ?, DATE_FORMAT(UTC_TIMESTAMP(3), '%Y-%m-%dT%H:%i:%S.%fZ'), ?)
           ON DUPLICATE KEY UPDATE
             project_id = VALUES(project_id),
             content_hash = VALUES(content_hash),
             model_version = VALUES(model_version),
             embedded_at = VALUES(embedded_at),
             extension_state = VALUES(extension_state)"#,
        chunk_id,
        project_id,
        content_hash,
        model_version,
        extension_state,
    )
    .execute(&mut *conn)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::code_chunk::{
        CodeChunkRepository, NoopCodeChunkVectorStore,
        chunker::{FileInput, RepoMetadata, SymbolChunkKind, SymbolInput},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_claim_project_coalesces_concurrent_calls() {
        // First claim should succeed; second should observe the slot held
        // and return None until the first guard drops.
        let project_id = "pipeline-inflight-test";
        let first = try_claim_project(project_id).await;
        assert!(first.is_some(), "first claim must succeed");
        let second = try_claim_project(project_id).await;
        assert!(second.is_none(), "second concurrent claim must coalesce");

        drop(first);
        // The drop spawns the release task; yield until it lands.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if try_claim_project(project_id).await.is_some() {
                return;
            }
        }
        panic!("slot never released after first guard drop");
    }

    /// Embedding provider that returns a fixed 8-d vector so we don't have
    /// to load nomic-bert in unit tests.
    struct FakeProvider;

    #[async_trait::async_trait]
    impl CodeChunkEmbeddingProvider for FakeProvider {
        fn model_version(&self) -> String {
            "fake-test-model@v1".to_string()
        }
        async fn embed_chunk(&self, _text: &str) -> std::result::Result<EmbeddedCodeChunk, String> {
            Ok(EmbeddedCodeChunk {
                values: vec![0.1_f32; 8],
                model_version: "fake-test-model@v1".to_string(),
            })
        }
    }

    fn small_function_file_input<'a>(
        source: &'a str,
        symbol: &'a [SymbolInput],
    ) -> FileInput<'a> {
        FileInput {
            path: "src/lib.rs",
            content: source,
            symbols: symbol,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_embeds_chunks_into_db_on_first_pass() {
        let db = Database::open_in_memory().expect("open test db");
        db.ensure_initialized().await.expect("migrate");

        let symbols = vec![SymbolInput {
            symbol_key: "rust . . crate . hello().".to_string(),
            display_name: "hello".to_string(),
            kind: SymbolChunkKind::Function,
            start_line: 1,
            end_line: 3,
            is_export: true,
            signature: Some("fn hello()".to_string()),
            documentation: vec![],
        }];
        let source = "fn hello() {\n    println!(\"hi\");\n}\n";
        let file = small_function_file_input(source, &symbols);

        let provider: Arc<dyn CodeChunkEmbeddingProvider> = Arc::new(FakeProvider);
        let store: Arc<dyn CodeChunkVectorStore> = Arc::new(NoopCodeChunkVectorStore);

        let report = chunk_and_embed_files(
            &db,
            provider.clone(),
            store.clone(),
            "proj-pipeline",
            RepoMetadata {
                owner: "djinnos",
                repo: "djinn",
            },
            std::slice::from_ref(&file),
            ChunkConfig::default(),
        )
        .await
        .expect("first pass succeeds");

        assert!(report.chunks_total >= 1, "chunker must produce ≥1 chunk");
        assert_eq!(report.chunks_embedded, report.chunks_total);
        assert_eq!(report.chunks_skipped_stale_match, 0);
        // Noop vector store reports `pending` since it has no real
        // backend — but the SQL row + meta still land.
        assert_eq!(report.chunks_pending, report.chunks_total);

        // Repository round-trip: the chunk + meta both exist and are
        // stamped with our model version.
        let rows = CodeChunkRepository::new(db.clone())
            .list_repair_embedding_rows("proj-pipeline")
            .await
            .expect("scan rows");
        assert!(!rows.is_empty(), "expected ≥1 chunk row in DB");
        for row in &rows {
            assert_eq!(row.meta_model_version.as_deref(), Some("fake-test-model@v1"));
            assert_eq!(row.meta_extension_state.as_deref(), Some("pending"));
        }

        // Idempotency: a second pass with identical content should skip
        // every chunk (stale-match short-circuit). With the noop store,
        // chunks land as `pending` so the freshness check still re-embeds
        // them — explicitly verify this matches the documented contract:
        // once a chunk is `ready` it skips, otherwise re-embed runs.
        let second = chunk_and_embed_files(
            &db,
            provider,
            store,
            "proj-pipeline",
            RepoMetadata {
                owner: "djinnos",
                repo: "djinn",
            },
            &[file],
            ChunkConfig::default(),
        )
        .await
        .expect("second pass succeeds");
        assert_eq!(second.chunks_total, report.chunks_total);
        // Pending rows are not "fresh" — they get re-embedded each time
        // until a vector store flips them to `ready`.
        assert_eq!(second.chunks_skipped_stale_match, 0);
        assert_eq!(second.chunks_embedded, second.chunks_total);
    }

    /// Vector store that flips meta to `ready` (simulates a successful
    /// Qdrant deployment). Used to verify the second-pass stale-match
    /// short-circuit fires once `ready` rows exist.
    struct ReadyVectorStub;

    #[async_trait::async_trait]
    impl CodeChunkVectorStore for ReadyVectorStub {
        fn backend(&self) -> super::super::CodeChunkVectorBackend {
            super::super::CodeChunkVectorBackend::Qdrant
        }
        async fn can_index(&self) -> Result<bool> {
            Ok(true)
        }
        async fn upsert_vector(
            &self,
            _input: &super::super::UpsertCodeChunkEmbedding<'_>,
        ) -> Result<&'static str> {
            Ok("ready")
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_short_circuits_when_ready_rows_match() {
        let db = Database::open_in_memory().expect("open test db");
        db.ensure_initialized().await.expect("migrate");

        let symbols = vec![SymbolInput {
            symbol_key: "rust . . crate . greet().".to_string(),
            display_name: "greet".to_string(),
            kind: SymbolChunkKind::Function,
            start_line: 1,
            end_line: 3,
            is_export: true,
            signature: Some("fn greet()".to_string()),
            documentation: vec![],
        }];
        let source = "fn greet() {\n    println!(\"hello\");\n}\n";
        let file = FileInput {
            path: "src/lib.rs",
            content: source,
            symbols: &symbols,
        };

        let provider: Arc<dyn CodeChunkEmbeddingProvider> = Arc::new(FakeProvider);
        let store: Arc<dyn CodeChunkVectorStore> = Arc::new(ReadyVectorStub);

        let first = chunk_and_embed_files(
            &db,
            provider.clone(),
            store.clone(),
            "proj-ready",
            RepoMetadata {
                owner: "djinnos",
                repo: "djinn",
            },
            std::slice::from_ref(&file),
            ChunkConfig::default(),
        )
        .await
        .expect("first pass");
        assert!(first.chunks_total >= 1);
        assert_eq!(first.chunks_ready, first.chunks_total);

        let second = chunk_and_embed_files(
            &db,
            provider,
            store,
            "proj-ready",
            RepoMetadata {
                owner: "djinnos",
                repo: "djinn",
            },
            &[file],
            ChunkConfig::default(),
        )
        .await
        .expect("second pass");
        assert_eq!(second.chunks_total, first.chunks_total);
        assert_eq!(second.chunks_skipped_stale_match, second.chunks_total);
        assert_eq!(second.chunks_embedded, 0);
    }
}

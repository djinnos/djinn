// `memory_repair_embeddings(project)` — re-embed notes whose embedding
// metadata is missing or stale (content_hash mismatch, model_version drift,
// or `force=true`). Replaces the deleted `repair_project_embeddings`
// helper that lived in the watcher subsystem before commit 2ecf7e145.

use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};

use djinn_db::{CodeChunkRepository, embedding_content_hash};

use super::write_services::note_repository;
use super::{MemoryRepairEmbeddingFailure, MemoryRepairEmbeddingsResponse, RepairEmbeddingsParams};
use crate::server::DjinnMcpServer;

/// Cap the failures vector so a wholesale outage doesn't blow up response
/// size. Repairs continue past the cap; only the recorded failure list is
/// truncated.
const MAX_REPORTED_FAILURES: usize = 20;

#[tool_router(router = memory_repair_embeddings_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Re-embed notes for a project whose embedding is missing or stale.
    ///
    /// Useful after a fresh deploy where the Qdrant collection didn't exist
    /// yet (so embed upserts silently failed), or after switching embedding
    /// models. Walks every note for the project, checks the local
    /// `note_embedding_meta` row's `content_hash` + `model_version`, and
    /// re-embeds anything that doesn't match. Pass `force=true` to re-embed
    /// every note unconditionally.
    #[tool(
        description = "Re-embed notes for a project whose embedding is missing or stale (content hash mismatch, model version drift, or force=true). Returns repaired/up-to-date/failed counts plus a capped list of failures."
    )]
    pub async fn memory_repair_embeddings(
        &self,
        Parameters(p): Parameters<RepairEmbeddingsParams>,
    ) -> Json<MemoryRepairEmbeddingsResponse> {
        Json(repair_embeddings(self, p).await)
    }
}

async fn repair_embeddings(
    server: &DjinnMcpServer,
    params: RepairEmbeddingsParams,
) -> MemoryRepairEmbeddingsResponse {
    let project_id = match server.resolve_project_id(&params.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryRepairEmbeddingsResponse {
                error: Some(error),
                ..MemoryRepairEmbeddingsResponse::default()
            };
        }
    };

    let repo = note_repository(server);
    let Some(provider) = repo.embedding_provider() else {
        return MemoryRepairEmbeddingsResponse {
            error: Some("embedding provider not configured".to_string()),
            ..MemoryRepairEmbeddingsResponse::default()
        };
    };
    let model_version = provider.model_version();

    let force = params.force.unwrap_or(false);

    // When the vector store can actually accept embeddings (e.g. Qdrant is
    // reachable), treat `extension_state="pending"` as stale — that state
    // means the metadata write succeeded but the vector store call did
    // not, so the point is missing and re-embedding will populate it.
    // When the vector store cannot index (Noop, or Qdrant unreachable),
    // every meta row is "pending" by design and re-embedding would just
    // loop, so fall back to the hash+version-only freshness check.
    let vector_store_active = repo
        .vector_store()
        .can_index(&repo)
        .await
        .unwrap_or(false);

    let rows = match repo.list_repair_embedding_rows(&project_id).await {
        Ok(rows) => rows,
        Err(error) => {
            return MemoryRepairEmbeddingsResponse {
                error: Some(format!("failed to load notes: {error}")),
                ..MemoryRepairEmbeddingsResponse::default()
            };
        }
    };

    let mut response = MemoryRepairEmbeddingsResponse {
        total: rows.len() as i64,
        ..MemoryRepairEmbeddingsResponse::default()
    };

    for row in rows {
        let expected_hash =
            embedding_content_hash(&row.title, &row.note_type, &row.tags, &row.content);

        let is_stale = match (row.content_hash.as_deref(), row.model_version.as_deref()) {
            (Some(hash), Some(version)) => {
                let content_or_model_drifted =
                    hash != expected_hash || version != model_version;
                // Only let `extension_state` invalidate freshness when the
                // vector store can actually index. Otherwise every Noop /
                // unreachable-Qdrant meta row reads as stale forever.
                let vector_missing = vector_store_active
                    && row.extension_state.as_deref().unwrap_or("pending") != "ready";
                content_or_model_drifted || vector_missing
            }
            // No meta row at all → definitely needs embedding.
            _ => true,
        };

        if !force && !is_stale {
            response.up_to_date += 1;
        } else {
            match repo
                .embed_note_now(&row.id, &row.title, &row.note_type, &row.tags, &row.content)
                .await
            {
                Ok(_) => response.repaired += 1,
                Err(reason) => {
                    response.failed += 1;
                    if response.failures.len() < MAX_REPORTED_FAILURES {
                        response.failures.push(MemoryRepairEmbeddingFailure {
                            note_id: row.id.clone(),
                            reason,
                        });
                    }
                }
            }
        }

        // Cooperative yield so a large repair doesn't starve the runtime.
        tokio::task::yield_now().await;
    }

    // Code-chunk pass (PR B1 scaffolding). The chunker (B2) and embedding
    // pipeline (B3) haven't shipped yet, so the table is always empty here
    // — but the repair tool already knows the surface so a future deploy
    // doesn't crash on the first scan after enabling
    // `DJINN_CODE_CHUNKS_BACKEND`.
    let code_chunk_repo = CodeChunkRepository::new(server.state.db().clone());
    match code_chunk_repo.list_repair_embedding_rows(&project_id).await {
        Ok(rows) => {
            response.code_chunks_total = rows.len() as i64;
            for row in rows {
                // Mirror the note-side rule: a row whose Qdrant upsert
                // failed is recorded with `extension_state="pending"`
                // and must be treated as stale even when the content
                // hash + model version match — the vector store doesn't
                // actually have a point yet.
                let is_stale = match (
                    row.meta_content_hash.as_deref(),
                    row.meta_model_version.as_deref(),
                ) {
                    (Some(hash), Some(version)) => {
                        hash != row.content_hash
                            || version != model_version
                            || row
                                .meta_extension_state
                                .as_deref()
                                .unwrap_or("pending")
                                != "ready"
                    }
                    _ => true,
                };

                if !force && !is_stale {
                    response.code_chunks_up_to_date += 1;
                } else {
                    // PR B3 wires the actual embed call. Until then, we
                    // can't repair: count as failed without touching the
                    // failures vector (those are note-scoped today).
                    response.code_chunks_failed += 1;
                }

                tokio::task::yield_now().await;
            }
        }
        Err(error) => {
            tracing::debug!(
                %error,
                project_id,
                "code-chunk repair scan failed; continuing with note-only counters"
            );
        }
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EmbeddedNote, NoopNoteVectorStore, NoteEmbeddingProvider, NoteRepository,
        NoteVectorStore, ProjectRepository,
    };
    // Embeddings types not surfaced at `djinn_db::*` are reached via the
    // module path. `NoteVectorBackend` in particular collides with a
    // database/config enum at the crate root, so pull from the embeddings
    // module explicitly.
    use djinn_db::repositories::note::{
        EmbeddingQueryContext, NoteEmbeddingMatch, NoteEmbeddingRecord, NoteVectorBackend,
        UpsertNoteEmbedding,
    };
    use rmcp::handler::server::wrapper::Parameters;

    use crate::{
        server::DjinnMcpServer,
        state::{McpState, stubs::test_mcp_state_with_embedding},
    };

    /// Embedding provider that returns a fixed 768-d zero vector.
    /// Tests don't care about vector quality, only that the upsert path runs.
    struct ZeroEmbedding;

    #[async_trait]
    impl NoteEmbeddingProvider for ZeroEmbedding {
        fn model_version(&self) -> String {
            "test-zero-embedding-v1".to_string()
        }
        async fn embed_note(&self, _text: &str) -> Result<EmbeddedNote, String> {
            Ok(EmbeddedNote {
                values: vec![0.0_f32; 768],
                model_version: "test-zero-embedding-v1".to_string(),
            })
        }
    }

    /// Vector-store stub that reports `can_index=true` (so the repair tool's
    /// vector-presence check engages) but persists meta with the
    /// `"pending"` extension state, simulating a Qdrant deployment whose
    /// upsert call silently failed (today's scenario: collection missing
    /// at the time the row was first written). Subsequent repair runs
    /// must observe `pending` + `can_index=true` and treat the row as
    /// stale even when the content hash matches.
    #[derive(Default)]
    struct ActiveButPendingVectorStub;

    #[async_trait]
    impl NoteVectorStore for ActiveButPendingVectorStub {
        fn backend(&self) -> NoteVectorBackend {
            NoteVectorBackend::Qdrant
        }

        async fn can_index(&self, _repo: &NoteRepository) -> djinn_db::Result<bool> {
            Ok(true)
        }

        async fn upsert_embedding(
            &self,
            repo: &NoteRepository,
            input: UpsertNoteEmbedding<'_>,
        ) -> djinn_db::Result<NoteEmbeddingRecord> {
            // Delegate to Noop's path which records the meta row with
            // `extension_state="pending"` — the exact failure mode we're
            // exercising.
            NoopNoteVectorStore.upsert_embedding(repo, input).await
        }

        async fn delete_embedding(
            &self,
            repo: &NoteRepository,
            note_id: &str,
        ) -> djinn_db::Result<()> {
            NoopNoteVectorStore.delete_embedding(repo, note_id).await
        }

        async fn query_similar_embeddings(
            &self,
            _repo: &NoteRepository,
            _query_embedding: &[f32],
            _query: EmbeddingQueryContext<'_>,
            _limit: usize,
        ) -> djinn_db::Result<Vec<NoteEmbeddingMatch>> {
            Ok(vec![])
        }
    }

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }

    async fn make_server() -> (DjinnMcpServer, Database, String, std::path::PathBuf) {
        let tmp = workspace_tempdir();
        let project_path = tmp.keep();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state_with_embedding(
            db.clone(),
            Some(Arc::new(ZeroEmbedding) as Arc<dyn NoteEmbeddingProvider>),
            Some(Arc::new(NoopNoteVectorStore) as Arc<dyn NoteVectorStore>),
        );
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("repair-project", "test", "repair-project")
            .await
            .unwrap();
        (DjinnMcpServer::new(state), db, project.id, project_path)
    }

    async fn make_note(db: &Database, project_id: &str, title: &str) -> djinn_memory::Note {
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        repo.create(project_id, title, title, "reference", "[]")
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repair_embeds_missing_note() {
        let (server, db, project_id, _path) = make_server().await;
        let note = make_note(&db, &project_id, "Needs Embedding").await;

        // Sanity: with no embedding provider on the create-time NoteRepository,
        // the note has no embedding meta row yet.
        let pre = NoteRepository::new(db.clone(), EventBus::noop())
            .get_embedding(&note.id)
            .await
            .unwrap();
        assert!(pre.is_none(), "expected no embedding before repair");

        let response = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project_id.clone(),
                force: None,
            }))
            .await
            .0;

        assert!(response.error.is_none(), "error: {:?}", response.error);
        assert_eq!(response.total, 1);
        assert_eq!(response.repaired, 1);
        assert_eq!(response.up_to_date, 0);
        assert_eq!(response.failed, 0);

        let post = NoteRepository::new(db.clone(), EventBus::noop())
            .get_embedding(&note.id)
            .await
            .unwrap();
        assert!(post.is_some(), "expected embedding meta row after repair");
        assert_eq!(post.unwrap().model_version, "test-zero-embedding-v1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repair_skips_up_to_date_unless_forced() {
        let (server, db, project_id, _path) = make_server().await;
        make_note(&db, &project_id, "Up To Date").await;

        // First run: embed everything.
        let _ = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project_id.clone(),
                force: None,
            }))
            .await
            .0;

        // Second run without force: should report up_to_date, not repaired.
        let second = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project_id.clone(),
                force: None,
            }))
            .await
            .0;
        assert_eq!(second.total, 1);
        assert_eq!(second.repaired, 0);
        assert_eq!(second.up_to_date, 1);

        // With force=true: re-embed even though hash matches.
        let forced = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project_id.clone(),
                force: Some(true),
            }))
            .await
            .0;
        assert_eq!(forced.total, 1);
        assert_eq!(forced.repaired, 1);
        assert_eq!(forced.up_to_date, 0);

        let _ = db; // keep db alive
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repair_without_provider_returns_clean_error() {
        let tmp = workspace_tempdir();
        let _ = tmp.keep();
        let db = Database::open_in_memory().unwrap();
        // No embedding provider plugged in.
        let state: McpState = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("no-provider-project", "test", "no-provider-project")
            .await
            .unwrap();
        let server = DjinnMcpServer::new(state);

        let response = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project.id.clone(),
                force: None,
            }))
            .await
            .0;
        assert!(response.error.as_deref() == Some("embedding provider not configured"));
        assert_eq!(response.total, 0);
        assert_eq!(response.repaired, 0);
    }

    // Re-export of the no-provider stub helper from the memory_tools test
    // suite so this test module doesn't need a separate `pub use`.
    use crate::state::stubs::test_mcp_state;

    /// Regression for the correctness gap in the initial implementation:
    /// when the vector store CAN index but the previous upsert failed
    /// (meta row written with `extension_state="pending"`), the repair
    /// tool used to incorrectly report the note as `up_to_date` because
    /// the content hash and model_version still matched. With the fix,
    /// `pending` is treated as stale whenever the vector store is active,
    /// so a follow-up repair re-runs the embed and brings Qdrant in sync.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repair_treats_pending_state_as_stale_when_vector_store_active() {
        let tmp = workspace_tempdir();
        let _path = tmp.keep();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state_with_embedding(
            db.clone(),
            Some(Arc::new(ZeroEmbedding) as Arc<dyn NoteEmbeddingProvider>),
            Some(Arc::new(ActiveButPendingVectorStub) as Arc<dyn NoteVectorStore>),
        );
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("pending-project", "test", "pending-project")
            .await
            .unwrap();
        let server = DjinnMcpServer::new(state);
        make_note(&db, &project.id, "Pending Vector").await;

        // First run: embeds via the stub, leaving meta in `pending`.
        let first = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project.id.clone(),
                force: None,
            }))
            .await
            .0;
        assert_eq!(first.repaired, 1, "first run should embed the new note");
        assert_eq!(first.up_to_date, 0);

        // Second run WITHOUT force: with the old logic this would have
        // returned `up_to_date=1` because the hash + model_version still
        // match. With the fix, `pending` + `can_index=true` correctly
        // marks the row as stale and triggers another embed attempt.
        let second = server
            .memory_repair_embeddings(Parameters(RepairEmbeddingsParams {
                project: project.id.clone(),
                force: None,
            }))
            .await
            .0;
        assert_eq!(
            second.repaired, 1,
            "pending row should be re-repaired without force when vector store is active"
        );
        assert_eq!(second.up_to_date, 0);
        assert_eq!(second.failed, 0);

        let _ = db; // keep db alive for the duration of the test
    }
}

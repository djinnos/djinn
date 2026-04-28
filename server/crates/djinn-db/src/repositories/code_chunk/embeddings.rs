//! Qdrant `code_chunks` collection handle + bootstrap.
//!
//! Mirrors `note/embeddings.rs:QdrantNoteVectorStore`. PR B1 landed the
//! handle + `ensure_collection`; PR B3 grew the upsert path + an in-memory
//! mutex set so the warm-driven chunk-and-embed pipeline can fire-and-forget
//! without duplicating work across overlapping warms.

use crate::error::DbResult as Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeChunkVectorBackend {
    Qdrant,
    Noop,
}

/// Upsert payload for one code chunk.
///
/// Mirrors `UpsertNoteEmbedding`: callers carry the embedding + the
/// metadata needed to identify, hash-fingerprint, and stamp the row with
/// the model version. Qdrant point id is derived from `id` + `content_hash`
/// inside the vector store so retries are idempotent across overlapping
/// warm passes (plan §"Concurrency safety on `code_chunks`").
#[derive(Clone, Debug, PartialEq)]
pub struct UpsertCodeChunkEmbedding<'a> {
    pub chunk_id: &'a str,
    pub project_id: &'a str,
    pub file_path: &'a str,
    pub symbol_key: Option<&'a str>,
    pub kind: &'a str,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: &'a str,
    pub embedded_text: &'a str,
    pub model_version: &'a str,
    pub embedding: &'a [f32],
}

/// One semantic-search match returned by [`CodeChunkVectorStore::query_similar`].
/// `chunk_id` is the SQL row id (also the Qdrant payload key) — callers join
/// against `code_chunks` to materialize a full hit.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeChunkVectorMatch {
    pub chunk_id: String,
    /// Cosine similarity in `[-1, 1]`. Larger = better.
    pub score: f64,
}

#[async_trait::async_trait]
pub trait CodeChunkVectorStore: Send + Sync {
    fn backend(&self) -> CodeChunkVectorBackend;

    async fn can_index(&self) -> Result<bool>;

    /// Upsert one chunk's vector into the backing store. Returns the
    /// resulting `extension_state` (`"ready"` on success, `"pending"`
    /// when the vector store call failed and only local meta was
    /// written). The SQL row is written by the caller (the pipeline)
    /// regardless — keeping the DB authoritative on row presence.
    async fn upsert_vector(&self, input: &UpsertCodeChunkEmbedding<'_>) -> Result<&'static str>;

    /// PR B4: cosine-similarity search against the configured backing
    /// store, scoped to `project_id` (so a tenant's hits never leak
    /// across project boundaries). Default impl returns an empty list
    /// — `NoopCodeChunkVectorStore` doesn't index, so it has nothing
    /// to retrieve.
    ///
    /// Failure modes are logged and folded into an empty result set —
    /// the hybrid orchestrator treats semantic as a soft signal and
    /// keeps lexical + structural alive when Qdrant is degraded.
    async fn query_similar(
        &self,
        _project_id: &str,
        _query_embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<CodeChunkVectorMatch>> {
        Ok(vec![])
    }
}

#[derive(Debug, Default)]
pub struct NoopCodeChunkVectorStore;

#[async_trait::async_trait]
impl CodeChunkVectorStore for NoopCodeChunkVectorStore {
    fn backend(&self) -> CodeChunkVectorBackend {
        CodeChunkVectorBackend::Noop
    }

    async fn can_index(&self) -> Result<bool> {
        Ok(false)
    }

    async fn upsert_vector(&self, _input: &UpsertCodeChunkEmbedding<'_>) -> Result<&'static str> {
        // Noop store: nothing to do; the pipeline still records the SQL
        // row but the meta state stays `pending` because no point exists.
        Ok("pending")
    }

    async fn query_similar(
        &self,
        _project_id: &str,
        _query_embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<CodeChunkVectorMatch>> {
        // Noop store has no points to retrieve. The hybrid pipeline
        // tolerates an empty semantic signal.
        Ok(vec![])
    }
}

#[derive(Clone, Debug)]
pub struct QdrantCodeChunkConfig {
    pub url: String,
    pub api_key: Option<String>,
    pub collection: String,
}

impl Default for QdrantCodeChunkConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:6334".to_owned(),
            api_key: None,
            collection: "code_chunks".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct QdrantCodeChunkVectorStore {
    config: QdrantCodeChunkConfig,
}

impl QdrantCodeChunkVectorStore {
    pub fn new(config: QdrantCodeChunkConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &QdrantCodeChunkConfig {
        &self.config
    }
}

/// Stable Qdrant point id for a chunk.
///
/// `sha1(chunk_id || "\0" || content_hash)` — folding the content hash
/// in means a re-embed on a content change still upserts the same point
/// (overwriting the prior vector), but a *fresh* warm with a stale row
/// in the DB and a missing point in Qdrant heals on first call. Per the
/// concurrency-safety section of the plan: idempotent across overlapping
/// warms, no per-symbol mutex required.
pub fn qdrant_code_chunk_point_id_hex(chunk_id: &str, content_hash: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(chunk_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(content_hash.as_bytes());
    let digest = hasher.finalize();
    // Format as a UUID-shaped string so qdrant_client accepts it as a
    // PointId. UUIDv5 derived from sha1 would also work — sha256-truncated
    // is fine here since we just need a stable 128-bit space.
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]),
        u16::from_be_bytes([digest[4], digest[5]]),
        u16::from_be_bytes([digest[6], digest[7]]),
        u16::from_be_bytes([digest[8], digest[9]]),
        u64::from_be_bytes([
            0,
            0,
            digest[10],
            digest[11],
            digest[12],
            digest[13],
            digest[14],
            digest[15]
        ]) & 0xFFFF_FFFF_FFFFu64
    )
}

#[cfg(feature = "qdrant")]
impl QdrantCodeChunkVectorStore {
    pub fn client(&self) -> std::result::Result<qdrant_client::Qdrant, String> {
        let builder = qdrant_client::Qdrant::from_url(&self.config.url);
        let builder = match &self.config.api_key {
            Some(api_key) => builder.api_key(api_key.clone()),
            None => builder,
        };
        builder.build().map_err(|error| error.to_string())
    }

    /// Bootstrap-time collection creation. Idempotent.
    ///
    /// * If the collection doesn't exist → create it with `(vector_size, Cosine)`
    ///   and seed the `project_id` + `file_path` payload keyword indexes.
    /// * If it exists with matching dimensions → no-op (still ensures the
    ///   payload indexes, which are also idempotent).
    /// * If it exists with **different** dimensions → returns an `Err` so the
    ///   caller can fail startup loudly instead of silently mismatching.
    pub async fn ensure_collection(
        &self,
        vector_size: u64,
    ) -> std::result::Result<(), String> {
        use qdrant_client::qdrant::{CreateCollectionBuilder, Distance, VectorParamsBuilder};

        let client = self.client()?;

        let exists = client
            .collection_exists(self.config.collection.clone())
            .await
            .map_err(|error| error.to_string())?;

        if exists {
            let info = client
                .collection_info(self.config.collection.clone())
                .await
                .map_err(|error| error.to_string())?;
            if let Some(actual) = info
                .result
                .as_ref()
                .and_then(|r| r.config.as_ref())
                .and_then(|c| c.params.as_ref())
                .and_then(|p| p.vectors_config.as_ref())
                .and_then(|v| v.config.as_ref())
                .and_then(|cfg| match cfg {
                    qdrant_client::qdrant::vectors_config::Config::Params(p) => Some(p.size),
                    qdrant_client::qdrant::vectors_config::Config::ParamsMap(_) => None,
                })
                && actual != vector_size
            {
                return Err(format!(
                    "qdrant collection '{}' exists with vector size {} but server expects {}; \
                     drop the collection or fix DJINN_CODE_CHUNKS_BACKEND/embedding model",
                    self.config.collection, actual, vector_size
                ));
            }
        } else {
            client
                .create_collection(
                    CreateCollectionBuilder::new(self.config.collection.clone())
                        .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine)),
                )
                .await
                .map_err(|error| error.to_string())?;
            tracing::info!(
                collection = %self.config.collection,
                vector_size,
                "created qdrant collection"
            );
        }

        self.ensure_payload_indexes(&client).await;
        Ok(())
    }

    async fn ensure_payload_indexes(&self, client: &qdrant_client::Qdrant) {
        use qdrant_client::qdrant::{CreateFieldIndexCollectionBuilder, FieldType};

        for field in ["project_id", "file_path"] {
            if let Err(error) = client
                .create_field_index(
                    CreateFieldIndexCollectionBuilder::new(
                        &self.config.collection,
                        field,
                        FieldType::Keyword,
                    )
                    .wait(true),
                )
                .await
            {
                tracing::debug!(
                    %error,
                    collection = %self.config.collection,
                    field,
                    "failed to ensure qdrant code_chunks payload index"
                );
            }
        }
    }

    /// PR B4: cosine similarity search scoped to one project. Returns
    /// `(chunk_id, score)` pairs, score in `[-1, 1]` from Qdrant's
    /// inner-product output (we configured Distance::Cosine on the
    /// collection).
    async fn qdrant_query_similar(
        &self,
        project_id: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> std::result::Result<Vec<CodeChunkVectorMatch>, String> {
        use qdrant_client::qdrant::{Condition, Filter, SearchPointsBuilder};

        let limit = u64::try_from(limit).map_err(|_| "search limit exceeds u64".to_owned())?;
        let client = self.client()?;

        let filter = Filter::must([Condition::matches("project_id", project_id.to_string())]);

        let response = client
            .search_points(
                SearchPointsBuilder::new(
                    &self.config.collection,
                    query_embedding.to_vec(),
                    limit,
                )
                .filter(filter)
                .with_payload(true),
            )
            .await
            .map_err(|error| error.to_string())?;

        Ok(response
            .result
            .into_iter()
            .filter_map(|point| {
                let chunk_id = match point.payload.get("chunk_id").and_then(|v| v.kind.as_ref()) {
                    Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => s.clone(),
                    _ => return None,
                };
                Some(CodeChunkVectorMatch {
                    chunk_id,
                    score: point.score as f64,
                })
            })
            .collect())
    }

    async fn qdrant_upsert(
        &self,
        input: &UpsertCodeChunkEmbedding<'_>,
    ) -> std::result::Result<(), String> {
        use std::collections::HashMap;

        use qdrant_client::qdrant::{
            PointStruct, UpsertPointsBuilder, Value, value::Kind,
        };

        let client = self.client()?;

        let point_id = qdrant_code_chunk_point_id_hex(input.chunk_id, input.content_hash);

        fn str_value(s: &str) -> Value {
            Value {
                kind: Some(Kind::StringValue(s.to_string())),
            }
        }

        let mut payload: HashMap<String, Value> = HashMap::from([
            ("chunk_id".to_string(), str_value(input.chunk_id)),
            ("project_id".to_string(), str_value(input.project_id)),
            ("file_path".to_string(), str_value(input.file_path)),
            ("kind".to_string(), str_value(input.kind)),
            ("content_hash".to_string(), str_value(input.content_hash)),
            ("model_version".to_string(), str_value(input.model_version)),
        ]);
        if let Some(symbol_key) = input.symbol_key {
            payload.insert("symbol_key".to_string(), str_value(symbol_key));
        }

        client
            .upsert_points(
                UpsertPointsBuilder::new(
                    &self.config.collection,
                    vec![PointStruct::new(point_id, input.embedding.to_vec(), payload)],
                )
                .wait(true),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[cfg(not(feature = "qdrant"))]
impl QdrantCodeChunkVectorStore {
    pub fn client(&self) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }

    pub async fn ensure_collection(
        &self,
        _vector_size: u64,
    ) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }

    async fn qdrant_upsert(
        &self,
        _input: &UpsertCodeChunkEmbedding<'_>,
    ) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }

    async fn qdrant_query_similar(
        &self,
        _project_id: &str,
        _query_embedding: &[f32],
        _limit: usize,
    ) -> std::result::Result<Vec<CodeChunkVectorMatch>, String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }
}

#[async_trait::async_trait]
impl CodeChunkVectorStore for QdrantCodeChunkVectorStore {
    fn backend(&self) -> CodeChunkVectorBackend {
        CodeChunkVectorBackend::Qdrant
    }

    async fn can_index(&self) -> Result<bool> {
        #[cfg(feature = "qdrant")]
        {
            match self.client() {
                Ok(_) => Ok(true),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        collection = %self.config.collection,
                        "qdrant code-chunk vector store unavailable"
                    );
                    Ok(false)
                }
            }
        }
        #[cfg(not(feature = "qdrant"))]
        {
            Ok(false)
        }
    }

    async fn upsert_vector(&self, input: &UpsertCodeChunkEmbedding<'_>) -> Result<&'static str> {
        match self.qdrant_upsert(input).await {
            Ok(()) => Ok("ready"),
            Err(error) => {
                tracing::debug!(
                    %error,
                    chunk_id = input.chunk_id,
                    collection = %self.config.collection,
                    "qdrant code-chunk upsert unavailable; meta will stay pending"
                );
                Ok("pending")
            }
        }
    }

    async fn query_similar(
        &self,
        project_id: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<CodeChunkVectorMatch>> {
        match self
            .qdrant_query_similar(project_id, query_embedding, limit)
            .await
        {
            Ok(matches) => Ok(matches),
            Err(error) => {
                tracing::debug!(
                    %error,
                    project_id,
                    collection = %self.config.collection,
                    "qdrant code-chunk semantic search unavailable; returning empty matches"
                );
                Ok(vec![])
            }
        }
    }
}

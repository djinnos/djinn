//! Qdrant `code_chunks` collection handle + bootstrap.
//!
//! Mirrors `note/embeddings.rs:QdrantNoteVectorStore` but trimmed to the
//! surface PR B1 needs: a config, a typed handle, and `ensure_collection`.
//! Upsert/delete/search land in B3 once the chunker (B2) actually produces
//! rows.

use crate::error::DbResult as Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeChunkVectorBackend {
    Qdrant,
    Noop,
}

#[async_trait::async_trait]
pub trait CodeChunkVectorStore: Send + Sync {
    fn backend(&self) -> CodeChunkVectorBackend;

    async fn can_index(&self) -> Result<bool>;
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
}

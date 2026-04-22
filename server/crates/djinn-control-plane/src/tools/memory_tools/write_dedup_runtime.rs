use async_trait::async_trait;
use djinn_db::Database;
use djinn_provider::{CompletionRequest, CompletionResponse, complete, resolve_memory_provider};

#[async_trait]
pub(crate) trait MemoryWriteProviderRuntime: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, String>;
}

pub(crate) struct LlmMemoryWriteProviderRuntime {
    db: Database,
}

impl LlmMemoryWriteProviderRuntime {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MemoryWriteProviderRuntime for LlmMemoryWriteProviderRuntime {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, String> {
        let provider = resolve_memory_provider(&self.db)
            .await
            .map_err(|error| error.to_string())?;
        complete(provider.as_ref(), request)
            .await
            .map_err(|error| error.to_string())
    }
}

use async_trait::async_trait;
use djinn_db::Database;
use djinn_provider::provider::{LlmProvider, TelemetryMeta, create_provider};
use djinn_provider::{
    CompletionRequest, CompletionResponse, complete, resolve_memory_provider_for_user,
};

#[async_trait]
pub(crate) trait MemoryWriteProviderRuntime: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, String>;
}

pub(crate) struct LlmMemoryWriteProviderRuntime {
    db: Database,
    user_id: Option<String>,
}

impl LlmMemoryWriteProviderRuntime {
    pub(crate) fn new(db: Database, user_id: Option<String>) -> Self {
        Self { db, user_id }
    }

    fn telemetry_meta(&self) -> TelemetryMeta {
        TelemetryMeta {
            operation: Some("memory_write_dedup".to_string()),
            user_id: self.user_id.clone(),
            task_id: None,
            agent_type: Some("memory_write_dedup".to_string()),
            session_id: None,
        }
    }

    pub(crate) async fn resolve_provider(&self) -> Result<Box<dyn LlmProvider>, String> {
        let provider = resolve_memory_provider_for_user(&self.db, self.user_id.as_deref())
            .await
            .map_err(|error| error.to_string())?;

        let Some(mut config) = provider.config_snapshot() else {
            return Ok(provider);
        };
        config.telemetry = Some(self.telemetry_meta());
        Ok(create_provider(config))
    }
}

#[async_trait]
impl MemoryWriteProviderRuntime for LlmMemoryWriteProviderRuntime {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, String> {
        let provider = self.resolve_provider().await?;
        complete(provider.as_ref(), request)
            .await
            .map_err(|error| error.to_string())
    }
}

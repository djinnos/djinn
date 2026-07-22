use crate::bridge::{RuntimeOps, SemanticQueryEmbedding};

pub(crate) struct SemanticRuntimeOps {
    pub(crate) embedding: Vec<f32>,
}

pub(crate) struct FailingSemanticRuntimeOps;

#[async_trait::async_trait]
impl RuntimeOps for SemanticRuntimeOps {
    async fn apply_settings(&self, _: &djinn_core::models::DjinnSettings) -> Result<(), String> {
        Ok(())
    }

    async fn embed_memory_query(&self, _: &str) -> Result<Option<SemanticQueryEmbedding>, String> {
        Ok(Some(SemanticQueryEmbedding {
            values: self.embedding.clone(),
        }))
    }

    async fn reset_runtime_settings(&self) {}
    async fn persist_model_health_state(&self) {}
    async fn apply_environment_config(
        &self,
        _: &str,
        _: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_mirror_refresh(&self, _: &str) {}
    async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_graph_warm(&self, _: &str) {}
    async fn apply_user_model_change(&self) {}
    async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn list_taskrun_jobs(&self) -> Result<Vec<crate::bridge::TaskrunJobRef>, String> {
        Ok(Vec::new())
    }
    async fn cleanup_task_branches(&self, _: &str) {}
}

#[async_trait::async_trait]
impl RuntimeOps for FailingSemanticRuntimeOps {
    async fn apply_settings(&self, _: &djinn_core::models::DjinnSettings) -> Result<(), String> {
        Ok(())
    }

    async fn embed_memory_query(&self, _: &str) -> Result<Option<SemanticQueryEmbedding>, String> {
        Err("embedding model unavailable".to_string())
    }

    async fn reset_runtime_settings(&self) {}
    async fn persist_model_health_state(&self) {}
    async fn apply_environment_config(
        &self,
        _: &str,
        _: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_mirror_refresh(&self, _: &str) {}
    async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_graph_warm(&self, _: &str) {}
    async fn apply_user_model_change(&self) {}
    async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn list_taskrun_jobs(&self) -> Result<Vec<crate::bridge::TaskrunJobRef>, String> {
        Ok(Vec::new())
    }
    async fn cleanup_task_branches(&self, _: &str) {}
}

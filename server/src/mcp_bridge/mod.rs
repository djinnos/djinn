/// Bridge trait implementations: connect djinn-control-plane's abstract traits to
/// the server's concrete actor handles and managers.
///
/// Newtypes are required for CoordinatorHandle, SlotPoolHandle, and LspManager
/// because both the trait (djinn-control-plane) and the implementor (djinn-agent) are
/// external to the server — orphan rule.
/// AppState is a server-local type so it implements RuntimeOps and GitOps directly.
use std::path::Path;

use async_trait::async_trait;
use djinn_control_plane::bridge::{GitOps, RuntimeOps, SemanticQueryEmbedding, TaskrunJobRef};
use djinn_git::{GitActorHandle, GitError};

mod bridges;
mod extension_diagnostics_probe;
pub(crate) mod graph_neighbors;
mod graph_ops;
pub(crate) mod hybrid_search;
mod memory_enrichment;
mod refactor;
mod shared;
mod snapshot;
#[cfg(test)]
mod tests;

pub(crate) use self::graph_ops::RepoGraphBridge;
#[allow(unused_imports)]
pub(crate) use self::refactor::{
    RefactorCandidateInput, assign_refactor_tiers, compute_refactor_candidates,
};
pub(crate) use self::snapshot::build_snapshot_payload;

// ── AppState → RuntimeOps + GitOps + mcp_state() ─────────────────────────────

use crate::server::AppState;

#[async_trait]
impl RuntimeOps for AppState {
    async fn apply_settings(
        &self,
        settings: &djinn_core::models::DjinnSettings,
    ) -> Result<(), String> {
        AppState::apply_settings(self, settings).await
    }

    async fn embed_memory_query(
        &self,
        query: &str,
    ) -> Result<Option<SemanticQueryEmbedding>, String> {
        match self.embedding_service().embed_query(query).await {
            djinn_provider::embeddings::EmbeddingOutcome::Ready(vector) => {
                Ok(Some(SemanticQueryEmbedding {
                    values: vector.values,
                }))
            }
            djinn_provider::embeddings::EmbeddingOutcome::Degraded(_) => Ok(None),
        }
    }

    async fn reset_runtime_settings(&self) {
        AppState::reset_runtime_settings(self).await;
    }

    async fn apply_user_model_change(&self) {
        AppState::apply_user_model_change(self).await;
    }

    async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), String> {
        // The K8s graph warmer is the server component that owns the live kube
        // client; the in-process warmer's default impl returns a clear
        // unsupported error for dev/test runtimes without Kubernetes.
        self.graph_warmer()
            .await
            .teardown_taskrun_job(task_run_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_taskrun_jobs(&self) -> Result<Vec<TaskrunJobRef>, String> {
        let jobs = self
            .graph_warmer()
            .await
            .list_taskrun_jobs()
            .await
            .map_err(|e| e.to_string())?;
        Ok(jobs
            .into_iter()
            .map(|job| TaskrunJobRef {
                job_name: job.job_name,
                task_run_id: job.task_run_id,
                created_at: job.created_at,
            })
            .collect())
    }

    async fn cleanup_task_branches(&self, task_id: &str) {
        let mirror = self.mirror();
        djinn_agent::task_merge::cleanup_task_branches_post_close(
            task_id,
            self.db(),
            &self.event_bus(),
            Some(mirror.as_ref()),
        )
        .await;
    }

    async fn persist_model_health_state(&self) {
        AppState::persist_model_health_state(self).await;
    }

    async fn apply_environment_config(
        &self,
        project_id: &str,
        config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        // Route through the image-controller in prod so the runtime
        // ConfigMap gets upserted alongside the DB write. In dev mode
        // without a kube client there's no CM to reconcile; just write
        // the DB.
        if let Some(controller) = self.image_controller().await {
            controller
                .apply_environment_config(project_id, config)
                .await
                .map_err(|e| e.to_string())
        } else {
            let repo = djinn_db::ProjectRepository::new(
                self.db().clone(),
                djinn_core::events::EventBus::noop(),
            );
            let json = serde_json::to_string(config)
                .map_err(|e| format!("serialize environment_config: {e}"))?;
            repo.set_environment_config(project_id, &json)
                .await
                .map_err(|e| format!("db write: {e}"))
        }
    }

    async fn trigger_mirror_refresh(&self, project_id: &str) {
        // Fire-and-forget: a fresh mirror clone + stack detection + image
        // enqueue can take many seconds, and the caller (project_add) wants a
        // snappy response. Errors are logged and swallowed — the periodic
        // mirror-fetch tick retries anything that fails here.
        let state = self.clone();
        let project_id = project_id.to_string();
        tokio::spawn(async move {
            match crate::mirror_fetcher::fetch_project(&state, &project_id).await {
                Ok(true) => {
                    tracing::info!(project_id, "post-add mirror refresh complete")
                }
                Ok(false) => tracing::debug!(
                    project_id,
                    "post-add mirror refresh skipped: project not GitHub-linked yet"
                ),
                Err(err) => tracing::warn!(
                    project_id,
                    error = %err,
                    "post-add mirror refresh failed; periodic tick will retry"
                ),
            }
        });
    }

    async fn enqueue_image_build(&self, image_id: &str) -> Result<(), String> {
        // No controller in dev mode (no kube client) — the badge stays
        // `none` locally, which is correct: nothing builds images locally.
        let Some(controller) = self.image_controller().await else {
            return Ok(());
        };
        let image_repo = djinn_db::ImageRepository::new(self.db().clone());
        let image = image_repo
            .get(image_id)
            .await
            .map_err(|e| format!("get image {image_id}: {e}"))?
            .ok_or_else(|| format!("image not found: {image_id}"))?;
        controller
            .enqueue_image(image_id.to_string(), &image_repo, image)
            .await
            .map_err(|e| e.to_string())
    }

    async fn trigger_graph_warm(&self, project_id: &str) {
        // Fire-and-forget: the warm Job dispatch + watch can take a while and
        // the caller (image assignment) wants a snappy response. The warmer's
        // own freshness gate + single-flight guard make this cheap if nothing
        // changed or the image isn't ready yet.
        let warmer = self.graph_warmer().await;
        let project_id = project_id.to_string();
        tokio::spawn(async move {
            warmer.trigger(&project_id).await;
        });
    }
}

#[async_trait]
impl GitOps for AppState {
    async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        AppState::git_actor(self, path).await
    }
}

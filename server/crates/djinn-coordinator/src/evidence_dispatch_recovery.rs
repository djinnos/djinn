//! Re-drive typed evidence allocations without manufacturing a replacement task.

use super::actor::CoordinatorActor;
use djinn_db::{
    DispatchTypedEvidenceDemandInput, DispatchTypedEvidenceRetryInput, TaskRepository,
    TypedEvidenceDemandDispatchErrorInput, TypedEvidenceRepository,
    TypedEvidenceRetryDispatchErrorInput,
};

impl CoordinatorActor {
    /// Re-enqueue every exact typed attempt that remains `demanded`.
    ///
    /// The allocation is the durability boundary: a pool failure records an
    /// append-only error and leaves it demanded, while only an accepted enqueue
    /// calls the repository transition primitive.
    pub(super) async fn redrive_demanded_evidence_dispatches(&mut self) {
        let typed = TypedEvidenceRepository::new(self.db.clone());
        let allocations = match typed.demanded_dispatches().await {
            Ok(allocations) => allocations,
            Err(error) => {
                tracing::warn!(%error, "failed to inventory demanded typed evidence attempts");
                return;
            }
        };
        let tasks = TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        for allocation in allocations {
            // The repository inventory excludes closed tasks. Re-read the task
            // before enqueue so a concurrent close is never reopened.
            let Some(task) = tasks.get(&allocation.spike_task_id).await.ok().flatten() else {
                continue;
            };
            if task.status == "closed" || task.issue_type != "spike" {
                continue;
            }
            let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
                self.append_evidence_enqueue_error(
                    &typed,
                    &allocation,
                    "project path unavailable".into(),
                )
                .await;
                continue;
            };
            let models = self
                .resolve_dispatch_models_for_role(
                    "architect",
                    Some(task.created_by_user_id.as_str()),
                )
                .await;
            let Some(model_id) = models.first() else {
                self.append_evidence_enqueue_error(
                    &typed,
                    &allocation,
                    "no eligible architect model".into(),
                )
                .await;
                continue;
            };
            match self.pool.dispatch(&task.id, &project_path, model_id).await {
                Ok(()) if allocation.is_retry => {
                    let Ok(mut tx) = self.db.pool().begin().await else {
                        continue;
                    };
                    let result = TypedEvidenceRepository::dispatch_retry_success_in_transaction(
                        &mut tx,
                        DispatchTypedEvidenceRetryInput {
                            finding_id: allocation.finding_id.clone(),
                            attempt_id: allocation.attempt_id.clone(),
                            spike_task_id: allocation.spike_task_id.clone(),
                            transition_id: uuid::Uuid::now_v7().to_string(),
                            actor_task_id: None,
                        },
                    )
                    .await;
                    if result.is_ok() {
                        let _ = tx.commit().await;
                    }
                }
                Ok(()) => {
                    let Ok(mut tx) = self.db.pool().begin().await else {
                        continue;
                    };
                    let result = TypedEvidenceRepository::dispatch_demand_success_in_transaction(
                        &mut tx,
                        DispatchTypedEvidenceDemandInput {
                            finding_id: allocation.finding_id.clone(),
                            attempt_id: allocation.attempt_id.clone(),
                            spike_task_id: allocation.spike_task_id.clone(),
                            transition_id: uuid::Uuid::now_v7().to_string(),
                            actor_task_id: None,
                        },
                    )
                    .await;
                    if result.is_ok() {
                        let _ = tx.commit().await;
                    }
                }
                Err(error) => {
                    self.append_evidence_enqueue_error(&typed, &allocation, error.to_string())
                        .await
                }
            }
        }
    }

    async fn append_evidence_enqueue_error(
        &self,
        typed: &TypedEvidenceRepository,
        allocation: &djinn_db::DemandedTypedEvidenceDispatch,
        error: String,
    ) {
        if allocation.is_retry {
            let _ = typed
                .append_retry_dispatch_error(TypedEvidenceRetryDispatchErrorInput {
                    finding_id: allocation.finding_id.clone(),
                    attempt_id: allocation.attempt_id.clone(),
                    spike_task_id: allocation.spike_task_id.clone(),
                    error,
                })
                .await;
        } else {
            let _ = typed
                .append_demand_dispatch_error(TypedEvidenceDemandDispatchErrorInput {
                    finding_id: allocation.finding_id.clone(),
                    attempt_id: allocation.attempt_id.clone(),
                    spike_task_id: allocation.spike_task_id.clone(),
                    error,
                })
                .await;
        }
    }
}

//! Re-drive typed evidence allocations without manufacturing a replacement task.

use super::actor::CoordinatorActor;
use djinn_db::{
    DispatchTypedEvidenceDemandInput, DispatchTypedEvidenceRetryInput, TaskRepository,
    TypedEvidenceDemandDispatchErrorInput, TypedEvidenceRepository,
    TypedEvidenceRetryDispatchErrorInput,
};
use djinn_slot::PoolError;

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) enum EvidenceDispatchTestOutcome {
    Accepted,
    EnqueueFailed,
    AlreadyActive,
}

#[cfg(test)]
#[derive(Default)]
struct EvidenceDispatchTestScript {
    outcomes: std::collections::VecDeque<EvidenceDispatchTestOutcome>,
    fail_activation_once: bool,
    dispatches: usize,
}

#[cfg(test)]
static EVIDENCE_DISPATCH_TEST_SCRIPTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, EvidenceDispatchTestScript>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(super) fn set_evidence_dispatch_test_script(
    task_id: &str,
    outcomes: impl IntoIterator<Item = EvidenceDispatchTestOutcome>,
    fail_activation_once: bool,
) {
    EVIDENCE_DISPATCH_TEST_SCRIPTS
        .lock()
        .expect("evidence dispatch test seam lock")
        .insert(
            task_id.to_owned(),
            EvidenceDispatchTestScript {
                outcomes: outcomes.into_iter().collect(),
                fail_activation_once,
                dispatches: 0,
            },
        );
}

#[cfg(test)]
pub(super) fn evidence_dispatch_test_count(task_id: &str) -> usize {
    EVIDENCE_DISPATCH_TEST_SCRIPTS
        .lock()
        .expect("evidence dispatch test seam lock")
        .get(task_id)
        .map(|script| script.dispatches)
        .unwrap_or_default()
}

#[cfg(test)]
fn scripted_dispatch(task_id: &str) -> Option<Result<(), PoolError>> {
    let mut scripts = EVIDENCE_DISPATCH_TEST_SCRIPTS
        .lock()
        .expect("evidence dispatch test seam lock");
    let script = scripts.get_mut(task_id)?;
    script.dispatches += 1;
    Some(match script.outcomes.pop_front() {
        Some(EvidenceDispatchTestOutcome::Accepted) => Ok(()),
        Some(EvidenceDispatchTestOutcome::EnqueueFailed) => Err(PoolError::ActorDead),
        Some(EvidenceDispatchTestOutcome::AlreadyActive) => Err(PoolError::SessionAlreadyActive {
            task_id: task_id.to_owned(),
        }),
        None => panic!("evidence dispatch test script exhausted for {task_id}"),
    })
}

#[cfg(test)]
fn fail_scripted_activation(task_id: &str) -> bool {
    let mut scripts = EVIDENCE_DISPATCH_TEST_SCRIPTS
        .lock()
        .expect("evidence dispatch test seam lock");
    let Some(script) = scripts.get_mut(task_id) else {
        return false;
    };
    std::mem::take(&mut script.fail_activation_once)
}

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
            #[cfg(test)]
            let dispatch = match scripted_dispatch(&task.id) {
                Some(result) => result,
                None => self.pool.dispatch(&task.id, &project_path, model_id).await,
            };
            #[cfg(not(test))]
            let dispatch = self.pool.dispatch(&task.id, &project_path, model_id).await;
            match dispatch {
                Ok(()) => self.activate_evidence_dispatch(&allocation).await,
                // A previous delivery can reach the pool but lose its database
                // commit. The pool's exact-task active result is therefore an
                // acknowledgement, not another dispatch failure.
                Err(PoolError::SessionAlreadyActive { task_id }) if task_id == task.id => {
                    self.activate_evidence_dispatch(&allocation).await;
                }
                Err(error) => {
                    self.append_evidence_enqueue_error(&typed, &allocation, error.to_string())
                        .await;
                }
            }
        }
    }

    /// Persist the exact repository-owned activation after pool acceptance.
    /// A transition or commit failure deliberately leaves the allocation
    /// demanded, so duplicate delivery/restart can use `SessionAlreadyActive`
    /// to retry this transition without dispatching a replacement task.
    async fn activate_evidence_dispatch(
        &self,
        allocation: &djinn_db::DemandedTypedEvidenceDispatch,
    ) {
        #[cfg(test)]
        if fail_scripted_activation(&allocation.spike_task_id) {
            tracing::warn!(finding_id=%allocation.finding_id, attempt_id=%allocation.attempt_id, "injected accepted evidence activation failure");
            return;
        }
        let mut tx = match self.db.pool().begin().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::warn!(%error, finding_id=%allocation.finding_id, attempt_id=%allocation.attempt_id, "accepted evidence enqueue could not begin activation transaction");
                return;
            }
        };
        let result = if allocation.is_retry {
            TypedEvidenceRepository::dispatch_retry_success_in_transaction(
                &mut tx,
                DispatchTypedEvidenceRetryInput {
                    finding_id: allocation.finding_id.clone(),
                    attempt_id: allocation.attempt_id.clone(),
                    spike_task_id: allocation.spike_task_id.clone(),
                    transition_id: uuid::Uuid::now_v7().to_string(),
                    actor_task_id: None,
                },
            )
            .await
        } else {
            TypedEvidenceRepository::dispatch_demand_success_in_transaction(
                &mut tx,
                DispatchTypedEvidenceDemandInput {
                    finding_id: allocation.finding_id.clone(),
                    attempt_id: allocation.attempt_id.clone(),
                    spike_task_id: allocation.spike_task_id.clone(),
                    transition_id: uuid::Uuid::now_v7().to_string(),
                    actor_task_id: None,
                },
            )
            .await
        };
        if let Err(error) = result {
            tracing::warn!(%error, finding_id=%allocation.finding_id, attempt_id=%allocation.attempt_id, "accepted evidence enqueue could not persist activation");
            return;
        }
        if let Err(error) = tx.commit().await {
            tracing::warn!(%error, finding_id=%allocation.finding_id, attempt_id=%allocation.attempt_id, "accepted evidence enqueue could not commit activation");
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

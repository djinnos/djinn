use super::*;
use crate::actors::slot::run_task_lifecycle;

impl AgentSupervisor {
    pub(super) async fn dispatch(
        &mut self,
        task_id: String,
        project_path: String,
        model_id: String,
    ) -> Result<(), SupervisorError> {
        if self.sessions.contains_key(&task_id) || self.lifecycle_handles.contains_key(&task_id) {
            return Err(SupervisorError::SessionAlreadyActive { task_id });
        }

        self.in_flight.insert(task_id.clone());

        let max_for_model = self.max_for_model(&model_id);
        let (active, max) = {
            let entry = self
                .capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity {
                    active: 0,
                    max: max_for_model,
                });
            (entry.active, entry.max)
        };
        if active >= max {
            return Err(SupervisorError::ModelAtCapacity {
                model_id,
                active,
                max,
            });
        }

        if model_id == "test/mock" {
            if let Some(entry) = self.capacity.get_mut(&model_id) {
                entry.active += 1;
            }
            self.spawn_mock_session(task_id, model_id);
            return Ok(());
        }

        let task = self.load_task(&task_id).await?;

        let kill = CancellationToken::new();
        let pause = CancellationToken::new();
        let join = tokio::spawn(run_task_lifecycle(
            task_id.clone(),
            project_path,
            model_id.clone(),
            self.app_state.clone(),
            self.session_manager.clone(),
            kill.clone(),
            pause.clone(),
            self.slot_event_tx.clone(),
        ));

        if let Some(entry) = self.capacity.get_mut(&model_id) {
            entry.active += 1;
        }

        self.lifecycle_handles.insert(
            task_id,
            LifecycleHandle {
                join,
                kill,
                pause,
                model_id,
                project_id: task.project_id,
                started_at: Instant::now(),
            },
        );

        Ok(())
    }
}

use super::*;

impl AgentSupervisor {
    pub(super) async fn load_task(&self, task_id: &str) -> Result<Task, SupervisorError> {
        let repo =
            TaskRepository::new(self.app_state.db().clone(), self.app_state.events().clone());
        let task = repo
            .get(task_id)
            .await
            .map_err(|e| SupervisorError::Goose(e.to_string()))?;
        task.ok_or_else(|| SupervisorError::TaskNotFound {
            task_id: task_id.to_owned(),
        })
    }

    pub(super) async fn project_path_for_id(&self, project_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?1")
            .bind(project_id)
            .fetch_optional(self.app_state.db().pool())
            .await
            .ok()
            .flatten()
    }
}

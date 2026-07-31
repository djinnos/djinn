use super::*;

impl TaskRepository {
    /// Allocate the next durable execution generation for dispatch admission.
    ///
    /// The canonical task row is locked before advancing its counter, so a
    /// concurrent kill fence or dispatch allocation cannot return the same
    /// generation.
    pub async fn allocate_execution_generation(&self, task_id: &str) -> Result<i64> {
        self.advance_execution_generation(task_id).await
    }

    /// Advance the durable execution generation to fence an admitted dispatch.
    ///
    /// This deliberately does not record a killed task state: the returned
    /// generation is the fence that rejects stale dispatch work.
    pub async fn fence_execution_generation_for_kill(&self, task_id: &str) -> Result<i64> {
        self.advance_execution_generation(task_id).await
    }

    async fn advance_execution_generation(&self, task_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let found: Option<i64> =
            sqlx::query_scalar("SELECT execution_generation FROM tasks WHERE id = $1 FOR UPDATE")
                .bind(task_id)
                .fetch_optional(&mut *tx)
                .await?;
        if found.is_none() {
            return Err(Error::InvalidData(format!("task not found: {task_id}")));
        }

        let generation: i64 = sqlx::query_scalar(
            "UPDATE tasks \
             SET execution_generation = execution_generation + 1 \
             WHERE id = $1 \
             RETURNING execution_generation",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(generation)
    }
}

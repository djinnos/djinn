use super::*;

impl TaskRepository {
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        epic_id: &str,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
        status: Option<&str>,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let project_id =
            sqlx::query_scalar::<_, String>("SELECT project_id FROM epics WHERE id = ?1")
                .bind(epic_id)
                .fetch_optional(self.db.pool())
                .await?
                .ok_or_else(|| Error::Internal(format!("epic not found: {epic_id}")))?;
        self.create_in_project(
            &project_id,
            Some(epic_id),
            title,
            description,
            design,
            issue_type,
            priority,
            owner,
            status,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_in_project(
        &self,
        project_id: &str,
        epic_id: Option<&str>,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
        status: Option<&str>,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        sqlx::query(
            "INSERT INTO tasks
                (id, project_id, short_id, epic_id, title, description, design,
                 issue_type, priority, owner, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, COALESCE(?11, 'backlog'))",
        )
        .bind(&id)
        .bind(project_id)
        .bind(&short_id)
        .bind(epic_id)
        .bind(title)
        .bind(description)
        .bind(design)
        .bind(issue_type)
        .bind(priority)
        .bind(owner)
        .bind(status)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(self.db.pool())
            .await?;

        if let Some(epic_id) = epic_id {
            let epic_repo = EpicRepository::new(self.db.clone(), self.events.clone());
            if let Some(epic) = epic_repo.get(epic_id).await?
                && (epic.status == "closed" || epic.status == "in_review")
            {
                let _ = epic_repo.reopen(epic_id).await?;
            }
        }

        let _ = self.events.send(DjinnEvent::TaskCreated { task: task.clone(), from_sync: false });
        Ok(task)
    }

    /// Test helper: create a task with a specific short_id.
    /// This bypasses the normal short_id generation for testing collision scenarios.
    #[cfg(test)]
    pub async fn create_with_short_id(
        &self,
        id: &str,
        project_id: &str,
        title: &str,
        status: &str,
        short_id: &str,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO tasks
                (id, project_id, short_id, epic_id, title, description, design,
                 issue_type, priority, owner, status, continuation_count, memory_refs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, '[]')",
        )
        .bind(id)
        .bind(project_id)
        .bind(short_id)
        .bind(None::<&str>) // epic_id
        .bind(title)
        .bind("") // description
        .bind("") // design
        .bind("task") // issue_type
        .bind(1i64) // priority
        .bind("") // owner
        .bind(status)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskCreated { task: task.clone(), from_sync: false });
        Ok(task)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        id: &str,
        title: &str,
        description: &str,
        design: &str,
        priority: i64,
        owner: &str,
        labels: &str,
        acceptance_criteria: &str,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET
                title = ?2, description = ?3, design = ?4,
                priority = ?5, owner = ?6, labels = ?7,
                acceptance_criteria = ?8,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(design)
        .bind(priority)
        .bind(owner)
        .bind(labels)
        .bind(acceptance_criteria)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false });
        Ok(task)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM tasks WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        let _ = self
            .events
            .send(DjinnEvent::TaskDeleted { id: id.to_owned() });
        Ok(())
    }

    /// Store the squash-merge commit SHA for a task after merge completes.
    pub async fn set_merge_commit_sha(&self, id: &str, sha: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET merge_commit_sha = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(sha)
        .execute(self.db.pool())
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false });
        Ok(task)
    }

    /// Increment `continuation_count` by 1 (used by compaction).
    pub async fn increment_continuation_count(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET continuation_count = continuation_count + 1,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Replace the `memory_refs` JSON array on a task.
    pub async fn update_memory_refs(&self, id: &str, memory_refs_json: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET memory_refs = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(memory_refs_json)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false });
        Ok(task)
    }
}

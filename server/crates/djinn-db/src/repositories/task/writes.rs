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
        self.create_with_ac(
            epic_id,
            title,
            description,
            design,
            issue_type,
            priority,
            owner,
            status,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_ac(
        &self,
        epic_id: &str,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
        status: Option<&str>,
        acceptance_criteria: Option<&str>,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let project_id =
            sqlx::query_scalar::<_, String>("SELECT project_id FROM epics WHERE id = ?")
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
            acceptance_criteria,
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
        acceptance_criteria: Option<&str>,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let ac = acceptance_criteria.unwrap_or("[]");
        sqlx::query(
            "INSERT INTO tasks
                (id, project_id, short_id, epic_id, title, description, design,
                 issue_type, priority, owner, `status`, acceptance_criteria)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, 'open'), ?)",
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
        .bind(ac)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(self.db.pool())
            .await?;

        if let Some(epic_id) = epic_id {
            maybe_reopen_epic(&self.db, &self.events, epic_id).await?;
        }

        self.events
            .send(DjinnEventEnvelope::task_created(&task, false));
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
                 issue_type, priority, owner, `status`, continuation_count, memory_refs)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, '[]')",
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

        self.events
            .send(DjinnEventEnvelope::task_created(&task, false));
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
                title = ?, description = ?, design = ?,
                priority = ?, owner = ?, labels = ?,
                acceptance_criteria = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(title)
        .bind(description)
        .bind(design)
        .bind(priority)
        .bind(owner)
        .bind(labels)
        .bind(acceptance_criteria)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::task_deleted(id));
        Ok(())
    }

    /// Store the squash-merge commit SHA for a task after merge completes.
    pub async fn set_merge_commit_sha(&self, id: &str, sha: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET merge_commit_sha = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(sha)
        .bind(id)
        .execute(self.db.pool())
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Store the GitHub PR URL for a task after PR creation.
    ///
    /// Set when the GitHub App is connected and a PR is opened instead of
    /// using the direct-push merge path.
    pub async fn set_pr_url(&self, id: &str, url: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET pr_url = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(url)
        .bind(id)
        .execute(self.db.pool())
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Set or clear the merge conflict metadata JSON on a task.
    ///
    /// Used by the worktree lifecycle when a rebase detects conflicts
    /// (outside of a state-machine transition).
    pub async fn set_merge_conflict_metadata(
        &self,
        id: &str,
        metadata: Option<&str>,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET merge_conflict_metadata = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(metadata)
        .bind(id)
        .execute(self.db.pool())
        .await?;

        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Increment `continuation_count` by 1 (used by compaction).
    pub async fn increment_continuation_count(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET continuation_count = continuation_count + 1,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;

        let task = self
            .get(id)
            .await?
            .ok_or_else(|| Error::Internal(format!("task not found: {id}")))?;
        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));

        Ok(())
    }

    /// Set or clear the `agent_type` specialist name on a task.
    pub async fn update_agent_type(&self, id: &str, agent_type: Option<&str>) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET agent_type = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(agent_type)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Replace the `memory_refs` JSON array on a task.
    pub async fn update_memory_refs(&self, id: &str, memory_refs_json: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE tasks SET memory_refs = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(memory_refs_json)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        let task: Task = sqlx::query_as(TASK_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }
}

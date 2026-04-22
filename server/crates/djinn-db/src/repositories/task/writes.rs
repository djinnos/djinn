use super::task_select_where_id;
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
        let project_id = sqlx::query_scalar!("SELECT project_id FROM epics WHERE id = ?", epic_id)
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
        let ac = acceptance_criteria.unwrap_or("[]").to_owned();
        // Phase 3B: stamp `created_by_user_id` from the task-local set at
        // the MCP dispatch root (`SESSION_USER_ID`). `None` for
        // agent/background callers with no user context — schema allows
        // NULL and Phase 4 will tighten where appropriate.
        let created_by_user_id = djinn_core::auth_context::current_user_id();

        let project_id_owned = project_id.to_owned();
        let epic_id_owned = epic_id.map(|s| s.to_owned());
        let title_owned = title.to_owned();
        let description_owned = description.to_owned();
        let design_owned = design.to_owned();
        let issue_type_owned = issue_type.to_owned();
        let owner_owned = owner.to_owned();
        let status_owned = status.map(|s| s.to_owned());

        // Retry on Dolt 1213: the INSERT hits the hot `tasks` table and
        // autocommits; concurrent writers routinely trip serialization
        // failures. The INSERT is idempotent because `id` is fixed for this
        // call — a retry that succeeds after a prior partial commit would
        // hit a UNIQUE violation, which `is_serialization_failure` does not
        // match, so the original error surfaces unchanged. The follow-up
        // SELECT is retried independently.
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id.clone();
                let project_id = project_id_owned.clone();
                let epic_id = epic_id_owned.clone();
                let short_id = short_id.clone();
                let title = title_owned.clone();
                let description = description_owned.clone();
                let design = design_owned.clone();
                let issue_type = issue_type_owned.clone();
                let owner = owner_owned.clone();
                let status = status_owned.clone();
                let ac = ac.clone();
                let created_by_user_id = created_by_user_id.clone();
                async move {
                    sqlx::query!(
                        "INSERT INTO tasks
                            (id, project_id, short_id, epic_id, title, description, design,
                             issue_type, priority, owner, `status`, acceptance_criteria,
                             created_by_user_id)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, 'open'), ?, ?)",
                        id,
                        project_id,
                        short_id,
                        epic_id,
                        title,
                        description,
                        design,
                        issue_type,
                        priority,
                        owner,
                        status,
                        ac,
                        created_by_user_id
                    )
                    .execute(self.db.pool())
                    .await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id.clone();
                async move {
                    let task: Task =
                        task_select_where_id!(&id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
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
        let empty = "";
        let epic_id_none: Option<&str> = None;
        let issue_type = "task";
        let priority = 1_i64;
        sqlx::query!(
            "INSERT INTO tasks
                (id, project_id, short_id, epic_id, title, description, design,
                 issue_type, priority, owner, `status`, continuation_count, memory_refs)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, '[]')",
            id,
            project_id,
            short_id,
            epic_id_none,
            title,
            empty,
            empty,
            issue_type,
            priority,
            empty,
            status
        )
        .execute(self.db.pool())
        .await?;
        let task: Task = task_select_where_id!(id).fetch_one(self.db.pool()).await?;

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
        let id_owned = id.to_owned();
        let title_owned = title.to_owned();
        let description_owned = description.to_owned();
        let design_owned = design.to_owned();
        let owner_owned = owner.to_owned();
        let labels_owned = labels.to_owned();
        let ac_owned = acceptance_criteria.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let title = title_owned.clone();
                let description = description_owned.clone();
                let design = design_owned.clone();
                let owner = owner_owned.clone();
                let labels = labels_owned.clone();
                let acceptance_criteria = ac_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET
                            title = ?, description = ?, design = ?,
                            priority = ?, owner = ?, labels = ?,
                            acceptance_criteria = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        title,
                        description,
                        design,
                        priority,
                        owner,
                        labels,
                        acceptance_criteria,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();

        // DELETE is idempotent; retry on Dolt 1213.
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                async move {
                    sqlx::query!("DELETE FROM tasks WHERE id = ?", id)
                        .execute(self.db.pool())
                        .await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;

        self.events.send(DjinnEventEnvelope::task_deleted(id));
        Ok(())
    }

    /// Store the squash-merge commit SHA for a task after merge completes.
    pub async fn set_merge_commit_sha(&self, id: &str, sha: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let sha_owned = sha.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let sha = sha_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET merge_commit_sha = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        sha,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
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
        let id_owned = id.to_owned();
        let url_owned = url.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let url = url_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET pr_url = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        url,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
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
        let id_owned = id.to_owned();
        let metadata_owned = metadata.map(|s| s.to_owned());

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let metadata = metadata_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET merge_conflict_metadata = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        metadata,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Increment `continuation_count` by 1 (used by compaction).
    pub async fn increment_continuation_count(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();

        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET continuation_count = continuation_count + 1,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
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
        let id_owned = id.to_owned();
        let agent_type_owned = agent_type.map(|s| s.to_owned());

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let agent_type = agent_type_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET agent_type = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        agent_type,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Replace the `acceptance_criteria` JSON on a task.
    ///
    /// Focused setter used by the reviewer finalize path (which rewrites the
    /// whole AC array after a `submit_review` tool call) and by tests that
    /// seed AC directly. Production writes that merge AC into a wider
    /// update should go through [`Self::update`].
    pub async fn set_acceptance_criteria(&self, id: &str, ac_json: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let ac_owned = ac_json.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let ac = ac_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET acceptance_criteria = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        ac,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Force-set `continuation_count` on a task.
    ///
    /// Used by tests that need to seed a specific continuation value;
    /// production code mutates this through [`Self::increment_continuation_count`].
    pub async fn set_continuation_count(&self, id: &str, count: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();

        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET continuation_count = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        count,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;
        Ok(())
    }

    /// Force-set `verification_failure_count` on a task.
    ///
    /// Used by tests that drive the verification-failure accounting pipeline;
    /// production code increments this through the transition path.
    pub async fn set_verification_failure_count(&self, id: &str, count: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();

        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET verification_failure_count = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        count,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;
        Ok(())
    }

    /// Reset the reopen/continuation counters and bump intervention counters
    /// atomically. Used by the `task_reset_counters` admin tool when a human
    /// intervenes on a stuck task.
    pub async fn reset_intervention_counters(&self, id: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET
                            reopen_count = 0,
                            continuation_count = 0,
                            intervention_count = intervention_count + 1,
                            last_intervention_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }

    /// Replace the `memory_refs` JSON array on a task.
    pub async fn update_memory_refs(&self, id: &str, memory_refs_json: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let memory_refs_owned = memory_refs_json.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let memory_refs_json = memory_refs_owned.clone();
                async move {
                    sqlx::query!(
                        "UPDATE tasks SET memory_refs = ?,
                            updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                         WHERE id = ?",
                        memory_refs_json,
                        id
                    )
                    .execute(self.db.pool())
                    .await?;
                    let task: Task =
                        task_select_where_id!(id).fetch_one(self.db.pool()).await?;
                    Ok::<_, crate::Error>(task)
                }
            },
        )
        .await?;

        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        Ok(task)
    }
}

#[cfg(test)]
mod created_by_tests {
    //! Phase 3B — verify that task inserts stamp `created_by_user_id` from the
    //! `SESSION_USER_ID` task-local and default to NULL when no user context
    //! is in scope.

    use super::TaskRepository;
    use crate::database::Database;
    use crate::repositories::user::UserRepository;
    use djinn_core::auth_context::SESSION_USER_ID;
    use djinn_core::events::EventBus;

    async fn seed_project_and_epic(db: &Database) -> (String, String) {
        db.ensure_initialized().await.unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO projects (id, name, path, verification_rules) VALUES (?, ?, ?, ?)",
            project_id,
            "p",
            "/tmp/p",
            "[]"
        )
        .execute(db.pool())
        .await
        .unwrap();

        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            epic_id,
            project_id,
            "ep01",
            "Epic",
            "",
            "",
            "",
            "",
            "[]"
        )
        .execute(db.pool())
        .await
        .unwrap();
        (project_id, epic_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_in_project_stamps_created_by_user_id_from_task_local() {
        let db = Database::open_in_memory().unwrap();
        let (project_id, epic_id) = seed_project_and_epic(&db).await;

        // Seed a real user row — the FK on tasks.created_by_user_id requires
        // the referenced users.id to exist.
        let user = UserRepository::new(db.clone())
            .upsert_from_github(424242, "phase3b-tester", Some("Tester"), None)
            .await
            .unwrap();
        let user_id = user.id.clone();

        let repo = TaskRepository::new(db.clone(), EventBus::noop());

        // With SESSION_USER_ID set, the insert must stamp the column.
        let created_id = SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                let task = repo
                    .create_in_project(
                        &project_id,
                        Some(&epic_id),
                        "Attributed",
                        "",
                        "",
                        "task",
                        0,
                        "",
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                task.id
            })
            .await;

        let stamped: Option<String> = sqlx::query_scalar!(
            "SELECT created_by_user_id FROM tasks WHERE id = ?",
            created_id
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            stamped.as_deref(),
            Some(user_id.as_str()),
            "created_by_user_id must match the SESSION_USER_ID task-local"
        );

        // Without SESSION_USER_ID in scope, created_by_user_id stays NULL —
        // agent/background insert semantics.
        let unattributed = repo
            .create_in_project(
                &project_id,
                Some(&epic_id),
                "Unattributed",
                "",
                "",
                "task",
                0,
                "",
                None,
                None,
            )
            .await
            .unwrap();
        let stamped: Option<String> = sqlx::query_scalar!(
            "SELECT created_by_user_id FROM tasks WHERE id = ?",
            unattributed.id
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(
            stamped.is_none(),
            "task created outside SESSION_USER_ID scope must leave created_by_user_id NULL"
        );
    }
}

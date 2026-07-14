use super::task_select_where_id;
use super::*;

/// Producer provenance for creator attribution. Producers provide facts, never
/// a locally resolved fallback identity.
#[derive(Debug, Clone, Copy, Default)]
pub struct EffectiveCreatorProvenance<'a> {
    pub explicit_user_id: Option<&'a str>,
    pub source_task_id: Option<&'a str>,
    pub proposal_id: Option<&'a str>,
}

const EFFECTIVE_CREATOR_UNAVAILABLE: &str = "effective_creator_unavailable";

/// Parse a caller-supplied JSON string destined for one of the array-shaped
/// JSONB columns (`labels`, `acceptance_criteria`, `memory_refs`).
///
/// These columns are `JSONB NOT NULL DEFAULT '[]'::jsonb` (see
/// `migrations_postgres/1_initial_schema.sql` + `4_default_json_columns.sql`).
/// Many callers — and the test helpers — pass an empty/blank string to mean
/// "no value", which is not valid JSON and would otherwise blow up with an
/// `EOF while parsing` error after the MySQL→Postgres cut-over (MySQL's JSON
/// path tolerated this; JSONB does not). Treat blank input as the column
/// default `[]` so empty input round-trips instead of erroring.
fn parse_json_array_column(column: &str, raw: &str) -> Result<serde_json::Value> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!([]));
    }
    serde_json::from_str(raw)
        .map_err(|e| crate::Error::InvalidData(format!("invalid json for tasks.{column}: {e}")))
}

/// Resolve an attributable user while the caller's insert transaction is open.
/// A disabled/retained user is intentionally valid: existence, not current org
/// membership, is the attribution invariant. Explicit identities are different
/// from fallback provenance: an invalid explicit value is an immediate error.
async fn resolve_effective_creator(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    provenance: EffectiveCreatorProvenance<'_>,
    parent_epic_id: Option<&str>,
) -> Result<String> {
    async fn existing_user(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        candidate: Option<String>,
    ) -> Result<Option<String>> {
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        Ok(
            sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id = $1")
                .bind(&candidate)
                .fetch_optional(&mut **tx)
                .await?,
        )
    }

    if let Some(explicit) = provenance.explicit_user_id {
        return existing_user(tx, Some(explicit.to_owned()))
            .await?
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "{EFFECTIVE_CREATOR_UNAVAILABLE}: invalid_explicit_identity"
                ))
            });
    }

    let source_creator = if let Some(source_task_id) = provenance.source_task_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT created_by_user_id FROM tasks WHERE id = $1",
        )
        .bind(source_task_id)
        .fetch_optional(&mut **tx)
        .await?
        .flatten()
    } else {
        None
    };
    if let Some(user) = existing_user(tx, source_creator).await? {
        return Ok(user);
    }

    let (epic_creator, epic_proposal_id) = if let Some(epic_id) = parent_epic_id {
        sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT created_by_user_id, proposal_id FROM epics WHERE id = $1",
        )
        .bind(epic_id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or((None, None))
    } else {
        (None, None)
    };
    if let Some(user) = existing_user(tx, epic_creator).await? {
        return Ok(user);
    }

    let proposal_id = provenance
        .proposal_id
        .map(str::to_owned)
        .or(epic_proposal_id);
    if let Some(proposal_id) = proposal_id {
        let proposal = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT build_owner_user_id, author_user_id FROM proposals WHERE id = $1",
        )
        .bind(proposal_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((build_owner, author)) = proposal {
            if let Some(user) = existing_user(tx, build_owner).await? {
                return Ok(user);
            }
            if let Some(user) = existing_user(tx, author).await? {
                return Ok(user);
            }
        }
    }

    Err(Error::InvalidData(EFFECTIVE_CREATOR_UNAVAILABLE.to_owned()))
}

impl TaskRepository {
    /// Test-only compatibility helper. Production callers must use a
    /// provenance-requiring creation boundary.
    #[cfg(any(test, feature = "test-support"))]
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

    /// Test-only compatibility helper. Production callers must use a
    /// provenance-requiring creation boundary.
    #[cfg(any(test, feature = "test-support"))]
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
        let project_id = sqlx::query_scalar!("SELECT project_id FROM epics WHERE id = $1", epic_id)
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
    /// Create a task together with its outgoing blocker edges in a SINGLE
    /// transaction. The two MUST commit atomically: otherwise the task lands
    /// `open` (dispatchable) a beat before its blockers are written, and the
    /// coordinator's ready-poll — running on a separate connection — can claim
    /// and run it ahead of its dependency. (Observed: a planner-spawned task
    /// dispatched ~1s after creation, before its `blocked-by` edge existed.)
    ///
    /// `blocker_ids` are the tasks this new task is blocked BY (already resolved
    /// to ids). A fresh task has no dependents, so its outgoing edges cannot
    /// form a cycle — no cycle check is needed here.
    pub async fn create_in_project_with_blockers(
        &self,
        project_id: &str,
        epic_id: Option<&str>,
        provenance: EffectiveCreatorProvenance<'_>,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
        status: Option<&str>,
        acceptance_criteria: Option<&str>,
        blocker_ids: &[String],
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let ac =
            parse_json_array_column("acceptance_criteria", acceptance_criteria.unwrap_or("[]"))?;
        // This is provenance, not a resolved value. Resolution happens after
        // opening the transaction immediately below.
        let explicit_creator = provenance
            .explicit_user_id
            .map(str::to_owned)
            .or_else(djinn_core::auth_context::current_user_id);
        let source_task_id = provenance.source_task_id.map(str::to_owned);
        let proposal_id = provenance.proposal_id.map(str::to_owned);

        let project_id_owned = project_id.to_owned();
        let epic_id_owned = epic_id.map(|s| s.to_owned());
        let title_owned = title.to_owned();
        let description_owned = description.to_owned();
        let design_owned = design.to_owned();
        let issue_type_owned = issue_type.to_owned();
        let owner_owned = owner.to_owned();
        let status_owned = status.map(|s| s.to_owned());

        // Retry on Dolt 1213: the INSERT hits the hot `tasks` table; concurrent
        // writers routinely trip serialization failures. Task + blocker rows go
        // in ONE transaction so a separate-connection reader (the coordinator's
        // dispatch poll) never sees the task as a dispatchable `open` row before
        // its blockers exist. The whole tx is idempotent under retry: `id` is
        // fixed, so a retry after a partial commit hits a UNIQUE violation (not
        // a serialization failure) and the original error surfaces unchanged.
        // The follow-up SELECT is retried independently.
        let blocker_ids_owned: Vec<String> = blocker_ids.to_vec();
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
                let explicit_creator = explicit_creator.clone();
                let source_task_id = source_task_id.clone();
                let proposal_id = proposal_id.clone();
                let blocker_ids = blocker_ids_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    let created_by_user_id = resolve_effective_creator(
                        &mut tx,
                        EffectiveCreatorProvenance {
                            explicit_user_id: explicit_creator.as_deref(),
                            source_task_id: source_task_id.as_deref(),
                            proposal_id: proposal_id.as_deref(),
                        },
                        epic_id.as_deref(),
                    )
                    .await?;
                    sqlx::query!(
                        "INSERT INTO tasks
                            (id, project_id, short_id, epic_id, title, description, design,
                             issue_type, priority, owner, status, acceptance_criteria,
                             created_by_user_id)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE($11, 'open'), $12, $13)",
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
                    .execute(&mut *tx)
                    .await?;
                    // Outgoing blocker edges, in the same tx. `ON CONFLICT DO
                    // NOTHING` (PK is `(task_id, blocking_task_id)`) keeps it
                    // idempotent across retries; the task row above satisfies the
                    // `blockers.task_id` FK within the tx.
                    for blocking_id in &blocker_ids {
                        if &id == blocking_id {
                            return Err(crate::Error::Internal(
                                "task cannot block itself".into(),
                            ));
                        }
                        sqlx::query!(
                            "INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)
                             ON CONFLICT DO NOTHING",
                            id,
                            blocking_id
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                    tx.commit().await?;
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
                    let task: Task = task_select_where_id!(&id).fetch_one(self.db.pool()).await?;
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

    /// Create a task with no blocker edges (the common path). See
    /// [`Self::create_in_project_with_blockers`].
    #[cfg(any(test, feature = "test-support"))]
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
        // Fixture callers must never exercise the production NULL-creator
        // path. Preserve an explicitly scoped fixture user when one exists;
        // otherwise create a durable fixture user for this test database.
        let fixture_creator = if djinn_core::auth_context::current_user_id().is_some() {
            None
        } else {
            Some(
                crate::repositories::user::UserRepository::new(self.db.clone())
                    .upsert_from_github(9_999_999, "task-fixture-creator", None, None)
                    .await?
                    .id,
            )
        };
        self.create_in_project_with_blockers(
            project_id,
            epic_id,
            EffectiveCreatorProvenance {
                explicit_user_id: fixture_creator.as_deref(),
                source_task_id: None,
                proposal_id: None,
            },
            title,
            description,
            design,
            issue_type,
            priority,
            owner,
            status,
            acceptance_criteria,
            &[],
        )
        .await
    }

    /// Test-only fixture helper for dispatch and refinement tests that need to
    /// exercise creator-specific behavior after constructing a shared task.
    /// Production attribution is resolved before insertion by the provenance
    /// creation boundary and must not use this mutation.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_created_by_user_id(&self, id: &str, user_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE tasks SET created_by_user_id = $1 WHERE id = $2")
            .bind(user_id)
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Production creation boundary: producers supply facts and this repository resolves a concrete creator in the insert transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_in_project_with_provenance(
        &self,
        project_id: &str,
        epic_id: Option<&str>,
        provenance: EffectiveCreatorProvenance<'_>,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
        status: Option<&str>,
        acceptance_criteria: Option<&str>,
    ) -> Result<Task> {
        self.create_in_project_with_blockers(
            project_id,
            epic_id,
            provenance,
            title,
            description,
            design,
            issue_type,
            priority,
            owner,
            status,
            acceptance_criteria,
            &[],
        )
        .await
    }

    /// Test helper: create a task with a specific short_id.
    /// This bypasses the normal short_id generation for testing collision scenarios.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn create_with_short_id(
        &self,
        id: &str,
        project_id: &str,
        title: &str,
        status: &str,
        short_id: &str,
    ) -> Result<Task> {
        self.db.ensure_initialized().await?;
        // This direct fixture insert preserves its caller-provided short id,
        // but must still produce a concretely attributable task. Unlike the
        // normal fixture helper it does not enter the provenance resolver, so
        // establish the durable fixture user explicitly before inserting.
        let fixture_creator = crate::repositories::user::UserRepository::new(self.db.clone())
            .upsert_from_github(9_999_999, "task-fixture-creator", None, None)
            .await?
            .id;
        let empty = "";
        let epic_id_none: Option<&str> = None;
        let issue_type = "task";
        let priority = 1_i64;
        sqlx::query(
            "INSERT INTO tasks
                (id, project_id, short_id, epic_id, title, description, design,
                 issue_type, priority, owner, status, continuation_count, memory_refs,
                 created_by_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 0, '[]'::jsonb, $12)",
        )
        .bind(id)
        .bind(project_id)
        .bind(short_id)
        .bind(epic_id_none)
        .bind(title)
        .bind(empty)
        .bind(empty)
        .bind(issue_type)
        .bind(priority)
        .bind(empty)
        .bind(status)
        .bind(fixture_creator)
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
        let labels_value = parse_json_array_column("labels", labels)?;
        let ac_value = parse_json_array_column("acceptance_criteria", acceptance_criteria)?;
        let id_owned = id.to_owned();
        let title_owned = title.to_owned();
        let description_owned = description.to_owned();
        let design_owned = design.to_owned();
        let owner_owned = owner.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let title = title_owned.clone();
                let description = description_owned.clone();
                let design = design_owned.clone();
                let owner = owner_owned.clone();
                let labels = labels_value.clone();
                let acceptance_criteria = ac_value.clone();
                async move {
                    sqlx::query!(
                        r#"UPDATE tasks SET
                            title = $1, description = $2, design = $3,
                            priority = $4, owner = $5, labels = $6,
                            acceptance_criteria = $7,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $8"#,
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

    /// Update only the labels JSONB column for a task.
    ///
    /// Use this for system-maintained markers when callers must not rewrite the
    /// task's editable fields or unrelated JSON columns as a side effect.
    pub async fn update_labels(&self, id: &str, labels: &str) -> Result<Task> {
        self.db.ensure_initialized().await?;
        let labels_value = parse_json_array_column("labels", labels)?;
        let id_owned = id.to_owned();

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let labels = labels_value.clone();
                async move {
                    sqlx::query(
                        r#"UPDATE tasks SET
                            labels = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
                    )
                    .bind(labels)
                    .bind(&id)
                    .execute(self.db.pool())
                    .await?;
                    let task: Task = task_select_where_id!(id).fetch_one(self.db.pool()).await?;
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
        crate::retry::retry_on_serialization_failure(crate::retry::DEFAULT_MAX_TX_RETRIES, || {
            let id = id_owned.clone();
            async move {
                sqlx::query!("DELETE FROM tasks WHERE id = $1", id)
                    .execute(self.db.pool())
                    .await?;
                Ok::<_, crate::Error>(())
            }
        })
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
                        r#"UPDATE tasks SET merge_commit_sha = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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
                        r#"UPDATE tasks SET pr_url = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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
                        r#"UPDATE tasks SET merge_conflict_metadata = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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
                        r#"UPDATE tasks SET continuation_count = continuation_count + 1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $1"#,
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
                        r#"UPDATE tasks SET agent_type = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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
        let ac_value = parse_json_array_column("acceptance_criteria", ac_json)?;

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let ac = ac_value.clone();
                async move {
                    sqlx::query!(
                        r#"UPDATE tasks SET acceptance_criteria = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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
                        r#"UPDATE tasks SET continuation_count = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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
                        r#"UPDATE tasks SET
                            reopen_count = 0,
                            continuation_count = 0,
                            intervention_count = intervention_count + 1,
                            last_intervention_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $1"#,
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
        let memory_refs_value = parse_json_array_column("memory_refs", memory_refs_json)?;

        let task: Task = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let memory_refs_json = memory_refs_value.clone();
                async move {
                    sqlx::query!(
                        r#"UPDATE tasks SET memory_refs = $1,
                            updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                         WHERE id = $2"#,
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

    /// Force-set `updated_at` to a literal timestamp string.
    ///
    /// Exists as a deliberate test fixture: board-health / board-reconcile
    /// tests backdate a task's `updated_at` beyond the stale threshold to
    /// exercise the reconciliation path. Production writes stamp
    /// `updated_at = NOW()` automatically.
    pub async fn set_updated_at(&self, id: &str, updated_at: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let updated_at_owned = updated_at.to_owned();

        crate::retry::retry_on_serialization_failure(crate::retry::DEFAULT_MAX_TX_RETRIES, || {
            let id = id_owned.clone();
            let updated_at = updated_at_owned.clone();
            async move {
                sqlx::query!(
                    "UPDATE tasks SET updated_at = $1 WHERE id = $2",
                    updated_at,
                    id
                )
                .execute(self.db.pool())
                .await?;
                Ok::<_, crate::Error>(())
            }
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod created_by_tests {
    //! Phase 3B — verify that task inserts stamp `created_by_user_id` from the
    //! `SESSION_USER_ID` task-local and default to NULL when no user context
    //! is in scope.

    use super::{EFFECTIVE_CREATOR_UNAVAILABLE, EffectiveCreatorProvenance, TaskRepository};
    use crate::database::Database;
    use crate::repositories::user::UserRepository;
    use djinn_core::auth_context::SESSION_USER_ID;
    use djinn_core::events::EventBus;

    async fn seed_project_and_epic(db: &Database) -> (String, String) {
        db.ensure_initialized().await.unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        let owner = "test";
        let repo_slug = format!("task-writes-{project_id}");
        sqlx::query!(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
            project_id,
            "p",
            owner,
            repo_slug,
        )
        .execute(db.pool())
        .await
        .unwrap();

        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '[]'::jsonb)",
            epic_id,
            project_id,
            "ep01",
            "Epic",
            "",
            "",
            "",
            ""
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
            "SELECT created_by_user_id FROM tasks WHERE id = $1",
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

        // Unresolvable provenance must fail before INSERT instead of committing NULL.
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let error = repo
            .create_in_project_with_provenance(
                &project_id,
                Some(&epic_id),
                EffectiveCreatorProvenance::default(),
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
            .unwrap_err();
        assert!(error.to_string().contains(EFFECTIVE_CREATOR_UNAVAILABLE));
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(before, after, "unresolvable creator must not commit a task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_with_short_id_stamps_the_durable_fixture_creator() {
        let db = Database::open_in_memory().unwrap();
        let (project_id, _) = seed_project_and_epic(&db).await;
        let repo = TaskRepository::new(db.clone(), EventBus::noop());

        let task = repo
            .create_with_short_id(
                &uuid::Uuid::now_v7().to_string(),
                &project_id,
                "Fixture task",
                "open",
                "fixture01",
            )
            .await
            .unwrap();

        let creator_id = task
            .created_by_user_id
            .as_deref()
            .expect("fixture task must have a concrete creator");
        let creator_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = $1)")
                .bind(creator_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(creator_exists, "fixture creator must be a persisted user");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_support_creator_setter_only_assigns_a_persisted_fixture_user() {
        let db = Database::open_in_memory().unwrap();
        let (project_id, _) = seed_project_and_epic(&db).await;
        let repo = TaskRepository::new(db.clone(), EventBus::noop());
        let task = repo
            .create_with_short_id(
                &uuid::Uuid::now_v7().to_string(),
                &project_id,
                "Fixture task",
                "open",
                "fixture02",
            )
            .await
            .unwrap();
        let user = UserRepository::new(db.clone())
            .upsert_from_github(10_003, "dispatch-fixture-user", None, None)
            .await
            .unwrap();

        repo.set_created_by_user_id(&task.id, &user.id)
            .await
            .unwrap();

        let stamped = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            stamped.created_by_user_id.as_deref(),
            Some(user.id.as_str())
        );
    }

    /// A background/agent caller (no SESSION_USER_ID) creating a task under an
    /// epic that *does* have a creator must inherit the epic's creator, so
    /// Planner-spawned tasks belong to the human who owns the epic rather than
    /// appearing as unowned/system. A session user, when present, still wins.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_in_project_inherits_created_by_from_epic_when_no_session() {
        let db = Database::open_in_memory().unwrap();
        let (project_id, _) = seed_project_and_epic(&db).await;

        let users = UserRepository::new(db.clone());
        let epic_owner = users
            .upsert_from_github(515151, "epic-owner", Some("Epic Owner"), None)
            .await
            .unwrap();
        let session_user = users
            .upsert_from_github(626262, "session-user", Some("Session User"), None)
            .await
            .unwrap();

        // An epic owned by `epic_owner`.
        let owned_epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs, created_by_user_id)
             VALUES ($1, $2, $3, '', '', '', '', '', '[]'::jsonb, $4)",
            owned_epic_id,
            project_id,
            "ep02",
            epic_owner.id,
        )
        .execute(db.pool())
        .await
        .unwrap();

        let repo = TaskRepository::new(db.clone(), EventBus::noop());

        // No session in scope → inherit the epic's creator.
        let inherited = repo
            .create_in_project(
                &project_id,
                Some(&owned_epic_id),
                "Inherited",
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
        assert_eq!(
            inherited.created_by_user_id.as_deref(),
            Some(epic_owner.id.as_str()),
            "background task under an owned epic must inherit the epic's creator"
        );

        // Session user present → it wins over the epic's creator.
        let session_owned = SESSION_USER_ID
            .scope(Some(session_user.id.clone()), async {
                repo.create_in_project(
                    &project_id,
                    Some(&owned_epic_id),
                    "SessionOwned",
                    "",
                    "",
                    "task",
                    0,
                    "",
                    None,
                    None,
                )
                .await
                .unwrap()
            })
            .await;
        assert_eq!(
            session_owned.created_by_user_id.as_deref(),
            Some(session_user.id.as_str()),
            "an in-scope SESSION_USER_ID must take precedence over the epic's creator"
        );
    }

    /// Regression for the planner↔dispatch race: a task created with blockers
    /// must have its blocker edge present the instant creation returns (same
    /// transaction), so the coordinator's ready filter never sees it as a
    /// dispatchable `open` task before it's blocked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_with_blockers_wires_edge_atomically_and_holds_from_ready() {
        let db = Database::open_in_memory().unwrap();
        let (project_id, epic_id) = seed_project_and_epic(&db).await;
        let repo = TaskRepository::new(db.clone(), EventBus::noop());

        // Blocking task — stays `open` (unresolved).
        let blocker = repo
            .create_in_project(
                &project_id,
                Some(&epic_id),
                "Blocker",
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

        // New task created together WITH its blocker, in one call.
        let blocked = repo
            .create_in_project_with_blockers(
                &project_id,
                Some(&epic_id),
                EffectiveCreatorProvenance::default(),
                "Blocked",
                "",
                "",
                "task",
                0,
                "",
                None,
                None,
                std::slice::from_ref(&blocker.id),
            )
            .await
            .unwrap();

        // The edge exists the moment creation returns — no window where the task
        // is dispatchable-but-unblocked.
        let edge: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "c!" FROM blockers WHERE task_id = $1 AND blocking_task_id = $2"#,
            blocked.id,
            blocker.id
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            edge, 1,
            "blocker edge must be wired atomically with the task insert"
        );

        // The dispatcher's ready filter (open + no non-closed blocker) must
        // exclude the blocked task while its blocker is unresolved, but keep the
        // (unblocked) blocker itself.
        let ready = repo.list_by_status_filtered("open", true).await.unwrap();
        assert!(
            ready.iter().any(|t| t.id == blocker.id),
            "the unblocked blocker task is ready"
        );
        assert!(
            !ready.iter().any(|t| t.id == blocked.id),
            "the blocked task must NOT be ready until its blocker closes"
        );
    }
}

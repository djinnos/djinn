use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{SessionRecord, SessionStatus};
use serde_json::Value;

use crate::Result;
use crate::database::Database;

/// Inlined SESSION_COLS projection for each `query_as!(SessionRecord, ...)`
/// call site.  `query_as!` requires a string-literal SQL argument; concat!()
/// doesn't satisfy it (verified on agent.rs in batch 4).

pub struct SessionRepository {
    db: Database,
    events: EventBus,
}

pub struct CreateSessionParams<'a> {
    pub project_id: &'a str,
    pub task_id: Option<&'a str>,
    pub model: &'a str,
    pub agent_type: &'a str,
    pub worktree_path: Option<&'a str>,
    pub metadata_json: Option<&'a str>,
}

impl SessionRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub async fn create(&self, params: CreateSessionParams<'_>) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let _ = params.metadata_json;

        // Phase 3B: stamp `created_by_user_id` from the task-local set at
        // the MCP dispatch root. Sessions spawned from the agent
        // coordinator's internal loops have no user context and stay
        // NULL; sessions created in response to a user MCP call (e.g.
        // `session_start` via chat) inherit the calling user's id.
        let created_by_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO sessions
                (id, project_id, task_id, model_id, agent_type, `status`, worktree_path,
                 created_by_user_id)
             VALUES (?, ?, ?, ?, ?, 'running', ?, ?)",
            id,
            params.project_id,
            params.task_id,
            params.model,
            params.agent_type,
            params.worktree_path,
            created_by_user_id
        )
        .execute(self.db.pool())
        .await?;

        let session = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope {
            entity_type: "session",
            action: "started",
            payload: serde_json::to_value(&session).unwrap_or_default(),
            id: None,
            project_id: None,
            from_sync: false,
        });
        tracing::info!(
            session_id = %session.id,
            task_id = ?session.task_id,
            "SessionRepository: emitted session.started SSE event"
        );
        Ok(session)
    }

    /// Re-fetch a session by id and emit `SessionUpdated`.
    async fn fetch_and_emit_update(&self, id: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let session = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;
        let action = match session.status.as_str() {
            "running" => "started",
            "completed" => "completed",
            "interrupted" => "interrupted",
            "failed" => "failed",
            _ => "updated",
        };
        self.events.send(DjinnEventEnvelope {
            entity_type: "session",
            action,
            payload: serde_json::to_value(&session).unwrap_or_default(),
            id: None,
            project_id: None,
            from_sync: false,
        });
        Ok(session)
    }

    pub async fn update(
        &self,
        id: &str,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        let status_str = status.as_str();
        sqlx::query!(
            "UPDATE sessions
             SET `status` = ?,
                 tokens_in = ?,
                 tokens_out = ?,
                 ended_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            status_str,
            tokens_in,
            tokens_out,
            id
        )
        .execute(self.db.pool())
        .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Mark all `running` sessions as `interrupted`.
    /// Called once at server startup — no runtime sessions can exist yet.
    pub async fn interrupt_all_running(&self) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let running_sessions = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE `status` = 'running'"#
        )
        .fetch_all(self.db.pool())
        .await?;

        if running_sessions.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query!(
            "UPDATE sessions
             SET `status` = 'interrupted',
                 ended_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE `status` = 'running'"
        )
        .execute(self.db.pool())
        .await?;

        for session in running_sessions {
            let _ = self.fetch_and_emit_update(&session.id).await?;
        }

        Ok(result.rows_affected())
    }

    /// Mark all `running` sessions for a specific task as `interrupted`.
    /// Used by stuck-task recovery to clean up orphaned session records.
    pub async fn interrupt_running_for_task(&self, task_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let orphans = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE task_id = ? AND `status` = 'running'"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?;

        if orphans.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query!(
            "UPDATE sessions
             SET `status` = 'interrupted',
                 ended_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE task_id = ? AND `status` = 'running'",
            task_id
        )
        .execute(self.db.pool())
        .await?;

        for session in &orphans {
            let _ = self.fetch_and_emit_update(&session.id).await;
        }

        Ok(result.rows_affected())
    }

    pub async fn get(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE id = ?"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_in_project(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE project_id = ? AND id = ?"#,
            project_id,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions WHERE task_id = ? ORDER BY started_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_for_task_in_project(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions
             WHERE project_id = ? AND task_id = ? ORDER BY started_at DESC"#,
            project_id,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active(&self) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions
             WHERE `status` = 'running' ORDER BY started_at DESC"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active_in_project(&self, project_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions
             WHERE project_id = ? AND `status` = 'running' ORDER BY started_at DESC"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Returns any running sessions with `agent_type = 'planner'` whose task
    /// is attached to the given epic.  Used by ADR-051 §7 reentrance guard
    /// to suppress auto-dispatch of a new planning wave while a Planner is
    /// actively reshaping the epic.
    pub async fn active_planner_for_epic(&self, epic_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT s.id, s.project_id, s.task_id, s.model_id, s.agent_type,
                    s.started_at, s.ended_at,
                    s.`status` AS "status!", s.tokens_in, s.tokens_out, s.worktree_path
             FROM sessions s
             INNER JOIN tasks t ON t.id = s.task_id
             WHERE s.`status` = 'running' AND s.agent_type = 'planner' AND t.epic_id = ?
             ORDER BY s.started_at DESC"#,
            epic_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn active_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions
             WHERE task_id = ? AND `status` = 'running' ORDER BY started_at DESC LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn count_for_task(&self, task_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_scalar!("SELECT COUNT(*) FROM sessions WHERE task_id = ?", task_id)
                .fetch_one(self.db.pool())
                .await?,
        )
    }

    /// Batch count sessions per task for a list of task IDs.
    pub async fn count_for_tasks(
        &self,
        task_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, i64>> {
        self.db.ensure_initialized().await?;
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: Vec<String> = (0..task_ids.len()).map(|_| "?".to_string()).collect();
        // NOTE: dynamic SQL (IN list built at runtime) — compile-time check not possible
        let sql = format!(
            "SELECT task_id, COUNT(*) as cnt FROM sessions WHERE task_id IN ({}) GROUP BY task_id",
            placeholders.join(", ")
        );
        let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
        for id in task_ids {
            q = q.bind(*id);
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows.into_iter().collect())
    }

    /// Set session status to Paused without setting ended_at.
    /// Used when a worker completes (Done) but its worktree is kept alive for the review cycle.
    pub async fn pause(&self, id: &str, tokens_in: i64, tokens_out: i64) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "UPDATE sessions SET `status` = 'paused', tokens_in = ?, tokens_out = ? WHERE id = ?",
            tokens_in,
            tokens_out,
            id
        )
        .execute(self.db.pool())
        .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Set a paused session back to Running (for resume cycles).
    pub async fn set_running(&self, id: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!("UPDATE sessions SET `status` = 'running' WHERE id = ?", id)
            .execute(self.db.pool())
            .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Find the most recent paused session for a task (if any).
    pub async fn paused_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions
             WHERE task_id = ? AND `status` = 'paused' ORDER BY started_at DESC LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Store the event taxonomy JSON on a completed session record.
    ///
    /// Called after structural extraction completes. A best-effort write:
    /// callers should log errors but must not propagate them to the slot.
    pub async fn set_event_taxonomy(&self, id: &str, taxonomy_json: &str) -> Result<()> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "UPDATE sessions SET event_taxonomy = ? WHERE id = ?",
            taxonomy_json,
            id
        )
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// Return the most recent non-null event taxonomy JSON for a task.
    pub async fn latest_event_taxonomy_for_task(&self, task_id: &str) -> Result<Option<Value>> {
        self.db.ensure_initialized().await?;

        let row: Option<Option<String>> = sqlx::query_scalar!(
            "SELECT event_taxonomy FROM sessions
             WHERE task_id = ? AND event_taxonomy IS NOT NULL
             ORDER BY started_at DESC LIMIT 1",
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row
            .flatten()
            .and_then(|json| serde_json::from_str::<Value>(&json).ok()))
    }

    /// Return the most recent non-null `worktree_path` recorded for any session
    /// that belongs to the given task.  Used by the coordinator to locate the
    /// on-disk worktree of a finished simple-lifecycle session so it can probe
    /// for uncommitted changes before deciding whether to short-circuit close.
    pub async fn latest_worktree_path_for_task(&self, task_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;

        let row: Option<Option<String>> = sqlx::query_scalar!(
            "SELECT worktree_path FROM sessions
             WHERE task_id = ? AND worktree_path IS NOT NULL
             ORDER BY started_at DESC LIMIT 1",
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.flatten())
    }

    /// Find the most recent paused session for a task that matches the given
    /// agent type.  Used during dispatch so that e.g. a PM session never
    /// accidentally resumes a worker's paused conversation.
    pub async fn paused_for_task_by_type(
        &self,
        task_id: &str,
        agent_type: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                `status` AS "status!", tokens_in, tokens_out, worktree_path
             FROM sessions
             WHERE task_id = ? AND `status` = 'paused' AND agent_type = ?
             ORDER BY started_at DESC LIMIT 1"#,
            task_id,
            agent_type
        )
        .fetch_optional(self.db.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::SessionRecord;

    use super::*;
    use crate::repositories::epic::EpicRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
    }

    /// Create a task via raw SQL (no TaskRepository dep), returns (project_id, task_id).
    async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus);
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, `status`, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES (?, ?, ?, ?, 'Task', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]')",
            task_id,
            epic.project_id,
            short_id,
            epic.id
        )
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_emits_event() {
        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db, bus);

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                worktree_path: Some("/tmp/djinn-worktree-task"),
                metadata_json: None,
            })
            .await
            .unwrap();
        assert_eq!(created.status, "running");

        {
            let events = captured.lock().unwrap();
            let started = events
                .iter()
                .find(|e| e.entity_type == "session" && e.action == "started");
            assert!(started.is_some(), "expected session.started event");
            let s: SessionRecord =
                serde_json::from_value(started.unwrap().payload.clone()).unwrap();
            assert_eq!(s.id, created.id);
        }

        captured.lock().unwrap().clear();

        let updated = repo
            .update(&created.id, SessionStatus::Completed, 10, 20)
            .await
            .unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.tokens_in, 10);
        assert_eq!(updated.tokens_out, 20);
        assert!(updated.ended_at.is_some());

        let events = captured.lock().unwrap();
        let completed = events
            .iter()
            .find(|e| e.entity_type == "session" && e.action == "completed");
        assert!(completed.is_some(), "expected session.completed event");
        let s: SessionRecord = serde_json::from_value(completed.unwrap().payload.clone()).unwrap();
        assert_eq!(s.id, created.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_and_resume_preserve_session_identity_and_worktree() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                worktree_path: Some("/tmp/djinn-worktree-resume"),
                metadata_json: None,
            })
            .await
            .unwrap();

        assert_eq!(created.status, SessionStatus::Running.as_str());
        assert_eq!(
            created.worktree_path.as_deref(),
            Some("/tmp/djinn-worktree-resume")
        );
        assert!(
            created.ended_at.is_none(),
            "new sessions should start without ended_at"
        );

        let paused = repo.pause(&created.id, 12, 34).await.unwrap();
        assert_eq!(paused.id, created.id);
        assert_eq!(paused.status, SessionStatus::Paused.as_str());
        assert_eq!(paused.tokens_in, 12);
        assert_eq!(paused.tokens_out, 34);
        assert!(paused.ended_at.is_none(), "paused sessions stay resumable");
        assert_eq!(paused.worktree_path, created.worktree_path);

        let paused_lookup = repo.paused_for_task(&task_id).await.unwrap().unwrap();
        assert_eq!(paused_lookup.id, created.id);
        assert_eq!(paused_lookup.status, SessionStatus::Paused.as_str());

        let resumed = repo.set_running(&created.id).await.unwrap();
        assert_eq!(resumed.id, created.id);
        assert_eq!(resumed.status, SessionStatus::Running.as_str());
        assert!(
            resumed.ended_at.is_none(),
            "resumed session should remain open"
        );
        assert_eq!(resumed.worktree_path, created.worktree_path);

        let active = repo.active_for_task(&task_id).await.unwrap().unwrap();
        assert_eq!(active.id, created.id);
        assert_eq!(active.status, SessionStatus::Running.as_str());

        let sessions = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "resume should reuse existing session row"
        );
        assert_eq!(sessions[0].id, created.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_running_for_task_only_updates_running_sessions_for_target_task() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let (other_project_id, other_task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let first_running_target = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                worktree_path: Some("/tmp/target-running"),
                metadata_json: None,
            })
            .await
            .unwrap();

        let paused_target = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5-pause",
                agent_type: "worker",
                worktree_path: Some("/tmp/target-paused"),
                metadata_json: None,
            })
            .await
            .unwrap();
        let paused_target = repo.pause(&paused_target.id, 7, 8).await.unwrap();

        let second_running_target = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5-mini",
                agent_type: "worker",
                worktree_path: Some("/tmp/target-running-2"),
                metadata_json: None,
            })
            .await
            .unwrap();

        let other_task_running = repo
            .create(CreateSessionParams {
                project_id: &other_project_id,
                task_id: Some(&other_task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                worktree_path: Some("/tmp/other-running"),
                metadata_json: None,
            })
            .await
            .unwrap();

        let interrupted = repo.interrupt_running_for_task(&task_id).await.unwrap();
        assert_eq!(
            interrupted, 2,
            "only running rows for the target task should be interrupted"
        );

        let first = repo.get(&first_running_target.id).await.unwrap().unwrap();
        assert_eq!(first.status, SessionStatus::Interrupted.as_str());
        assert!(
            first.ended_at.is_some(),
            "interrupted sessions should be closed"
        );

        let second = repo.get(&second_running_target.id).await.unwrap().unwrap();
        assert_eq!(second.status, SessionStatus::Interrupted.as_str());
        assert!(second.ended_at.is_some());

        let paused_after = repo.get(&paused_target.id).await.unwrap().unwrap();
        assert_eq!(paused_after.status, SessionStatus::Paused.as_str());
        assert!(
            paused_after.ended_at.is_none(),
            "paused resumable session must remain open"
        );

        let other_after = repo.get(&other_task_running.id).await.unwrap().unwrap();
        assert_eq!(other_after.status, SessionStatus::Running.as_str());
        assert!(other_after.ended_at.is_none());
    }

    /// Insert a task under a given existing epic.  Returns the task id.
    async fn create_task_under_epic(db: &Database, project_id: &str, epic_id: &str) -> String {
        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, `status`, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES (?, ?, ?, ?, 'Task', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]')",
            task_id,
            project_id,
            short_id,
            epic_id
        )
        .execute(db.pool())
        .await
        .unwrap();
        task_id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_planner_for_epic_filters_correctly() {
        let db = test_db();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic_a = epic_repo
            .create("Epic A", "", "", "", "", None)
            .await
            .unwrap();
        let epic_b = epic_repo
            .create("Epic B", "", "", "", "", None)
            .await
            .unwrap();

        let task_a1 = create_task_under_epic(&db, &epic_a.project_id, &epic_a.id).await;
        let task_a2 = create_task_under_epic(&db, &epic_a.project_id, &epic_a.id).await;
        let task_b1 = create_task_under_epic(&db, &epic_b.project_id, &epic_b.id).await;

        let repo = SessionRepository::new(db.clone(), bus);

        // 1. Running planner on epic A → should match.
        let planner_a = repo
            .create(CreateSessionParams {
                project_id: &epic_a.project_id,
                task_id: Some(&task_a1),
                model: "openai/gpt-5",
                agent_type: "planner",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        // 2. Running planner on epic B → should NOT match epic A.
        let _planner_b = repo
            .create(CreateSessionParams {
                project_id: &epic_b.project_id,
                task_id: Some(&task_b1),
                model: "openai/gpt-5",
                agent_type: "planner",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        // 3. Running worker on epic A → wrong agent_type, should NOT match.
        let _worker_a = repo
            .create(CreateSessionParams {
                project_id: &epic_a.project_id,
                task_id: Some(&task_a2),
                model: "openai/gpt-5",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        // 4. Completed planner on epic A → not running, should NOT match.
        let finished_planner = repo
            .create(CreateSessionParams {
                project_id: &epic_a.project_id,
                task_id: Some(&task_a2),
                model: "openai/gpt-5",
                agent_type: "planner",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        repo.update(&finished_planner.id, SessionStatus::Completed, 0, 0)
            .await
            .unwrap();

        let matches = repo.active_planner_for_epic(&epic_a.id).await.unwrap();
        assert_eq!(
            matches.len(),
            1,
            "only the running planner on epic A matches"
        );
        assert_eq!(matches[0].id, planner_a.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_planner_for_epic_returns_empty_when_none() {
        let db = test_db();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();
        let repo = SessionRepository::new(db, bus);
        let matches = repo.active_planner_for_epic(&epic.id).await.unwrap();
        assert!(matches.is_empty());
    }
}

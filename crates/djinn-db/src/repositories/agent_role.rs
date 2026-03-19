use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::AgentRole;

use crate::database::Database;
use crate::{Error, Result};

pub const VALID_BASE_ROLES: &[&str] = &[
    "worker", "lead", "planner", "architect", "reviewer", "resolver",
];

pub struct AgentRoleCreateInput<'a> {
    pub name: &'a str,
    pub base_role: &'a str,
    pub description: &'a str,
    pub system_prompt_extensions: &'a str,
    pub model_preference: Option<&'a str>,
    pub verification_command: Option<&'a str>,
    pub mcp_servers: Option<&'a str>,
    pub skills: Option<&'a str>,
    pub is_default: bool,
}

pub struct AgentRoleUpdateInput<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub system_prompt_extensions: &'a str,
    pub model_preference: Option<&'a str>,
    pub verification_command: Option<&'a str>,
    pub mcp_servers: &'a str,
    pub skills: &'a str,
    /// Final learned_prompt value to persist. Pass None to clear (set NULL).
    /// MCP layer resolves the "keep existing / set / clear" logic before calling.
    pub learned_prompt: Option<&'a str>,
}

pub struct AgentRoleListQuery {
    pub project_id: String,
    pub base_role: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

pub struct AgentRoleListResult {
    pub roles: Vec<AgentRole>,
    pub total_count: i64,
}

/// Per-role aggregated effectiveness metrics.
pub struct AgentRoleMetrics {
    /// Fraction of closed tasks that completed successfully (0.0–1.0).
    pub success_rate: f64,
    /// Average reopen_count across closed tasks for this role.
    pub avg_reopens: f64,
    /// Fraction of closed tasks with zero verification failures (0.0–1.0).
    pub verification_pass_rate: f64,
    /// Number of closed tasks included in calculations.
    pub completed_task_count: i64,
    /// Average total tokens (in + out) per completed session in the window.
    pub avg_tokens: f64,
    /// Average session duration in seconds (completed sessions in the window).
    pub avg_time_seconds: f64,
}

pub struct AgentRoleRepository {
    db: Database,
    events: EventBus,
}

impl AgentRoleRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub async fn get(&self, id: &str) -> Result<Option<AgentRole>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, AgentRole>(
            "SELECT id, project_id, name, base_role, description,
                    system_prompt_extensions, model_preference, verification_command,
                    mcp_servers, skills, is_default, learned_prompt, created_at, updated_at
             FROM agent_roles WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_name_in_project(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<Option<AgentRole>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, AgentRole>(
            "SELECT id, project_id, name, base_role, description,
                    system_prompt_extensions, model_preference, verification_command,
                    mcp_servers, skills, is_default, learned_prompt, created_at, updated_at
             FROM agent_roles WHERE project_id = ?1 AND name = ?2",
        )
        .bind(project_id)
        .bind(name)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn create_for_project(
        &self,
        project_id: &str,
        input: AgentRoleCreateInput<'_>,
    ) -> Result<AgentRole> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO agent_roles (
                id, project_id, name, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills, is_default
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(input.name)
        .bind(input.base_role)
        .bind(input.description)
        .bind(input.system_prompt_extensions)
        .bind(input.model_preference)
        .bind(input.verification_command)
        .bind(input.mcp_servers.unwrap_or("[]"))
        .bind(input.skills.unwrap_or("[]"))
        .bind(input.is_default as i64)
        .execute(self.db.pool())
        .await?;

        let role = self
            .get(&id)
            .await?
            .ok_or_else(|| Error::InvalidData("agent_role insert failed".into()))?;
        self.events.send(DjinnEventEnvelope::agent_role_created(&role));
        Ok(role)
    }

    pub async fn update(&self, id: &str, input: AgentRoleUpdateInput<'_>) -> Result<AgentRole> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE agent_roles
             SET name = ?2, description = ?3, system_prompt_extensions = ?4,
                 model_preference = ?5, verification_command = ?6,
                 mcp_servers = ?7, skills = ?8, learned_prompt = ?9,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(input.name)
        .bind(input.description)
        .bind(input.system_prompt_extensions)
        .bind(input.model_preference)
        .bind(input.verification_command)
        .bind(input.mcp_servers)
        .bind(input.skills)
        .bind(input.learned_prompt)
        .execute(self.db.pool())
        .await?;

        let role = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("agent_role not found: {id}")))?;
        self.events.send(DjinnEventEnvelope::agent_role_updated(&role));
        Ok(role)
    }

    pub async fn delete(&self, id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM agent_roles WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        self.events
            .send(DjinnEventEnvelope::agent_role_deleted(id, project_id));
        Ok(())
    }

    pub async fn list_for_project(&self, query: AgentRoleListQuery) -> Result<AgentRoleListResult> {
        self.db.ensure_initialized().await?;

        let (where_sql, params) = build_where(&query.project_id, &query.base_role);

        let total_sql = format!("SELECT COUNT(*) FROM agent_roles WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            total_q = total_q.bind(p.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let sql = format!(
            "SELECT id, project_id, name, base_role, description,
                    system_prompt_extensions, model_preference, verification_command,
                    mcp_servers, skills, is_default, learned_prompt, created_at, updated_at
             FROM agent_roles WHERE {where_sql}
             ORDER BY is_default DESC, base_role ASC, name ASC
             LIMIT ? OFFSET ?"
        );
        let mut role_q = sqlx::query_as::<_, AgentRole>(&sql);
        for p in &params {
            role_q = role_q.bind(p.clone());
        }
        let roles = role_q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

        Ok(AgentRoleListResult {
            roles,
            total_count: total,
        })
    }

    /// Compute aggregated effectiveness metrics for a role identified by its
    /// base_role→agent_type mapping. `window_days` limits session data lookback.
    pub async fn get_metrics(
        &self,
        project_id: &str,
        agent_type: &str,
        window_days: i64,
    ) -> Result<AgentRoleMetrics> {
        self.db.ensure_initialized().await?;

        // Task-level metrics: closed tasks that had at least one session of this agent_type.
        let task_row: (f64, f64, f64, i64) = sqlx::query_as(
            "SELECT
                CAST(SUM(CASE WHEN t.close_reason = 'completed' THEN 1 ELSE 0 END) AS REAL)
                    / CAST(MAX(1, COUNT(DISTINCT t.id)) AS REAL),
                COALESCE(AVG(CAST(t.reopen_count AS REAL)), 0.0),
                CAST(SUM(CASE WHEN t.verification_failure_count = 0 THEN 1 ELSE 0 END) AS REAL)
                    / CAST(MAX(1, COUNT(DISTINCT t.id)) AS REAL),
                COUNT(DISTINCT t.id)
             FROM tasks t
             WHERE t.project_id = ?1
               AND t.status = 'closed'
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = ?2
               )",
        )
        .bind(project_id)
        .bind(agent_type)
        .fetch_one(self.db.pool())
        .await
        .unwrap_or((0.0, 0.0, 0.0, 0));

        // Session-level metrics: completed sessions within the lookback window.
        let session_row: (f64, f64) = sqlx::query_as(
            "SELECT
                COALESCE(AVG(CAST(s.tokens_in + s.tokens_out AS REAL)), 0.0),
                COALESCE(AVG(
                    CASE WHEN s.ended_at IS NOT NULL
                        THEN CAST((julianday(s.ended_at) - julianday(s.started_at)) * 86400 AS REAL)
                        ELSE NULL END
                ), 0.0)
             FROM sessions s
             JOIN tasks t ON t.id = s.task_id
             WHERE t.project_id = ?1
               AND s.agent_type = ?2
               AND s.status = 'completed'
               AND s.started_at >= datetime('now', '-' || ?3 || ' days')",
        )
        .bind(project_id)
        .bind(agent_type)
        .bind(window_days)
        .fetch_one(self.db.pool())
        .await
        .unwrap_or((0.0, 0.0));

        Ok(AgentRoleMetrics {
            success_rate: task_row.0,
            avg_reopens: task_row.1,
            verification_pass_rate: task_row.2,
            completed_task_count: task_row.3,
            avg_tokens: session_row.0,
            avg_time_seconds: session_row.1,
        })
    }
}

fn build_where(project_id: &str, base_role: &Option<String>) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = vec!["project_id = ?".to_owned()];
    let mut params: Vec<String> = vec![project_id.to_owned()];

    if let Some(br) = base_role {
        clauses.push("base_role = ?".to_owned());
        params.push(br.clone());
    }

    (clauses.join(" AND "), params)
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;

    use super::*;
    use crate::database::Database;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn create_project(db: &Database) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)")
            .bind(&id)
            .bind("test")
            .bind("/tmp/test")
            .execute(db.pool())
            .await
            .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_role() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRoleRepository::new(db, EventBus::noop());

        let role = repo
            .create_for_project(
                &project_id,
                AgentRoleCreateInput {
                    name: "DB Expert",
                    base_role: "worker",
                    description: "Database migrations specialist",
                    system_prompt_extensions: "Focus on safe migrations.",
                    model_preference: None,
                    verification_command: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(role.name, "DB Expert");
        assert_eq!(role.base_role, "worker");
        assert!(!role.is_default);

        let fetched = repo.get(&role.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, role.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_uniqueness_within_project() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRoleRepository::new(db, EventBus::noop());

        repo.create_for_project(
            &project_id,
            AgentRoleCreateInput {
                name: "My Role",
                base_role: "worker",
                description: "",
                system_prompt_extensions: "",
                model_preference: None,
                verification_command: None,
                mcp_servers: None,
                skills: None,
                is_default: false,
            },
        )
        .await
        .unwrap();

        let result = repo
            .create_for_project(
                &project_id,
                AgentRoleCreateInput {
                    name: "My Role",
                    base_role: "planner",
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    verification_command: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_role() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRoleRepository::new(db, EventBus::noop());

        let role = repo
            .create_for_project(
                &project_id,
                AgentRoleCreateInput {
                    name: "Worker",
                    base_role: "worker",
                    description: "original",
                    system_prompt_extensions: "",
                    model_preference: None,
                    verification_command: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();

        let updated = repo
            .update(
                &role.id,
                AgentRoleUpdateInput {
                    name: "Worker",
                    description: "updated",
                    system_prompt_extensions: "extra prompt",
                    model_preference: Some("claude-opus-4-6"),
                    verification_command: Some("cargo test"),
                    mcp_servers: "[]",
                    skills: "[]",
                    learned_prompt: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.description, "updated");
        assert_eq!(updated.system_prompt_extensions, "extra prompt");
        assert_eq!(
            updated.model_preference.as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_with_base_role_filter() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRoleRepository::new(db, EventBus::noop());

        for (name, base_role) in [("W1", "worker"), ("W2", "worker"), ("P1", "planner")] {
            repo.create_for_project(
                &project_id,
                AgentRoleCreateInput {
                    name,
                    base_role,
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    verification_command: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();
        }

        let workers = repo
            .list_for_project(AgentRoleListQuery {
                project_id: project_id.clone(),
                base_role: Some("worker".to_string()),
                limit: 25,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(workers.total_count, 2);
        assert_eq!(workers.roles.len(), 2);

        let all = repo
            .list_for_project(AgentRoleListQuery {
                project_id,
                base_role: None,
                limit: 25,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(all.total_count, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRoleRepository::new(db, bus);

        repo.create_for_project(
            &project_id,
            AgentRoleCreateInput {
                name: "Event Role",
                base_role: "worker",
                description: "",
                system_prompt_extensions: "",
                model_preference: None,
                verification_command: None,
                mcp_servers: None,
                skills: None,
                is_default: false,
            },
        )
        .await
        .unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "agent_role");
        assert_eq!(events[0].action, "created");
    }
}

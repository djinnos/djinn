//! Backing-service catalog + provisioning state (migration 47, Phase C).
//!
//! `ServicePresetRepository` reads the curated preset catalog;
//! `ServiceInstanceRepository` is the per-task provisioning state machine;
//! `ServicePolicyRepository` is the per-project authorization kill-switch.
//! Non-macro `sqlx::query` form (like the other Phase A/B repos).

use sqlx::Row;

use crate::Result;
use crate::database::Database;

/// Provisioning lifecycle states.
pub struct ServiceInstanceStatus;
impl ServiceInstanceStatus {
    pub const REQUESTED: &'static str = "requested";
    pub const PROVISIONING: &'static str = "provisioning";
    pub const READY: &'static str = "ready";
    pub const FAILED: &'static str = "failed";
    pub const RELEASED: &'static str = "released";
}

#[derive(Clone, Debug)]
pub struct ServicePreset {
    pub id: String,
    pub name: String,
    pub service_type: String,
    pub image: String,
    pub port: i32,
    pub env: String,       // JSON object (text)
    pub resources: String, // JSON object (text)
    pub conn_template: String,
    pub conn_env_var: String,
}

fn map_preset(r: &sqlx::postgres::PgRow) -> ServicePreset {
    ServicePreset {
        id: r.get("id"),
        name: r.get("name"),
        service_type: r.get("service_type"),
        image: r.get("image"),
        port: r.get("port"),
        env: r.get("env"),
        resources: r.get("resources"),
        conn_template: r.get("conn_template"),
        conn_env_var: r.get("conn_env_var"),
    }
}

const PRESET_COLS: &str = r#"id, name, service_type, image, port,
    env::text AS env, resources::text AS resources, conn_template, conn_env_var"#;

pub struct ServicePresetRepository {
    db: Database,
}

impl ServicePresetRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<ServicePreset>> {
        self.db.ensure_initialized().await?;
        let q = format!("SELECT {PRESET_COLS} FROM service_presets ORDER BY name");
        let rows = sqlx::query(&q).fetch_all(self.db.pool()).await?;
        Ok(rows.iter().map(map_preset).collect())
    }

    pub async fn get(&self, id: &str) -> Result<Option<ServicePreset>> {
        self.db.ensure_initialized().await?;
        let q = format!("SELECT {PRESET_COLS} FROM service_presets WHERE id = $1");
        let row = sqlx::query(&q)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.as_ref().map(map_preset))
    }
}

#[derive(Clone, Debug)]
pub struct ServiceInstance {
    pub id: String,
    pub task_run_id: String,
    pub project_id: String,
    pub preset_id: String,
    pub service_type: String,
    pub status: String,
    pub pod_name: Option<String>,
    pub service_name: Option<String>,
    pub secret_ref: Option<String>,
    pub conn_env_var: Option<String>,
    pub conn_string: Option<String>,
    pub error: Option<String>,
}

fn map_instance(r: &sqlx::postgres::PgRow) -> ServiceInstance {
    ServiceInstance {
        id: r.get("id"),
        task_run_id: r.get("task_run_id"),
        project_id: r.get("project_id"),
        preset_id: r.get("preset_id"),
        service_type: r.get("service_type"),
        status: r.get("status"),
        pod_name: r.get("pod_name"),
        service_name: r.get("service_name"),
        secret_ref: r.get("secret_ref"),
        conn_env_var: r.get("conn_env_var"),
        conn_string: r.get("conn_string"),
        error: r.get("error"),
    }
}

const INSTANCE_COLS: &str = r#"id, task_run_id, project_id, preset_id, service_type, status,
    pod_name, service_name, secret_ref, conn_env_var, conn_string, error"#;

pub struct ServiceInstanceRepository {
    db: Database,
}

impl ServiceInstanceRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Idempotent claim: insert a `requested` row for (task_run, service_type)
    /// or return the existing one. Returns `(instance, created)`.
    pub async fn claim(
        &self,
        id: &str,
        task_run_id: &str,
        project_id: &str,
        preset_id: &str,
        service_type: &str,
    ) -> Result<(ServiceInstance, bool)> {
        self.db.ensure_initialized().await?;
        // ON CONFLICT DO NOTHING then read back — returns the existing row when
        // a concurrent/earlier request already claimed this (task, type).
        let res = sqlx::query(
            r#"INSERT INTO service_instances
                  (id, task_run_id, project_id, preset_id, service_type, status, conn_env_var)
               VALUES ($1, $2, $3, $4, $5, 'requested', NULL)
               ON CONFLICT (task_run_id, service_type) DO NOTHING"#,
        )
        .bind(id)
        .bind(task_run_id)
        .bind(project_id)
        .bind(preset_id)
        .bind(service_type)
        .execute(self.db.pool())
        .await?;
        let created = res.rows_affected() > 0;
        let q = format!(
            "SELECT {INSTANCE_COLS} FROM service_instances WHERE task_run_id = $1 AND service_type = $2"
        );
        let row = sqlx::query(&q)
            .bind(task_run_id)
            .bind(service_type)
            .fetch_one(self.db.pool())
            .await?;
        Ok((map_instance(&row), created))
    }

    pub async fn get(&self, id: &str) -> Result<Option<ServiceInstance>> {
        self.db.ensure_initialized().await?;
        let q = format!("SELECT {INSTANCE_COLS} FROM service_instances WHERE id = $1");
        let row = sqlx::query(&q)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.as_ref().map(map_instance))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn mark_ready(
        &self,
        id: &str,
        pod_name: &str,
        service_name: &str,
        secret_ref: Option<&str>,
        conn_env_var: &str,
        conn_string: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE service_instances
                  SET status = 'ready', pod_name = $2, service_name = $3, secret_ref = $4,
                      conn_env_var = $5, conn_string = $6,
                      ready_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                WHERE id = $1"#,
        )
        .bind(id)
        .bind(pod_name)
        .bind(service_name)
        .bind(secret_ref)
        .bind(conn_env_var)
        .bind(conn_string)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_failed(&self, id: &str, error: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE service_instances SET status = 'failed', error = $2 WHERE id = $1")
            .bind(id)
            .bind(error)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    pub async fn mark_released(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE service_instances
                  SET status = 'released',
                      released_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                WHERE id = $1"#,
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn list_for_task(&self, task_run_id: &str) -> Result<Vec<ServiceInstance>> {
        self.db.ensure_initialized().await?;
        let q = format!("SELECT {INSTANCE_COLS} FROM service_instances WHERE task_run_id = $1");
        let rows = sqlx::query(&q)
            .bind(task_run_id)
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows.iter().map(map_instance).collect())
    }
}

pub struct ServicePolicyRepository {
    db: Database,
}

impl ServicePolicyRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// True unless a row explicitly disables services for the project (default
    /// allow — the image allowlist is the primary gate).
    pub async fn allow_services(&self, project_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query("SELECT allow_services FROM service_policy WHERE project_id = $1")
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row
            .map(|r| r.get::<bool, _>("allow_services"))
            .unwrap_or(true))
    }

    pub async fn set_allow_services(&self, project_id: &str, allow: bool) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO service_policy (project_id, allow_services)
               VALUES ($1, $2)
               ON CONFLICT (project_id) DO UPDATE
                   SET allow_services = EXCLUDED.allow_services,
                       updated_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
        )
        .bind(project_id)
        .bind(allow)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;

    use crate::repositories::project::ProjectRepository;

    async fn seed_project(db: &Database, id: &str) {
        db.ensure_initialized().await.unwrap();
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create_with_id(id, &format!("p-{id}"), "test", id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn default_presets_seeded() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ServicePresetRepository::new(db.clone());
        let presets = repo.list().await.unwrap();
        assert_eq!(presets.len(), 3, "postgres/redis/rabbitmq seeded");
        let pg = repo
            .get("preset-postgres-18")
            .await
            .unwrap()
            .expect("pg preset");
        assert_eq!(pg.service_type, "postgres");
        assert_eq!(pg.conn_env_var, "TEST_POSTGRES_URL");
    }

    #[tokio::test]
    async fn instance_claim_is_idempotent_per_task_type() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ServiceInstanceRepository::new(db.clone());
        let (a, created_a) = repo
            .claim("i1", "tr1", "p1", "preset-postgres-18", "postgres")
            .await
            .unwrap();
        assert!(created_a);
        let (b, created_b) = repo
            .claim("i2", "tr1", "p1", "preset-postgres-18", "postgres")
            .await
            .unwrap();
        assert!(
            !created_b,
            "second claim for same (task,type) returns existing"
        );
        assert_eq!(a.id, b.id);
        repo.mark_ready(
            &a.id,
            "pod-x",
            "svc-x",
            None,
            "TEST_POSTGRES_URL",
            "postgres://x",
        )
        .await
        .unwrap();
        assert_eq!(repo.get(&a.id).await.unwrap().unwrap().status, "ready");
    }

    #[tokio::test]
    async fn policy_defaults_allow_and_toggles() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ServicePolicyRepository::new(db.clone());
        assert!(repo.allow_services("p1").await.unwrap());
        repo.set_allow_services("p1", false).await.unwrap();
        assert!(!repo.allow_services("p1").await.unwrap());
    }
}

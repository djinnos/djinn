//! Backing-service preset catalog (migration 47, renamed/trimmed in 66).
//!
//! `ServicePresetRepository` reads the curated catalog of injectable services
//! (Postgres/Redis/RabbitMQ). Which presets an image injects lives in the
//! `image_service_presets` junction (see [`crate::ImageRepository`]); the
//! injection itself is done by djinn-k8s as a native sidecar. There is no
//! per-task provisioning state any more — the sidecar's lifecycle is the Pod's.
//! Non-macro `sqlx::query` form (like the other Phase A/B repos).

use sqlx::Row;

use crate::Result;
use crate::database::Database;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}

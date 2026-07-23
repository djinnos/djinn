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
    /// Immutable manifest digest consumed by strict canonical verification.
    pub image_digest: Option<String>,
    /// Revision of the catalog wrapper verification protocol.
    pub verification_protocol_revision: Option<i32>,
    pub port: i32,
    pub env: String,       // JSON object (text)
    pub resources: String, // JSON object (text)
    pub conn_template: String,
    /// One OR MORE env-var names (comma-separated) the worker exports for this
    /// connection. The same rendered connection string is emitted under each
    /// name — e.g. `DATABASE_URL,TEST_POSTGRES_URL` exports both.
    pub conn_env_var: String,
    /// Optional system (apt) package providing this service's command-line
    /// client (e.g. `postgresql-client` for `psql`). Auto-installed into any
    /// catalog image that attaches the preset. `None` = no client to install.
    pub client_package: Option<String>,
}

fn map_preset(r: &sqlx::postgres::PgRow) -> ServicePreset {
    ServicePreset {
        id: r.get("id"),
        name: r.get("name"),
        service_type: r.get("service_type"),
        image: r.get("image"),
        image_digest: r.try_get("image_digest").ok(),
        verification_protocol_revision: r.try_get("verification_protocol_revision").ok(),
        port: r.get("port"),
        env: r.get("env"),
        resources: r.get("resources"),
        conn_template: r.get("conn_template"),
        conn_env_var: r.get("conn_env_var"),
        // NULL maps to None; tolerate the column being absent on older schemas.
        client_package: r.try_get("client_package").ok().flatten(),
    }
}

const PRESET_COLS: &str = r#"id, name, service_type, image, port,
    env::text AS env, resources::text AS resources, conn_template, conn_env_var,
    client_package, image_digest, verification_protocol_revision"#;

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
        // Migration 73 overloads conn_env_var as a comma-separated list so the
        // sidecar exports the connection under the conventional DATABASE_URL too.
        assert_eq!(pg.conn_env_var, "DATABASE_URL,TEST_POSTGRES_URL");
        assert_eq!(pg.client_package.as_deref(), Some("postgresql-client"));
    }
}

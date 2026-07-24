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
    /// Catalog-owned wrapper artifact repository reference (no digest suffix).
    /// The deployable wrapper image is `{wrapper_image}@{image_digest}`; the
    /// wrapper packages the stock service runtime plus the protocol-v1 control
    /// server. `None` = legacy preset with no wrapper (dispatch-only, never
    /// eligible for strict canonical verification).
    pub wrapper_image: Option<String>,
    /// Immutable manifest digest of the wrapper artifact, consumed by strict
    /// canonical verification. Recorded by the build/controller path; the seed
    /// leaves it NULL so strict resolution fails closed until a real digest is
    /// published.
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
        wrapper_image: r.try_get("wrapper_image").ok().flatten(),
        image_digest: r.try_get("image_digest").ok().flatten(),
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
    client_package, wrapper_image, image_digest, verification_protocol_revision"#;

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

    /// Record the build/controller-published wrapper artifact identity for a
    /// preset: the wrapper repository reference and its immutable manifest
    /// digest. This is the sole write path for wrapper identity — presets are
    /// otherwise seeded by migrations, and strict resolution stays fail-closed
    /// until a real digest is recorded here.
    ///
    /// `wrapper_image` must be a plain repository reference with no digest
    /// suffix; `image_digest` must be a `sha256:<64 lowercase hex>` literal.
    /// Malformed arguments are rejected before touching the database so a
    /// fabricated or mutable identity can never be persisted. Returns the number
    /// of rows updated (0 when the preset id is unknown).
    pub async fn set_wrapper_identity(
        &self,
        preset_id: &str,
        wrapper_image: &str,
        image_digest: &str,
    ) -> Result<u64> {
        if wrapper_image.trim().is_empty() || wrapper_image.contains('@') {
            return Err(crate::error::DbError::InvalidData(format!(
                "wrapper_image for preset {preset_id} must be a digest-free repository reference"
            )));
        }
        if !is_sha256_digest(image_digest) {
            return Err(crate::error::DbError::InvalidData(format!(
                "image_digest for preset {preset_id} must be sha256:<64 lowercase hex>"
            )));
        }
        self.db.ensure_initialized().await?;
        let result = sqlx::query(
            "UPDATE service_presets SET wrapper_image = $2, image_digest = $3, \
             verification_protocol_revision = COALESCE(verification_protocol_revision, 1) \
             WHERE id = $1",
        )
        .bind(preset_id)
        .bind(wrapper_image)
        .bind(image_digest)
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }
}

/// A `sha256:<64 lowercase hex>` digest literal.
fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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

    /// AC1: the seed records catalog-owned wrapper repositories but leaves the
    /// immutable digest NULL, so strict resolution stays fail-closed until a
    /// real published digest is recorded (no fabricated literals survive).
    #[tokio::test]
    async fn seed_records_wrapper_repository_without_a_digest() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ServicePresetRepository::new(db.clone());
        for (id, wrapper) in [
            (
                "preset-postgres-18",
                "ghcr.io/djinnos/djinn-postgres-wrapper",
            ),
            ("preset-redis-7", "ghcr.io/djinnos/djinn-redis-wrapper"),
            (
                "preset-rabbitmq-4",
                "ghcr.io/djinnos/djinn-rabbitmq-wrapper",
            ),
        ] {
            let preset = repo.get(id).await.unwrap().expect("seeded preset");
            assert_eq!(preset.wrapper_image.as_deref(), Some(wrapper));
            assert!(
                preset.image_digest.is_none(),
                "fabricated placeholder digest must be cleared for {id}"
            );
        }
    }

    /// AC1: the build/controller write path records a real digest and rejects
    /// mutable/fabricated identities before touching the database.
    #[tokio::test]
    async fn set_wrapper_identity_records_real_digest_and_rejects_malformed() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ServicePresetRepository::new(db.clone());
        let digest = "sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

        // A digest-suffixed wrapper reference is a mutable/ambiguous identity.
        assert!(matches!(
            repo.set_wrapper_identity("preset-redis-7", "repo@sha256:dead", digest)
                .await,
            Err(crate::error::DbError::InvalidData(_))
        ));
        // A non-sha256 digest is rejected before the database write.
        assert!(matches!(
            repo.set_wrapper_identity("preset-redis-7", "repo", "latest")
                .await,
            Err(crate::error::DbError::InvalidData(_))
        ));

        let updated = repo
            .set_wrapper_identity(
                "preset-redis-7",
                "ghcr.io/djinnos/djinn-redis-wrapper",
                digest,
            )
            .await
            .unwrap();
        assert_eq!(updated, 1);
        let redis = repo.get("preset-redis-7").await.unwrap().expect("preset");
        assert_eq!(redis.image_digest.as_deref(), Some(digest));
        assert_eq!(redis.verification_protocol_revision, Some(1));

        // Unknown preset id updates no rows.
        assert_eq!(
            repo.set_wrapper_identity("preset-nope", "repo", digest)
                .await
                .unwrap(),
            0
        );
    }
}

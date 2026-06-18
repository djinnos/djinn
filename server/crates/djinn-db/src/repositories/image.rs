//! Registered image catalog (`images` + `image_builds`, migration 46).
//!
//! A small curated set of named images projects pick from. An image's `config`
//! is a serialized `djinn_stack` EnvironmentConfig (build fields), so the
//! existing Dockerfile generator + content hash apply unchanged. Identity is
//! the content hash + the immutable registry digest.
//!
//! Non-macro `sqlx::query` form (like the verification repos) so a fresh table
//! doesn't require regenerating the offline `.sqlx` cache.

use sqlx::Row;

use crate::Result;
use crate::database::Database;

/// Image build lifecycle states (mirror `ProjectImageStatus`).
pub struct ImageStatus;
impl ImageStatus {
    pub const NONE: &'static str = "none";
    pub const BUILDING: &'static str = "building";
    pub const READY: &'static str = "ready";
    pub const FAILED: &'static str = "failed";
}

/// A row of `images`. The JSON `config` field is returned as raw text; callers
/// parse it against `djinn-stack`. The set of injected backing services lives in
/// the `image_service_presets` junction (see [`ImageRepository::list_service_presets`]),
/// not on the row.
#[derive(Clone, Debug)]
pub struct Image {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub config: String,
    pub config_hash: Option<String>,
    pub tag: Option<String>,
    pub registry_digest: Option<String>,
    pub status: String,
    pub last_error: Option<String>,
}

fn map_image(r: &sqlx::postgres::PgRow) -> Image {
    Image {
        id: r.get("id"),
        name: r.get("name"),
        description: r.get("description"),
        config: r.get("config"),
        config_hash: r.get("config_hash"),
        tag: r.get("tag"),
        registry_digest: r.get("registry_digest"),
        status: r.get("status"),
        last_error: r.get("last_error"),
    }
}

const SELECT_COLS: &str = r#"id, name, description,
    config::text AS config, config_hash, tag, registry_digest, status, last_error"#;

pub struct ImageRepository {
    db: Database,
}

impl ImageRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Register a new image (status `none` — the controller builds it on the
    /// next tick). `config_json` is a serialized EnvironmentConfig.
    pub async fn create(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        config_json: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let config: serde_json::Value = serde_json::from_str(config_json)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        sqlx::query(
            r#"INSERT INTO images (id, name, description, config, status)
               VALUES ($1, $2, $3, $4::jsonb, 'none')"#,
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(config)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Image>> {
        self.db.ensure_initialized().await?;
        let q = format!("SELECT {SELECT_COLS} FROM images WHERE id = $1");
        let row = sqlx::query(&q)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.as_ref().map(map_image))
    }

    pub async fn list(&self) -> Result<Vec<Image>> {
        self.db.ensure_initialized().await?;
        let q = format!("SELECT {SELECT_COLS} FROM images ORDER BY name");
        let rows = sqlx::query(&q).fetch_all(self.db.pool()).await?;
        Ok(rows.iter().map(map_image).collect())
    }

    /// Update an image's name/description/config. Changing the config resets the
    /// build state (status → none, hash/tag/digest cleared) so the controller
    /// rebuilds it on the next tick.
    pub async fn update(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        config_json: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let config: serde_json::Value = serde_json::from_str(config_json)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        sqlx::query(
            r#"UPDATE images
                  SET name = $2, description = $3, config = $4::jsonb,
                      status = 'none', config_hash = NULL, tag = NULL,
                      registry_digest = NULL, last_error = NULL,
                      updated_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                WHERE id = $1"#,
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(config)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Delete an image. Fails (FK RESTRICT) if a project still references it.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM images WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    // ── controller-facing build status transitions ──────────────────────────

    pub async fn mark_building(&self, id: &str, config_hash: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE images SET status = 'building', config_hash = $2, last_error = NULL WHERE id = $1",
        )
        .bind(id)
        .bind(config_hash)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_ready(
        &self,
        id: &str,
        tag: &str,
        registry_digest: Option<&str>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE images SET status = 'ready', tag = $2, registry_digest = $3, last_error = NULL,
                      updated_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1"#,
        )
        .bind(id)
        .bind(tag)
        .bind(registry_digest)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_failed(&self, id: &str, error: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE images SET status = 'failed', last_error = $2 WHERE id = $1")
            .bind(id)
            .bind(error)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    // ── injected backing services (junction table, migration 66) ────────────

    /// Service-preset ids injected as native sidecars into every Pod that runs
    /// this image.
    pub async fn list_service_presets(&self, image_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query("SELECT preset_id FROM image_service_presets WHERE image_id = $1")
            .bind(image_id)
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| r.get::<String, _>("preset_id"))
            .collect())
    }

    /// Replace the image's injected-service set wholesale.
    pub async fn set_service_presets(&self, image_id: &str, preset_ids: &[String]) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("DELETE FROM image_service_presets WHERE image_id = $1")
            .bind(image_id)
            .execute(&mut *tx)
            .await?;
        for preset_id in preset_ids {
            sqlx::query(
                "INSERT INTO image_service_presets (image_id, preset_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(image_id)
            .bind(preset_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // ── project ↔ image selection ───────────────────────────────────────────

    /// Assign (or clear, with `None`) a project's selected catalog image.
    pub async fn set_project_image(&self, project_id: &str, image_id: Option<&str>) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE projects SET selected_image_id = $2 WHERE id = $1")
            .bind(project_id)
            .bind(image_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Project ids currently assigned this image (for re-applying config on
    /// an image edit).
    pub async fn projects_using(&self, image_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query("SELECT id FROM projects WHERE selected_image_id = $1")
            .bind(image_id)
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Resolve a project's selected catalog image. Returns `None` when the
    /// project has no selection (→ per-project build fallback). Two-step (read
    /// the FK, then fetch the image) to avoid `id`/`name` column ambiguity in a
    /// projects⋈images join.
    pub async fn resolve_for_project(&self, project_id: &str) -> Result<Option<Image>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query("SELECT selected_image_id FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await?;
        let image_id: Option<String> = row.and_then(|r| r.get("selected_image_id"));
        match image_id {
            Some(id) => self.get(&id).await,
            None => Ok(None),
        }
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
    async fn create_get_list_update() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Go", Some("the go image"), r#"{"schema_version":1}"#)
            .await
            .unwrap();
        let img = repo.get("i1").await.unwrap().expect("row");
        assert_eq!(img.name, "Go");
        assert_eq!(img.status, ImageStatus::NONE);
        repo.mark_ready("i1", "ghcr/x:abc", Some("sha256:deadbeef"))
            .await
            .unwrap();
        assert_eq!(repo.get("i1").await.unwrap().unwrap().status, "ready");
        // Editing config resets build state.
        repo.update(
            "i1",
            "Go",
            None,
            r#"{"schema_version":1,"system_packages":["libpq-dev"]}"#,
        )
        .await
        .unwrap();
        let after = repo.get("i1").await.unwrap().unwrap();
        assert_eq!(after.status, "none");
        assert!(after.registry_digest.is_none());
        assert_eq!(repo.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn project_selection_and_resolve() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Rust", None, "{}").await.unwrap();
        assert!(repo.resolve_for_project("p1").await.unwrap().is_none());
        repo.set_project_image("p1", Some("i1")).await.unwrap();
        let resolved = repo
            .resolve_for_project("p1")
            .await
            .unwrap()
            .expect("resolved");
        assert_eq!(resolved.id, "i1");
        repo.set_project_image("p1", None).await.unwrap();
        assert!(repo.resolve_for_project("p1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_dispatch_image_is_catalog_only() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let images = ImageRepository::new(db.clone());
        let projects = ProjectRepository::new(db.clone(), EventBus::noop());

        // Unknown project → None.
        assert!(
            projects
                .resolve_dispatch_image("nope")
                .await
                .unwrap()
                .is_none()
        );

        // No catalog image assigned → project exists but is NOT dispatchable
        // (projects no longer build a bespoke image — they need a catalog one).
        let d = projects
            .resolve_dispatch_image("p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(d.from_catalog, None);
        assert!(d.pull_ref().is_none());

        // Assign a catalog image that is NOT ready yet → not dispatchable.
        images.create("i1", "Rust", None, "{}").await.unwrap();
        images.set_project_image("p1", Some("i1")).await.unwrap();
        let d = projects
            .resolve_dispatch_image("p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(d.from_catalog.as_deref(), Some("i1"));
        assert!(
            d.pull_ref().is_none(),
            "catalog image not ready ⇒ not dispatchable"
        );

        // Mark the catalog image ready with a digest → digest-pinned pull ref.
        images
            .mark_ready("i1", "reg/djinn-image-i1:hash", Some("sha256:abc"))
            .await
            .unwrap();
        let d = projects
            .resolve_dispatch_image("p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(d.from_catalog.as_deref(), Some("i1"));
        assert_eq!(
            d.pull_ref().as_deref(),
            Some("reg/djinn-image-i1@sha256:abc"),
            "ready catalog image with a digest dispatches on the digest-pinned ref"
        );

        // Clearing the selection → back to not-dispatchable (needs setup).
        images.set_project_image("p1", None).await.unwrap();
        let d = projects
            .resolve_dispatch_image("p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(d.from_catalog, None);
        assert!(d.pull_ref().is_none());
    }

    #[tokio::test]
    async fn delete_restricted_while_referenced() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Node", None, "{}").await.unwrap();
        repo.set_project_image("p1", Some("i1")).await.unwrap();
        // FK RESTRICT: delete must fail while p1 references it.
        assert!(repo.delete("i1").await.is_err());
        repo.set_project_image("p1", None).await.unwrap();
        assert!(repo.delete("i1").await.is_ok());
    }
}

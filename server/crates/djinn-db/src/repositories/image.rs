//! Registered image catalog (`images` + `image_service_presets`, migration 46/66).
//!
//! A small curated set of named images projects pick from. An image's `config`
//! is a serialized `djinn_stack` EnvironmentConfig (build fields), so the
//! existing Dockerfile generator + content hash apply unchanged. Identity is
//! the content hash + the immutable registry digest.
//!
//! These queries deliberately use the **runtime** `sqlx::query` API (not the
//! compile-time `query!` macros). The image tables are young and their schema
//! still evolves; runtime queries keep them buildable without regenerating the
//! committed `.sqlx` offline cache on every migration. If the tables stabilise,
//! converting to `query!` would add compile-time SQL validation.

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

/// A catalog image currently selected by at least one project, with the
/// data retention preflight needs to prove it remains pullable after
/// destructive tag deletion.
///
/// Unlike [`crate::DispatchImage`] (which resolves a single project's
/// selection), this struct aggregates *all* projects selecting the same
/// image so the retention preflight can report blast radius. It deliberately
/// includes not-ready and digestless images — fail-closed safety requires
/// knowing about every selected image, even if it's not yet pullable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedCatalogImage {
    /// `images.id`.
    pub image_id: String,
    /// Human-readable name from `images.name`.
    pub name: String,
    /// Registry tag (e.g. `reg/djinn-image-<id>:<hash>`), `None` if not yet built.
    pub tag: Option<String>,
    /// Immutable manifest digest (`sha256:…`), `None` if not captured.
    pub registry_digest: Option<String>,
    /// Build status: `none` | `building` | `ready` | `failed`.
    pub status: String,
    /// Last build error, surfaced in preflight reports.
    pub last_error: Option<String>,
    /// Project ids selecting this image (sorted; non-empty by construction).
    pub selected_project_ids: Vec<String>,
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
        let rows = sqlx::query(
            "SELECT preset_id FROM image_service_presets WHERE image_id = $1 ORDER BY preset_id",
        )
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

    /// Enumerate every catalog image currently selected by at least one
    /// project, with enough data for retention preflight to prove each
    /// remains pullable after destructive tag deletion.
    ///
    /// Each row aggregates *all* projects selecting the same image
    /// (`selected_project_ids`, sorted) so the preflight can report the
    /// blast radius. Images that are not yet `ready`, or that lack a
    /// digest, are still included — fail-closed safety demands knowing
    /// about every selected image, even if it's not yet pullable.
    ///
    /// Returns rows ordered by image id for deterministic iteration.
    pub async fn list_selected_catalog_images(&self) -> Result<Vec<SelectedCatalogImage>> {
        self.db.ensure_initialized().await?;
        // Single join: every image that at least one project points at.
        // `selected_project_ids` is built by collecting per-image project ids
        // in a second pass (avoids array_agg ordering non-determinism across
        // Postgres versions).
        let rows = sqlx::query(
            r#"SELECT i.id      AS image_id,
                      i.name     AS name,
                      i.tag      AS tag,
                      i.registry_digest AS registry_digest,
                      i.status   AS status,
                      i.last_error AS last_error,
                      p.id       AS project_id
                 FROM images i
                 JOIN projects p ON p.selected_image_id = i.id
                ORDER BY i.id, p.id"#,
        )
        .fetch_all(self.db.pool())
        .await?;

        // Group by image_id, preserving the image-id ordering.
        let mut out: Vec<SelectedCatalogImage> = Vec::new();
        for r in &rows {
            let image_id: String = r.get("image_id");
            if out.last().is_none_or(|e| e.image_id != image_id) {
                out.push(SelectedCatalogImage {
                    image_id: image_id.clone(),
                    name: r.get("name"),
                    tag: r.get("tag"),
                    registry_digest: r.get("registry_digest"),
                    status: r.get("status"),
                    last_error: r.get("last_error"),
                    selected_project_ids: Vec::new(),
                });
            }
            let project_id: String = r.get("project_id");
            out.last_mut()
                .unwrap()
                .selected_project_ids
                .push(project_id);
        }
        Ok(out)
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

    /// Prove that `lifecycle.pre_task` JSONB is preserved through the full
    /// create → get → list → update → get round-trip without dropping or
    /// reshaping fields.  This is the repository-level proof required by the
    /// "round-trip lifecycle.pre_task through image repository" task.
    #[tokio::test]
    async fn pre_task_jsonb_round_trip() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ImageRepository::new(db.clone());

        // Config with a single pre-task command (all fields populated).
        let config_json = r#"{
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [
                    {
                        "name": "install-deps",
                        "command": "pip install -e .",
                        "timeout_seconds": 120,
                        "failure_policy": "blocking"
                    }
                ]
            }
        }"#;

        // ── create ──────────────────────────────────────────────────────────
        repo.create("pt1", "Python-pre-task", Some("pre-task test"), config_json)
            .await
            .unwrap();

        // ── get ─────────────────────────────────────────────────────────────
        let img = repo.get("pt1").await.unwrap().expect("row after create");
        let parsed: serde_json::Value =
            serde_json::from_str(&img.config).expect("config must be valid JSON after get");
        let pre_task = &parsed["lifecycle"]["pre_task"];
        let cmds = pre_task.as_array().expect("pre_task is array");
        assert_eq!(cmds.len(), 1, "expected one pre-task command");
        assert_eq!(cmds[0]["name"], "install-deps");
        assert_eq!(cmds[0]["command"], "pip install -e .");
        assert_eq!(cmds[0]["timeout_seconds"], 120);
        assert_eq!(cmds[0]["failure_policy"], "blocking");

        // ── list ────────────────────────────────────────────────────────────
        let all = repo.list().await.unwrap();
        assert_eq!(all.len(), 1);
        let listed_parsed: serde_json::Value =
            serde_json::from_str(&all[0].config).expect("list config JSON");
        let listed_cmds = listed_parsed["lifecycle"]["pre_task"].as_array().unwrap();
        assert_eq!(listed_cmds.len(), 1);
        assert_eq!(listed_cmds[0]["name"], "install-deps");
        assert_eq!(listed_cmds[0]["command"], "pip install -e .");
        assert_eq!(listed_cmds[0]["timeout_seconds"], 120);
        assert_eq!(listed_cmds[0]["failure_policy"], "blocking");

        // ── update with a different pre-task set ────────────────────────────
        let updated_config = r#"{
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [
                    {
                        "name": "migrate",
                        "command": "python manage.py migrate",
                        "timeout_seconds": 180,
                        "failure_policy": "best_effort"
                    },
                    {
                        "command": "npm ci",
                        "timeout_seconds": 300
                    }
                ]
            }
        }"#;
        repo.update("pt1", "Python-pre-task", None, updated_config)
            .await
            .unwrap();

        // Build state reset on update.
        let after = repo.get("pt1").await.unwrap().expect("row after update");
        assert_eq!(after.status, "none", "update resets status");
        assert!(after.config_hash.is_none());

        // Verify updated pre-task JSONB.
        let after_parsed: serde_json::Value =
            serde_json::from_str(&after.config).expect("updated config JSON");
        let after_cmds = after_parsed["lifecycle"]["pre_task"]
            .as_array()
            .expect("pre_task is array after update");
        assert_eq!(
            after_cmds.len(),
            2,
            "expected two pre-task commands after update"
        );
        assert_eq!(after_cmds[0]["name"], "migrate");
        assert_eq!(after_cmds[0]["command"], "python manage.py migrate");
        assert_eq!(after_cmds[0]["timeout_seconds"], 180);
        assert_eq!(after_cmds[0]["failure_policy"], "best_effort");
        assert_eq!(after_cmds[1]["command"], "npm ci");
        assert_eq!(after_cmds[1]["timeout_seconds"], 300);

        // ── list after update ───────────────────────────────────────────────
        let all_after = repo.list().await.unwrap();
        assert_eq!(all_after.len(), 1);
        let list_after_parsed: serde_json::Value =
            serde_json::from_str(&all_after[0].config).expect("list config JSON after update");
        let list_after_cmds = list_after_parsed["lifecycle"]["pre_task"]
            .as_array()
            .unwrap();
        assert_eq!(list_after_cmds.len(), 2);
        assert_eq!(list_after_cmds[0]["name"], "migrate");
        assert_eq!(list_after_cmds[1]["command"], "npm ci");
    }

    /// Config without a `lifecycle` key must store and return a config where
    /// `lifecycle.pre_task` is absent (serde default), not corrupted.
    #[tokio::test]
    async fn absent_lifecycle_survives_jsonb_round_trip() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ImageRepository::new(db.clone());

        repo.create("abs1", "No-lifecycle", None, r#"{"schema_version":1}"#)
            .await
            .unwrap();

        let img = repo.get("abs1").await.unwrap().expect("row");
        let parsed: serde_json::Value = serde_json::from_str(&img.config).expect("config JSON");
        // lifecycle should default to empty if absent.
        let lifecycle = &parsed["lifecycle"];
        if !lifecycle.is_null() {
            // If serde re-emits the default lifecycle, pre_task must be empty.
            let pre_task = &lifecycle["pre_task"];
            assert!(
                pre_task.is_null() || pre_task.as_array().is_none_or(|a| a.is_empty()),
                "absent lifecycle should not have spurious pre_task entries, got: {pre_task}"
            );
        }
    }

    // ── list_selected_catalog_images ──────────────────────────────────────

    #[tokio::test]
    async fn list_selected_catalog_images_no_selections() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ImageRepository::new(db.clone());
        // Image exists but no project selects it.
        repo.create("i1", "Rust", None, "{}").await.unwrap();
        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert!(rows.is_empty(), "no projects select anything");
    }

    #[tokio::test]
    async fn list_selected_catalog_images_multiple_projects_same_image() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        seed_project(&db, "p2").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Rust", None, "{}").await.unwrap();
        repo.set_project_image("p1", Some("i1")).await.unwrap();
        repo.set_project_image("p2", Some("i1")).await.unwrap();

        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].image_id, "i1");
        assert_eq!(rows[0].selected_project_ids, vec!["p1", "p2"]);
    }

    #[tokio::test]
    async fn list_selected_catalog_images_ready_with_digest() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Go", None, "{}").await.unwrap();
        repo.set_project_image("p1", Some("i1")).await.unwrap();
        repo.mark_ready("i1", "reg/djinn-image-i1:hash", Some("sha256:abc"))
            .await
            .unwrap();

        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "ready");
        assert_eq!(rows[0].tag.as_deref(), Some("reg/djinn-image-i1:hash"));
        assert_eq!(rows[0].registry_digest.as_deref(), Some("sha256:abc"));
    }

    #[tokio::test]
    async fn list_selected_catalog_images_ready_without_digest() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Node", None, "{}").await.unwrap();
        repo.set_project_image("p1", Some("i1")).await.unwrap();
        repo.mark_ready("i1", "reg/djinn-image-i1:hash", None)
            .await
            .unwrap();

        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "ready");
        assert_eq!(rows[0].tag.as_deref(), Some("reg/djinn-image-i1:hash"));
        assert!(
            rows[0].registry_digest.is_none(),
            "digestless image must still be enumerated for fail-closed safety"
        );
    }

    #[tokio::test]
    async fn list_selected_catalog_images_includes_not_ready() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Python", None, "{}").await.unwrap();
        repo.set_project_image("p1", Some("i1")).await.unwrap();
        // Image is `none` (not built yet) — fail-closed safety needs to see it.
        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "none");
        assert!(rows[0].tag.is_none());
        assert!(rows[0].registry_digest.is_none());
    }

    #[tokio::test]
    async fn list_selected_catalog_images_excludes_unselected() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        seed_project(&db, "p2").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("i1", "Rust", None, "{}").await.unwrap();
        repo.create("i2", "Go", None, "{}").await.unwrap();
        // Only p1 selects i1; p2 has no selection.
        repo.set_project_image("p1", Some("i1")).await.unwrap();

        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].image_id, "i1");
    }

    #[tokio::test]
    async fn list_selected_catalog_images_deterministic_order() {
        let db = Database::open_in_memory().unwrap();
        seed_project(&db, "p1").await;
        seed_project(&db, "p2").await;
        let repo = ImageRepository::new(db.clone());
        repo.create("img-b", "Beta", None, "{}").await.unwrap();
        repo.create("img-a", "Alpha", None, "{}").await.unwrap();
        repo.set_project_image("p1", Some("img-a")).await.unwrap();
        repo.set_project_image("p2", Some("img-b")).await.unwrap();

        let rows = repo.list_selected_catalog_images().await.unwrap();
        assert_eq!(rows.len(), 2);
        // Ordered by image id.
        assert_eq!(rows[0].image_id, "img-a");
        assert_eq!(rows[1].image_id, "img-b");
    }
}

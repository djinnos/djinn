use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;

use crate::Result;
use crate::database::Database;

/// A single verification rule: a glob pattern matched against changed file paths,
/// and the commands to run when that pattern matches.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct VerificationRule {
    /// Glob pattern (e.g. `src/**/*.rs`, `**` for catch-all).
    pub match_pattern: String,
    /// One or more shell commands to execute when the pattern matches.
    pub commands: Vec<String>,
}

impl VerificationRule {
    /// Validate a single rule.
    ///
    /// Returns `Err` with a human-readable message if:
    /// - `match_pattern` is not valid glob syntax
    /// - `commands` is empty
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.commands.is_empty() {
            return Err(format!(
                "rule for pattern '{}': commands must not be empty",
                self.match_pattern
            ));
        }
        globset::GlobBuilder::new(&self.match_pattern)
            .build()
            .map_err(|e| {
                format!(
                    "rule for pattern '{}': invalid glob syntax: {e}",
                    self.match_pattern
                )
            })?;
        Ok(())
    }
}

/// Validate a slice of verification rules. Returns `Err` with the first error found.
pub fn validate_verification_rules(rules: &[VerificationRule]) -> std::result::Result<(), String> {
    for rule in rules {
        rule.validate()?;
    }
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, sqlx::FromRow)]
pub struct ProjectConfig {
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
    /// JSON-encoded Vec<VerificationRule>, stored as TEXT in SQLite.
    pub verification_rules: String,
}

pub struct ProjectRepository {
    db: Database,
    events: EventBus,
}

impl ProjectRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub async fn list(&self) -> Result<Vec<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name, path,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      target_branch,
                      auto_merge as "auto_merge: bool",
                      sync_enabled as "sync_enabled: bool",
                      sync_remote
               FROM projects ORDER BY name"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name, path,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      target_branch,
                      auto_merge as "auto_merge: bool",
                      sync_enabled as "sync_enabled: bool",
                      sync_remote
               FROM projects WHERE id = ?"#,
            id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_path(&self, path: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name, path,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      target_branch,
                      auto_merge as "auto_merge: bool",
                      sync_enabled as "sync_enabled: bool",
                      sync_remote
               FROM projects WHERE path = ?"#,
            path,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a project path to its ID. Normalizes trailing slashes.
    pub async fn resolve_id_by_path(&self, project_path: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let normalized = project_path.trim_end_matches('/');
        Ok(
            sqlx::query_scalar!("SELECT id FROM projects WHERE path = ?", normalized)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    /// Resolve a project path to its ID, with fuzzy matching for subdirectories.
    /// If exact match fails, finds the project whose path is the longest prefix
    /// of the given path (useful when agents pass a subdirectory).
    pub async fn resolve_id_by_path_fuzzy(&self, project_path: &str) -> Result<Option<String>> {
        let normalized = project_path.trim_end_matches('/');

        // Try exact match first.
        if let Some(id) = self.resolve_id_by_path(normalized).await? {
            return Ok(Some(id));
        }

        // Fuzzy: find the project whose path is the longest prefix.
        self.db.ensure_initialized().await?;
        let all_rows = sqlx::query!("SELECT id, path FROM projects")
            .fetch_all(self.db.pool())
            .await?;

        let mut best: Option<(String, usize)> = None;
        for row in all_rows {
            let id = row.id;
            let path = row.path;
            let root = path.trim_end_matches('/');
            let matches = normalized
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with('/'));
            if matches {
                let len = root.len();
                if best.as_ref().is_none_or(|(_, best_len)| len > *best_len) {
                    best = Some((id, len));
                }
            }
        }

        Ok(best.map(|(id, _)| id))
    }

    /// Resolve a project path to its ID, creating a new project entry if not found.
    pub async fn resolve_or_create(&self, project_path: &str) -> Result<String> {
        if let Some(id) = self.resolve_id_by_path(project_path).await? {
            return Ok(id);
        }

        let name = std::path::Path::new(project_path)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("project");

        self.create(name, project_path).await.map(|p| p.id)
    }

    /// Get the filesystem path for a project by ID.
    pub async fn get_path(&self, id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_scalar!("SELECT path FROM projects WHERE id = ?", id)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    pub async fn create(&self, name: &str, path: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO projects (id, name, path, verification_rules) VALUES (?, ?, ?, ?)",
            id,
            name,
            path,
            "[]"
        )
        .execute(self.db.pool())
        .await?;
        let project = sqlx::query_as!(
            Project,
            r#"SELECT id, name, path, created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.seed_default_roles(&id).await?;

        self.events
            .send(DjinnEventEnvelope::project_created(&project));
        Ok(project)
    }

    /// Resolve the GitHub owner/repo coordinates persisted for a project.
    ///
    /// Returns `Ok(None)` if the project exists but was created before
    /// Migration 2 (legacy host-path rows leave `github_owner`/`github_repo`
    /// NULL) or if the project id is unknown. Callers use the presence of
    /// these coordinates to decide whether GitHub-App-authenticated pushes
    /// are possible at all for this project.
    pub async fn get_github_coords(&self, project_id: &str) -> Result<Option<(String, String)>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            "SELECT github_owner, github_repo FROM projects WHERE id = ?",
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.and_then(|r| match (r.github_owner, r.github_repo) {
            (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => {
                Some((owner, repo))
            }
            _ => None,
        }))
    }

    /// Return the cached GitHub App installation id for a project, if any.
    ///
    /// Returns `Ok(None)` when the project row has no cached installation
    /// (legacy host-path projects, Migration-2 rows written before Migration
    /// 4, or the project id is unknown). The push/PR-create path uses this
    /// to mint an installation token without discovering the installation
    /// at request time.
    pub async fn get_installation_id(&self, project_id: &str) -> Result<Option<u64>> {
        self.db.ensure_initialized().await?;
        let row: Option<Option<i64>> = sqlx::query_scalar!(
            "SELECT installation_id FROM projects WHERE id = ?",
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.flatten().map(|v| v as u64))
    }

    /// Resolve the default branch persisted for a project.
    ///
    /// Returns `Ok(None)` when the project row is unknown or the column
    /// is unset (legacy host-path rows written before Migration 2). The
    /// column is populated at clone time from the GitHub API response.
    pub async fn get_default_branch(&self, project_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row: Option<Option<String>> = sqlx::query_scalar!(
            "SELECT default_branch FROM projects WHERE id = ?",
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.flatten().filter(|s| !s.is_empty()))
    }

    /// Find a project by its GitHub owner/repo coordinates.
    ///
    /// Returns `Ok(None)` if no project row has both columns set to the
    /// provided values (e.g. legacy host-path projects).
    pub async fn get_by_github(&self, owner: &str, repo: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name, path, created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects
             WHERE github_owner = ? AND github_repo = ?"#,
            owner,
            repo
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Create a project backed by a GitHub repo the server has cloned locally.
    ///
    /// `clone_path` should be the absolute path inside the server container
    /// where the repo has been cloned (e.g. `/var/lib/djinn/projects/acme/widgets`
    /// under the Helm chart, or `~/.djinn/projects/acme/widgets` in docker-compose).
    /// `path` is set equal to `clone_path` so existing path-keyed joins and
    /// resolvers keep working without changes.
    ///
    /// `installation_id` is the GitHub App installation id that grants
    /// access to the repo; caching it here lets every later push/PR-create
    /// path mint installation tokens without walking the authenticated
    /// user's installations first (Migration 4).
    pub async fn create_from_github(
        &self,
        name: &str,
        owner: &str,
        repo: &str,
        default_branch: &str,
        clone_path: &str,
        installation_id: Option<u64>,
    ) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        // SQLite stores u64 as i64; cast lossily (installation IDs fit in i64
        // comfortably — GitHub uses ~8-digit values today).
        let installation_id_i64: Option<i64> = installation_id.map(|v| v as i64);
        sqlx::query!(
            "INSERT INTO projects
                (id, name, path, github_owner, github_repo, default_branch, clone_path, target_branch, installation_id, verification_rules)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            name,
            clone_path,
            owner,
            repo,
            default_branch,
            clone_path,
            default_branch,
            installation_id_i64,
            "[]"
        )
        .execute(self.db.pool())
        .await?;

        let project = sqlx::query_as!(
            Project,
            r#"SELECT id, name, path, created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.seed_default_roles(&id).await?;

        self.events
            .send(DjinnEventEnvelope::project_created(&project));
        Ok(project)
    }

    /// Insert 5 default agent roles (one per base_role) for a newly created project.
    /// Uses INSERT OR IGNORE so re-seeding is safe if called on an existing project.
    async fn seed_default_roles(&self, project_id: &str) -> Result<()> {
        const DEFAULT_ROLES: &[(&str, &str)] = &[
            (
                "worker",
                "Implements tasks: writes code, runs tests, resolves issues.",
            ),
            (
                "lead",
                "Technical lead: reviews plans, guides implementation, unblocks workers.",
            ),
            (
                "planner",
                "Decomposes epics into tasks and maintains the project board.",
            ),
            (
                "architect",
                "Proactive health monitoring, strategic re-planning, epic oversight.",
            ),
            (
                "reviewer",
                "Reviews pull requests and verifies task quality.",
            ),
        ];
        for (base_role, description) in DEFAULT_ROLES {
            let role_id = uuid::Uuid::now_v7().to_string();
            sqlx::query!(
                "INSERT IGNORE INTO agents
                    (id, project_id, `name`, base_role, description, is_default)
                 VALUES (?, ?, ?, ?, ?, 1)",
                role_id,
                project_id,
                base_role, // name = base_role for defaults
                base_role,
                description
            )
            .execute(self.db.pool())
            .await?;
        }
        Ok(())
    }

    pub async fn update(&self, id: &str, name: &str, path: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE projects SET name = ?, path = ? WHERE id = ?",
            name,
            path,
            id
        )
        .execute(self.db.pool())
        .await?;
        let project = sqlx::query_as!(
            Project,
            r#"SELECT id, name, path, created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events
            .send(DjinnEventEnvelope::project_updated(&project));
        Ok(project)
    }

    pub async fn get_config(&self, id: &str) -> Result<Option<ProjectConfig>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProjectConfig,
            r#"SELECT target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote, verification_rules
             FROM projects WHERE id = ?"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn update_config_field(
        &self,
        id: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<ProjectConfig>> {
        self.db.ensure_initialized().await?;
        match key {
            "target_branch" => {
                sqlx::query!(
                    "UPDATE projects SET target_branch = ? WHERE id = ?",
                    value,
                    id
                )
                .execute(self.db.pool())
                .await?;
            }
            "auto_merge" => {
                let v = matches!(value, "true" | "1");
                sqlx::query!("UPDATE projects SET auto_merge = ? WHERE id = ?", v, id)
                    .execute(self.db.pool())
                    .await?;
            }
            "sync_enabled" => {
                let v = matches!(value, "true" | "1");
                sqlx::query!("UPDATE projects SET sync_enabled = ? WHERE id = ?", v, id)
                    .execute(self.db.pool())
                    .await?;
            }
            "sync_remote" => {
                let val = if value.is_empty() { None } else { Some(value) };
                sqlx::query!("UPDATE projects SET sync_remote = ? WHERE id = ?", val, id)
                    .execute(self.db.pool())
                    .await?;
            }
            "verification_rules" => {
                // Parse the incoming JSON and validate each rule before persisting.
                let rules: Vec<VerificationRule> = serde_json::from_str(value).map_err(|e| {
                    crate::error::DbError::InvalidData(format!(
                        "verification_rules: invalid JSON: {e}"
                    ))
                })?;
                validate_verification_rules(&rules).map_err(crate::error::DbError::InvalidData)?;
                sqlx::query!(
                    "UPDATE projects SET verification_rules = ? WHERE id = ?",
                    value,
                    id
                )
                .execute(self.db.pool())
                .await?;
            }
            _ => return Ok(None),
        }

        let Some(config) = self.get_config(id).await? else {
            return Ok(None);
        };
        self.events
            .send(DjinnEventEnvelope::project_config_updated(id, &config));
        Ok(Some(config))
    }

    /// List all projects with `sync_enabled = true` (SYNC-07).
    pub async fn list_sync_enabled(&self) -> Result<Vec<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name, path, created_at, target_branch,
                      auto_merge AS "auto_merge!: bool",
                      sync_enabled AS "sync_enabled!: bool",
                      sync_remote
               FROM projects WHERE sync_enabled = 1 ORDER BY name"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Resolve a project reference (path or name) to its ID.
    ///
    /// Tries, in order:
    /// 1. Exact match on `path` or `name` column.
    /// 2. Longest-prefix match (the project whose path is a parent of the given
    ///    path), so `/home/user/myapp/src` resolves to a project at
    ///    `/home/user/myapp`.
    pub async fn resolve(&self, project_ref: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let normalized = project_ref.trim_end_matches('/');

        // 1. Exact match by path or name.
        let exact = sqlx::query_scalar!(
            "SELECT id FROM projects WHERE path = ? OR name = ? LIMIT 1",
            normalized,
            normalized
        )
        .fetch_optional(self.db.pool())
        .await?;

        if exact.is_some() {
            return Ok(exact);
        }

        // 2. Longest-prefix match (subdirectory of a known project).
        let all_rows = sqlx::query!("SELECT id, path FROM projects")
            .fetch_all(self.db.pool())
            .await?;

        let mut best: Option<(String, usize)> = None;
        for row in all_rows {
            let id = row.id;
            let path = row.path;
            let root = path.trim_end_matches('/');
            let is_match = normalized == root
                || normalized
                    .strip_prefix(root)
                    .map(|suffix| suffix.starts_with('/'))
                    .unwrap_or(false);
            if is_match {
                let len = root.len();
                if best.as_ref().map(|(_, bl)| len > *bl).unwrap_or(true) {
                    best = Some((id, len));
                }
            }
        }

        Ok(best.map(|(id, _)| id))
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("DELETE FROM projects WHERE id = ?", id)
            .execute(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::project_deleted(id));
        Ok(())
    }

    /// Persist the detected stack JSON for a project.
    ///
    /// Callers (today: `mirror_fetcher::fetch_one`) own the serialization
    /// so `djinn-db` doesn't take a dep on `djinn-stack`. The `stack_json`
    /// argument must be a serialized `djinn_stack::Stack`; any non-empty
    /// JSON object is accepted by the column, but the MCP tool returning
    /// it expects the `Stack` shape.
    ///
    /// Returns `true` if the row was updated, `false` if the incoming JSON
    /// was identical to what was already persisted. The mirror fetcher
    /// fires every ~60s and stack detection almost always returns the
    /// same result between ticks, so skipping the UPDATE when unchanged
    /// avoids a no-op write per project per tick — critical on Dolt where
    /// that write can take 30–60s under contention.
    pub async fn set_stack(&self, project_id: &str, stack_json: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let current: Option<String> = sqlx::query_scalar!(
            "SELECT stack FROM projects WHERE id = ?",
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        if matches!(&current, Some(existing) if existing == stack_json) {
            return Ok(false);
        }
        sqlx::query!(
            "UPDATE projects SET stack = ? WHERE id = ?",
            stack_json,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(true)
    }

    /// Fetch the raw `stack` JSON string for a project.
    ///
    /// Returns `Ok(None)` when the project id is unknown. A project that
    /// exists but has not yet been detected returns `Ok(Some("{}"))`
    /// (the migration-7 default); the MCP tool translates that to a
    /// `None` typed payload.
    pub async fn get_stack(&self, project_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_scalar!("SELECT stack FROM projects WHERE id = ?", project_id)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    /// Fetch the persisted devcontainer-image state for a project (Phase 3 PR 5).
    ///
    /// Returns `Ok(None)` when the project id is unknown. A project whose
    /// `image_*` columns have never been written returns a
    /// [`ProjectImage`] with `tag = None`, `hash = None`, `status = "none"`
    /// (migration-8 default). Callers that need a typed [`ProjectImageStatus`]
    /// can use [`ProjectImageStatus::from_str`].
    pub async fn get_project_image(&self, project_id: &str) -> Result<Option<ProjectImage>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            "SELECT image_tag, image_hash, image_status, image_last_error
               FROM projects WHERE id = ?",
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| ProjectImage {
            tag: r.image_tag,
            hash: r.image_hash,
            status: r.image_status,
            last_error: r.image_last_error,
        }))
    }

    /// Write the full devcontainer-image state for a project.
    ///
    /// Used by `djinn_image_controller` immediately after computing a new
    /// hash (flip `status` to `building`) and on Job completion (flip to
    /// `ready` + fill `tag`) or failure (flip to `failed` + fill
    /// `last_error`). Callers stringify [`ProjectImageStatus`] into the
    /// `status` field.
    pub async fn set_project_image(
        &self,
        project_id: &str,
        image: &ProjectImage,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE projects
                SET image_tag = ?,
                    image_hash = ?,
                    image_status = ?,
                    image_last_error = ?
              WHERE id = ?",
            image.tag,
            image.hash,
            image.status,
            image.last_error,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Return the readiness snapshot the dispatch gate consults before it is
    /// willing to run tasks for a project: the current `image_status` plus
    /// whether the canonical-graph warm pipeline has produced a
    /// `graph_warmed_at` stamp. Returns `Ok(None)` when the project id is
    /// unknown.
    ///
    /// Uses the non-macro `sqlx::query` form because the `graph_warmed_at`
    /// column was added in migration 9 — older DBs that haven't run the
    /// migration yet still compile (the query fails at runtime with a
    /// clear schema error, which the caller surfaces).
    pub async fn get_dispatch_readiness(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectDispatchReadiness>> {
        self.db.ensure_initialized().await?;
        use sqlx::Row;
        // Format the timestamp as ISO-8601 UTC server-side so sqlx decodes it
        // as `Option<String>` instead of a platform-dependent TIMESTAMP type.
        let row = sqlx::query(
            "SELECT image_status,
                    DATE_FORMAT(graph_warmed_at, '%Y-%m-%dT%H:%i:%s.%fZ')
                        AS graph_warmed_at_iso
               FROM projects WHERE id = ?",
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| ProjectDispatchReadiness {
            image_status: r.get::<String, _>("image_status"),
            graph_warmed_at: r
                .try_get::<Option<String>, _>("graph_warmed_at_iso")
                .ok()
                .flatten(),
        }))
    }

    /// Flip only the `image_status` column (e.g. transitioning a previously-
    /// `ready` project into `building` while retaining the existing tag/hash
    /// until the new Job lands).
    pub async fn set_image_status(&self, project_id: &str, status: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE projects SET image_status = ? WHERE id = ?",
            status,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

/// Per-project devcontainer-image record as persisted in migration 8.
///
/// All four columns round-trip as raw strings at the DB boundary so
/// `djinn-db` doesn't have to know the status vocabulary. The
/// `djinn-image-controller` crate owns the typed [`ProjectImageStatus`]
/// vocabulary and converts on the way in/out.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectImage {
    pub tag: Option<String>,
    pub hash: Option<String>,
    pub status: String,
    pub last_error: Option<String>,
}

impl ProjectImage {
    /// Empty record matching migration 8's column defaults — used by tests
    /// + the controller's first-ever write path.
    pub fn none() -> Self {
        Self {
            tag: None,
            hash: None,
            status: ProjectImageStatus::NONE.to_string(),
            last_error: None,
        }
    }
}

/// Readiness fields the coordinator needs to decide whether a project can
/// receive task dispatches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectDispatchReadiness {
    /// Current `image_status` string (`"none"` / `"building"` / `"ready"` / `"failed"`).
    pub image_status: String,
    /// Wall-clock timestamp of the most recent successful canonical-graph warm.
    /// `None` means the warm has never completed for this project.
    pub graph_warmed_at: Option<String>,
}

impl ProjectDispatchReadiness {
    /// True when the project's devcontainer image is `ready` AND the
    /// canonical-graph warmer has stamped at least one completion. This is
    /// the coordinator's hard gate for task dispatch.
    pub fn is_ready_for_dispatch(&self) -> bool {
        self.image_status == ProjectImageStatus::READY && self.graph_warmed_at.is_some()
    }
}

/// Canonical vocabulary for the `projects.image_status` column.
///
/// Stored as a raw string in the DB (migration-8 column is VARCHAR(32));
/// this newtype wrapper + `const` namespace lets controllers spell the
/// variants without duplicating literals.
pub struct ProjectImageStatus;

impl ProjectImageStatus {
    /// No build has been enqueued for this project.
    pub const NONE: &'static str = "none";
    /// A build Job has been submitted and is waiting or running.
    pub const BUILDING: &'static str = "building";
    /// A build succeeded and `image_tag` / `image_hash` are populated.
    pub const READY: &'static str = "ready";
    /// The most recent build failed; `image_last_error` carries detail.
    pub const FAILED: &'static str = "failed";
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Project;

    use super::*;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_project() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        let project = repo.create("myapp", "/home/user/myapp").await.unwrap();
        assert_eq!(project.name, "myapp");
        assert_eq!(project.path, "/home/user/myapp");
        assert!(!project.id.is_empty());

        let fetched = repo.get(&project.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "myapp");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = ProjectRepository::new(test_db(), bus);

        repo.create("proj", "/tmp/proj").await.unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "project");
        assert_eq!(events[0].action, "created");
        let p: Project = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(p.name, "proj");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_project() {
        let (bus, captured) = capturing_bus();
        let repo = ProjectRepository::new(test_db(), bus);

        let project = repo.create("old", "/old").await.unwrap();
        captured.lock().unwrap().clear();

        let updated = repo.update(&project.id, "new", "/new").await.unwrap();
        assert_eq!(updated.name, "new");
        assert_eq!(updated.path, "/new");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "project");
        assert_eq!(events[0].action, "updated");
        let p: Project = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(p.name, "new");
    }

    // `project.create` seeds five default agent rows (one per base role); the
    // subsequent `DELETE FROM projects` fans out across ~8 cascade targets
    // (epics, tasks, notes, sessions, agents, consolidation_metrics,
    // verification_cache, ...). Dolt currently drops the connection mid-
    // cascade and the driver surfaces it as `Sqlx(Io UnexpectedEof)` —
    // reproducible on 100% of runs against the current image. The same
    // code path works fine against vanilla MySQL 8.0; filed as a Dolt
    // cascade-limitation issue. Re-enable once Dolt can execute the
    // multi-cascade DELETE without closing the conn.
    #[ignore = "Dolt multi-cascade DELETE drops the connection; tracked as server-side regression"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_project() {
        let (bus, captured) = capturing_bus();
        let repo = ProjectRepository::new(test_db(), bus);

        let project = repo.create("del", "/del").await.unwrap();
        captured.lock().unwrap().clear();

        repo.delete(&project.id).await.unwrap();
        assert!(repo.get(&project.id).await.unwrap().is_none());

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "project");
        assert_eq!(events[0].action, "deleted");
        assert_eq!(events[0].payload["id"].as_str().unwrap(), project.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_projects() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        repo.create("beta", "/beta").await.unwrap();
        repo.create("alpha", "/alpha").await.unwrap();

        let projects = repo.list().await.unwrap();
        assert_eq!(projects.len(), 2);
        // Ordered by name.
        assert_eq!(projects[0].name, "alpha");
        assert_eq!(projects[1].name, "beta");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_path_returns_project() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        let project = repo.create("lookup", "/lookup/path").await.unwrap();
        let found = repo.get_by_path("/lookup/path").await.unwrap().unwrap();
        assert_eq!(found.id, project.id);
        assert_eq!(found.path, "/lookup/path");

        // Missing path returns None.
        assert!(repo.get_by_path("/nonexistent").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_seeds_five_default_roles() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = repo.create("seeded", "/seeded").await.unwrap();

        let rows_raw = sqlx::query!(
            r#"SELECT `name`, base_role, is_default AS "is_default!: i64" FROM agents WHERE project_id = ? ORDER BY base_role"#,
            project.id
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let rows: Vec<(String, String, i64)> = rows_raw
            .into_iter()
            .map(|r| (r.name, r.base_role, r.is_default))
            .collect();

        let expected_base_roles = ["architect", "lead", "planner", "reviewer", "worker"];

        assert_eq!(
            rows.len(),
            5,
            "expected 5 default roles, got {}",
            rows.len()
        );
        for ((name, base_role, is_default), expected) in rows.iter().zip(expected_base_roles.iter())
        {
            assert_eq!(base_role, expected);
            assert_eq!(name, expected, "default role name should equal base_role");
            assert_eq!(*is_default, 1, "role {base_role} should be is_default=1");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seeding_is_idempotent_on_different_projects() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("proj-a", "/proj-a").await.unwrap();
        repo.create("proj-b", "/proj-b").await.unwrap();

        let total: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM agents WHERE is_default = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(total, 10, "2 projects × 5 roles = 10 default rows");
    }

    // ── VerificationRule validation ──────────────────────────────────────────

    #[test]
    fn verification_rule_valid_glob() {
        let rule = VerificationRule {
            match_pattern: "src/**/*.rs".to_string(),
            commands: vec!["cargo test".to_string()],
        };
        assert!(rule.validate().is_ok());
    }

    #[test]
    fn verification_rule_catch_all_is_valid() {
        let rule = VerificationRule {
            match_pattern: "**".to_string(),
            commands: vec!["cargo test".to_string()],
        };
        assert!(rule.validate().is_ok());
    }

    #[test]
    fn verification_rule_empty_commands_rejected() {
        let rule = VerificationRule {
            match_pattern: "**".to_string(),
            commands: vec![],
        };
        let err = rule.validate().unwrap_err();
        assert!(err.contains("commands must not be empty"), "got: {err}");
    }

    #[test]
    fn verification_rule_invalid_glob_rejected() {
        // A lone `[` is invalid glob syntax.
        let rule = VerificationRule {
            match_pattern: "[invalid".to_string(),
            commands: vec!["echo ok".to_string()],
        };
        let err = rule.validate().unwrap_err();
        assert!(err.contains("invalid glob syntax"), "got: {err}");
    }

    #[test]
    fn validate_verification_rules_stops_at_first_error() {
        let rules = vec![
            VerificationRule {
                match_pattern: "**".to_string(),
                commands: vec!["echo ok".to_string()],
            },
            VerificationRule {
                match_pattern: "**".to_string(),
                commands: vec![],
            },
        ];
        assert!(validate_verification_rules(&rules).is_err());
    }

    #[test]
    fn validate_verification_rules_empty_is_ok() {
        assert!(validate_verification_rules(&[]).is_ok());
    }

    // ── DB round-trip ────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_config_default_verification_rules_is_empty_json_array() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("cfg-default", "/cfg-default").await.unwrap();
        let config = repo.get_config(&project.id).await.unwrap().unwrap();
        assert_eq!(config.verification_rules, "[]");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_verification_rules_round_trip() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("cfg-vr", "/cfg-vr").await.unwrap();

        let rules_json = r#"[{"match_pattern":"src/**/*.rs","commands":["cargo test"]}]"#;
        let config = repo
            .update_config_field(&project.id, "verification_rules", rules_json)
            .await
            .unwrap()
            .expect("should return config");

        let stored: Vec<VerificationRule> =
            serde_json::from_str(&config.verification_rules).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].match_pattern, "src/**/*.rs");
        assert_eq!(stored[0].commands, vec!["cargo test"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_verification_rules_catch_all() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("cfg-catch", "/cfg-catch").await.unwrap();

        let rules_json = r#"[{"match_pattern":"**","commands":["cargo clippy"]}]"#;
        let config = repo
            .update_config_field(&project.id, "verification_rules", rules_json)
            .await
            .unwrap()
            .expect("should return config");

        let stored: Vec<VerificationRule> =
            serde_json::from_str(&config.verification_rules).unwrap();
        assert_eq!(stored[0].match_pattern, "**");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_verification_rules_invalid_json_returns_error() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo
            .create("cfg-invalid-json", "/cfg-invalid-json")
            .await
            .unwrap();

        let result = repo
            .update_config_field(&project.id, "verification_rules", "not-json")
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("verification_rules") || err.contains("invalid"),
            "got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_verification_rules_empty_commands_returns_error() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo
            .create("cfg-empty-cmds", "/cfg-empty-cmds")
            .await
            .unwrap();

        let rules_json = r#"[{"match_pattern":"**","commands":[]}]"#;
        let result = repo
            .update_config_field(&project.id, "verification_rules", rules_json)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("commands must not be empty"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_verification_rules_invalid_glob_returns_error() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("cfg-bad-glob", "/cfg-bad-glob").await.unwrap();

        let rules_json = r#"[{"match_pattern":"[invalid","commands":["echo ok"]}]"#;
        let result = repo
            .update_config_field(&project.id, "verification_rules", rules_json)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid glob syntax"), "got: {err}");
    }

    // ── Stack column round-trip ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_stack_default_is_empty_json_object() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("stack-default", "/stack-default").await.unwrap();
        let stack = repo.get_stack(&project.id).await.unwrap().unwrap();
        assert_eq!(stack, "{}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_stack_round_trips_arbitrary_json() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("stack-rt", "/stack-rt").await.unwrap();

        let payload = r#"{"primary_language":"Rust","is_monorepo":false}"#;
        repo.set_stack(&project.id, payload).await.unwrap();

        let fetched = repo.get_stack(&project.id).await.unwrap().unwrap();
        assert_eq!(fetched, payload);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_stack_unknown_project_returns_none() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        assert!(repo.get_stack("nonexistent-id").await.unwrap().is_none());
    }
}

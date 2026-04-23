use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;

use crate::Result;
use crate::database::Database;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProjectConfig {
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
    /// Glob patterns that `code_graph` (cycles / orphans / ranked)
    /// drops from results. Added in migration 12. Stored as a JSON
    /// array of strings; parsed lazily on read, so an invalid JSON
    /// blob degrades to an empty list rather than failing the query.
    #[serde(default)]
    pub graph_excluded_paths: Vec<String>,
    /// Exact file paths the Dead-code panel (`code_graph orphans`)
    /// silently drops. Added in migration 12. Same JSON-array shape
    /// as `graph_excluded_paths`.
    #[serde(default)]
    pub graph_orphan_ignore: Vec<String>,
}

fn parse_json_string_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn encode_json_string_list(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".into())
}

/// Validate + canonicalize a JSON-array-of-strings value before we
/// persist it via `update_config_field`. Rejects non-array shapes and
/// non-string elements so we don't smuggle malformed JSON into the
/// `projects.graph_*` columns. Returns the re-serialized JSON string
/// (trimmed of surrounding whitespace, deduplicated while preserving
/// order) so consumers always see a canonical blob.
fn normalize_json_string_list(raw: &str) -> core::result::Result<String, String> {
    let parsed: Vec<String> = serde_json::from_str(raw)
        .map_err(|e| format!("expected JSON array of strings: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(parsed.len());
    for item in parsed {
        let trimmed = item.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    Ok(encode_json_string_list(&out))
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

    /// Insert a project row with a caller-chosen id.
    ///
    /// Tests that need a stable, well-known id (e.g. to satisfy a
    /// foreign-key reference from `verification_cache` seeded under the
    /// same id) use this instead of [`Self::create`]'s generated UUID.
    /// Production code should always use [`Self::create`]; mismatched ids
    /// break downstream consumers that expect UUIDv7.
    pub async fn create_with_id(&self, id: &str, name: &str, path: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let name_owned = name.to_owned();
        let path_owned = path.to_owned();

        let project: Project = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let name = name_owned.clone();
                let path = path_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    sqlx::query!(
                        "INSERT INTO projects (id, name, path) VALUES (?, ?, ?)",
                        id,
                        name,
                        path,
                    )
                    .execute(&mut *tx)
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
                    .fetch_one(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    Ok::<_, crate::Error>(project)
                }
            },
        )
        .await?;

        self.seed_default_roles(&project.id).await?;

        self.events
            .send(DjinnEventEnvelope::project_created(&project));
        Ok(project)
    }

    pub async fn create(&self, name: &str, path: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let name_owned = name.to_owned();
        let path_owned = path.to_owned();

        // create_test_project fires this function from ~every test; under
        // concurrent nextest load Dolt's commit-time conflict detection
        // rejects a fraction of the INSERTs with 1213. Wrap the
        // INSERT+SELECT in a single transaction so a failure rolls back
        // both and the retry's fresh transaction sees a clean slate
        // (otherwise a committed INSERT + failed SELECT on a retry would
        // hit a UNIQUE-constraint violation on `projects.id`).
        let project: Project = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id.clone();
                let name = name_owned.clone();
                let path = path_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    sqlx::query!(
                        "INSERT INTO projects (id, name, path) VALUES (?, ?, ?)",
                        id,
                        name,
                        path,
                    )
                    .execute(&mut *tx)
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
                    .fetch_one(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    Ok::<_, crate::Error>(project)
                }
            },
        )
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
                (id, name, path, github_owner, github_repo, default_branch, clone_path, target_branch, installation_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            name,
            clone_path,
            owner,
            repo,
            default_branch,
            clone_path,
            default_branch,
            installation_id_i64,
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
        // Non-macro form because `graph_excluded_paths` and
        // `graph_orphan_ignore` (migration 12) aren't in the sqlx
        // offline cache yet; matches the pattern used for
        // `environment_config` / `graph_warmed_at` which also post-date
        // the cache baseline.
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT target_branch, auto_merge, sync_enabled, sync_remote,
                    graph_excluded_paths, graph_orphan_ignore
               FROM projects WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let graph_excluded_paths: String =
            row.try_get("graph_excluded_paths").unwrap_or_default();
        let graph_orphan_ignore: String =
            row.try_get("graph_orphan_ignore").unwrap_or_default();
        Ok(Some(ProjectConfig {
            target_branch: row.try_get("target_branch")?,
            auto_merge: row.try_get("auto_merge")?,
            sync_enabled: row.try_get("sync_enabled")?,
            sync_remote: row.try_get("sync_remote").ok(),
            graph_excluded_paths: parse_json_string_list(&graph_excluded_paths),
            graph_orphan_ignore: parse_json_string_list(&graph_orphan_ignore),
        }))
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
            "graph_excluded_paths" => {
                let canonical = normalize_json_string_list(value)
                    .map_err(|e| crate::Error::InvalidData(format!(
                        "graph_excluded_paths: {e}"
                    )))?;
                // Non-macro UPDATE because the column post-dates the
                // sqlx offline cache baseline (migration 12).
                sqlx::query(
                    "UPDATE projects SET graph_excluded_paths = ? WHERE id = ?",
                )
                .bind(&canonical)
                .bind(id)
                .execute(self.db.pool())
                .await?;
            }
            "graph_orphan_ignore" => {
                let canonical = normalize_json_string_list(value)
                    .map_err(|e| crate::Error::InvalidData(format!(
                        "graph_orphan_ignore: {e}"
                    )))?;
                sqlx::query(
                    "UPDATE projects SET graph_orphan_ignore = ? WHERE id = ?",
                )
                .bind(&canonical)
                .bind(id)
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

    /// Fetch the raw `environment_config` JSON string for a project.
    ///
    /// Returns `Ok(None)` when the project id is unknown. A project that
    /// has not been reseeded by the P5 boot hook yet returns
    /// `Ok(Some("{}"))` (the migration-10 default); the reseed hook
    /// treats that as its trigger.
    ///
    /// Uses the non-macro `sqlx::query_scalar` form because the
    /// `environment_config` column was added in migration 10 — the
    /// offline cache + compile-time macro check haven't seen it yet in
    /// the prod Dolt used by `cargo check`. Runs fine at runtime; the
    /// query fails with a clear schema error if migration 10 hasn't
    /// been applied, which is what the caller wants.
    pub async fn get_environment_config(&self, project_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT environment_config FROM projects WHERE id = ?",
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Persist the `environment_config` JSON for a project and null
    /// `image_hash` so the image-controller re-runs the build on the
    /// next tick. `environment_config_json` must be a serialized
    /// `djinn_stack::environment::EnvironmentConfig`; callers (MCP,
    /// boot reseed hook) validate before calling.
    ///
    /// Nulling `image_hash` is intentional: changing the environment
    /// config necessarily changes the image content, so the cached
    /// image is no longer authoritative. The next reconcile tick sees
    /// the `None` and builds again.
    ///
    /// Non-macro `sqlx::query` — see `get_environment_config` for the
    /// reason.
    pub async fn set_environment_config(
        &self,
        project_id: &str,
        environment_config_json: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE projects
                SET environment_config = ?,
                    image_hash = NULL
              WHERE id = ?",
        )
        .bind(environment_config_json)
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// List every project id + its current `environment_config` +
    /// `stack`. Used by the boot reseed hook which walks the table once
    /// at startup, spots rows with an empty config, and rewrites them
    /// from the stack.
    ///
    /// Non-macro `sqlx::query_as` — see `get_environment_config` for
    /// the reason.
    pub async fn list_for_reseed(&self) -> Result<Vec<ProjectReseedRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectReseedRow>(
            "SELECT id, stack, environment_config FROM projects",
        )
        .fetch_all(self.db.pool())
        .await?)
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
        // `graph_warmed_at` is a VARCHAR(64) RFC3339 string matching the
        // schema-wide timestamp convention (see migration 2). An empty
        // string means "never warmed".
        let row = sqlx::query(
            "SELECT image_status, graph_warmed_at FROM projects WHERE id = ?",
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| {
            let stamp = r
                .try_get::<String, _>("graph_warmed_at")
                .unwrap_or_default();
            ProjectDispatchReadiness {
                image_status: r.get::<String, _>("image_status"),
                graph_warmed_at: if stamp.is_empty() { None } else { Some(stamp) },
            }
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

/// One row the boot reseed hook walks. All columns come back as raw
/// strings at the DB boundary — `djinn_stack` handles JSON shape,
/// `djinn-db` doesn't. `environment_config` is non-null with a `'{}'`
/// default; `stack` is non-null with a `'{}'` default.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ProjectReseedRow {
    pub id: String,
    pub stack: String,
    pub environment_config: String,
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

    // ── Graph exclusion columns (migration 12) ───────────────────────────────

    #[test]
    fn normalize_json_string_list_rejects_non_array() {
        assert!(normalize_json_string_list("\"foo\"").is_err());
        assert!(normalize_json_string_list("{\"a\":1}").is_err());
        assert!(normalize_json_string_list("not-json").is_err());
    }

    #[test]
    fn normalize_json_string_list_dedupes_and_trims() {
        let out = normalize_json_string_list(
            r#"["  **/workspace-hack/**  ", "**/workspace-hack/**", "", "**/test-support/**"]"#,
        )
        .unwrap();
        assert_eq!(
            out,
            r#"["**/workspace-hack/**","**/test-support/**"]"#,
            "trimmed, empty-dropped, deduplicated while preserving insertion order"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_config_defaults_to_empty_graph_exclusion_lists() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo
            .create("graph-defaults", "/graph-defaults")
            .await
            .unwrap();
        let cfg = repo.get_config(&project.id).await.unwrap().unwrap();
        assert!(cfg.graph_excluded_paths.is_empty());
        assert!(cfg.graph_orphan_ignore.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_field_round_trips_graph_excluded_paths() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("graph-rt", "/graph-rt").await.unwrap();

        let updated = repo
            .update_config_field(
                &project.id,
                "graph_excluded_paths",
                r#"["**/workspace-hack/**", "**/test-support/**"]"#,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.graph_excluded_paths,
            vec!["**/workspace-hack/**".to_string(), "**/test-support/**".to_string()]
        );
        assert!(updated.graph_orphan_ignore.is_empty());

        let refetched = repo.get_config(&project.id).await.unwrap().unwrap();
        assert_eq!(refetched.graph_excluded_paths, updated.graph_excluded_paths);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_field_round_trips_graph_orphan_ignore() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("graph-orphan", "/graph-orphan").await.unwrap();

        let updated = repo
            .update_config_field(
                &project.id,
                "graph_orphan_ignore",
                r#"["crates/test-support/src/db.rs"]"#,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.graph_orphan_ignore,
            vec!["crates/test-support/src/db.rs".to_string()]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_field_rejects_malformed_graph_excluded_paths() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("graph-bad", "/graph-bad").await.unwrap();

        let err = repo
            .update_config_field(&project.id, "graph_excluded_paths", "not-json")
            .await
            .unwrap_err();
        // `InvalidData(...)` Display prefix.
        assert!(
            err.to_string().contains("graph_excluded_paths"),
            "error should name the field, got: {err}"
        );
    }
}

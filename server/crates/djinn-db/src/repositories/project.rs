// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;
use sqlx::Row;

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
    let parsed: Vec<String> =
        serde_json::from_str(raw).map_err(|e| format!("expected JSON array of strings: {e}"))?;
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
            r#"SELECT id, name,
                      github_owner AS "github_owner!: String",
                      github_repo AS "github_repo!: String",
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

    /// Return one bounded keyset page of project IDs.
    ///
    /// Background jobs that only need to visit projects must use this rather
    /// than [`Self::list`], which materializes every full project row. IDs are
    /// ordered by their stable primary key and the cursor is exclusive, so a
    /// caller can drop each page before fetching the next one.
    pub async fn list_ids_page_after(
        &self,
        after_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar(
            "SELECT id FROM projects \
             WHERE ($1::TEXT IS NULL OR id > $1) \
             ORDER BY id LIMIT $2",
        )
        .bind(after_id)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name,
                      github_owner AS "github_owner!: String",
                      github_repo AS "github_repo!: String",
                      CAST(created_at AS CHAR) as "created_at!: String",
                      target_branch,
                      auto_merge as "auto_merge: bool",
                      sync_enabled as "sync_enabled: bool",
                      sync_remote
               FROM projects WHERE id = $1"#,
            id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a project reference to its ID. Accepts a UUID (passed
    /// through when the row exists) or an `"owner/repo"` slug.
    pub async fn resolve(&self, project_ref: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let trimmed = project_ref.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        // UUID path: if the ref parses as a UUID and maps to a row, use it.
        if uuid::Uuid::parse_str(trimmed).is_ok()
            && let Some(id) = sqlx::query_scalar!("SELECT id FROM projects WHERE id = $1", trimmed)
                .fetch_optional(self.db.pool())
                .await?
        {
            return Ok(Some(id));
        }

        // Slug path: `owner/repo`.
        if let Some((owner, repo)) = trimmed.split_once('/') {
            let id = sqlx::query_scalar!(
                "SELECT id FROM projects WHERE github_owner = $1 AND github_repo = $2",
                owner,
                repo,
            )
            .fetch_optional(self.db.pool())
            .await?;
            return Ok(id);
        }

        Ok(None)
    }

    /// Insert a project row with a caller-chosen id.
    ///
    /// Tests that need a stable, well-known id use this instead of
    /// [`Self::create`]'s generated UUID.
    /// Production code should always use [`Self::create_from_github`];
    /// mismatched ids break downstream consumers that expect UUIDv7.
    pub async fn create_with_id(
        &self,
        id: &str,
        name: &str,
        owner: &str,
        repo: &str,
    ) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id_owned = id.to_owned();
        let name_owned = name.to_owned();
        let owner_owned = owner.to_owned();
        let repo_owned = repo.to_owned();

        let project: Project = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id_owned.clone();
                let name = name_owned.clone();
                let owner = owner_owned.clone();
                let repo = repo_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    sqlx::query!(
                        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
                        id,
                        name,
                        owner,
                        repo,
                    )
                    .execute(&mut *tx)
                    .await?;
                    let project = sqlx::query_as!(
                        Project,
                        r#"SELECT id, name,
                    github_owner AS "github_owner!: String",
                    github_repo AS "github_repo!: String",
                    created_at, target_branch,
                                auto_merge AS "auto_merge!: bool",
                                sync_enabled AS "sync_enabled!: bool",
                                sync_remote
                         FROM projects WHERE id = $1"#,
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

    /// Create a project row with a generated UUID. Prefer
    /// [`Self::create_from_github`] in production; this exists for
    /// test fixtures that don't go through the clone path.
    pub async fn create(&self, name: &str, owner: &str, repo: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let name_owned = name.to_owned();
        let owner_owned = owner.to_owned();
        let repo_owned = repo.to_owned();

        // create_test_project fires this function from ~every test; under
        // concurrent nextest load Dolt's commit-time conflict detection
        // rejects a fraction of the INSERTs with 1213. Wrap the
        // INSERT+SELECT in a single transaction so a failure rolls back
        // both and the retry's fresh transaction sees a clean slate.
        let project: Project = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id = id.clone();
                let name = name_owned.clone();
                let owner = owner_owned.clone();
                let repo = repo_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    sqlx::query!(
                        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
                        id,
                        name,
                        owner,
                        repo,
                    )
                    .execute(&mut *tx)
                    .await?;
                    let project = sqlx::query_as!(
                        Project,
                        r#"SELECT id, name,
                    github_owner AS "github_owner!: String",
                    github_repo AS "github_repo!: String",
                    created_at, target_branch,
                                auto_merge AS "auto_merge!: bool",
                                sync_enabled AS "sync_enabled!: bool",
                                sync_remote
                         FROM projects WHERE id = $1"#,
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
    /// Returns `Ok(None)` only when the project id is unknown.
    pub async fn get_github_coords(&self, project_id: &str) -> Result<Option<(String, String)>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            r#"SELECT
                 github_owner AS "github_owner!: String",
                 github_repo  AS "github_repo!: String"
               FROM projects WHERE id = $1"#,
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| (r.github_owner, r.github_repo)))
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
            "SELECT installation_id FROM projects WHERE id = $1",
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
            "SELECT default_branch FROM projects WHERE id = $1",
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.flatten().filter(|s| !s.is_empty()))
    }

    /// Find a project by its GitHub owner/repo coordinates.
    pub async fn get_by_github(&self, owner: &str, repo: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Project,
            r#"SELECT id, name,
                    github_owner AS "github_owner!: String",
                    github_repo AS "github_repo!: String",
                    created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects
             WHERE github_owner = $1 AND github_repo = $2"#,
            owner,
            repo
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Create a project backed by a GitHub repo the server has cloned locally.
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
        installation_id: Option<u64>,
    ) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        // SQLite stores u64 as i64; cast lossily (installation IDs fit in i64
        // comfortably — GitHub uses ~8-digit values today).
        let installation_id_i64: Option<i64> = installation_id.map(|v| v as i64);
        sqlx::query!(
            "INSERT INTO projects
                (id, name, github_owner, github_repo, default_branch, target_branch, installation_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            id,
            name,
            owner,
            repo,
            default_branch,
            default_branch,
            installation_id_i64,
        )
        .execute(self.db.pool())
        .await?;

        let project = sqlx::query_as!(
            Project,
            r#"SELECT id, name,
                    github_owner AS "github_owner!: String",
                    github_repo AS "github_repo!: String",
                    created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects WHERE id = $1"#,
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
                "INSERT INTO agents
                    (id, project_id, name, base_role, description, is_default)
                 VALUES ($1, $2, $3, $4, $5, TRUE)
                 ON CONFLICT (project_id, name) DO NOTHING",
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

    pub async fn update(&self, id: &str, name: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        sqlx::query!("UPDATE projects SET name = $1 WHERE id = $2", name, id)
            .execute(self.db.pool())
            .await?;
        let project = sqlx::query_as!(
            Project,
            r#"SELECT id, name,
                    github_owner AS "github_owner!: String",
                    github_repo AS "github_repo!: String",
                    created_at, target_branch,
                    auto_merge AS "auto_merge!: bool",
                    sync_enabled AS "sync_enabled!: bool",
                    sync_remote
             FROM projects WHERE id = $1"#,
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
        let row = sqlx::query!(
            r#"SELECT target_branch, auto_merge, sync_enabled, sync_remote,
                    graph_excluded_paths::text AS graph_excluded_paths,
                    graph_orphan_ignore::text AS graph_orphan_ignore
               FROM projects WHERE id = $1"#,
            id,
        )
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let graph_excluded_paths: String = row.graph_excluded_paths.unwrap_or_default();
        let graph_orphan_ignore: String = row.graph_orphan_ignore.unwrap_or_default();
        Ok(Some(ProjectConfig {
            target_branch: row.target_branch,
            auto_merge: row.auto_merge,
            sync_enabled: row.sync_enabled,
            sync_remote: row.sync_remote,
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
                    "UPDATE projects SET target_branch = $1 WHERE id = $2",
                    value,
                    id
                )
                .execute(self.db.pool())
                .await?;
            }
            "auto_merge" => {
                let v = matches!(value, "true" | "1");
                sqlx::query!("UPDATE projects SET auto_merge = $1 WHERE id = $2", v, id)
                    .execute(self.db.pool())
                    .await?;
            }
            "sync_enabled" => {
                let v = matches!(value, "true" | "1");
                sqlx::query!("UPDATE projects SET sync_enabled = $1 WHERE id = $2", v, id)
                    .execute(self.db.pool())
                    .await?;
            }
            "sync_remote" => {
                let val = if value.is_empty() { None } else { Some(value) };
                sqlx::query!(
                    "UPDATE projects SET sync_remote = $1 WHERE id = $2",
                    val,
                    id
                )
                .execute(self.db.pool())
                .await?;
            }
            "graph_excluded_paths" => {
                let canonical = normalize_json_string_list(value)
                    .map_err(|e| crate::Error::InvalidData(format!("graph_excluded_paths: {e}")))?;
                // The `$1::jsonb` cast makes the macro type this bind as
                // `serde_json::Value`; `canonical` is already-validated JSON.
                let canonical: serde_json::Value = serde_json::from_str(&canonical)
                    .unwrap_or_else(|_| serde_json::Value::Array(vec![]));
                sqlx::query!(
                    "UPDATE projects SET graph_excluded_paths = $1::jsonb WHERE id = $2",
                    canonical,
                    id,
                )
                .execute(self.db.pool())
                .await?;
            }
            "graph_orphan_ignore" => {
                let canonical = normalize_json_string_list(value)
                    .map_err(|e| crate::Error::InvalidData(format!("graph_orphan_ignore: {e}")))?;
                let canonical: serde_json::Value = serde_json::from_str(&canonical)
                    .unwrap_or_else(|_| serde_json::Value::Array(vec![]));
                sqlx::query!(
                    "UPDATE projects SET graph_orphan_ignore = $1::jsonb WHERE id = $2",
                    canonical,
                    id,
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
            r#"SELECT id, name,
                    github_owner AS "github_owner!: String",
                    github_repo AS "github_repo!: String",
                    created_at, target_branch,
                      auto_merge AS "auto_merge!: bool",
                      sync_enabled AS "sync_enabled!: bool",
                      sync_remote
               FROM projects WHERE sync_enabled = TRUE ORDER BY name"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut transaction = self.db.pool().begin().await?;
        // Preserve a minimal durable source before the cascading delete removes
        // every project-owned row. The warm-base GC uses it to distinguish a
        // deleted project from a UUID directory that was never registered.
        // Selecting from `projects` prevents a no-op delete from creating a
        // tombstone for an arbitrary id.
        sqlx::query(
            "INSERT INTO deleted_projects (project_id) \
             SELECT id FROM projects WHERE id = $1 \
             ON CONFLICT (project_id) DO NOTHING",
        )
        .bind(id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query!("DELETE FROM projects WHERE id = $1", id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;

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
            r#"SELECT stack::text AS "stack!" FROM projects WHERE id = $1"#,
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        if matches!(&current, Some(existing) if existing == stack_json) {
            return Ok(false);
        }
        let stack: serde_json::Value = serde_json::from_str(stack_json).map_err(|e| {
            crate::Error::InvalidData(format!("invalid json for projects.stack: {e}"))
        })?;
        sqlx::query!(
            "UPDATE projects SET stack = $1 WHERE id = $2",
            stack,
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
        Ok(sqlx::query_scalar!(
            r#"SELECT stack::text AS "stack!" FROM projects WHERE id = $1"#,
            project_id
        )
        .fetch_optional(self.db.pool())
        .await?)
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
        Ok(sqlx::query_scalar!(
            r#"SELECT environment_config::text AS "environment_config!" FROM projects WHERE id = $1"#,
            project_id,
        )
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
    pub async fn set_environment_config(
        &self,
        project_id: &str,
        environment_config_json: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        // The `$1::jsonb` cast makes the macro type this bind as
        // `serde_json::Value`; callers pass pre-validated JSON.
        let environment_config: serde_json::Value = serde_json::from_str(environment_config_json)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        sqlx::query!(
            "UPDATE projects
                SET environment_config = $1::jsonb,
                    image_hash = NULL
              WHERE id = $2",
            environment_config,
            project_id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// List every project id + its current `environment_config` +
    /// `stack`. Used by the boot reseed hook which walks the table once
    /// at startup, spots rows with an empty config, and rewrites them
    /// from the stack.
    ///
    pub async fn list_for_reseed(&self) -> Result<Vec<ProjectReseedRow>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProjectReseedRow,
            r#"SELECT id, stack::text AS "stack!", environment_config::text AS "environment_config!" FROM projects"#,
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
               FROM projects WHERE id = $1",
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
    pub async fn set_project_image(&self, project_id: &str, image: &ProjectImage) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE projects
                SET image_tag = $1,
                    image_hash = $2,
                    image_status = $3,
                    image_last_error = $4
              WHERE id = $5",
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
    /// whether the canonical-graph warm pipeline has produced freshness in
    /// project-scoped `project_workspace_graph`. `repo_graph_cache` remains a
    /// non-warm graph-blob cache and never contributes to dispatch readiness.
    /// Returns `Ok(None)`
    /// when the project id is unknown.
    ///
    pub async fn get_dispatch_readiness(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectDispatchReadiness>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"WITH project_exists AS (
                   SELECT 1 FROM projects WHERE id = $1
               ), freshness AS (
                   SELECT warmed_at
                     FROM project_workspace_graph
                    WHERE project_id = $1 AND warmed_at <> ''
               )
               SELECT
                   (SELECT warmed_at
                      FROM freshness
                     ORDER BY warmed_at DESC
                     LIMIT 1) AS graph_warmed_at,
                   EXISTS(SELECT 1 FROM project_workspace_graph WHERE project_id = $1) AS has_graph_freshness
                 FROM project_exists"#,
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        let Some(r) = row else {
            return Ok(None);
        };
        // Image readiness is sourced from the resolved *dispatch* image — the
        // catalog image the project selected (migration 46's single source of
        // truth) — NOT the legacy `projects.image_status` column. That column
        // is only flipped to `ready` by the build watcher for *per-project*
        // builds (keyed by project_id); a project that selects a shared
        // catalog image stays `image_status = 'none'` forever even after the
        // catalog image is built, which would defer its dispatch indefinitely.
        // `resolve_dispatch_image` returns `Ok(None)` only for an unknown
        // project (already handled above), so a `None` here is treated as the
        // not-dispatchable `none` status.
        let image_status = match self.resolve_dispatch_image(project_id).await? {
            Some(img) => img.status,
            None => ProjectImageStatus::NONE.to_string(),
        };
        let stamp: Option<String> = r.get("graph_warmed_at");
        let has_graph_freshness: bool = r.get("has_graph_freshness");
        Ok(Some(ProjectDispatchReadiness {
            image_status,
            graph_warmed_at: match stamp {
                Some(stamp) if !stamp.is_empty() => Some(stamp),
                Some(_) | None if has_graph_freshness => Some(String::new()),
                Some(_) | None => None,
            },
        }))
    }

    /// Resolve the image a task-run / warm Pod must use for a project.
    ///
    /// Catalog images are the **only** source of a runtime image (migration
    /// 46) — projects no longer build a bespoke per-project image:
    ///
    /// * If the project selected a catalog image (`selected_image_id` set),
    ///   the resolver returns that shared image's status/tag/digest.
    /// * Otherwise the project has no image and is **not dispatchable** — it
    ///   needs a catalog image assigned (`tag = None`, `from_catalog = None`).
    ///   There is no fallback to the legacy per-project `image_*` columns.
    ///
    /// Returns `Ok(None)` only when the project id is unknown. A resolved
    /// image that isn't `ready` comes back with `tag = None` so the caller
    /// hard-fails (task dispatch) or skips (warm) — never a silent downgrade.
    ///
    /// Non-macro `sqlx::query` form (like [`ImageRepository`]) so the
    /// migration-46 columns don't require regenerating the offline cache.
    pub async fn resolve_dispatch_image(&self, project_id: &str) -> Result<Option<DispatchImage>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query("SELECT selected_image_id FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let selected_image_id: Option<String> = row.get("selected_image_id");

        let Some(image_id) = selected_image_id else {
            // No catalog image assigned → the project needs setup. Not
            // dispatchable; no bespoke-image fallback any more.
            return Ok(Some(DispatchImage {
                tag: None,
                status: ProjectImageStatus::NONE.to_string(),
                digest: None,
                from_catalog: None,
                last_error: None,
            }));
        };

        let img = sqlx::query(
            "SELECT tag, registry_digest, status, last_error FROM images WHERE id = $1",
        )
        .bind(&image_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(Some(match img {
            Some(i) => DispatchImage {
                tag: i.get("tag"),
                status: i.get("status"),
                digest: i.get("registry_digest"),
                from_catalog: Some(image_id),
                last_error: i.get("last_error"),
            },
            // FK RESTRICT makes a dangling selection impossible in practice;
            // treat it as "not ready" rather than panicking.
            None => DispatchImage {
                tag: None,
                status: ProjectImageStatus::NONE.to_string(),
                digest: None,
                from_catalog: Some(image_id),
                last_error: None,
            },
        }))
    }

    /// Flip only the `image_status` column (e.g. transitioning a previously-
    /// `ready` project into `building` while retaining the existing tag/hash
    /// until the new Job lands).
    pub async fn set_image_status(&self, project_id: &str, status: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE projects SET image_status = $1 WHERE id = $2",
            status,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Record successful graph freshness for a project with no indexable code.
    ///
    /// The normal warm path records `project_workspace_graph` rows for each
    /// discovered workspace and writes the merged `repo_graph_cache` blob. A
    /// docs / memory-only repo has no detected language and nothing to index,
    /// so the warm pipeline is correctly skipped — but without durable
    /// freshness the UI badge would derive to "Warming" forever. Recording a
    /// synthetic workspace row here means "nothing to warm = considered
    /// warmed", resolving the badge to "ready" while keeping all graph
    /// freshness off the `projects` table.
    pub async fn mark_graph_warmed(&self, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        crate::repositories::project_workspace_graph::ProjectWorkspaceGraphRepository::new(
            self.db.clone(),
        )
        .upsert(
            crate::repositories::project_workspace_graph::ProjectWorkspaceGraphUpsert {
                project_id,
                workspace_slug:
                    crate::repositories::project_workspace_graph::CODELESS_WORKSPACE_SLUG,
                commit_sha: "no-code",
                status: "ready",
            },
        )
        .await
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

/// The image a task-run / warm Pod should use for a project after catalog
/// precedence is applied — see [`ProjectRepository::resolve_dispatch_image`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchImage {
    /// The `ready` tag to pull, or `None` when the resolved image isn't
    /// built yet (caller hard-fails task dispatch / skips warming).
    pub tag: Option<String>,
    /// Resolved image status: `none` | `building` | `ready` | `failed`.
    pub status: String,
    /// Immutable registry digest of the resolved image, when captured.
    pub digest: Option<String>,
    /// `Some(image_id)` when the project resolved to a shared catalog image;
    /// `None` when it uses its own per-project build.
    pub from_catalog: Option<String>,
    /// Last build error of the resolved image, surfaced in UI banners.
    pub last_error: Option<String>,
}

impl DispatchImage {
    /// The pull ref a Pod should use: the immutable `tag@digest` when a
    /// digest was captured, else the content-addressed tag. Returns `None`
    /// until the image is `ready`.
    pub fn pull_ref(&self) -> Option<String> {
        let tag = self.tag.as_deref().filter(|t| !t.is_empty())?;
        if self.status != ProjectImageStatus::READY {
            return None;
        }
        match self.digest.as_deref().filter(|d| !d.is_empty()) {
            // `repo:tag@sha256:...` is a valid OCI ref accepted by every
            // registry/runtime and pins the exact manifest.
            Some(digest) => Some(format!("{}@{}", strip_tag_suffix(tag), digest)),
            None => Some(tag.to_string()),
        }
    }
}

/// Strip a `:tag` suffix from an image ref so a digest can be appended,
/// leaving any `host:port/` registry prefix intact (only a `:` after the
/// last `/` is a tag separator).
fn strip_tag_suffix(image_ref: &str) -> &str {
    let last_slash = image_ref.rfind('/').map(|i| i + 1).unwrap_or(0);
    match image_ref[last_slash..].find(':') {
        Some(rel) => &image_ref[..last_slash + rel],
        None => image_ref,
    }
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
    /// Derived wall-clock timestamp of the most recent successful canonical-
    /// graph warm. For rollout compatibility this field name is retained, but
    /// the value now comes exclusively from `project_workspace_graph.warmed_at`;
    /// it is not read from `repo_graph_cache` or the legacy project-table graph
    /// freshness scalar. `None` means no workspace graph row exists for this
    /// project.
    pub graph_warmed_at: Option<String>,
}

impl ProjectDispatchReadiness {
    /// True when the project's devcontainer image is `ready`.
    ///
    /// The devcontainer image is a genuine hard prerequisite — without it the
    /// worker Pod has no environment to run in. The canonical-graph warm is
    /// NOT: it's an optimization (RAG / architect context), and workers
    /// tolerate a missing or stale graph. Gating dispatch on `graph_warmed_at`
    /// meant a warmer that can't complete (e.g. OOM on a large monorepo, a
    /// flaky SCIP indexer) permanently wedged ALL work on the project. We no
    /// longer block on it — the warm still runs in the background and the
    /// graph fills in for later tasks once it succeeds.
    pub fn is_ready_for_dispatch(&self) -> bool {
        self.image_status == ProjectImageStatus::READY
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

    use crate::repositories::project_workspace_graph::{
        ProjectWorkspaceGraphRepository, ProjectWorkspaceGraphUpsert,
    };
    use crate::repositories::repo_graph_cache::{RepoGraphCacheInsert, RepoGraphCacheRepository};

    use super::*;

    #[test]
    fn dispatch_readiness_gates_on_image_only_not_graph_warm() {
        // Image ready, graph never warmed → still dispatchable (warm is an
        // optimization, not a prerequisite; a stuck warmer must not wedge work).
        let ready_no_graph = ProjectDispatchReadiness {
            image_status: ProjectImageStatus::READY.to_string(),
            graph_warmed_at: None,
        };
        assert!(ready_no_graph.is_ready_for_dispatch());

        // Image ready + graph warmed → dispatchable.
        let ready_warmed = ProjectDispatchReadiness {
            image_status: ProjectImageStatus::READY.to_string(),
            graph_warmed_at: Some("2026-05-29T00:00:00.000Z".to_string()),
        };
        assert!(ready_warmed.is_ready_for_dispatch());

        // Image NOT ready → never dispatchable, regardless of graph.
        for status in [
            ProjectImageStatus::NONE,
            ProjectImageStatus::BUILDING,
            ProjectImageStatus::FAILED,
        ] {
            let img_not_ready = ProjectDispatchReadiness {
                image_status: status.to_string(),
                graph_warmed_at: Some("2026-05-29T00:00:00.000Z".to_string()),
            };
            assert!(
                !img_not_ready.is_ready_for_dispatch(),
                "image_status {status} must block dispatch"
            );
        }
    }

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

    async fn dispatch_graph_warmed_at(
        repo: &ProjectRepository,
        project_id: &str,
    ) -> Option<String> {
        repo.get_dispatch_readiness(project_id)
            .await
            .expect("readiness")
            .expect("project exists")
            .graph_warmed_at
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_graph_warmed_records_synthetic_workspace_freshness() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("docs", "acme", "docs").await.unwrap();

        repo.mark_graph_warmed(&project.id).await.unwrap();

        assert!(dispatch_graph_warmed_at(&repo, &project.id).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_readiness_leaves_cache_only_project_unwarmed() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let cache_repo = RepoGraphCacheRepository::new(db);
        let project = repo
            .create("cache-only", "acme", "cache-only")
            .await
            .unwrap();

        assert_eq!(dispatch_graph_warmed_at(&repo, &project.id).await, None);

        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: "abc123",
                graph_blob: b"graph",
            })
            .await
            .expect("cache upsert");
        let cached = cache_repo
            .latest_for_project(&project.id)
            .await
            .expect("cache latest")
            .expect("cache row");
        assert_eq!(cached.graph_blob, b"graph");

        assert_eq!(
            dispatch_graph_warmed_at(&repo, &project.id).await,
            None,
            "a graph cache row is not graph-warm authority"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_readiness_workspace_warm_is_project_scoped() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let cache_repo = RepoGraphCacheRepository::new(db.clone());
        let workspace_repo = ProjectWorkspaceGraphRepository::new(db);
        let warmed_project = repo
            .create("workspace-only", "acme", "workspace-only")
            .await
            .unwrap();
        let cache_only_sibling = repo
            .create("cache-only-sibling", "acme", "cache-only-sibling")
            .await
            .unwrap();

        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &cache_only_sibling.id,
                commit_sha: "cache-sha",
                graph_blob: b"cache",
            })
            .await
            .expect("cache upsert");
        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &warmed_project.id,
                workspace_slug: "root",
                commit_sha: "abc123",
                status: "ready",
            })
            .await
            .expect("workspace upsert");
        let workspace_stamp = workspace_repo
            .latest_warmed_state(&warmed_project.id)
            .await
            .expect("workspace latest")
            .expect("workspace row")
            .warmed_at;

        assert_eq!(
            dispatch_graph_warmed_at(&repo, &warmed_project.id).await,
            Some(workspace_stamp)
        );
        assert_eq!(
            dispatch_graph_warmed_at(&repo, &cache_only_sibling.id).await,
            None,
            "a workspace warm record must not warm another project"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_readiness_ignores_cache_when_workspace_graph_exists() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let cache_repo = RepoGraphCacheRepository::new(db.clone());
        let workspace_repo = ProjectWorkspaceGraphRepository::new(db);
        let project = repo
            .create("both-graph-sources", "acme", "both-graph-sources")
            .await
            .unwrap();

        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: "cache-sha",
                graph_blob: b"graph",
            })
            .await
            .expect("cache upsert");
        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &project.id,
                workspace_slug: "root",
                commit_sha: "workspace-sha",
                status: "ready",
            })
            .await
            .expect("workspace upsert");

        let workspace_stamp = workspace_repo
            .latest_warmed_state(&project.id)
            .await
            .expect("workspace latest")
            .expect("workspace row")
            .warmed_at;

        assert_eq!(
            dispatch_graph_warmed_at(&repo, &project.id).await,
            Some(workspace_stamp),
            "workspace freshness is the sole derived readiness stamp"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_project() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        let project = repo.create("myapp", "acme", "myapp").await.unwrap();
        assert_eq!(project.name, "myapp");
        assert_eq!(project.github_owner, "acme");
        assert_eq!(project.github_repo, "myapp");
        assert!(!project.id.is_empty());

        let fetched = repo.get(&project.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "myapp");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = ProjectRepository::new(test_db(), bus);

        repo.create("proj", "acme", "proj").await.unwrap();

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

        let project = repo.create("old", "acme", "old").await.unwrap();
        captured.lock().unwrap().clear();

        let updated = repo.update(&project.id, "new").await.unwrap();
        assert_eq!(updated.name, "new");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "project");
        assert_eq!(events[0].action, "updated");
        let p: Project = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(p.name, "new");
    }

    // `project.create` seeds five default agent rows (one per base role); the
    // subsequent `DELETE FROM projects` fans out across ~8 cascade targets
    // (epics, tasks, notes, sessions, agents, consolidation_metrics, ...).
    // Dolt currently drops the connection mid-
    // cascade and the driver surfaces it as `Sqlx(Io UnexpectedEof)` —
    // reproducible on 100% of runs against the current image. The same
    // The Dolt multi-cascade DELETE bug that forced this `#[ignore]` was
    // resolved by the MySQL→Postgres cut-over; re-enabled (Postgres executes
    // the cascade DELETE cleanly).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_project() {
        let (bus, captured) = capturing_bus();
        let repo = ProjectRepository::new(test_db(), bus);

        let project = repo.create("del", "acme", "del").await.unwrap();
        captured.lock().unwrap().clear();

        repo.delete(&project.id).await.unwrap();
        assert!(repo.get(&project.id).await.unwrap().is_none());
        let tombstone: Option<String> =
            sqlx::query_scalar("SELECT project_id FROM deleted_projects WHERE project_id = $1")
                .bind(&project.id)
                .fetch_optional(repo.db.pool())
                .await
                .unwrap();
        assert_eq!(tombstone.as_deref(), Some(project.id.as_str()));

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "project");
        assert_eq!(events[0].action, "deleted");
        assert_eq!(events[0].payload["id"].as_str().unwrap(), project.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_projects() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        repo.create("beta", "acme", "beta").await.unwrap();
        repo.create("alpha", "acme", "alpha").await.unwrap();

        let projects = repo.list().await.unwrap();
        assert_eq!(projects.len(), 2);
        // Ordered by name.
        assert_eq!(projects[0].name, "alpha");
        assert_eq!(projects[1].name, "beta");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_accepts_uuid_and_slug() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        let project = repo.create("lookup", "acme", "lookup").await.unwrap();

        // UUID round-trip.
        let by_id = repo.resolve(&project.id).await.unwrap().unwrap();
        assert_eq!(by_id, project.id);

        // Slug round-trip.
        let by_slug = repo.resolve("acme/lookup").await.unwrap().unwrap();
        assert_eq!(by_slug, project.id);

        // Unknown slug + unknown UUID both return None.
        assert!(repo.resolve("ghost/missing").await.unwrap().is_none());
        assert!(
            repo.resolve("00000000-0000-0000-0000-000000000000")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_github_returns_project() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());

        let project = repo.create("lookup", "acme", "lookup").await.unwrap();
        let found = repo.get_by_github("acme", "lookup").await.unwrap().unwrap();
        assert_eq!(found.id, project.id);
        assert_eq!(found.github_owner, "acme");
        assert_eq!(found.github_repo, "lookup");

        assert!(
            repo.get_by_github("ghost", "missing")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_seeds_five_default_roles() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = repo.create("seeded", "acme", "seeded").await.unwrap();

        let rows_raw = sqlx::query!(
            r#"SELECT name, base_role, is_default AS "is_default!: bool" FROM agents WHERE project_id = $1 ORDER BY base_role"#,
            project.id
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let rows: Vec<(String, String, bool)> = rows_raw
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
            assert!(*is_default, "role {base_role} should be is_default=true");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seeding_is_idempotent_on_different_projects() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("proj-a", "acme", "proj-a").await.unwrap();
        repo.create("proj-b", "acme", "proj-b").await.unwrap();

        let total: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM agents WHERE is_default = TRUE"#
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(total, 10, "2 projects × 5 roles = 10 default rows");
    }

    // ── Stack column round-trip ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_stack_default_is_empty_json_object() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo
            .create("stack-default", "acme", "stack-default")
            .await
            .unwrap();
        let stack = repo.get_stack(&project.id).await.unwrap().unwrap();
        assert_eq!(stack, "{}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_stack_round_trips_arbitrary_json() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("stack-rt", "acme", "stack-rt").await.unwrap();

        let payload = r#"{"primary_language":"Rust","is_monorepo":false}"#;
        repo.set_stack(&project.id, payload).await.unwrap();

        let fetched = repo.get_stack(&project.id).await.unwrap().unwrap();
        // JSONB normalizes whitespace/key-order on round-trip, so compare the
        // parsed values rather than the raw serialized strings.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fetched).unwrap(),
            serde_json::from_str::<serde_json::Value>(payload).unwrap(),
        );
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
            out, r#"["**/workspace-hack/**","**/test-support/**"]"#,
            "trimmed, empty-dropped, deduplicated while preserving insertion order"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_config_defaults_to_empty_graph_exclusion_lists() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo
            .create("graph-defaults", "acme", "graph-defaults")
            .await
            .unwrap();
        let cfg = repo.get_config(&project.id).await.unwrap().unwrap();
        assert!(cfg.graph_excluded_paths.is_empty());
        assert!(cfg.graph_orphan_ignore.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_field_round_trips_graph_excluded_paths() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo.create("graph-rt", "acme", "graph-rt").await.unwrap();

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
            vec![
                "**/workspace-hack/**".to_string(),
                "**/test-support/**".to_string()
            ]
        );
        assert!(updated.graph_orphan_ignore.is_empty());

        let refetched = repo.get_config(&project.id).await.unwrap().unwrap();
        assert_eq!(refetched.graph_excluded_paths, updated.graph_excluded_paths);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_config_field_round_trips_graph_orphan_ignore() {
        let repo = ProjectRepository::new(test_db(), EventBus::noop());
        let project = repo
            .create("graph-orphan", "acme", "graph-orphan")
            .await
            .unwrap();

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
        let project = repo.create("graph-bad", "acme", "graph-bad").await.unwrap();

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

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
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote FROM projects ORDER BY name",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_path(&self, path: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE path = ?1",
        )
        .bind(path)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a project path to its ID. Normalizes trailing slashes.
    pub async fn resolve_id_by_path(&self, project_path: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let normalized = project_path.trim_end_matches('/');
        Ok(
            sqlx::query_scalar::<_, String>("SELECT id FROM projects WHERE path = ?1")
                .bind(normalized)
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
        let all = sqlx::query_as::<_, (String, String)>("SELECT id, path FROM projects")
            .fetch_all(self.db.pool())
            .await?;

        let mut best: Option<(String, usize)> = None;
        for (id, path) in all {
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
            sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?1")
                .bind(id)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    pub async fn create(&self, name: &str, path: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)")
            .bind(&id)
            .bind(name)
            .bind(path)
            .execute(self.db.pool())
            .await?;
        let project = sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;

        self.seed_default_roles(&id).await?;

        self.events
            .send(DjinnEventEnvelope::project_created(&project));
        Ok(project)
    }

    /// Insert 6 default agent roles (one per base_role) for a newly created project.
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
            sqlx::query(
                "INSERT OR IGNORE INTO agents
                    (id, project_id, name, base_role, description, is_default)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            )
            .bind(&role_id)
            .bind(project_id)
            .bind(base_role) // name = base_role for defaults
            .bind(base_role)
            .bind(description)
            .execute(self.db.pool())
            .await?;
        }
        Ok(())
    }

    pub async fn update(&self, id: &str, name: &str, path: &str) -> Result<Project> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE projects SET name = ?2, path = ?3 WHERE id = ?1")
            .bind(id)
            .bind(name)
            .bind(path)
            .execute(self.db.pool())
            .await?;
        let project = sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        self.events
            .send(DjinnEventEnvelope::project_updated(&project));
        Ok(project)
    }

    pub async fn get_config(&self, id: &str) -> Result<Option<ProjectConfig>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectConfig>(
            "SELECT target_branch, auto_merge, sync_enabled, sync_remote, verification_rules FROM projects WHERE id = ?1",
        )
        .bind(id)
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
                sqlx::query("UPDATE projects SET target_branch = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(value)
                    .execute(self.db.pool())
                    .await?;
            }
            "auto_merge" => {
                let v = matches!(value, "true" | "1");
                sqlx::query("UPDATE projects SET auto_merge = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(v)
                    .execute(self.db.pool())
                    .await?;
            }
            "sync_enabled" => {
                let v = matches!(value, "true" | "1");
                sqlx::query("UPDATE projects SET sync_enabled = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(v)
                    .execute(self.db.pool())
                    .await?;
            }
            "sync_remote" => {
                let val = if value.is_empty() { None } else { Some(value) };
                sqlx::query("UPDATE projects SET sync_remote = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(val)
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
                sqlx::query("UPDATE projects SET verification_rules = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(value)
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
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE sync_enabled = 1 ORDER BY name",
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
        let exact = sqlx::query_scalar::<_, String>(
            "SELECT id FROM projects WHERE path = ?1 OR name = ?1 LIMIT 1",
        )
        .bind(normalized)
        .fetch_optional(self.db.pool())
        .await?;

        if exact.is_some() {
            return Ok(exact);
        }

        // 2. Longest-prefix match (subdirectory of a known project).
        let all = sqlx::query_as::<_, (String, String)>("SELECT id, path FROM projects")
            .fetch_all(self.db.pool())
            .await?;

        let mut best: Option<(String, usize)> = None;
        for (id, path) in all {
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
        sqlx::query("DELETE FROM projects WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::project_deleted(id));
        Ok(())
    }
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
    async fn create_seeds_six_default_roles() {
        let db = test_db();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = repo.create("seeded", "/seeded").await.unwrap();

        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT name, base_role, is_default FROM agents WHERE project_id = ?1 ORDER BY base_role",
        )
        .bind(&project.id)
        .fetch_all(db.pool())
        .await
        .unwrap();

        let expected_base_roles = ["architect", "lead", "planner", "reviewer", "worker"];

        assert_eq!(
            rows.len(),
            5,
            "expected 6 default roles, got {}",
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

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE is_default = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(total, 12, "2 projects × 6 roles = 12 default rows");
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
}

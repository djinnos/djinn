use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;

use crate::database::Database;
use crate::Result;

#[derive(Clone, Debug, serde::Serialize, sqlx::FromRow)]
pub struct ProjectConfig {
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
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

        self.events.send(DjinnEventEnvelope::project_created(&project));
        Ok(project)
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

        self.events.send(DjinnEventEnvelope::project_updated(&project));
        Ok(project)
    }

    pub async fn get_config(&self, id: &str) -> Result<Option<ProjectConfig>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProjectConfig>(
            "SELECT target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
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
            _ => return Ok(None),
        }

        let Some(config) = self.get_config(id).await? else {
            return Ok(None);
        };
        self.events.send(DjinnEventEnvelope::project_config_updated(id, &config));
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
}

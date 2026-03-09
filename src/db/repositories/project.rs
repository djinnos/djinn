use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::project::Project;

#[derive(Clone, Debug, serde::Serialize, sqlx::FromRow)]
pub struct ProjectConfig {
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
}

pub struct ProjectRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl ProjectRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn list(&self) -> Result<Vec<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects ORDER BY name",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_path(&self, path: &str) -> Result<Option<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE path = ?1",
        )
        .bind(path)
        .fetch_optional(self.db.pool())
        .await?)
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
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self
            .events
            .send(DjinnEvent::ProjectCreated(project.clone()));
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
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self
            .events
            .send(DjinnEvent::ProjectUpdated(project.clone()));
        Ok(project)
    }

    pub async fn update_commands(
        &self,
        id: &str,
        setup_commands: &str,
        verification_commands: &str,
    ) -> Result<Project> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE projects SET setup_commands = ?2, verification_commands = ?3 WHERE id = ?1",
        )
        .bind(id)
        .bind(setup_commands)
        .bind(verification_commands)
        .execute(self.db.pool())
        .await?;
        let project = sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self
            .events
            .send(DjinnEvent::ProjectUpdated(project.clone()));
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
        let _ = self.events.send(DjinnEvent::ProjectConfigUpdated {
            project_id: id.to_owned(),
            config: config.clone(),
        });
        Ok(Some(config))
    }

    /// List all projects with `sync_enabled = true` (SYNC-07).
    pub async fn list_sync_enabled(&self) -> Result<Vec<Project>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, setup_commands, verification_commands, target_branch, auto_merge, sync_enabled, sync_remote FROM projects WHERE sync_enabled = 1 ORDER BY name",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM projects WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        let _ = self
            .events
            .send(DjinnEvent::ProjectDeleted { id: id.to_owned() });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_project() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = ProjectRepository::new(db, tx);

        let project = repo.create("myapp", "/home/user/myapp").await.unwrap();
        assert_eq!(project.name, "myapp");
        assert_eq!(project.path, "/home/user/myapp");
        assert!(!project.id.is_empty());

        let fetched = repo.get(&project.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "myapp");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = ProjectRepository::new(db, tx);

        repo.create("proj", "/tmp/proj").await.unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            DjinnEvent::ProjectCreated(p) => assert_eq!(p.name, "proj"),
            _ => panic!("expected ProjectCreated event"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_project() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = ProjectRepository::new(db, tx);

        let project = repo.create("old", "/old").await.unwrap();
        let _ = rx.recv().await.unwrap(); // consume create event

        let updated = repo.update(&project.id, "new", "/new").await.unwrap();
        assert_eq!(updated.name, "new");
        assert_eq!(updated.path, "/new");

        match rx.recv().await.unwrap() {
            DjinnEvent::ProjectUpdated(p) => assert_eq!(p.name, "new"),
            _ => panic!("expected ProjectUpdated event"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_project() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let repo = ProjectRepository::new(db, tx);

        let project = repo.create("del", "/del").await.unwrap();
        let _ = rx.recv().await.unwrap(); // consume create event

        repo.delete(&project.id).await.unwrap();
        assert!(repo.get(&project.id).await.unwrap().is_none());

        match rx.recv().await.unwrap() {
            DjinnEvent::ProjectDeleted { id } => assert_eq!(id, project.id),
            _ => panic!("expected ProjectDeleted event"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_projects() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = ProjectRepository::new(db, tx);

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
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(1024);
        let repo = ProjectRepository::new(db, tx);

        let project = repo.create("lookup", "/lookup/path").await.unwrap();
        let found = repo.get_by_path("/lookup/path").await.unwrap().unwrap();
        assert_eq!(found.id, project.id);
        assert_eq!(found.path, "/lookup/path");

        // Missing path returns None.
        assert!(repo.get_by_path("/nonexistent").await.unwrap().is_none());
    }
}

use tokio::sync::broadcast;

use crate::db::connection::{Database, OptionalExt};
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::project::Project;

pub struct ProjectRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl ProjectRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn list(&self) -> Result<Vec<Project>> {
        self.db
            .call(|conn| {
                let mut stmt =
                    conn.prepare("SELECT id, name, path, created_at FROM projects ORDER BY name")?;
                let projects = stmt
                    .query_map([], |row| {
                        Ok(Project {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            path: row.get(2)?,
                            created_at: row.get(3)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(projects)
            })
            .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<Project>> {
        let id = id.to_owned();
        self.db
            .call(move |conn| {
                let project = conn
                    .query_row(
                        "SELECT id, name, path, created_at FROM projects WHERE id = ?1",
                        [&id],
                        |row| {
                            Ok(Project {
                                id: row.get(0)?,
                                name: row.get(1)?,
                                path: row.get(2)?,
                                created_at: row.get(3)?,
                            })
                        },
                    )
                    .optional()?;
                Ok(project)
            })
            .await
    }

    pub async fn create(&self, name: &str, path: &str) -> Result<Project> {
        let id = uuid::Uuid::now_v7().to_string();
        let name = name.to_owned();
        let path = path.to_owned();
        let project = self
            .db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)",
                    [&id, &name, &path],
                )?;
                let project = conn.query_row(
                    "SELECT id, name, path, created_at FROM projects WHERE id = ?1",
                    [&id],
                    |row| {
                        Ok(Project {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            path: row.get(2)?,
                            created_at: row.get(3)?,
                        })
                    },
                )?;
                Ok(project)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::ProjectCreated(project.clone()));
        Ok(project)
    }

    pub async fn update(&self, id: &str, name: &str, path: &str) -> Result<Project> {
        let id = id.to_owned();
        let name = name.to_owned();
        let path = path.to_owned();
        let project = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE projects SET name = ?2, path = ?3 WHERE id = ?1",
                    [&id, &name, &path],
                )?;
                let project = conn.query_row(
                    "SELECT id, name, path, created_at FROM projects WHERE id = ?1",
                    [&id],
                    |row| {
                        Ok(Project {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            path: row.get(2)?,
                            created_at: row.get(3)?,
                        })
                    },
                )?;
                Ok(project)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::ProjectUpdated(project.clone()));
        Ok(project)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.db
            .write({
                let id = id.clone();
                move |conn| {
                    conn.execute("DELETE FROM projects WHERE id = ?1", [&id])?;
                    Ok(())
                }
            })
            .await?;

        let _ = self.events.send(DjinnEvent::ProjectDeleted { id });
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
}

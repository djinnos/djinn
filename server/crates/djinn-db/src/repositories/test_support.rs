use std::path::Path;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Project;
use tokio::sync::broadcast;

use crate::database::Database;

pub fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}

pub async fn make_project(db: &Database, path: &Path) -> Project {
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let path_slug = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("root");
    let project_name = format!("test-project-{path_slug}-{id}");
    sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)")
        .bind(&id)
        .bind(&project_name)
        .bind(path.to_str().unwrap())
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query_as::<_, Project>(
        "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote \
         FROM projects WHERE id = ?1",
    )
    .bind(&id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

use std::path::Path;

use djinn_core::events::EventBus;
use djinn_core::models::Note;
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use crate::TaskRepository;
use crate::database::Database;
use crate::repositories::test_support::{
    build_multi_project_housekeeping_fixture, event_bus_for, make_project,
};

use super::*;

mod consolidation_housekeeping;
mod crud_storage;
mod embeddings;
mod graph_scoring;
mod scope_paths_regressions;
mod search_ranking;
mod session_scoped_consolidation;
mod wikilink_graph;

/// Mutex kept around so embedding tests can serialize against the
/// shared sqlite-vec extension state. With the MySQL migration the
/// extension is gone but the embedding tests still acquire the lock
/// (harmless) so we keep the helper as a no-op pending their own
/// rewrite to target Qdrant.
fn sqlite_vec_test_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn make_epic(db: &Database, project_id: &str) -> String {
    let epic_id = uuid::Uuid::now_v7().to_string();
    // `epics.short_id` is VARCHAR(32); a full UUID + "ep-" prefix overflows.
    // Use the last 12 hex digits of the UUID (time_hi + node) — unique enough
    // per test run and always fits.
    let short_id = format!("ep-{}", &epic_id[epic_id.len() - 12..]);
    sqlx::query!(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        epic_id,
        project_id,
        short_id,
        "Epic",
        "",
        "",
        "",
        "",
        "[]",
    )
    .execute(db.pool())
    .await
    .unwrap();
    epic_id
}

async fn make_session(
    db: &Database,
    project_id: &str,
    task_id: Option<&str>,
    _branch: &str,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let task_id = match task_id {
        Some(task_id) => Some(task_id.to_string()),
        None => {
            let epic_id = make_epic(db, project_id).await;
            Some(
                TaskRepository::new(db.clone(), EventBus::noop())
                    .create_with_ac(
                        &epic_id,
                        "Session Task",
                        "session task",
                        "session task design",
                        "task",
                        1,
                        "worker",
                        None,
                        Some(r#"[{"title":"session-ac"}]"#),
                    )
                    .await
                    .unwrap()
                    .id,
            )
        }
    };
    // The MySQL `sessions` table no longer carries a `branch` column — the
    // caller-supplied `_branch` is accepted for legacy signature
    // compatibility but ignored here.
    sqlx::query!(
        "INSERT INTO sessions (
            id,
            project_id,
            task_id,
            model_id,
            agent_type,
            started_at,
            status,
            tokens_in,
            tokens_out
        )
        VALUES (
            ?,
            ?,
            ?,
            'test-model',
            'worker',
            DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
            'completed',
            0,
            0
        )",
        id,
        project_id,
        task_id,
    )
    .execute(db.pool())
    .await
    .unwrap();
    id
}

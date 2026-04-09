use std::path::Path;

use djinn_core::events::EventBus;
use djinn_core::models::Note;
use tokio::sync::broadcast;

use crate::TaskRepository;
use crate::database::Database;
use crate::repositories::test_support::{
    build_multi_project_housekeeping_fixture, event_bus_for, make_project,
};

use super::*;

mod consolidation_housekeeping;
mod crud_storage;
mod graph_scoring;
mod scope_paths_regressions;
mod search_ranking;
mod session_scoped_consolidation;
mod wikilink_graph;

async fn make_epic(db: &Database, project_id: &str) -> String {
    let epic_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("ep-{}", epic_id);
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(short_id)
    .bind("Epic")
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind("[]")
    .execute(db.pool())
    .await
    .unwrap();
    epic_id
}

async fn make_session(
    db: &Database,
    project_id: &str,
    task_id: Option<&str>,
    branch: &str,
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
    let has_branch: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'branch'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    if has_branch > 0 {
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, branch, status, started_at)
             VALUES (?1, ?2, ?3, ?4, 'completed', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        )
        .bind(&id)
        .bind(project_id)
        .bind(task_id.as_deref())
        .bind(branch)
        .execute(db.pool())
        .await
        .unwrap();
    } else {
        sqlx::query(
            "INSERT INTO sessions (
                id,
                project_id,
                task_id,
                model_id,
                agent_type,
                started_at,
                status,
                tokens_in,
                tokens_out,
                worktree_path
            )
            VALUES (
                ?1,
                ?2,
                ?3,
                'test-model',
                ?4,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                'completed',
                0,
                0,
                NULL
            )",
        )
        .bind(&id)
        .bind(project_id)
        .bind(task_id.as_deref())
        .bind(branch)
        .execute(db.pool())
        .await
        .unwrap();
    }
    id
}

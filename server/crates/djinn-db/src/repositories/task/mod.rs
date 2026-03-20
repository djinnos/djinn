use sqlx::SqlitePool;

use crate::database::Database;
use crate::{Error, Result};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{ActivityEntry, Task, TaskStatus, TransitionAction, compute_transition};

mod activity;
mod blockers;
mod queries;
mod reads;
mod status;
pub(crate) mod verification;
mod writes;

// ── Query / result types ──────────────────────────────────────────────────────

/// Filters and pagination for [`TaskRepository::list_filtered`].
pub struct ListQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    /// Filter by epic_id (already resolved to a UUID).
    pub parent: Option<String>,
    /// "priority" | "created" | "created_desc" | "updated" | "updated_desc" | "closed"
    pub sort: String,
    pub limit: i64,
    pub offset: i64,
}

pub struct CreateTaskParams<'a> {
    pub epic_id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub design: &'a str,
    pub issue_type: &'a str,
    pub priority: i64,
    pub owner: &'a str,
    pub status: Option<&'a str>,
}

pub struct CreateTaskInProjectParams<'a> {
    pub project_id: &'a str,
    pub epic_id: Option<&'a str>,
    pub title: &'a str,
    pub description: &'a str,
    pub design: &'a str,
    pub issue_type: &'a str,
    pub priority: i64,
    pub owner: &'a str,
    pub status: Option<&'a str>,
}

pub struct UpdateTaskParams<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub design: &'a str,
    pub priority: i64,
    pub owner: &'a str,
    pub labels: &'a str,
    pub acceptance_criteria: &'a str,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            status: None,
            project_id: None,
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            sort: "priority".to_owned(),
            limit: 25,
            offset: 0,
        }
    }
}

pub struct ListResult {
    pub tasks: Vec<Task>,
    pub total_count: i64,
}

/// Filters for [`TaskRepository::count_grouped`].
pub struct CountQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub parent: Option<String>,
    /// "status" | "priority" | "issue_type" | "parent"
    pub group_by: Option<String>,
}

/// Filters for [`TaskRepository::query_activity`].
pub struct ActivityQuery {
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub event_type: Option<String>,
    pub actor_role: Option<String>,
    pub from_time: Option<String>,
    pub to_time: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ActivityQuery {
    fn default() -> Self {
        Self {
            task_id: None,
            project_id: None,
            event_type: None,
            actor_role: None,
            from_time: None,
            to_time: None,
            limit: 50,
            offset: 0,
        }
    }
}

/// Minimal task reference returned by blocker listing queries.
#[derive(Debug, sqlx::FromRow)]
pub struct BlockerRef {
    pub task_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug)]
pub(super) enum SqlParam {
    Text(String),
    Integer(i64),
}

/// Filters for [`TaskRepository::list_ready`].
pub struct ReadyQuery {
    pub project_id: Option<String>,
    pub issue_type: Option<String>,
    pub label: Option<String>,
    pub owner: Option<String>,
    pub priority_max: Option<i64>,
    pub limit: i64,
}

impl Default for ReadyQuery {
    fn default() -> Self {
        Self {
            issue_type: None,
            project_id: None,
            label: None,
            owner: None,
            priority_max: None,
            limit: 25,
        }
    }
}

pub struct TaskRepository {
    pub(super) db: Database,
    pub(super) events: EventBus,
}

impl TaskRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub(super) async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::Internal(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), "tasks", &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), "tasks", &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::Internal(
            "short_id collision after 16 retries".into(),
        ))
    }
}

pub(super) const TASK_SELECT_WHERE_ID: &str =
    "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
            status, priority, owner, labels, acceptance_criteria,
            reopen_count, continuation_count, verification_failure_count, created_at, updated_at, closed_at,
            close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
     FROM tasks WHERE id = ?1";

pub(super) fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616)
}

pub(super) fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

/// Check if a constraint violation occurred.
pub(super) fn is_constraint_violation(db_err: &dyn sqlx::error::DatabaseError) -> bool {
    db_err.is_unique_violation()
        || db_err.is_foreign_key_violation()
        || db_err.message().contains("constraint failed")
}

/// Extract the constraint name from a database error message.
pub(super) fn extract_constraint_name(db_err: &dyn sqlx::error::DatabaseError) -> Option<String> {
    let message = db_err.message();
    // SQLite constraint messages follow patterns like:
    // "UNIQUE constraint failed: tasks.short_id"
    // "FOREIGN KEY constraint failed"
    if message.contains("short_id") {
        Some("short_id".to_string())
    } else {
        None
    }
}

pub(super) async fn short_id_exists(
    pool: &SqlitePool,
    table: &str,
    short_id: &str,
) -> Result<bool> {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?1)");
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?
        > 0)
}

/// Reopen a closed epic when a task is added to it or moved to it.
/// Inlined from EpicRepository::reopen to avoid a circular dependency.
pub(super) async fn maybe_reopen_epic(
    db: &Database,
    events: &EventBus,
    epic_id: &str,
) -> Result<()> {
    let closed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM epics WHERE id = ?1 AND status = 'closed'")
            .bind(epic_id)
            .fetch_one(db.pool())
            .await?;

    if closed == 0 {
        return Ok(());
    }

    sqlx::query(
        "UPDATE epics SET status = 'open', closed_at = NULL,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
    )
    .bind(epic_id)
    .execute(db.pool())
    .await?;

    if let Some(epic) = sqlx::query_as::<_, djinn_core::models::Epic>(
        "SELECT id, project_id, short_id, title, description, emoji, color, status,
                owner, created_at, updated_at, closed_at, memory_refs
         FROM epics WHERE id = ?1",
    )
    .bind(epic_id)
    .fetch_optional(db.pool())
    .await?
    {
        events.send(DjinnEventEnvelope::epic_updated(&epic));
    }

    Ok(())
}

use tokio::sync::broadcast;

use crate::db::connection::{Database, OptionalExt};
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::task::{ActivityEntry, Task, TaskStatus, TransitionAction, compute_transition};

// ── Query / result types ──────────────────────────────────────────────────────

/// Filters and pagination for [`TaskRepository::list_filtered`].
pub struct ListQuery {
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

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            status: None,
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
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub parent: Option<String>,
    /// "status" | "priority" | "issue_type" | "parent"
    pub group_by: Option<String>,
}

pub struct TaskRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl TaskRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn list_by_epic(&self, epic_id: &str) -> Result<Vec<Task>> {
        let epic_id = epic_id.to_owned();
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, short_id, epic_id, title, description, design, issue_type,
                            status, priority, owner, labels, acceptance_criteria,
                            reopen_count, continuation_count, created_at, updated_at, closed_at,
                            blocked_from_status, close_reason
                     FROM tasks WHERE epic_id = ?1 ORDER BY priority, created_at",
                )?;
                let tasks = stmt
                    .query_map([&epic_id], row_to_task)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(tasks)
            })
            .await
    }

    pub async fn list_by_status(&self, status: &str) -> Result<Vec<Task>> {
        let status = status.to_owned();
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, short_id, epic_id, title, description, design, issue_type,
                            status, priority, owner, labels, acceptance_criteria,
                            reopen_count, continuation_count, created_at, updated_at, closed_at,
                            blocked_from_status, close_reason
                     FROM tasks WHERE status = ?1 ORDER BY priority, created_at",
                )?;
                let tasks = stmt
                    .query_map([&status], row_to_task)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(tasks)
            })
            .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<Task>> {
        let id = id.to_owned();
        self.db
            .call(move |conn| {
                Ok(conn
                    .query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)
                    .optional()?)
            })
            .await
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Task>> {
        let short_id = short_id.to_owned();
        self.db
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, short_id, epic_id, title, description, design, issue_type,
                                status, priority, owner, labels, acceptance_criteria,
                                reopen_count, continuation_count, created_at, updated_at, closed_at,
                                blocked_from_status, close_reason
                         FROM tasks WHERE short_id = ?1",
                        [&short_id],
                        row_to_task,
                    )
                    .optional()?)
            })
            .await
    }

    pub async fn create(
        &self,
        epic_id: &str,
        title: &str,
        description: &str,
        design: &str,
        issue_type: &str,
        priority: i64,
        owner: &str,
    ) -> Result<Task> {
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let epic_id = epic_id.to_owned();
        let title = title.to_owned();
        let description = description.to_owned();
        let design = design.to_owned();
        let issue_type = issue_type.to_owned();
        let owner = owner.to_owned();

        let task: Task = self
            .db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO tasks
                        (id, short_id, epic_id, title, description, design,
                         issue_type, priority, owner)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        &id, &short_id, &epic_id, &title, &description,
                        &design, &issue_type, priority, &owner
                    ],
                )?;
                Ok(conn.query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)?)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::TaskCreated(task.clone()));
        Ok(task)
    }

    pub async fn update(
        &self,
        id: &str,
        title: &str,
        description: &str,
        design: &str,
        priority: i64,
        owner: &str,
        labels: &str,
        acceptance_criteria: &str,
    ) -> Result<Task> {
        let id = id.to_owned();
        let title = title.to_owned();
        let description = description.to_owned();
        let design = design.to_owned();
        let owner = owner.to_owned();
        let labels = labels.to_owned();
        let acceptance_criteria = acceptance_criteria.to_owned();

        let task: Task = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE tasks SET
                        title = ?2, description = ?3, design = ?4,
                        priority = ?5, owner = ?6, labels = ?7,
                        acceptance_criteria = ?8,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![
                        &id, &title, &description, &design,
                        priority, &owner, &labels, &acceptance_criteria
                    ],
                )?;
                Ok(conn.query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)?)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Transition a task through the state machine.
    ///
    /// Validates the action against the current status, applies all side effects
    /// (reopen_count, continuation_count, closed_at, blocked_from_status, close_reason),
    /// writes an activity_log entry, and emits a `TaskUpdated` event — all atomically.
    pub async fn transition(
        &self,
        id: &str,
        action: TransitionAction,
        actor_id: &str,
        actor_role: &str,
        reason: Option<&str>,
        target_override: Option<TaskStatus>,
    ) -> Result<Task> {
        // Check reason requirement before touching the DB.
        if action.requires_reason() && reason.map(str::is_empty).unwrap_or(true) {
            return Err(Error::InvalidTransition(format!(
                "{action:?} requires a non-empty reason"
            )));
        }

        let id = id.to_owned();
        let actor_id = actor_id.to_owned();
        let actor_role = actor_role.to_owned();
        let reason_str = reason.unwrap_or("").to_owned();

        let task: Task = self
            .db
            .write(move |conn| {
                // Load current task.
                let current = conn.query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)?;
                let from = TaskStatus::parse(&current.status)?;

                // Validate and compute side effects.
                let apply = compute_transition(&action, &from, target_override.as_ref())?;

                // For Start: block if any unresolved blockers exist.
                if action == TransitionAction::Start {
                    let count: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM blockers WHERE task_id = ?1",
                        [&id],
                        |r| r.get(0),
                    )?;
                    if count > 0 {
                        return Err(Error::InvalidTransition(
                            "task has unresolved blockers".into(),
                        ));
                    }
                }

                // Resolve final target status.
                // Unblock (to_status = None) restores blocked_from_status, defaulting to Open.
                let to_status = match apply.to_status {
                    Some(s) => s,
                    None => current
                        .blocked_from_status
                        .as_deref()
                        .and_then(|s| TaskStatus::parse(s).ok())
                        .unwrap_or(TaskStatus::Open),
                };
                let to_str = to_status.as_str();
                let from_str = from.as_str();

                // Apply all side effects atomically.
                conn.execute(
                    "UPDATE tasks SET
                        status = ?2,
                        reopen_count = reopen_count + ?3,
                        continuation_count = CASE WHEN ?4 THEN 0 ELSE continuation_count END,
                        closed_at = CASE
                            WHEN ?5 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                            WHEN ?6 THEN NULL
                            ELSE closed_at
                        END,
                        blocked_from_status = CASE
                            WHEN ?7 THEN ?10
                            WHEN ?8 THEN NULL
                            ELSE blocked_from_status
                        END,
                        close_reason = CASE
                            WHEN ?11 IS NOT NULL THEN ?11
                            WHEN ?9 THEN NULL
                            ELSE close_reason
                        END,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![
                        &id,                                               // ?1
                        to_str,                                            // ?2
                        if apply.increment_reopen { 1i64 } else { 0 },    // ?3
                        apply.reset_continuation,                          // ?4
                        apply.set_closed_at,                               // ?5
                        apply.clear_closed_at,                             // ?6
                        apply.save_blocked_from,                           // ?7
                        apply.clear_blocked_from,                          // ?8
                        apply.clear_close_reason,                          // ?9
                        from_str,                                          // ?10 (save_blocked_from value)
                        apply.close_reason,                                // ?11
                    ],
                )?;

                // Append activity log entry.
                let activity_id = uuid::Uuid::now_v7().to_string();
                let payload = serde_json::json!({
                    "from_status": from_str,
                    "to_status": to_str,
                    "reason": if reason_str.is_empty() { None } else { Some(reason_str.as_str()) },
                })
                .to_string();

                conn.execute(
                    "INSERT INTO activity_log
                        (id, task_id, actor_id, actor_role, event_type, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        &activity_id,
                        &id,
                        &actor_id,
                        &actor_role,
                        apply.activity_type,
                        &payload,
                    ],
                )?;

                Ok(conn.query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)?)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Raw status update, bypassing state machine validation.
    ///
    /// Only used for tests and admin tooling. Production code should use `transition`.
    pub async fn set_status(&self, id: &str, status: &str) -> Result<Task> {
        let id = id.to_owned();
        let status = status.to_owned();

        let task: Task = self
            .db
            .write(move |conn| {
                let closed_at_sql = if status == "closed" {
                    "closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),"
                } else {
                    ""
                };
                let reopen_inc: i64 = if status == "open" { 1 } else { 0 };
                conn.execute(
                    &format!(
                        "UPDATE tasks SET status = ?2, {closed_at_sql}
                            reopen_count = reopen_count + ?3,
                            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                         WHERE id = ?1"
                    ),
                    rusqlite::params![&id, &status, reopen_inc],
                )?;
                Ok(conn.query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)?)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    /// Move a task to a different epic.
    pub async fn move_to_epic(&self, id: &str, new_epic_id: &str) -> Result<Task> {
        let id = id.to_owned();
        let new_epic_id = new_epic_id.to_owned();
        let task: Task = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE tasks SET epic_id = ?2,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![&id, &new_epic_id],
                )?;
                Ok(conn.query_row(TASK_SELECT_WHERE_ID, [&id], row_to_task)?)
            })
            .await?;
        let _ = self.events.send(DjinnEvent::TaskUpdated(task.clone()));
        Ok(task)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.db
            .write({
                let id = id.clone();
                move |conn| {
                    conn.execute("DELETE FROM tasks WHERE id = ?1", [&id])?;
                    Ok(())
                }
            })
            .await?;

        let _ = self.events.send(DjinnEvent::TaskDeleted { id });
        Ok(())
    }

    // ── Blockers ─────────────────────────────────────────────────────────────

    pub async fn add_blocker(&self, task_id: &str, blocking_id: &str) -> Result<()> {
        let task_id = task_id.to_owned();
        let blocking_id = blocking_id.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO blockers (task_id, blocking_task_id)
                     VALUES (?1, ?2)",
                    [&task_id, &blocking_id],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn remove_blocker(&self, task_id: &str, blocking_id: &str) -> Result<()> {
        let task_id = task_id.to_owned();
        let blocking_id = blocking_id.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "DELETE FROM blockers WHERE task_id = ?1 AND blocking_task_id = ?2",
                    [&task_id, &blocking_id],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn list_blockers(&self, task_id: &str) -> Result<Vec<String>> {
        let task_id = task_id.to_owned();
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT blocking_task_id FROM blockers WHERE task_id = ?1",
                )?;
                let ids = stmt
                    .query_map([&task_id], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<String>, _>>()?;
                Ok(ids)
            })
            .await
    }

    // ── Activity log ─────────────────────────────────────────────────────────

    pub async fn log_activity(
        &self,
        task_id: Option<&str>,
        actor_id: &str,
        actor_role: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<ActivityEntry> {
        let id = uuid::Uuid::now_v7().to_string();
        let task_id = task_id.map(ToOwned::to_owned);
        let actor_id = actor_id.to_owned();
        let actor_role = actor_role.to_owned();
        let event_type = event_type.to_owned();
        let payload = payload.to_owned();

        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO activity_log
                        (id, task_id, actor_id, actor_role, event_type, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        &id, &task_id, &actor_id, &actor_role, &event_type, &payload
                    ],
                )?;
                Ok(conn.query_row(
                    "SELECT id, task_id, actor_id, actor_role, event_type, payload, created_at
                     FROM activity_log WHERE id = ?1",
                    [&id],
                    row_to_activity,
                )?)
            })
            .await
    }

    pub async fn list_activity(&self, task_id: &str) -> Result<Vec<ActivityEntry>> {
        let task_id = task_id.to_owned();
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, task_id, actor_id, actor_role, event_type, payload, created_at
                     FROM activity_log WHERE task_id = ?1 ORDER BY created_at",
                )?;
                let entries = stmt
                    .query_map([&task_id], row_to_activity)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(entries)
            })
            .await
    }

    /// Resolve a task by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Task>> {
        let id = id_or_short.to_owned();
        self.db
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id, short_id, epic_id, title, description, design, issue_type,
                                status, priority, owner, labels, acceptance_criteria,
                                reopen_count, continuation_count, created_at, updated_at, closed_at,
                                blocked_from_status, close_reason
                         FROM tasks WHERE id = ?1 OR short_id = ?1",
                        [&id],
                        row_to_task,
                    )
                    .optional()?)
            })
            .await
    }

    /// List tasks with filters, sorting, and pagination.
    pub async fn list_filtered(&self, query: ListQuery) -> Result<ListResult> {
        self.db
            .call(move |conn| {
                let (where_sql, params) = build_where(
                    &query.status,
                    &query.issue_type,
                    query.priority,
                    &query.label,
                    &query.text,
                    &query.parent,
                );
                let order_sql = sort_to_sql(&query.sort);

                let total: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM tasks WHERE {where_sql}"),
                    rusqlite::params_from_iter(params.iter()),
                    |r| r.get(0),
                )?;

                let mut list_params = params.clone();
                list_params.push(rusqlite::types::Value::Integer(query.limit));
                list_params.push(rusqlite::types::Value::Integer(query.offset));

                let sql = format!(
                    "SELECT id, short_id, epic_id, title, description, design, issue_type,
                            status, priority, owner, labels, acceptance_criteria,
                            reopen_count, continuation_count, created_at, updated_at, closed_at,
                            blocked_from_status, close_reason
                     FROM tasks WHERE {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
                );
                let mut stmt = conn.prepare(&sql)?;
                let tasks = stmt
                    .query_map(rusqlite::params_from_iter(list_params.iter()), row_to_task)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;

                Ok(ListResult { tasks, total_count: total })
            })
            .await
    }

    /// Count tasks with optional grouping.
    pub async fn count_grouped(&self, query: CountQuery) -> Result<serde_json::Value> {
        self.db
            .call(move |conn| {
                let (where_sql, params) = build_where(
                    &query.status,
                    &query.issue_type,
                    query.priority,
                    &query.label,
                    &query.text,
                    &query.parent,
                );

                match query.group_by.as_deref() {
                    Some(gb) => {
                        let col = match gb {
                            "status" => "status",
                            "priority" => "priority",
                            "issue_type" => "issue_type",
                            "parent" => "epic_id",
                            other => {
                                return Err(Error::Internal(format!("unknown group_by: {other}")));
                            }
                        };
                        let sql = format!(
                            "SELECT {col}, COUNT(*) FROM tasks WHERE {where_sql}
                             GROUP BY {col} ORDER BY COUNT(*) DESC"
                        );
                        let mut stmt = conn.prepare(&sql)?;
                        let groups: Vec<serde_json::Value> = stmt
                            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                                let key: String =
                                    row.get::<_, rusqlite::types::Value>(0).map(|v| match v {
                                        rusqlite::types::Value::Text(s) => s,
                                        rusqlite::types::Value::Integer(i) => i.to_string(),
                                        _ => String::new(),
                                    })?;
                                let count: i64 = row.get(1)?;
                                Ok((key, count))
                            })?
                            .filter_map(|r| r.ok())
                            .map(|(key, count)| serde_json::json!({"key": key, "count": count}))
                            .collect();
                        Ok(serde_json::json!({ "groups": groups }))
                    }
                    None => {
                        let total: i64 = conn.query_row(
                            &format!("SELECT COUNT(*) FROM tasks WHERE {where_sql}"),
                            rusqlite::params_from_iter(params.iter()),
                            |r| r.get(0),
                        )?;
                        Ok(serde_json::json!({ "total_count": total }))
                    }
                }
            })
            .await
    }

    async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        let seed_id = seed_id.to_owned();
        self.db
            .call(move |conn| {
                let seed = uuid::Uuid::parse_str(&seed_id)
                    .map_err(|e| Error::Internal(e.to_string()))?;
                let candidate = short_id_from_uuid(&seed);
                if !short_id_exists(conn, "tasks", &candidate)? {
                    return Ok(candidate);
                }
                for _ in 0..16 {
                    let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
                    if !short_id_exists(conn, "tasks", &candidate)? {
                        return Ok(candidate);
                    }
                }
                Err(Error::Internal("short_id collision after 16 retries".into()))
            })
            .await
    }
}

const TASK_SELECT_WHERE_ID: &str =
    "SELECT id, short_id, epic_id, title, description, design, issue_type,
            status, priority, owner, labels, acceptance_criteria,
            reopen_count, continuation_count, created_at, updated_at, closed_at,
            blocked_from_status, close_reason
     FROM tasks WHERE id = ?1";

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        short_id: row.get(1)?,
        epic_id: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        design: row.get(5)?,
        issue_type: row.get(6)?,
        status: row.get(7)?,
        priority: row.get(8)?,
        owner: row.get(9)?,
        labels: row.get(10)?,
        acceptance_criteria: row.get(11)?,
        reopen_count: row.get(12)?,
        continuation_count: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
        closed_at: row.get(16)?,
        blocked_from_status: row.get(17)?,
        close_reason: row.get(18)?,
    })
}

fn row_to_activity(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActivityEntry> {
    Ok(ActivityEntry {
        id: row.get(0)?,
        task_id: row.get(1)?,
        actor_id: row.get(2)?,
        actor_role: row.get(3)?,
        event_type: row.get(4)?,
        payload: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616)
}

fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

// ── Dynamic query helpers ─────────────────────────────────────────────────────

/// Build a SQL WHERE clause + params vector from optional filter fields.
///
/// Returns `("1=1", [])` when no filters are supplied.
fn build_where(
    status: &Option<String>,
    issue_type: &Option<String>,
    priority: Option<i64>,
    label: &Option<String>,
    text: &Option<String>,
    parent: &Option<String>,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(s) = status {
        clauses.push("status = ?".to_owned());
        params.push(rusqlite::types::Value::Text(s.clone()));
    }

    if let Some(it) = issue_type {
        if let Some(neg) = it.strip_prefix('!') {
            clauses.push("issue_type != ?".to_owned());
            params.push(rusqlite::types::Value::Text(neg.to_owned()));
        } else {
            clauses.push("issue_type = ?".to_owned());
            params.push(rusqlite::types::Value::Text(it.clone()));
        }
    }

    if let Some(p) = priority {
        clauses.push("priority = ?".to_owned());
        params.push(rusqlite::types::Value::Integer(p));
    }

    if let Some(lbl) = label {
        clauses.push(
            "EXISTS (SELECT 1 FROM json_each(labels) WHERE value = ?)".to_owned(),
        );
        params.push(rusqlite::types::Value::Text(lbl.clone()));
    }

    if let Some(t) = text {
        clauses.push("(title LIKE ? OR description LIKE ?)".to_owned());
        let pattern = format!("%{t}%");
        params.push(rusqlite::types::Value::Text(pattern.clone()));
        params.push(rusqlite::types::Value::Text(pattern));
    }

    if let Some(p) = parent {
        clauses.push("epic_id = ?".to_owned());
        params.push(rusqlite::types::Value::Text(p.clone()));
    }

    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };

    (where_sql, params)
}

/// Convert a sort key to a SQL ORDER BY clause.
fn sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        "closed" => "closed_at DESC, created_at DESC",
        _ => "priority ASC, created_at ASC", // default: "priority"
    }
}

fn short_id_exists(
    conn: &rusqlite::Connection,
    table: &str,
    short_id: &str,
) -> rusqlite::Result<bool> {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?1)");
    conn.query_row(&sql, [short_id], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::epic::EpicRepository;
    use crate::models::task::{TaskStatus, TransitionAction};
    use crate::test_helpers;

    async fn make_epic(db: &Database, tx: broadcast::Sender<DjinnEvent>) -> crate::models::epic::Epic {
        EpicRepository::new(db.clone(), tx)
            .create("Test Epic", "", "", "", "")
            .await
            .unwrap()
    }

    async fn open_task(repo: &TaskRepository, epic_id: &str) -> Task {
        repo.create(epic_id, "T", "", "", "task", 0, "").await.unwrap()
    }

    // ── Existing tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_and_get_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo
            .create(&epic.id, "My Task", "", "", "task", 0, "user@example.com")
            .await
            .unwrap();
        assert_eq!(task.title, "My Task");
        assert_eq!(task.status, "open");
        assert_eq!(task.short_id.len(), 4);

        let fetched = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My Task");
    }

    #[tokio::test]
    async fn short_id_lookup() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo.create(&epic.id, "T", "", "", "task", 0, "").await.unwrap();
        let found = repo.get_by_short_id(&task.short_id).await.unwrap().unwrap();
        assert_eq!(found.id, task.id);
    }

    #[tokio::test]
    async fn create_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let _ = rx.recv().await.unwrap(); // consume EpicCreated
        let repo = TaskRepository::new(db, tx);

        repo.create(&epic.id, "Event Task", "", "", "task", 0, "").await.unwrap();
        match rx.recv().await.unwrap() {
            DjinnEvent::TaskCreated(t) => assert_eq!(t.title, "Event Task"),
            _ => panic!("expected TaskCreated"),
        }
    }

    #[tokio::test]
    async fn set_status_transitions() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo.create(&epic.id, "T", "", "", "task", 0, "").await.unwrap();
        let updated = repo.set_status(&task.id, "in_progress").await.unwrap();
        assert_eq!(updated.status, "in_progress");

        let closed = repo.set_status(&task.id, "closed").await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());
    }

    #[tokio::test]
    async fn reopen_increments_counter() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo.create(&epic.id, "T", "", "", "task", 0, "").await.unwrap();
        repo.set_status(&task.id, "closed").await.unwrap();
        let reopened = repo.set_status(&task.id, "open").await.unwrap();
        assert_eq!(reopened.reopen_count, 1);
    }

    #[tokio::test]
    async fn blocker_management() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = repo.create(&epic.id, "T1", "", "", "task", 0, "").await.unwrap();
        let t2 = repo.create(&epic.id, "T2", "", "", "task", 1, "").await.unwrap();

        repo.add_blocker(&t2.id, &t1.id).await.unwrap();
        let blockers = repo.list_blockers(&t2.id).await.unwrap();
        assert_eq!(blockers, vec![t1.id.clone()]);

        repo.remove_blocker(&t2.id, &t1.id).await.unwrap();
        assert!(repo.list_blockers(&t2.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn activity_log() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = repo.create(&epic.id, "T", "", "", "task", 0, "").await.unwrap();
        repo.log_activity(
            Some(&task.id),
            "user@example.com",
            "user",
            "comment",
            r#"{"body":"hello"}"#,
        )
        .await
        .unwrap();

        let entries = repo.list_activity(&task.id).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, "comment");
        assert_eq!(entries[0].task_id.as_deref(), Some(task.id.as_str()));
    }

    #[tokio::test]
    async fn list_by_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        repo.create(&epic.id, "A", "", "", "task", 1, "").await.unwrap();
        repo.create(&epic.id, "B", "", "", "feature", 0, "").await.unwrap();

        let tasks = repo.list_by_epic(&epic.id).await.unwrap();
        assert_eq!(tasks.len(), 2);
        // Ordered by priority then created_at — B (priority 0) first.
        assert_eq!(tasks[0].title, "B");
    }

    #[tokio::test]
    async fn delete_task_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let _ = rx.recv().await.unwrap();
        let repo = TaskRepository::new(db, tx);

        let task = repo.create(&epic.id, "Del", "", "", "task", 0, "").await.unwrap();
        let _ = rx.recv().await.unwrap();

        repo.delete(&task.id).await.unwrap();
        match rx.recv().await.unwrap() {
            DjinnEvent::TaskDeleted { id } => assert_eq!(id, task.id),
            _ => panic!("expected TaskDeleted"),
        }
    }

    // ── State machine tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn status_enum_roundtrips() {
        let statuses = [
            "draft", "open", "in_progress", "needs_task_review", "in_task_review",
            "needs_phase_review", "in_phase_review", "approved", "closed", "blocked",
        ];
        for s in statuses {
            let parsed = TaskStatus::parse(s).unwrap();
            assert_eq!(parsed.as_str(), s, "round-trip failed for {s}");
        }
        assert!(TaskStatus::parse("unknown").is_err());
    }

    #[tokio::test]
    async fn full_happy_path() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        // Tasks are created as "open".
        let task = open_task(&repo, &epic.id).await;
        assert_eq!(task.status, "open");

        // start
        let t = repo.transition(&task.id, TransitionAction::Start, "", "system", None, None).await.unwrap();
        assert_eq!(t.status, "in_progress");

        // submit_task_review
        let t = repo.transition(&t.id, TransitionAction::SubmitTaskReview, "", "system", None, None).await.unwrap();
        assert_eq!(t.status, "needs_task_review");

        // task_review_start
        let t = repo.transition(&t.id, TransitionAction::TaskReviewStart, "", "task_reviewer", None, None).await.unwrap();
        assert_eq!(t.status, "in_task_review");

        // task_review_approve
        let t = repo.transition(&t.id, TransitionAction::TaskReviewApprove, "", "task_reviewer", None, None).await.unwrap();
        assert_eq!(t.status, "needs_phase_review");

        // phase_review_start
        let t = repo.transition(&t.id, TransitionAction::PhaseReviewStart, "", "phase_reviewer", None, None).await.unwrap();
        assert_eq!(t.status, "in_phase_review");

        // phase_review_approve
        let t = repo.transition(&t.id, TransitionAction::PhaseReviewApprove, "", "phase_reviewer", None, None).await.unwrap();
        assert_eq!(t.status, "approved");

        // close
        let t = repo.transition(&t.id, TransitionAction::Close, "", "system", None, None).await.unwrap();
        assert_eq!(t.status, "closed");
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn invalid_transition_returns_error() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;

        // Can't submit_task_review from open (must be in_progress).
        let err = repo
            .transition(&task.id, TransitionAction::SubmitTaskReview, "", "system", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)), "expected InvalidTransition, got {err:?}");

        // Can't accept from open (must be draft).
        let err = repo
            .transition(&task.id, TransitionAction::Accept, "", "system", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn task_review_reject_increments_reopen() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo.transition(&task.id, TransitionAction::Start, "", "system", None, None).await.unwrap();
        let t = repo.transition(&t.id, TransitionAction::SubmitTaskReview, "", "system", None, None).await.unwrap();
        let t = repo.transition(&t.id, TransitionAction::TaskReviewStart, "", "task_reviewer", None, None).await.unwrap();

        let t = repo.transition(
            &t.id, TransitionAction::TaskReviewReject,
            "reviewer@example.com", "task_reviewer",
            Some("needs more tests"), None,
        ).await.unwrap();

        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 1);
        assert_eq!(t.continuation_count, 0);
    }

    #[tokio::test]
    async fn task_review_reject_conflict_does_not_increment_reopen() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo.transition(&task.id, TransitionAction::Start, "", "system", None, None).await.unwrap();
        let t = repo.transition(&t.id, TransitionAction::SubmitTaskReview, "", "system", None, None).await.unwrap();
        let t = repo.transition(&t.id, TransitionAction::TaskReviewStart, "", "task_reviewer", None, None).await.unwrap();

        let t = repo.transition(
            &t.id, TransitionAction::TaskReviewRejectConflict,
            "reviewer@example.com", "task_reviewer",
            Some("merge conflict"), None,
        ).await.unwrap();

        assert_eq!(t.status, "open");
        assert_eq!(t.reopen_count, 0); // conflict doesn't count against budget
        assert_eq!(t.continuation_count, 0);
    }

    #[tokio::test]
    async fn block_saves_and_unblock_restores_status() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        // Move to in_progress first.
        let t = repo.transition(&task.id, TransitionAction::Start, "", "system", None, None).await.unwrap();
        assert_eq!(t.status, "in_progress");

        // Block it — should store in_progress as blocked_from_status.
        let t = repo.transition(
            &t.id, TransitionAction::Block,
            "user", "user", Some("waiting on external API"), None,
        ).await.unwrap();
        assert_eq!(t.status, "blocked");
        assert_eq!(t.blocked_from_status.as_deref(), Some("in_progress"));

        // Check activity log event type.
        let entries = repo.list_activity(&t.id).await.unwrap();
        assert_eq!(entries.last().unwrap().event_type, "blocked");

        // Unblock — should restore to in_progress.
        let t = repo.transition(&t.id, TransitionAction::Unblock, "user", "user", None, None).await.unwrap();
        assert_eq!(t.status, "in_progress");
        assert!(t.blocked_from_status.is_none());

        let entries = repo.list_activity(&t.id).await.unwrap();
        assert_eq!(entries.last().unwrap().event_type, "unblocked");
    }

    #[tokio::test]
    async fn force_close_from_any_state() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo.transition(&task.id, TransitionAction::Start, "", "system", None, None).await.unwrap();

        let t = repo.transition(
            &t.id, TransitionAction::ForceClose,
            "admin", "user", Some("cancelled"), None,
        ).await.unwrap();

        assert_eq!(t.status, "closed");
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("force_closed"));
    }

    #[tokio::test]
    async fn reopen_clears_closed_at_and_increments_reopen() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        // Force-close it directly.
        let t = repo.transition(
            &task.id, TransitionAction::ForceClose,
            "admin", "user", Some("testing"), None,
        ).await.unwrap();
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("force_closed"));

        // Reopen.
        let t = repo.transition(
            &t.id, TransitionAction::Reopen,
            "user", "user", Some("still needed"), None,
        ).await.unwrap();
        assert_eq!(t.status, "open");
        assert!(t.closed_at.is_none());
        assert!(t.close_reason.is_none());
        assert_eq!(t.reopen_count, 1);
    }

    #[tokio::test]
    async fn start_blocked_by_unresolved_blockers() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let t1 = open_task(&repo, &epic.id).await;
        let t2 = open_task(&repo, &epic.id).await;
        repo.add_blocker(&t2.id, &t1.id).await.unwrap(); // t2 blocked by t1

        let err = repo
            .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));

        // After removing the blocker, start succeeds.
        repo.remove_blocker(&t2.id, &t1.id).await.unwrap();
        let t = repo
            .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
            .await
            .unwrap();
        assert_eq!(t.status, "in_progress");
    }

    #[tokio::test]
    async fn user_override_to_closed() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        let t = repo.transition(
            &task.id, TransitionAction::UserOverride,
            "admin", "user", None, Some(TaskStatus::Closed),
        ).await.unwrap();

        assert_eq!(t.status, "closed");
        assert!(t.closed_at.is_some());
        assert_eq!(t.close_reason.as_deref(), Some("force_closed"));
    }

    #[tokio::test]
    async fn requires_reason_enforced() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        // ForceClose requires a reason.
        let err = repo
            .transition(&task.id, TransitionAction::ForceClose, "", "user", None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTransition(_)));

        // With a reason it works.
        let t = repo
            .transition(&task.id, TransitionAction::ForceClose, "", "user", Some("testing"), None)
            .await
            .unwrap();
        assert_eq!(t.status, "closed");
    }

    #[tokio::test]
    async fn transition_writes_activity_log() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db, tx);

        let task = open_task(&repo, &epic.id).await;
        repo.transition(&task.id, TransitionAction::Start, "agent-1", "system", None, None)
            .await
            .unwrap();

        let entries = repo.list_activity(&task.id).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, "status_changed");
        assert_eq!(entries[0].actor_id, "agent-1");

        let payload: serde_json::Value = serde_json::from_str(&entries[0].payload).unwrap();
        assert_eq!(payload["from_status"], "open");
        assert_eq!(payload["to_status"], "in_progress");
    }
}

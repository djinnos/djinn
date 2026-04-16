use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Epic;

use crate::database::Database;
use crate::{Error, Result};

/// Inlined EPIC_COLS projection for each `query_as!(Epic, ...)` call site.
/// `query_as!` requires a string-literal SQL argument; concat!()-produced
/// literals don't satisfy it (verified during batch 4 on agent.rs).  Each
/// caller therefore passes the full SELECT body as a raw string literal.

// ── Query / result types ─────────────────────────────────────────────────────

/// Aggregate child-task counts for an epic.
pub struct EpicTaskCounts {
    pub task_count: i64,
    pub open_count: i64,
    pub in_progress_count: i64,
    pub closed_count: i64,
}

/// Filters and pagination for [`EpicRepository::list_filtered`].
pub struct EpicListQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub text: Option<String>,
    pub sort: String,
    pub limit: i64,
    pub offset: i64,
}

impl Default for EpicListQuery {
    fn default() -> Self {
        Self {
            status: None,
            project_id: None,
            text: None,
            sort: "created".to_owned(),
            limit: 25,
            offset: 0,
        }
    }
}

pub struct EpicListResult {
    pub epics: Vec<Epic>,
    pub total_count: i64,
}

/// Filters for [`EpicRepository::count_grouped`].
pub struct EpicCountQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub group_by: Option<String>,
}

#[derive(Clone, Debug)]
enum SqlParam {
    Text(String),
}

pub struct EpicCreateInput<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub emoji: &'a str,
    pub color: &'a str,
    pub owner: &'a str,
    pub memory_refs: Option<&'a str>,
    /// Epic status: "proposed", "drafting" (default), or "open".
    pub status: Option<&'a str>,
    /// ADR-051 Epic C — if `None`, defaults to `true` (existing behaviour).
    /// When `false`, the coordinator skips the epic_created breakdown
    /// auto-dispatch.
    pub auto_breakdown: Option<bool>,
    /// ADR-051 Epic C — slug of the accepted ADR that spawned this epic.
    pub originating_adr_id: Option<&'a str>,
}

pub type EpicUpdateInput<'a> = EpicCreateInput<'a>;

pub struct EpicRepository {
    db: Database,
    events: EventBus,
}

impl EpicRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub async fn list(&self) -> Result<Vec<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics ORDER BY created_at"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ?"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE short_id = ?"#,
            short_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn create(
        &self,
        title: &str,
        description: &str,
        emoji: &str,
        color: &str,
        owner: &str,
        memory_refs: Option<&str>,
    ) -> Result<Epic> {
        let project_id = self.ensure_default_project_id().await?;
        self.create_for_project(
            &project_id,
            EpicCreateInput {
                title,
                description,
                emoji,
                color,
                owner,
                memory_refs,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
            },
        )
        .await
    }

    pub async fn create_for_project(
        &self,
        project_id: &str,
        input: EpicCreateInput<'_>,
    ) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let status = input.status.unwrap_or("drafting");
        let auto_breakdown = i64::from(input.auto_breakdown.unwrap_or(true));
        let memory_refs = input.memory_refs.unwrap_or("[]");
        // Phase 3B: stamp `created_by_user_id` from the task-local set at
        // the MCP dispatch root. `None` when no user context is in scope.
        let created_by_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, `status`, owner, memory_refs, auto_breakdown, originating_adr_id, created_by_user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            project_id,
            short_id,
            input.title,
            input.description,
            input.emoji,
            input.color,
            status,
            input.owner,
            memory_refs,
            auto_breakdown,
            input.originating_adr_id,
            created_by_user_id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope::epic_created(&epic));
        Ok(epic)
    }

    pub async fn update(&self, id: &str, input: EpicUpdateInput<'_>) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let status = input.status.unwrap_or("drafting");
        let memory_refs = input.memory_refs.unwrap_or("[]");
        sqlx::query!(
            "UPDATE epics SET title = ?, description = ?, emoji = ?,
                    color = ?, `status` = ?, owner = ?, memory_refs = ?,
                    closed_at = CASE WHEN ? = 'closed' THEN COALESCE(closed_at, DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')) ELSE NULL END,
                    updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            input.title,
            input.description,
            input.emoji,
            input.color,
            status,
            input.owner,
            memory_refs,
            status,
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope::epic_updated(&epic));
        Ok(epic)
    }

    pub async fn close(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE epics SET `status` = 'closed',
                    closed_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
                    updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope::epic_updated(&epic));
        Ok(epic)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("DELETE FROM epics WHERE id = ?", id)
            .execute(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::epic_deleted(id));
        Ok(())
    }

    /// Replace the `memory_refs` JSON array on an epic.
    pub async fn update_memory_refs(&self, id: &str, memory_refs_json: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE epics SET memory_refs = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            memory_refs_json,
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope::epic_updated(&epic));
        Ok(epic)
    }

    // ── New methods (ADR-003) ────────────────────────────────────────────────

    /// Resolve an epic by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ? OR short_id = ?"#,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve an epic by UUID or short_id constrained to a project.
    pub async fn resolve_in_project(
        &self,
        project_id: &str,
        id_or_short: &str,
    ) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE project_id = ? AND (id = ? OR short_id = ?)"#,
            project_id,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Reopen a closed epic: set status=open, clear closed_at.
    pub async fn reopen(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("epic not found: {id}")))?;
        if current.status != "closed" {
            return Err(Error::InvalidTransition(format!(
                "epic must be closed to reopen (current: {})",
                current.status
            )));
        }
        sqlx::query!(
            "UPDATE epics SET `status` = 'open',
                    closed_at = NULL,
                    updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    `status` AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs, auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id
             FROM epics WHERE id = ?"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope::epic_updated(&epic));
        Ok(epic)
    }

    /// Aggregate child-task counts for an epic.
    pub async fn task_counts(&self, epic_id: &str) -> Result<EpicTaskCounts> {
        self.db.ensure_initialized().await?;
        // MySQL/Dolt returns SUM(...) as DECIMAL; casting to SIGNED keeps the
        // value decodeable as i64 via sqlx (otherwise large DECIMAL round-trip
        // blows up to sign-extended 2^62 on Dolt).
        let row = sqlx::query!(
            r#"SELECT
                COUNT(*) AS "task_count!: i64",
                CAST(COALESCE(SUM(CASE WHEN `status` = 'open' THEN 1 ELSE 0 END), 0) AS SIGNED) AS "open_count!: i64",
                CAST(COALESCE(SUM(CASE WHEN `status` = 'in_progress' THEN 1 ELSE 0 END), 0) AS SIGNED) AS "in_progress_count!: i64",
                CAST(COALESCE(SUM(CASE WHEN `status` = 'closed' THEN 1 ELSE 0 END), 0) AS SIGNED) AS "closed_count!: i64"
             FROM tasks WHERE epic_id = ?"#,
            epic_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(EpicTaskCounts {
            task_count: row.task_count,
            open_count: row.open_count,
            in_progress_count: row.in_progress_count,
            closed_count: row.closed_count,
        })
    }

    /// Count child tasks then CASCADE-delete the epic. Returns the child task count.
    pub async fn delete_with_count(&self, id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM tasks WHERE epic_id = ?",
            id
        )
        .fetch_one(self.db.pool())
        .await?;
        self.delete(id).await?;
        Ok(count)
    }

    /// List epics with optional filters, sorting, and pagination.
    pub async fn list_filtered(&self, query: EpicListQuery) -> Result<EpicListResult> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = epic_build_where(&query.project_id, &query.status, &query.text);
        let order_sql = epic_sort_to_sql(&query.sort);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM epics WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            total_q = total_q.bind(s.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        // NOTE: dynamic SQL (WHERE + ORDER clauses built from optional filters; uses inlined EPIC_COLS projection) — compile-time check not possible
        let sql = format!(
            "SELECT id, project_id, short_id, title, description, emoji, color, `status`, \
                    owner, created_at, updated_at, closed_at, memory_refs, \
                    auto_breakdown, originating_adr_id \
             FROM epics WHERE {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
        );
        let mut epic_q = sqlx::query_as::<_, Epic>(&sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            epic_q = epic_q.bind(s.clone());
        }
        let epics = epic_q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

        Ok(EpicListResult {
            epics,
            total_count: total,
        })
    }

    /// Count epics with optional group_by.
    pub async fn count_grouped(&self, query: EpicCountQuery) -> Result<serde_json::Value> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = epic_build_where(&query.project_id, &query.status, &None);

        match query.group_by.as_deref() {
            Some("status") => {
                // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
                let sql = format!(
                    "SELECT status, COUNT(*) FROM epics WHERE {where_sql}
                     GROUP BY status ORDER BY COUNT(*) DESC"
                );
                let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
                for p in &params {
                    let SqlParam::Text(s) = p;
                    q = q.bind(s.clone());
                }
                let groups = q
                    .fetch_all(self.db.pool())
                    .await?
                    .into_iter()
                    .map(|(key, count)| serde_json::json!({"key": key, "count": count}))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "groups": groups }))
            }
            Some(other) => Err(Error::InvalidData(format!("unknown group_by: {other}"))),
            None => {
                // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
                let sql = format!("SELECT COUNT(*) FROM epics WHERE {where_sql}");
                let mut q = sqlx::query_scalar::<_, i64>(&sql);
                for p in &params {
                    let SqlParam::Text(s) = p;
                    q = q.bind(s.clone());
                }
                let total = q.fetch_one(self.db.pool()).await?;
                Ok(serde_json::json!({ "total_count": total }))
            }
        }
    }

    /// Generate a unique 4-char base36 short ID for the epics table.
    async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::InvalidData(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), "epics", &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), "epics", &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::InvalidData(
            "short_id collision after 16 retries".into(),
        ))
    }

    async fn ensure_default_project_id(&self) -> Result<String> {
        self.db.ensure_initialized().await?;
        if let Some(id) =
            sqlx::query_scalar!("SELECT id FROM projects ORDER BY created_at LIMIT 1")
                .fetch_optional(self.db.pool())
                .await?
        {
            return Ok(id);
        }

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO projects (id, name, path, verification_rules) VALUES (?, ?, ?, ?)",
            id,
            "default",
            ".",
            "[]"
        )
        .execute(self.db.pool())
        .await?;
        Ok(id)
    }
}

// ── Short ID helpers ─────────────────────────────────────────────────────────

/// Derive a 4-char base36 short ID from the last 4 bytes of a UUIDv7.
fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616) // 36^4
}

/// Encode `n` (0..1_679_615) as a zero-padded 4-char base36 string.
fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

// ── Dynamic query helpers ────────────────────────────────────────────────────

fn epic_build_where(
    project_id: &Option<String>,
    status: &Option<String>,
    text: &Option<String>,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(p) = project_id {
        clauses.push("project_id = ?".to_owned());
        params.push(SqlParam::Text(p.clone()));
    }

    if let Some(s) = status {
        clauses.push("status = ?".to_owned());
        params.push(SqlParam::Text(s.clone()));
    }
    if let Some(t) = text {
        clauses.push("(title LIKE ? OR description LIKE ?)".to_owned());
        let pattern = format!("%{t}%");
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern));
    }

    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };
    (where_sql, params)
}

fn epic_sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        _ => "created_at ASC",
    }
}

async fn short_id_exists(pool: &sqlx::MySqlPool, table: &str, short_id: &str) -> Result<bool> {
    // NOTE: dynamic SQL (table name interpolated; values are internal constants only) — compile-time check not possible
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?)");
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?
        > 0)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Epic;

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
    async fn create_and_get_epic() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo
            .create("My Epic", "", "🚀", "#8b5cf6", "user@example.com", None)
            .await
            .unwrap();
        assert_eq!(epic.title, "My Epic");
        assert_eq!(epic.status, "drafting");
        assert_eq!(epic.short_id.len(), 4);

        let fetched = repo.get(&epic.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My Epic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn short_id_lookup() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Lookup", "", "", "", "", None).await.unwrap();
        let found = repo.get_by_short_id(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(found.id, epic.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        repo.create("Event Epic", "", "", "", "", None)
            .await
            .unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "created");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.title, "Event Epic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo.create("Old", "", "", "", "", None).await.unwrap();
        captured.lock().unwrap().clear();

        let updated = repo
            .update(
                &epic.id,
                EpicUpdateInput {
                    title: "New",
                    description: "desc",
                    emoji: "🎯",
                    color: "#fff",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "New");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.title, "New");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo
            .create("Closeable", "", "", "", "", None)
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.id, epic.id);
        assert_eq!(e.status, "closed");
        assert!(e.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo.create("Reopen", "", "", "", "", None).await.unwrap();
        repo.close(&epic.id).await.unwrap();
        captured.lock().unwrap().clear();

        let reopened = repo.reopen(&epic.id).await.unwrap();
        assert_eq!(reopened.status, "open");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.id, epic.id);
        assert_eq!(e.status, "open");
        assert!(e.closed_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo
            .create("Delete me", "", "", "", "", None)
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        repo.delete(&epic.id).await.unwrap();
        assert!(repo.get(&epic.id).await.unwrap().is_none());

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "deleted");
        assert_eq!(events[0].payload["id"].as_str().unwrap(), epic.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_by_id_and_short_id() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Resolve", "", "", "", "", None).await.unwrap();

        let by_id = repo.resolve(&epic.id).await.unwrap().unwrap();
        assert_eq!(by_id.id, epic.id);

        let by_short = repo.resolve(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(by_short.id, epic.id);

        assert!(repo.resolve("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_closed() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Reopen", "", "", "", "", None).await.unwrap();
        repo.close(&epic.id).await.unwrap();

        let reopened = repo.reopen(&epic.id).await.unwrap();
        assert_eq!(reopened.status, "open");
        assert!(reopened.closed_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_open_is_error() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Open", "", "", "", "", None).await.unwrap();
        assert!(repo.reopen(&epic.id).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_counts_aggregation() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());

        let epic = repo.create("Counts", "", "", "", "", None).await.unwrap();
        let pool = db.pool();

        // Insert tasks directly via SQL.
        for short in ["t001", "t002"] {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query!(
                "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                    issue_type, priority, owner, `status`, continuation_count, labels, acceptance_criteria, memory_refs)
                 VALUES (?, ?, ?, ?, 'T', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]')",
                id,
                epic.project_id,
                short,
                epic.id
            )
            .execute(pool)
            .await
            .unwrap();
        }
        let t3_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, `status`, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES (?, ?, 't003', ?, 'T3', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]')",
            t3_id,
            epic.project_id,
            epic.id
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query!("UPDATE tasks SET `status` = 'closed' WHERE id = ?", t3_id)
            .execute(pool)
            .await
            .unwrap();

        let counts = repo.task_counts(&epic.id).await.unwrap();
        assert_eq!(counts.task_count, 3);
        assert_eq!(counts.open_count, 2);
        assert_eq!(counts.closed_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_with_count_returns_child_count() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());

        let epic = repo.create("Delete", "", "", "", "", None).await.unwrap();
        let pool = db.pool();

        for short in ["t001", "t002"] {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query!(
                "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                    issue_type, priority, owner, `status`, continuation_count, labels, acceptance_criteria, memory_refs)
                 VALUES (?, ?, ?, ?, 'T', '', '', 'task', 0, '', 'open', 0, '[]', '[]', '[]')",
                id,
                epic.project_id,
                short,
                epic.id
            )
            .execute(pool)
            .await
            .unwrap();
        }

        let count = repo.delete_with_count(&epic.id).await.unwrap();
        assert_eq!(count, 2);
        assert!(repo.get(&epic.id).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_defaults_to_drafting() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let epic = repo
            .create("Draft Epic", "", "", "", "", None)
            .await
            .unwrap();
        assert_eq!(epic.status, "drafting");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_with_explicit_open_status() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let project_id = repo.ensure_default_project_id().await.unwrap();
        let epic = repo
            .create_for_project(
                &project_id,
                EpicCreateInput {
                    title: "Open Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_with_explicit_drafting_status() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let project_id = repo.ensure_default_project_id().await.unwrap();
        let epic = repo
            .create_for_project(
                &project_id,
                EpicCreateInput {
                    title: "Drafting Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(epic.status, "drafting");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_from_drafting() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let epic = repo.create("Draft", "", "", "", "", None).await.unwrap();
        assert_eq!(epic.status, "drafting");
        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_drafting_is_error() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let epic = repo.create("Draft", "", "", "", "", None).await.unwrap();
        assert_eq!(epic.status, "drafting");
        assert!(repo.reopen(&epic.id).await.is_err());
    }

    #[test]
    fn encode_base36_roundtrip() {
        assert_eq!(encode_base36(0), "0000");
        assert_eq!(encode_base36(1_679_615), "zzzz");
        for s in [encode_base36(12345), encode_base36(999999)] {
            assert_eq!(s.len(), 4);
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() && !c.is_uppercase())
            );
        }
    }
}

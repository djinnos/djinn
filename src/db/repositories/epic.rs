use sqlx::Row;
use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::Epic;

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

pub struct EpicRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl EpicRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    pub async fn list(&self) -> Result<Vec<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Epic>(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                        owner, created_at, updated_at, closed_at
                 FROM epics ORDER BY created_at",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Epic>(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                        owner, created_at, updated_at, closed_at
                 FROM epics WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Epic>(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                        owner, created_at, updated_at, closed_at
                 FROM epics WHERE short_id = ?1",
        )
        .bind(short_id)
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
    ) -> Result<Epic> {
        let project_id = self.ensure_default_project_id().await?;
        self.create_for_project(&project_id, title, description, emoji, color, owner)
            .await
    }

    pub async fn create_for_project(
        &self,
        project_id: &str,
        title: &str,
        description: &str,
        emoji: &str,
        color: &str,
        owner: &str,
    ) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(&short_id)
        .bind(title)
        .bind(description)
        .bind(emoji)
        .bind(color)
        .bind(owner)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicCreated(epic.clone()));
        Ok(epic)
    }

    pub async fn update(
        &self,
        id: &str,
        title: &str,
        description: &str,
        emoji: &str,
        color: &str,
        owner: &str,
    ) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epics SET title = ?2, description = ?3, emoji = ?4,
                    color = ?5, owner = ?6,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(emoji)
        .bind(color)
        .bind(owner)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicUpdated(epic.clone()));
        Ok(epic)
    }

    pub async fn close(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epics SET status = 'closed',
                    closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicUpdated(epic.clone()));
        Ok(epic)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM epics WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        let _ = self
            .events
            .send(DjinnEvent::EpicDeleted { id: id.to_owned() });
        Ok(())
    }

    // ── New methods (ADR-003) ────────────────────────────────────────────────

    /// Resolve an epic by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Epic>(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1 OR short_id = ?1",
        )
        .bind(id_or_short)
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
        Ok(sqlx::query_as::<_, Epic>(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE project_id = ?1 AND (id = ?2 OR short_id = ?2)",
        )
        .bind(project_id)
        .bind(id_or_short)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Reopen a closed epic: set status=open, clear closed_at.
    pub async fn reopen(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        // Verify current status is closed.
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::Internal(format!("epic not found: {id}")))?;
        if current.status != "closed" {
            return Err(Error::InvalidTransition(format!(
                "epic must be closed to reopen (current: {})",
                current.status
            )));
        }
        sqlx::query(
            "UPDATE epics SET status = 'open',
                    closed_at = NULL,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
             FROM epics WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::EpicUpdated(epic.clone()));
        Ok(epic)
    }

    /// Aggregate child-task counts for an epic.
    pub async fn task_counts(&self, epic_id: &str) -> Result<EpicTaskCounts> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT
                COUNT(*) AS task_count,
                SUM(CASE WHEN status IN ('backlog', 'open') THEN 1 ELSE 0 END) AS open_count,
                SUM(CASE WHEN status = 'in_progress' THEN 1 ELSE 0 END) AS in_progress_count,
                SUM(CASE WHEN status = 'closed' THEN 1 ELSE 0 END) AS closed_count
             FROM tasks WHERE epic_id = ?1",
        )
        .bind(epic_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(EpicTaskCounts {
            task_count: row.get::<i64, _>(0),
            open_count: row.get::<Option<i64>, _>(1).unwrap_or(0),
            in_progress_count: row.get::<Option<i64>, _>(2).unwrap_or(0),
            closed_count: row.get::<Option<i64>, _>(3).unwrap_or(0),
        })
    }

    /// Count child tasks then CASCADE-delete the epic. Returns the child task count.
    pub async fn delete_with_count(&self, id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE epic_id = ?1")
            .bind(id)
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

        let total_sql = format!("SELECT COUNT(*) FROM epics WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            total_q = total_q.bind(s.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let sql = format!(
            "SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at
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
            Some(other) => Err(Error::Internal(format!("unknown group_by: {other}"))),
            None => {
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
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::Internal(e.to_string()))?;
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
        Err(Error::Internal(
            "short_id collision after 16 retries".into(),
        ))
    }

    async fn ensure_default_project_id(&self) -> Result<String> {
        self.db.ensure_initialized().await?;
        if let Some(id) =
            sqlx::query_scalar::<_, String>("SELECT id FROM projects ORDER BY created_at LIMIT 1")
                .fetch_optional(self.db.pool())
                .await?
        {
            return Ok(id);
        }

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)")
            .bind(&id)
            .bind("default")
            .bind(".")
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

async fn short_id_exists(pool: &sqlx::SqlitePool, table: &str, short_id: &str) -> Result<bool> {
    // Table name is from internal code only — not user input — so this is safe.
    let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = ?1)");
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(short_id)
        .fetch_one(pool)
        .await?
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_epic() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo
            .create("My Epic", "", "🚀", "#8b5cf6", "user@example.com")
            .await
            .unwrap();
        assert_eq!(epic.title, "My Epic");
        assert_eq!(epic.status, "open");
        assert_eq!(epic.short_id.len(), 4);

        let fetched = repo.get(&epic.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My Epic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn short_id_lookup() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Lookup", "", "", "", "").await.unwrap();
        let found = repo.get_by_short_id(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(found.id, epic.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        repo.create("Event Epic", "", "", "", "").await.unwrap();
        match rx.recv().await.unwrap() {
            DjinnEvent::EpicCreated(e) => assert_eq!(e.title, "Event Epic"),
            _ => panic!("expected EpicCreated"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Old", "", "", "", "").await.unwrap();
        let _ = rx.recv().await.unwrap();

        let updated = repo
            .update(&epic.id, "New", "desc", "🎯", "#fff", "")
            .await
            .unwrap();
        assert_eq!(updated.title, "New");

        match rx.recv().await.unwrap() {
            DjinnEvent::EpicUpdated(e) => assert_eq!(e.title, "New"),
            _ => panic!("expected EpicUpdated"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Closeable", "", "", "", "").await.unwrap();
        let _ = rx.recv().await.unwrap();
        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        match rx.recv().await.unwrap() {
            DjinnEvent::EpicUpdated(e) => {
                assert_eq!(e.id, epic.id);
                assert_eq!(e.status, "closed");
                assert!(e.closed_at.is_some());
            }
            _ => panic!("expected EpicUpdated"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Reopen", "", "", "", "").await.unwrap();
        let _ = rx.recv().await.unwrap();
        repo.close(&epic.id).await.unwrap();
        let _ = rx.recv().await.unwrap();

        let reopened = repo.reopen(&epic.id).await.unwrap();
        assert_eq!(reopened.status, "open");

        match rx.recv().await.unwrap() {
            DjinnEvent::EpicUpdated(e) => {
                assert_eq!(e.id, epic.id);
                assert_eq!(e.status, "open");
                assert!(e.closed_at.is_none());
            }
            _ => panic!("expected EpicUpdated"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Delete me", "", "", "", "").await.unwrap();
        let _ = rx.recv().await.unwrap();

        repo.delete(&epic.id).await.unwrap();
        assert!(repo.get(&epic.id).await.unwrap().is_none());

        match rx.recv().await.unwrap() {
            DjinnEvent::EpicDeleted { id } => assert_eq!(id, epic.id),
            _ => panic!("expected EpicDeleted"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_by_id_and_short_id() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Resolve", "", "", "", "").await.unwrap();

        let by_id = repo.resolve(&epic.id).await.unwrap().unwrap();
        assert_eq!(by_id.id, epic.id);

        let by_short = repo.resolve(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(by_short.id, epic.id);

        assert!(repo.resolve("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_closed() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Reopen", "", "", "", "").await.unwrap();
        repo.close(&epic.id).await.unwrap();

        let reopened = repo.reopen(&epic.id).await.unwrap();
        assert_eq!(reopened.status, "open");
        assert!(reopened.closed_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_open_is_error() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let repo = EpicRepository::new(db, tx);

        let epic = repo.create("Open", "", "", "", "").await.unwrap();
        assert!(repo.reopen(&epic.id).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_counts_aggregation() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let task_repo = crate::db::TaskRepository::new(db, tx);

        let epic = epic_repo.create("Counts", "", "", "", "").await.unwrap();
        task_repo
            .create(&epic.id, "T1", "", "", "task", 0, "", None)
            .await
            .unwrap();
        task_repo
            .create(&epic.id, "T2", "", "", "task", 0, "", None)
            .await
            .unwrap();
        let t3 = task_repo
            .create(&epic.id, "T3", "", "", "task", 0, "", None)
            .await
            .unwrap();
        task_repo.set_status(&t3.id, "closed").await.unwrap();

        let counts = epic_repo.task_counts(&epic.id).await.unwrap();
        assert_eq!(counts.task_count, 3);
        assert_eq!(counts.open_count, 2);
        assert_eq!(counts.closed_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_with_count_returns_child_count() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let task_repo = crate::db::TaskRepository::new(db, tx);

        let epic = epic_repo.create("Delete", "", "", "", "").await.unwrap();
        task_repo
            .create(&epic.id, "T1", "", "", "task", 0, "", None)
            .await
            .unwrap();
        task_repo
            .create(&epic.id, "T2", "", "", "task", 0, "", None)
            .await
            .unwrap();

        let count = epic_repo.delete_with_count(&epic.id).await.unwrap();
        assert_eq!(count, 2);
        assert!(epic_repo.get(&epic.id).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn encode_base36_roundtrip() {
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

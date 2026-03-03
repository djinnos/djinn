use tokio::sync::broadcast;

use crate::db::connection::{Database, OptionalExt};
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::task::{ActivityEntry, Task};

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
                            reopen_count, continuation_count, created_at, updated_at, closed_at
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
                            reopen_count, continuation_count, created_at, updated_at, closed_at
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
                                reopen_count, continuation_count, created_at, updated_at, closed_at
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

    /// Transition a task to a new status.
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
            reopen_count, continuation_count, created_at, updated_at, closed_at
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
    use crate::test_helpers;

    async fn make_epic(db: &Database, tx: broadcast::Sender<DjinnEvent>) -> crate::models::epic::Epic {
        EpicRepository::new(db.clone(), tx)
            .create("Test Epic", "", "", "", "")
            .await
            .unwrap()
    }

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
}

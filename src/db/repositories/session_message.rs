use tokio::sync::broadcast;

use crate::agent::message::{Conversation, Message, Role};
use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::session_message::SessionMessage;

pub struct SessionMessageRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl SessionMessageRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    /// Insert a single message into the conversation.
    pub async fn insert_message(
        &self,
        session_id: &str,
        task_id: &str,
        role: &str,
        content_json: &str,
        token_count: Option<i64>,
    ) -> Result<SessionMessage> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query(
            "INSERT INTO session_messages (id, session_id, role, content_json, token_count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&id)
        .bind(session_id)
        .bind(role)
        .bind(content_json)
        .bind(token_count)
        .execute(self.db.pool())
        .await?;

        let msg = sqlx::query_as::<_, SessionMessage>(
            "SELECT id, session_id, role, content_json, token_count, created_at
             FROM session_messages WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self.events.send(DjinnEvent::SessionMessageInserted {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            role: role.to_owned(),
        });

        Ok(msg)
    }

    /// Bulk insert messages (e.g. after compaction or session restore).
    pub async fn insert_messages_batch(
        &self,
        session_id: &str,
        task_id: &str,
        messages: &[Message],
    ) -> Result<()> {
        self.db.ensure_initialized().await?;

        for msg in messages {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content_json = serde_json::to_string(&msg.content)
                .unwrap_or_else(|_| "[]".to_string());
            let id = uuid::Uuid::now_v7().to_string();

            sqlx::query(
                "INSERT INTO session_messages (id, session_id, role, content_json)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(&id)
            .bind(session_id)
            .bind(role)
            .bind(&content_json)
            .execute(self.db.pool())
            .await?;

            let _ = self.events.send(DjinnEvent::SessionMessageInserted {
                session_id: session_id.to_owned(),
                task_id: task_id.to_owned(),
                role: role.to_owned(),
            });
        }

        Ok(())
    }

    /// Load full conversation ordered by created_at.
    pub async fn load_conversation(&self, session_id: &str) -> Result<Conversation> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query_as::<_, SessionMessage>(
            "SELECT id, session_id, role, content_json, token_count, created_at
             FROM session_messages
             WHERE session_id = ?1
             ORDER BY created_at ASC",
        )
        .bind(session_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut conv = Conversation::default();
        for row in rows {
            let role = match row.role.as_str() {
                "system" => Role::System,
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            let content = serde_json::from_str(&row.content_json).unwrap_or_default();
            conv.push(Message {
                role,
                content,
                metadata: None,
            });
        }

        Ok(conv)
    }

    /// Load messages for multiple sessions at once, returning (session_id, role, content_json, created_at) tuples.
    pub async fn load_for_sessions(
        &self,
        session_ids: &[String],
    ) -> Result<Vec<(String, String, String, String)>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.db.ensure_initialized().await?;

        // Build placeholders: (?1, ?2, ?3, ...)
        let placeholders: Vec<String> = (1..=session_ids.len())
            .map(|i| format!("?{i}"))
            .collect();
        let sql = format!(
            "SELECT session_id, role, content_json, created_at \
             FROM session_messages \
             WHERE session_id IN ({}) \
             ORDER BY created_at ASC",
            placeholders.join(", ")
        );

        let mut query = sqlx::query_as::<_, (String, String, String, String)>(&sql);
        for id in session_ids {
            query = query.bind(id);
        }

        Ok(query.fetch_all(self.db.pool()).await?)
    }

    /// Delete all messages for a session (used by compaction to replace with summary).
    pub async fn delete_conversation(&self, session_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query("DELETE FROM session_messages WHERE session_id = ?1")
            .bind(session_id)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::{Message, Role};
    use crate::db::connection::Database;
    use crate::db::repositories::epic::EpicRepository;
    use crate::db::repositories::session::SessionRepository;
    use crate::db::repositories::task::TaskRepository;
    use crate::events::DjinnEvent;
    use tokio::sync::broadcast;

    async fn make_test_db() -> (Database, broadcast::Sender<DjinnEvent>) {
        let db = Database::open_in_memory().expect("in-memory db");
        db.ensure_initialized().await.expect("migrate");
        let (tx, _rx) = broadcast::channel(64);
        (db, tx)
    }

    async fn create_session(
        db: Database,
        tx: broadcast::Sender<DjinnEvent>,
    ) -> (String, String, String) {
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("Epic", "", "", "", "").await.unwrap();

        let task_repo = TaskRepository::new(db.clone(), tx.clone());
        let task = task_repo
            .create(&epic.id, "Task", "", "", "task", 0 , "", Some("open"))
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db, tx);
        let session = session_repo
            .create(&task.project_id, Some(&task.id), "test-model", "worker", None, None)
            .await
            .unwrap();

        (task.project_id, task.id, session.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_insert_and_load() {
        let (db, tx) = make_test_db().await;
        let (_project_id, task_id, session_id) = create_session(db.clone(), tx.clone()).await;

        let repo = SessionMessageRepository::new(db, tx);

        let messages = vec![
            Message::system("You are a helpful assistant."),
            Message::user("Hello!"),
            Message::assistant("Hi there!"),
        ];

        repo.insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .expect("batch insert");

        let conv = repo
            .load_conversation(&session_id)
            .await
            .expect("load conversation");

        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[2].role, Role::Assistant);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_message_emits_event() {
        let (db, tx) = make_test_db().await;
        let (_project_id, task_id, session_id) = create_session(db.clone(), tx.clone()).await;
        let mut rx = tx.subscribe();

        let repo = SessionMessageRepository::new(db, tx);

        repo.insert_message(&session_id, &task_id, "user", r#"[{"type":"text","text":"hi"}]"#, None)
            .await
            .expect("insert");

        // Drain events looking for the SessionMessageInserted event.
        let mut found = false;
        for _ in 0..16 {
            match rx.try_recv() {
                Ok(DjinnEvent::SessionMessageInserted { role, .. }) => {
                    assert_eq!(role, "user");
                    found = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(found, "expected SessionMessageInserted event");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_conversation_removes_messages() {
        let (db, tx) = make_test_db().await;
        let (_project_id, task_id, session_id) = create_session(db.clone(), tx.clone()).await;

        let repo = SessionMessageRepository::new(db, tx);

        let messages = vec![Message::user("one"), Message::assistant("two")];
        repo.insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .unwrap();

        let deleted = repo.delete_conversation(&session_id).await.unwrap();
        assert_eq!(deleted, 2);

        let conv = repo.load_conversation(&session_id).await.unwrap();
        assert!(conv.is_empty());
    }
}

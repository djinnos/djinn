use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::message::{Conversation, Message, Role};
use djinn_core::models::SessionMessage;

use crate::Result;
use crate::database::Database;

pub struct SessionMessageRepository {
    db: Database,
    events: EventBus,
}

impl SessionMessageRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
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

        self.events.send(DjinnEventEnvelope {
            entity_type: "session_message",
            action: "inserted",
            payload: serde_json::json!({
                "session_id": session_id,
                "task_id": task_id,
                "role": role,
            }),
            id: None,
            project_id: None,
            from_sync: false,
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
            let content_json =
                serde_json::to_string(&msg.content).unwrap_or_else(|_| "[]".to_string());
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

            self.events.send(DjinnEventEnvelope {
                entity_type: "session_message",
                action: "inserted",
                payload: serde_json::json!({
                    "session_id": session_id,
                    "task_id": task_id,
                    "role": role,
                }),
                id: None,
                project_id: None,
                from_sync: false,
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
        let placeholders: Vec<String> = (1..=session_ids.len()).map(|i| format!("?{i}")).collect();
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
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::message::{Message, Role};

    use super::*;
    use crate::repositories::epic::EpicRepository;
    use crate::repositories::session::{CreateSessionParams, SessionRepository};

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

    async fn create_session(db: Database, bus: EventBus) -> (String, String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, memory_refs)
             VALUES (?1, ?2, ?3, ?4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]')",
        )
        .bind(&task_id)
        .bind(&epic.project_id)
        .bind(&short_id)
        .bind(&epic.id)
        .execute(db.pool())
        .await
        .unwrap();

        let session_repo = SessionRepository::new(db, bus);
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &epic.project_id,
                task_id: Some(&task_id),
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        (epic.project_id, task_id, session.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_insert_and_load() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionMessageRepository::new(db, EventBus::noop());

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
        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (_project_id, task_id, session_id) = create_session(db.clone(), bus.clone()).await;

        let repo = SessionMessageRepository::new(db, bus);

        captured.lock().unwrap().clear();

        repo.insert_message(
            &session_id,
            &task_id,
            "user",
            r#"[{"type":"text","text":"hi"}]"#,
            None,
        )
        .await
        .expect("insert");

        let events = captured.lock().unwrap();
        let found = events
            .iter()
            .find(|e| e.entity_type == "session_message" && e.action == "inserted");
        assert!(found.is_some(), "expected session_message.inserted event");
        assert_eq!(found.unwrap().payload["role"].as_str().unwrap(), "user");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_conversation_removes_messages() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionMessageRepository::new(db, EventBus::noop());

        let messages = vec![Message::user("one"), Message::assistant("two")];
        repo.insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .unwrap();

        let deleted = repo.delete_conversation(&session_id).await.unwrap();
        assert_eq!(deleted, 2);

        let conv = repo.load_conversation(&session_id).await.unwrap();
        assert!(conv.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_for_sessions_keeps_messages_scoped_and_ordered_by_session() {
        let db = test_db();
        let (_project_id, task_id, first_session_id) =
            create_session(db.clone(), EventBus::noop()).await;
        let (_project_id, _other_task_id, second_session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionMessageRepository::new(db, EventBus::noop());

        repo.insert_message(
            &first_session_id,
            &task_id,
            "user",
            r#"[{"type":"text","text":"first-user"}]"#,
            Some(11),
        )
        .await
        .unwrap();
        repo.insert_message(
            &first_session_id,
            &task_id,
            "assistant",
            r#"[{"type":"text","text":"first-assistant"}]"#,
            Some(13),
        )
        .await
        .unwrap();
        repo.insert_message(
            &second_session_id,
            &task_id,
            "user",
            r#"[{"type":"text","text":"second-user"}]"#,
            Some(17),
        )
        .await
        .unwrap();

        let rows = repo
            .load_for_sessions(&[first_session_id.clone(), second_session_id.clone()])
            .await
            .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, first_session_id);
        assert_eq!(rows[0].1, "user");
        assert!(rows[0].2.contains("first-user"));
        assert_eq!(rows[1].0, first_session_id);
        assert_eq!(rows[1].1, "assistant");
        assert!(rows[1].2.contains("first-assistant"));
        assert_eq!(rows[2].0, second_session_id);
        assert_eq!(rows[2].1, "user");
        assert!(rows[2].2.contains("second-user"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_insert_persists_token_count_and_round_trips_content() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionMessageRepository::new(db, EventBus::noop());

        let inserted = repo
            .insert_message(
                &session_id,
                &task_id,
                "assistant",
                r#"[{"type":"text","text":"persist me"}]"#,
                Some(42),
            )
            .await
            .unwrap();

        assert_eq!(inserted.session_id, session_id);
        assert_eq!(inserted.role, "assistant");
        assert_eq!(inserted.token_count, Some(42));
        assert!(inserted.content_json.contains("persist me"));

        let conv = repo.load_conversation(&session_id).await.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, Role::Assistant);
        let content_json = serde_json::to_string(&conv.messages[0].content).unwrap();
        assert!(content_json.contains("persist me"));
    }
}

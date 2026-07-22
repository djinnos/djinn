use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::message::{Conversation, Message, Role};
use djinn_core::models::SessionMessage;

use crate::Result;
use crate::database::Database;
use crate::repositories::session_compaction_boundary::{
    CompactionPhase, SessionCompactionBoundaryRepository,
};

pub struct SessionMessageRepository {
    db: Database,
    events: EventBus,
}

/// Model-visible boundary for a projected compaction.
///
/// Carries the compacted summary/marker metadata and the identity of the
/// first raw message that should be retained after the compaction. `load_conversation`
/// builds this struct from the latest completed `CompactionBoundary` and uses it
/// to splice the compacted head onto the retained raw tail.
#[derive(Debug, Clone)]
struct ProjectedCompaction {
    #[allow(dead_code)]
    boundary_id: String,
    summary_text: String,
    marker_metadata: Option<serde_json::Value>,
    #[allow(dead_code)]
    last_compacted_message_id: Option<String>,
    first_retained_message_id: Option<String>,
    retained_tail_hash: Option<String>,
}

impl SessionMessageRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    fn boundary_repo(&self) -> SessionCompactionBoundaryRepository {
        SessionCompactionBoundaryRepository::new(self.db.clone())
    }

    /// Find the latest completed compaction boundary for a session and build a
    /// projection from it. Returns `None` when no completed boundary exists.
    ///
    /// `Started` rows are intentionally ignored here; their presence is treated
    /// as if no boundary existed.
    async fn latest_projected_compaction(
        &self,
        session_id: &str,
    ) -> Result<Option<ProjectedCompaction>> {
        let boundary = self
            .boundary_repo()
            .latest_completed_boundary(session_id)
            .await?;
        let Some(boundary) = boundary else {
            return Ok(None);
        };

        // Defensive: `latest_completed_boundary` filters by phase = 'ended', but
        // double-check before trusting the summary/tail fields.
        if boundary.phase != CompactionPhase::Ended {
            return Ok(None);
        }

        let summary_text = boundary.summary_text.clone().unwrap_or_default();

        Ok(Some(ProjectedCompaction {
            boundary_id: boundary.id,
            summary_text,
            marker_metadata: boundary.marker_metadata.clone(),
            last_compacted_message_id: boundary.last_compacted_message_id.clone(),
            first_retained_message_id: boundary.first_retained_message_id.clone(),
            retained_tail_hash: boundary.retained_tail_hash.clone(),
        }))
    }

    /// Build a compacted summary message from a projected boundary.
    ///
    /// The summary is emitted as a `system` message so it appears before the
    /// retained tail without altering the original `session_messages` rows. The
    /// optional `marker_metadata` is attached as `MessageMeta.provider_data` so
    /// downstream callers (SSE, provider serialization) can inspect marker
    /// properties if needed, while normal messages are left untouched.
    fn summary_message(summary_text: &str, marker_metadata: &Option<serde_json::Value>) -> Message {
        let metadata = marker_metadata
            .as_ref()
            .map(|value| djinn_core::message::MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(value.clone()),
            });
        Message {
            role: Role::System,
            content: vec![djinn_core::message::ContentBlock::text(
                summary_text.to_owned(),
            )],
            metadata,
        }
    }

    /// Load all raw `session_messages` for a session into a `Conversation`.
    ///
    /// This is the pre-compaction fallback path and is also used to validate the
    /// retained tail before applying a projected boundary.
    async fn load_raw_conversation_internal(&self, session_id: &str) -> Result<Conversation> {
        let rows = sqlx::query_as!(
            SessionMessage,
            r#"SELECT id, session_id, role, content_json::text AS "content_json!", token_count, created_at
             FROM session_messages
             WHERE session_id = $1
             ORDER BY created_at ASC, id ASC"#,
            session_id,
        )
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

    /// Compute a stable hash of the retained tail for integrity checking.
    ///
    /// This is intentionally a simple concatenation hash based on raw message
    /// identity and content so the boundary record can detect drift between the
    /// persisted `first_retained_message_id` and the actual messages that now
    /// exist in the session. It must match the hash produced when the boundary
    /// was completed (callers are expected to use the same algorithm).
    fn retained_tail_hash(tail: &Conversation) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        for msg in &tail.messages {
            // Hash the role name as a stable string proxy.
            let role_str = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            role_str.hash(&mut hasher);
            // Hash the serialized content so text-equivalent messages produce
            // the same hash regardless of allocation.
            let content_json = serde_json::to_string(&msg.content).unwrap_or_default();
            content_json.hash(&mut hasher);
        }
        format!("djinn:default:{}", hasher.finish())
    }

    /// Return the raw tail that starts at `first_retained_message_id`, or the
    /// full raw conversation if the retained message cannot be found.
    ///
    /// Fail-safe: if the boundary's retained-tail identity does not match the
    /// available raw rows, we fall back to the full raw history rather than
    /// applying a suspect boundary. This preserves the conversation even when
    /// the boundary record is stale or the tail has been trimmed.
    async fn load_retained_tail(
        &self,
        session_id: &str,
        first_retained_message_id: Option<&str>,
        expected_hash: Option<&str>,
    ) -> Result<Conversation> {
        let raw = self.load_raw_conversation_internal(session_id).await?;
        let Some(start_id) = first_retained_message_id else {
            // No retained-tail identity recorded: fail-safe to raw history.
            return Ok(raw);
        };

        let rows = sqlx::query_as!(
            SessionMessage,
            r#"SELECT id, session_id, role, content_json::text AS "content_json!", token_count, created_at
             FROM session_messages
             WHERE session_id = $1
             ORDER BY created_at ASC, id ASC"#,
            session_id,
        )
        .fetch_all(self.db.pool())
        .await?;

        let start_idx = rows.iter().position(|r| r.id == start_id);
        let Some(start_idx) = start_idx else {
            // The retained message no longer exists in raw history. Fail-safe
            // to the full raw conversation rather than dropping messages.
            return Ok(raw);
        };

        let mut tail = Conversation::default();
        for row in rows.iter().skip(start_idx) {
            let role = match row.role.as_str() {
                "system" => Role::System,
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            let content = serde_json::from_str(&row.content_json).unwrap_or_default();
            tail.push(Message {
                role,
                content,
                metadata: None,
            });
        }

        if let Some(expected) = expected_hash {
            let actual = Self::retained_tail_hash(&tail);
            if actual != expected {
                // Tail drift detected: the raw messages after the boundary do not
                // match what the compaction package retained. Fail-safe to full raw history.
                return Ok(raw);
            }
        }

        Ok(tail)
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

        let content_value: serde_json::Value = serde_json::from_str(content_json).map_err(|e| {
            crate::Error::InvalidData(format!(
                "invalid json for session_messages.content_json: {e}"
            ))
        })?;
        sqlx::query!(
            "INSERT INTO session_messages (id, session_id, role, content_json, token_count)
             VALUES ($1, $2, $3, $4, $5)",
            id,
            session_id,
            role,
            content_value,
            token_count,
        )
        .execute(self.db.pool())
        .await?;

        let msg = sqlx::query_as!(
            SessionMessage,
            r#"SELECT id, session_id, role, content_json::text AS "content_json!", token_count, created_at
             FROM session_messages WHERE id = $1"#,
            id,
        )
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
            let content_value = serde_json::to_value(&msg.content)
                .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
            let id = uuid::Uuid::now_v7().to_string();

            sqlx::query!(
                "INSERT INTO session_messages (id, session_id, role, content_json)
                 VALUES ($1, $2, $3, $4)",
                id,
                session_id,
                role,
                content_value,
            )
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

    /// Load full conversation ordered by created_at, applying any completed
    /// compaction boundary projection.
    ///
    /// If no completed boundary exists, the original raw `session_messages` are
    /// returned unchanged. If a completed boundary exists, its summary is
    /// prepended as a system message and the retained raw tail follows. `Started`
    /// rows are ignored; a later incomplete `Started` after a completed boundary
    /// does not revert the projection.
    pub async fn load_conversation(&self, session_id: &str) -> Result<Conversation> {
        let projected = self.latest_projected_compaction(session_id).await?;
        let Some(projected) = projected else {
            // No completed boundary: preserve the original raw-history behavior.
            return self.load_raw_conversation_internal(session_id).await;
        };

        let tail = self
            .load_retained_tail(
                session_id,
                projected.first_retained_message_id.as_deref(),
                projected.retained_tail_hash.as_deref(),
            )
            .await?;

        let summary = Self::summary_message(&projected.summary_text, &projected.marker_metadata);

        // If the tail is the full raw conversation (because the retained-tail
        // identity did not match or was missing), we still prepend the compacted
        // summary. This is intentional: the completed boundary is the durable
        // contract, and the summary remains model-visible even when the raw tail
        // could not be exactly reconstructed. Callers that need raw history can
        // use `load_raw_conversation` directly.
        let mut conv = Conversation::default();
        conv.push(summary);
        for msg in tail.messages {
            conv.push(msg);
        }

        Ok(conv)
    }

    /// Load the raw conversation without applying any compaction projection.
    ///
    /// This is exposed for callers that need the unmodified `session_messages`
    /// history (e.g. compaction entry, audit, or diagnostics) and is also used
    /// internally as the no-boundary fallback.
    pub async fn load_raw_conversation(&self, session_id: &str) -> Result<Conversation> {
        self.load_raw_conversation_internal(session_id).await
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

        // NOTE: dynamic SQL — IN-clause placeholder count is runtime-dependent;
        // compile-time check not possible with variadic bindings. Postgres
        // positional placeholders ($1, $2, …), NOT MySQL `?` — the latter is
        // a syntax error inside `IN (...)` and 500'd the whole chat-sessions
        // list (`syntax error at or near ","`), leaving the chat tab empty.
        let placeholders: Vec<String> = (1..=session_ids.len()).map(|n| format!("${n}")).collect();
        // content_json is JSONB; cast to text so it decodes into the
        // String tuple slot (sqlx won't coerce JSONB→String otherwise).
        let sql = format!(
            "SELECT session_id, role, content_json::text, created_at \
             FROM session_messages \
             WHERE session_id IN ({}) \
             ORDER BY created_at ASC, id ASC",
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
        let result = sqlx::query!(
            "DELETE FROM session_messages WHERE session_id = $1",
            session_id,
        )
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
    use crate::repositories::session_compaction_boundary::{
        BeginCompactionParams, CompleteCompactionParams, SessionCompactionBoundaryRepository,
    };

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
        let creator = crate::repositories::test_support::seed_test_user(&db).await;
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
            task_id,
            epic.project_id,
            short_id,
            epic.id,
            creator,
        )
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
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
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

    // ── Compaction projection tests ───────────────────────────────────────

    /// Verifies that `load_conversation` returns raw history unchanged when no
    /// boundary records exist for the session. This is the compatibility
    /// contract: sessions that never used compaction projection must continue
    /// to behave exactly as they did before boundaries were introduced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_conversation_returns_raw_history_without_boundaries() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        // Confirm no boundary records exist.
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);
        let boundary = boundary_repo
            .latest_completed_boundary(&session_id)
            .await
            .unwrap();
        assert!(
            boundary.is_none(),
            "fresh session must have no boundary rows"
        );

        let messages = vec![
            Message::system("You are a helpful assistant."),
            Message::user("What is Rust?"),
            Message::assistant("Rust is a systems programming language."),
        ];
        msg_repo
            .insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .unwrap();

        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        // Must return exactly the raw messages in insertion order, unmodified.
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(
            conv.messages[0].text_content(),
            "You are a helpful assistant."
        );
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[1].text_content(), "What is Rust?");
        assert_eq!(conv.messages[2].role, Role::Assistant);
        assert_eq!(
            conv.messages[2].text_content(),
            "Rust is a systems programming language."
        );
        // No metadata should have been injected by projection.
        for msg in &conv.messages {
            assert!(
                msg.metadata.is_none(),
                "no metadata expected for raw history"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_conversation_ignores_started_only_boundaries() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        let messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
        ];
        msg_repo
            .insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .unwrap();

        // An incomplete (started-only) boundary must not change the projection.
        boundary_repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: None,
                last_compacted_message_id: None,
                first_retained_message_id: None,
                retained_tail_hash: None,
                marker_metadata: None,
            })
            .await
            .unwrap();

        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[2].role, Role::Assistant);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_conversation_projects_completed_boundary() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        let raw = vec![
            Message::system("sys"),
            Message::user("old user"),
            Message::assistant("old assistant"),
            Message::user("new user"),
        ];
        msg_repo
            .insert_messages_batch(&session_id, &task_id, &raw)
            .await
            .unwrap();

        // Capture the raw IDs so we can point the boundary at the retained tail.
        let raw_messages: Vec<SessionMessage> = sqlx::query_as!(
            SessionMessage,
            r#"SELECT id, session_id, role, content_json::text AS "content_json!", token_count, created_at
             FROM session_messages
             WHERE session_id = $1
             ORDER BY created_at ASC, id ASC"#,
            session_id,
        )
        .fetch_all(msg_repo.db.pool())
        .await
        .unwrap();
        assert_eq!(raw_messages.len(), 4);
        let first_retained_id = raw_messages[3].id.clone();

        let started = boundary_repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some(&raw_messages[1].id),
                last_compacted_message_id: Some(&raw_messages[2].id),
                first_retained_message_id: Some(&first_retained_id),
                retained_tail_hash: None,
                marker_metadata: Some(&serde_json::json!({
                    "marker_kind": "compaction_boundary",
                    "token_count": 42,
                })),
            })
            .await
            .unwrap();

        boundary_repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: &started.id,
                schema_version: 1,
                current_context_tokens_after: None,
                first_message_id: Some(&raw_messages[1].id),
                last_compacted_message_id: Some(&raw_messages[2].id),
                first_retained_message_id: Some(&first_retained_id),
                retained_tail_hash: None,
                summary_text: "Compacted summary of earlier turns.",
                marker_metadata: Some(&serde_json::json!({
                    "marker_kind": "compaction_boundary",
                    "token_count": 42,
                })),
            })
            .await
            .unwrap();

        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(
            conv.messages[0].text_content(),
            "Compacted summary of earlier turns."
        );
        assert!(
            conv.messages[0]
                .metadata
                .as_ref()
                .unwrap()
                .provider_data
                .as_ref()
                .unwrap()["marker_kind"]
                .as_str()
                .unwrap()
                .contains("compaction_boundary")
        );
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[1].text_content(), "new user");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_conversation_retains_prior_completed_boundary_after_later_started() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        let raw = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
        ];
        msg_repo
            .insert_messages_batch(&session_id, &task_id, &raw)
            .await
            .unwrap();

        let raw_messages: Vec<SessionMessage> = sqlx::query_as!(
            SessionMessage,
            r#"SELECT id, session_id, role, content_json::text AS "content_json!", token_count, created_at
             FROM session_messages
             WHERE session_id = $1
             ORDER BY created_at ASC, id ASC"#,
            session_id,
        )
        .fetch_all(msg_repo.db.pool())
        .await
        .unwrap();
        assert_eq!(raw_messages.len(), 5);

        // First compaction completes successfully, retaining u2/a2.
        let first = boundary_repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some(&raw_messages[1].id),
                last_compacted_message_id: Some(&raw_messages[2].id),
                first_retained_message_id: Some(&raw_messages[3].id),
                retained_tail_hash: None,
                marker_metadata: None,
            })
            .await
            .unwrap();
        boundary_repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: &first.id,
                schema_version: 1,
                current_context_tokens_after: None,
                first_message_id: Some(&raw_messages[1].id),
                last_compacted_message_id: Some(&raw_messages[2].id),
                first_retained_message_id: Some(&raw_messages[3].id),
                retained_tail_hash: None,
                summary_text: "First completed summary.",
                marker_metadata: None,
            })
            .await
            .unwrap();

        // A later compaction is started but never completed. The projection must
        // keep using the earlier completed boundary, not revert to raw history.
        boundary_repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some(&raw_messages[3].id),
                last_compacted_message_id: Some(&raw_messages[3].id),
                first_retained_message_id: Some(&raw_messages[4].id),
                retained_tail_hash: None,
                marker_metadata: None,
            })
            .await
            .unwrap();

        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].text_content(), "First completed summary.");
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[1].text_content(), "u2");
        assert_eq!(conv.messages[2].role, Role::Assistant);
        assert_eq!(conv.messages[2].text_content(), "a2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_conversation_falls_back_to_raw_when_tail_identity_does_not_match() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        let raw = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
        ];
        msg_repo
            .insert_messages_batch(&session_id, &task_id, &raw)
            .await
            .unwrap();

        // Complete a boundary that points at a non-existent retained message
        // with a mismatched hash. Projection should fail-safe to raw history.
        let started = boundary_repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: None,
                last_compacted_message_id: None,
                first_retained_message_id: Some("nonexistent-message-id"),
                retained_tail_hash: Some("djinn:default:12345"),
                marker_metadata: None,
            })
            .await
            .unwrap();
        boundary_repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: &started.id,
                schema_version: 1,
                current_context_tokens_after: None,
                first_message_id: None,
                last_compacted_message_id: None,
                first_retained_message_id: Some("nonexistent-message-id"),
                retained_tail_hash: Some("djinn:default:12345"),
                summary_text: "Should not drop messages.",
                marker_metadata: None,
            })
            .await
            .unwrap();

        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        // Summary is prepended, then the full raw tail (because the retained
        // identity did not match) is appended.
        assert_eq!(conv.messages.len(), 5);
        assert_eq!(conv.messages[0].text_content(), "Should not drop messages.");
        assert_eq!(conv.messages[1].role, Role::System);
        assert_eq!(conv.messages[1].text_content(), "sys");
        assert_eq!(conv.messages[2].role, Role::User);
        assert_eq!(conv.messages[2].text_content(), "u1");
        assert_eq!(conv.messages[3].role, Role::Assistant);
        assert_eq!(conv.messages[3].text_content(), "a1");
        assert_eq!(conv.messages[4].role, Role::User);
        assert_eq!(conv.messages[4].text_content(), "u2");
    }

    // ── Regression: raw history intact when only a failed-compaction
    //    Started boundary exists (no prior completed boundary) ──────────

    /// Regression: a session that has only a `Started` boundary from a failed
    /// compaction attempt (no prior completed boundary) must load the raw
    /// conversation unchanged. This proves that a summarizer error, early stream
    /// end, or process crash that leaves only a `Started` row does not corrupt
    /// or modify the visible history.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_conversation_raw_history_preserved_with_failed_started_only_boundary() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        let messages = vec![
            Message::system("You are a helpful assistant."),
            Message::user("What is Rust?"),
            Message::assistant("Rust is a systems programming language."),
            Message::user("Tell me more."),
            Message::assistant("Rust focuses on safety and performance."),
        ];
        msg_repo
            .insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .unwrap();

        // Simulate a failed compaction: only a Started row is written, never
        // completed (summarizer error, crash, early stream end, etc.).
        boundary_repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: None,
                last_compacted_message_id: None,
                first_retained_message_id: None,
                retained_tail_hash: None,
                marker_metadata: None,
            })
            .await
            .unwrap();

        // Verify the Started-only boundary exists.
        let latest_completed = boundary_repo
            .latest_completed_boundary(&session_id)
            .await
            .unwrap();
        assert!(
            latest_completed.is_none(),
            "no completed boundary should exist after failed compaction"
        );
        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            1,
            "started boundary row should exist"
        );

        // load_conversation must return raw history unchanged.
        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        assert_eq!(conv.messages.len(), 5);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(
            conv.messages[0].text_content(),
            "You are a helpful assistant."
        );
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[1].text_content(), "What is Rust?");
        assert_eq!(conv.messages[2].role, Role::Assistant);
        assert_eq!(
            conv.messages[2].text_content(),
            "Rust is a systems programming language."
        );
        assert_eq!(conv.messages[3].role, Role::User);
        assert_eq!(conv.messages[3].text_content(), "Tell me more.");
        assert_eq!(conv.messages[4].role, Role::Assistant);
        assert_eq!(
            conv.messages[4].text_content(),
            "Rust focuses on safety and performance."
        );
        for msg in &conv.messages {
            assert!(
                msg.metadata.is_none(),
                "no metadata should be injected for raw history"
            );
        }
    }

    // ── Regression: normal message turns do not create boundary rows ──

    /// Regression: multiple normal message-turn insertions (single + batch)
    /// must never create compaction boundary rows. Boundary rows are written
    /// only at compaction start/completion paths.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_normal_turns_never_create_boundary_rows() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        // Simulate several normal message turns (alternating single insert and
        // batch insert as the reply loop does).
        // Turn 1: batch insert of system + initial user message.
        msg_repo
            .insert_messages_batch(
                &session_id,
                &task_id,
                &[
                    Message::system("You are a worker."),
                    Message::user("Do the task."),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            0,
            "no boundary rows after initial batch insert"
        );

        // Turn 2: assistant response (single insert).
        msg_repo
            .insert_message(
                &session_id,
                &task_id,
                "assistant",
                r#"[{"type":"text","text":"Working on it."}]"#,
                Some(15),
            )
            .await
            .unwrap();
        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            0,
            "no boundary rows after assistant single insert"
        );

        // Turn 3: tool result (single insert).
        msg_repo
            .insert_message(
                &session_id,
                &task_id,
                "user",
                r#"[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]"#,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            0,
            "no boundary rows after tool result insert"
        );

        // Turn 4: batch insert (assistant + user follow-up).
        msg_repo
            .insert_messages_batch(
                &session_id,
                &task_id,
                &[
                    Message::assistant("Task is done."),
                    Message::user("Thanks."),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            0,
            "no boundary rows after follow-up batch insert"
        );

        // Verify the full conversation loads correctly with no boundary effect.
        let conv = msg_repo.load_conversation(&session_id).await.unwrap();
        assert_eq!(conv.messages.len(), 6);
    }
}

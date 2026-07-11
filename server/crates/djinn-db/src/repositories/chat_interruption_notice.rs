use sqlx::Row;

use crate::Result;
use crate::database::Database;

/// A durable, model-internal indication that an assistant stream was interrupted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatInterruptionNotice {
    pub interruption_notice_id: String,
    pub session_id: String,
    pub session_message_id: Option<String>,
    pub interrupted_turn: bool,
    pub discarded_tool_calls_count: i32,
    pub interrupted_at: String,
    pub consumed_at: Option<String>,
}

/// Parameters for recording an interrupted assistant turn.
pub struct CreateChatInterruptionNotice<'a> {
    pub session_id: &'a str,
    pub session_message_id: Option<&'a str>,
    pub discarded_tool_calls_count: i32,
}

/// Repository for interruption notices kept out of visible session messages.
pub struct ChatInterruptionNoticeRepository {
    db: Database,
}

impl ChatInterruptionNoticeRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn create(
        &self,
        params: CreateChatInterruptionNotice<'_>,
    ) -> Result<ChatInterruptionNotice> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO chat_interruption_notices \
             (interruption_notice_id, session_id, session_message_id, interrupted_turn, discarded_tool_calls_count) \
             VALUES ($1, $2, $3, true, $4)",
        )
        .bind(&id)
        .bind(params.session_id)
        .bind(params.session_message_id)
        .bind(params.discarded_tool_calls_count)
        .execute(self.db.pool())
        .await?;
        self.fetch_by_id(&id).await
    }

    /// List notices which have not yet been explicitly consumed, oldest first.
    pub async fn list_unconsumed(&self, session_id: &str) -> Result<Vec<ChatInterruptionNotice>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query(
            "SELECT interruption_notice_id, session_id, session_message_id, interrupted_turn, \
                    discarded_tool_calls_count, interrupted_at, consumed_at \
             FROM chat_interruption_notices \
             WHERE session_id = $1 AND consumed_at IS NULL \
             ORDER BY interrupted_at ASC, interruption_notice_id ASC",
        )
        .bind(session_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(row_to_notice).collect())
    }

    /// Mark only the supplied durable notice identities consumed. Unknown ids are ignored.
    pub async fn mark_consumed(&self, interruption_notice_ids: &[String]) -> Result<()> {
        self.db.ensure_initialized().await?;
        for id in interruption_notice_ids {
            sqlx::query(
                "UPDATE chat_interruption_notices \
                 SET consumed_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
                 WHERE interruption_notice_id = $1 AND consumed_at IS NULL",
            )
            .bind(id)
            .execute(self.db.pool())
            .await?;
        }
        Ok(())
    }

    async fn fetch_by_id(&self, id: &str) -> Result<ChatInterruptionNotice> {
        let row = sqlx::query(
            "SELECT interruption_notice_id, session_id, session_message_id, interrupted_turn, \
                    discarded_tool_calls_count, interrupted_at, consumed_at \
             FROM chat_interruption_notices WHERE interruption_notice_id = $1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(row_to_notice(row))
    }
}

fn row_to_notice(row: sqlx::postgres::PgRow) -> ChatInterruptionNotice {
    ChatInterruptionNotice {
        interruption_notice_id: row.get("interruption_notice_id"),
        session_id: row.get("session_id"),
        session_message_id: row.get("session_message_id"),
        interrupted_turn: row.get("interrupted_turn"),
        discarded_tool_calls_count: row.get("discarded_tool_calls_count"),
        interrupted_at: row.get("interrupted_at"),
        consumed_at: row.get("consumed_at"),
    }
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;

    use super::*;
    use crate::repositories::epic::EpicRepository;
    use crate::repositories::session::{CreateSessionParams, SessionRepository};

    async fn session_id(db: Database) -> String {
        let bus = EventBus::noop();
        let epic = EpicRepository::new(db.clone(), bus.clone())
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();
        let task_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs) VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)")
            .bind(&task_id).bind(&epic.project_id).bind(format!("t{}", &task_id[..8])).bind(&epic.id)
            .execute(db.pool()).await.unwrap();
        SessionRepository::new(db, bus)
            .create(CreateSessionParams {
                project_id: &epic.project_id,
                task_id: Some(&task_id),
                model: "test",
                agent_type: "chat",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lists_and_explicitly_consumes_notices() {
        let db = Database::open_in_memory().unwrap();
        let session_id = session_id(db.clone()).await;
        let repo = ChatInterruptionNoticeRepository::new(db);
        let partial = repo
            .create(CreateChatInterruptionNotice {
                session_id: &session_id,
                session_message_id: None,
                discarded_tool_calls_count: 0,
            })
            .await
            .unwrap();
        let tools = repo
            .create(CreateChatInterruptionNotice {
                session_id: &session_id,
                session_message_id: None,
                discarded_tool_calls_count: 2,
            })
            .await
            .unwrap();
        assert!(partial.interrupted_turn);
        assert!(!partial.interruption_notice_id.is_empty());
        assert!(!partial.interrupted_at.is_empty());
        assert_eq!(repo.list_unconsumed(&session_id).await.unwrap().len(), 2);
        repo.mark_consumed(std::slice::from_ref(&partial.interruption_notice_id))
            .await
            .unwrap();
        let unconsumed = repo.list_unconsumed(&session_id).await.unwrap();
        assert_eq!(unconsumed.len(), 1);
        assert_eq!(
            unconsumed[0].interruption_notice_id,
            tools.interruption_notice_id
        );
        assert_eq!(unconsumed[0].discarded_tool_calls_count, 2);
    }
}

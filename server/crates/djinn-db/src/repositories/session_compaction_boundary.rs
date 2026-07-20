use crate::Result;
use crate::database::Database;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// The lifecycle phase of a compaction boundary record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionPhase {
    /// Compaction has been initiated; no summary available yet.
    Started,
    /// Compaction completed successfully; summary + retained-tail available.
    Ended,
}

/// The validated reason a compaction boundary was recorded.
///
/// Values are persisted verbatim and match migration 134's `trigger`
/// constraint. Unknown persisted values are rejected rather than coerced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompactionTrigger {
    Proactive,
    ContextError,
    OrphanRepair,
    OversizedTransport,
    ManualTest,
    Fallback,
}

impl CompactionTrigger {
    pub const ALL: [Self; 6] = [
        Self::Proactive,
        Self::ContextError,
        Self::OrphanRepair,
        Self::OversizedTransport,
        Self::ManualTest,
        Self::Fallback,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proactive => "proactive",
            Self::ContextError => "context-error",
            Self::OrphanRepair => "orphan-repair",
            Self::OversizedTransport => "oversized-transport",
            Self::ManualTest => "manual/test",
            Self::Fallback => "fallback",
        }
    }

    /// Parse only a committed persisted trigger value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "proactive" => Some(Self::Proactive),
            "context-error" => Some(Self::ContextError),
            "orphan-repair" => Some(Self::OrphanRepair),
            "oversized-transport" => Some(Self::OversizedTransport),
            "manual/test" => Some(Self::ManualTest),
            "fallback" => Some(Self::Fallback),
            _ => None,
        }
    }
}

impl CompactionPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ended => "ended",
        }
    }
}

/// A persisted compaction boundary row.
#[derive(Clone, Debug)]
pub struct CompactionBoundary {
    pub id: String,
    pub session_id: String,
    pub phase: CompactionPhase,
    pub schema_version: i32,
    pub first_message_id: Option<String>,
    pub last_compacted_message_id: Option<String>,
    pub first_retained_message_id: Option<String>,
    pub retained_tail_hash: Option<String>,
    pub summary_text: Option<String>,
    pub marker_metadata: Option<serde_json::Value>,
    pub trigger: Option<CompactionTrigger>,
    /// Current-context occupancy at compaction start, never lifetime spend.
    pub current_context_tokens_before: Option<i64>,
    /// Current-context occupancy after compaction, never lifetime spend.
    pub current_context_tokens_after: Option<i64>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

/// Parameters for beginning a new compaction boundary.
pub struct BeginCompactionParams<'a> {
    pub session_id: &'a str,
    pub schema_version: i32,
    pub first_message_id: Option<&'a str>,
    pub last_compacted_message_id: Option<&'a str>,
    pub first_retained_message_id: Option<&'a str>,
    pub retained_tail_hash: Option<&'a str>,
    pub marker_metadata: Option<&'a serde_json::Value>,
    pub trigger: Option<CompactionTrigger>,
    /// Current-context occupancy at compaction start, not lifetime spend.
    pub current_context_tokens_before: Option<i64>,
}

/// Parameters for completing (ending) an existing compaction boundary.
pub struct CompleteCompactionParams<'a> {
    pub boundary_id: &'a str,
    pub schema_version: i32,
    pub first_message_id: Option<&'a str>,
    pub last_compacted_message_id: Option<&'a str>,
    pub first_retained_message_id: Option<&'a str>,
    pub retained_tail_hash: Option<&'a str>,
    pub summary_text: &'a str,
    pub marker_metadata: Option<&'a serde_json::Value>,
    /// Current-context occupancy after compaction, not lifetime spend.
    pub current_context_tokens_after: Option<i64>,
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

/// Typed API for durable compaction boundary records.
///
/// The two-phase contract is:
/// 1. [`record_compaction_started`] inserts a `started` row at compaction
///    entry.
/// 2. [`complete_compaction_boundary`] transitions that row to `ended` with
///    the accepted summary and retained-tail identity.
///
/// A crash or summarizer error between 1 and 2 leaves only a `started` row,
/// which projection will ignore.
pub struct SessionCompactionBoundaryRepository {
    db: Database,
}

impl SessionCompactionBoundaryRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist a new `started` compaction boundary and return the inserted
    /// row.
    pub async fn record_compaction_started(
        &self,
        params: BeginCompactionParams<'_>,
    ) -> Result<CompactionBoundary> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query!(
            "INSERT INTO session_compaction_boundaries
                (id, session_id, phase, schema_version,
                 first_message_id, last_compacted_message_id,
                 first_retained_message_id, retained_tail_hash, marker_metadata,
                 trigger, current_context_tokens_before)
             VALUES ($1, $2, 'started', $3, $4, $5, $6, $7, $8, $9, $10)",
            id,
            params.session_id,
            params.schema_version,
            params.first_message_id,
            params.last_compacted_message_id,
            params.first_retained_message_id,
            params.retained_tail_hash,
            params.marker_metadata,
            params.trigger.map(CompactionTrigger::as_str),
            params.current_context_tokens_before,
        )
        .execute(self.db.pool())
        .await?;

        self.fetch_by_id(&id).await
    }

    /// Transition an existing `started` boundary to `ended`, populating the
    /// summary text, retained-tail identity, and completion timestamp.
    ///
    /// Returns the updated row. Fails if the boundary does not exist or is
    /// already `ended`.
    pub async fn complete_compaction_boundary(
        &self,
        params: CompleteCompactionParams<'_>,
    ) -> Result<CompactionBoundary> {
        self.db.ensure_initialized().await?;

        // Use a CTE to atomically check the current phase and update only
        // if still `started`. This prevents double-completion.
        let result = sqlx::query!(
            r#"UPDATE session_compaction_boundaries
               SET phase = 'ended',
                   schema_version = $2,
                   first_message_id = $3,
                   last_compacted_message_id = $4,
                   first_retained_message_id = $5,
                   retained_tail_hash = $6,
                   summary_text = $7,
                   marker_metadata = $8,
                   current_context_tokens_after = $9,
                   completed_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1 AND phase = 'started'"#,
            params.boundary_id,
            params.schema_version,
            params.first_message_id,
            params.last_compacted_message_id,
            params.first_retained_message_id,
            params.retained_tail_hash,
            params.summary_text,
            params.marker_metadata,
            params.current_context_tokens_after,
        )
        .execute(self.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            // Either the row doesn't exist or it's already ended.
            // Check existence to give a precise error.
            let exists: bool = sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT 1 FROM session_compaction_boundaries WHERE id = $1) AS "exists!""#,
                params.boundary_id,
            )
            .fetch_one(self.db.pool())
            .await?;

            if exists {
                return Err(crate::Error::InvalidTransition(
                    "compaction boundary is already completed".to_owned(),
                ));
            }
            return Err(crate::Error::Internal(
                "compaction boundary not found".to_owned(),
            ));
        }

        self.fetch_by_id(params.boundary_id).await
    }

    /// Fetch a single boundary row by its id.
    pub async fn fetch_by_id(&self, id: &str) -> Result<CompactionBoundary> {
        self.db.ensure_initialized().await?;

        let row = sqlx::query!(
            r#"SELECT
                id, session_id, phase, schema_version,
                first_message_id, last_compacted_message_id,
                first_retained_message_id, retained_tail_hash,
                summary_text, marker_metadata, trigger, current_context_tokens_before,
                current_context_tokens_after,
                created_at, completed_at
             FROM session_compaction_boundaries
             WHERE id = $1"#,
            id,
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(CompactionBoundary {
            id: row.id,
            session_id: row.session_id,
            phase: parse_phase(&row.phase),
            schema_version: row.schema_version,
            first_message_id: row.first_message_id,
            last_compacted_message_id: row.last_compacted_message_id,
            first_retained_message_id: row.first_retained_message_id,
            retained_tail_hash: row.retained_tail_hash,
            summary_text: row.summary_text,
            marker_metadata: row.marker_metadata,
            trigger: parse_trigger(row.trigger)?,
            current_context_tokens_before: row.current_context_tokens_before,
            current_context_tokens_after: row.current_context_tokens_after,
            created_at: row.created_at,
            completed_at: row.completed_at,
        })
    }

    /// Return the most recently completed (`ended`) boundary for a session,
    /// or `None` if no completed boundary exists.
    pub async fn latest_completed_boundary(
        &self,
        session_id: &str,
    ) -> Result<Option<CompactionBoundary>> {
        self.db.ensure_initialized().await?;

        let row = sqlx::query!(
            r#"SELECT
                id, session_id, phase, schema_version,
                first_message_id, last_compacted_message_id,
                first_retained_message_id, retained_tail_hash,
                summary_text, marker_metadata, trigger, current_context_tokens_before,
                current_context_tokens_after,
                created_at, completed_at
             FROM session_compaction_boundaries
             WHERE session_id = $1 AND phase = 'ended'
             ORDER BY completed_at DESC, id DESC
             LIMIT 1"#,
            session_id,
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row.map(|r| CompactionBoundary {
            id: r.id,
            session_id: r.session_id,
            phase: parse_phase(&r.phase),
            schema_version: r.schema_version,
            first_message_id: r.first_message_id,
            last_compacted_message_id: r.last_compacted_message_id,
            first_retained_message_id: r.first_retained_message_id,
            retained_tail_hash: r.retained_tail_hash,
            summary_text: r.summary_text,
            marker_metadata: r.marker_metadata,
            trigger: parse_trigger(r.trigger)?,
            current_context_tokens_before: r.current_context_tokens_before,
            current_context_tokens_after: r.current_context_tokens_after,
            created_at: r.created_at,
            completed_at: r.completed_at,
        }))
    }

    /// Count all boundary rows (any phase) for a session.
    pub async fn boundary_count(&self, session_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;

        let count: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_compaction_boundaries WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(self.db.pool())
        .await?;

        Ok(count.unwrap_or(0))
    }
}

fn parse_phase(s: &str) -> CompactionPhase {
    match s {
        "ended" => CompactionPhase::Ended,
        _ => CompactionPhase::Started,
    }
}

fn parse_trigger(value: Option<String>) -> Result<Option<CompactionTrigger>> {
    value.map(|value| CompactionTrigger::parse(&value).ok_or_else(|| crate::Error::InvalidData(format!("unknown compaction trigger: {value}")))).transpose()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;
    use djinn_core::message::Message;

    use super::*;
    use crate::repositories::epic::EpicRepository;
    use crate::repositories::session::{CreateSessionParams, SessionRepository};
    use crate::repositories::session_message::SessionMessageRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn create_session(db: Database, bus: EventBus) -> (String, String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
            task_id,
            epic.project_id,
            short_id,
            epic.id,
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

    // ── Happy path: started-only row ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_started_creates_started_row() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        let boundary = repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some("msg-001"),
                last_compacted_message_id: Some("msg-010"),
                first_retained_message_id: Some("msg-011"),
                retained_tail_hash: Some("sha256:abc123"),
                marker_metadata: None,
            })
            .await
            .expect("record_compaction_started");

        assert_eq!(boundary.session_id, session_id);
        assert_eq!(boundary.phase, CompactionPhase::Started);
        assert_eq!(boundary.schema_version, 1);
        assert_eq!(boundary.first_message_id.as_deref(), Some("msg-001"));
        assert_eq!(
            boundary.last_compacted_message_id.as_deref(),
            Some("msg-010")
        );
        assert_eq!(
            boundary.first_retained_message_id.as_deref(),
            Some("msg-011")
        );
        assert_eq!(
            boundary.retained_tail_hash.as_deref(),
            Some("sha256:abc123")
        );
        assert!(boundary.summary_text.is_none());
        assert!(boundary.completed_at.is_none());
        assert!(!boundary.created_at.is_empty());
    }

    // ── Happy path: started → ended transition ────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_boundary_transitions_to_ended() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        let started = repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: Some(CompactionTrigger::Proactive),
                current_context_tokens_before: Some(1_024),
                first_message_id: Some("msg-001"),
                last_compacted_message_id: Some("msg-010"),
                first_retained_message_id: Some("msg-011"),
                retained_tail_hash: Some("sha256:abc123"),
                marker_metadata: None,
            })
            .await
            .unwrap();

        let marker = serde_json::json!({"marker_kind": "compaction", "token_count": 42});

        let completed = repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: &started.id,
                schema_version: 1,
                current_context_tokens_after: Some(256),
                first_message_id: Some("msg-001"),
                last_compacted_message_id: Some("msg-010"),
                first_retained_message_id: Some("msg-011"),
                retained_tail_hash: Some("sha256:abc123"),
                summary_text: "Conversation about testing.",
                marker_metadata: Some(&marker),
            })
            .await
            .expect("complete_compaction_boundary");

        assert_eq!(completed.phase, CompactionPhase::Ended);
        assert_eq!(
            completed.summary_text.as_deref(),
            Some("Conversation about testing.")
        );
        assert!(completed.completed_at.is_some());
        assert_eq!(
            completed.marker_metadata.as_ref().unwrap()["marker_kind"]
                .as_str()
                .unwrap(),
            "compaction"
        );
        assert_eq!(
            completed.marker_metadata.as_ref().unwrap()["token_count"]
                .as_i64()
                .unwrap(),
            42
        );
        assert_eq!(completed.trigger, Some(CompactionTrigger::Proactive));
        assert_eq!(completed.current_context_tokens_before, Some(1_024));
        assert_eq!(completed.current_context_tokens_after, Some(256));
        let fetched = repo.fetch_by_id(&started.id).await.unwrap();
        assert_eq!(fetched.trigger, Some(CompactionTrigger::Proactive));
        assert_eq!(fetched.current_context_tokens_before, Some(1_024));
        assert_eq!(fetched.current_context_tokens_after, Some(256));
        let latest = repo.latest_completed_boundary(&session_id).await.unwrap().unwrap();
        assert_eq!(latest.trigger, Some(CompactionTrigger::Proactive));
        assert_eq!(latest.current_context_tokens_before, Some(1_024));
        assert_eq!(latest.current_context_tokens_after, Some(256));
    }

    // ── Double-completion is rejected ─────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_already_ended_returns_invalid_transition() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        let started = repo
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

        // First completion succeeds.
        repo.complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
                current_context_tokens_after: None,
            first_message_id: Some("m1"),
            last_compacted_message_id: Some("m5"),
            first_retained_message_id: Some("m6"),
            retained_tail_hash: Some("h"),
            summary_text: "done",
            marker_metadata: None,
        })
        .await
        .unwrap();

        // Second completion fails with InvalidTransition.
        let err = repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: &started.id,
                schema_version: 1,
                current_context_tokens_after: None,
                first_message_id: Some("m1"),
                last_compacted_message_id: Some("m5"),
                first_retained_message_id: Some("m6"),
                retained_tail_hash: Some("h"),
                summary_text: "done again",
                marker_metadata: None,
            })
            .await
            .unwrap_err();

        match err {
            crate::Error::InvalidTransition(msg) => {
                assert!(msg.contains("already completed"));
            }
            other => panic!("expected InvalidTransition, got: {other}"),
        }
    }

    // ── Completing a nonexistent boundary fails ───────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_nonexistent_boundary_returns_not_found() {
        let db = test_db();
        let (_project_id, _task_id, _session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        let err = repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: "nonexistent-id",
                schema_version: 1,
                current_context_tokens_after: None,
                first_message_id: None,
                last_compacted_message_id: None,
                first_retained_message_id: None,
                retained_tail_hash: None,
                summary_text: "summary",
                marker_metadata: None,
            })
            .await
            .unwrap_err();

        match err {
            crate::Error::Internal(msg) => {
                assert!(msg.contains("not found"));
            }
            other => panic!("expected Internal (not found), got: {other}"),
        }
    }

    // ── latest_completed_boundary returns the right row ───────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_completed_returns_most_recent_ended() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        // Insert and complete first boundary.
        let b1 = repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some("m1"),
                last_compacted_message_id: Some("m5"),
                first_retained_message_id: Some("m6"),
                retained_tail_hash: Some("h1"),
                marker_metadata: None,
            })
            .await
            .unwrap();

        repo.complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &b1.id,
            schema_version: 1,
                current_context_tokens_after: None,
            first_message_id: Some("m1"),
            last_compacted_message_id: Some("m5"),
            first_retained_message_id: Some("m6"),
            retained_tail_hash: Some("h1"),
            summary_text: "first summary",
            marker_metadata: None,
        })
        .await
        .unwrap();

        // Insert and complete second boundary.
        let b2 = repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some("m6"),
                last_compacted_message_id: Some("m10"),
                first_retained_message_id: Some("m11"),
                retained_tail_hash: Some("h2"),
                marker_metadata: None,
            })
            .await
            .unwrap();

        repo.complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &b2.id,
            schema_version: 1,
                current_context_tokens_after: None,
            first_message_id: Some("m6"),
            last_compacted_message_id: Some("m10"),
            first_retained_message_id: Some("m11"),
            retained_tail_hash: Some("h2"),
            summary_text: "second summary",
            marker_metadata: None,
        })
        .await
        .unwrap();

        let latest = repo
            .latest_completed_boundary(&session_id)
            .await
            .unwrap()
            .expect("should have a completed boundary");

        assert_eq!(latest.summary_text.as_deref(), Some("second summary"));
        assert_eq!(latest.id, b2.id);
    }

    // ── latest_completed_boundary ignores started-only rows ───────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_completed_ignores_started_only() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        // Insert a started boundary but do NOT complete it.
        repo.record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
            first_message_id: Some("m1"),
            last_compacted_message_id: Some("m5"),
            first_retained_message_id: Some("m6"),
            retained_tail_hash: Some("h"),
            marker_metadata: None,
        })
        .await
        .unwrap();

        let latest = repo.latest_completed_boundary(&session_id).await.unwrap();

        assert!(
            latest.is_none(),
            "started-only rows should not be returned by latest_completed_boundary"
        );
    }

    // ── boundary_count counts all phases ──────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundary_count_includes_all_phases() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        assert_eq!(repo.boundary_count(&session_id).await.unwrap(), 0);

        repo.record_compaction_started(BeginCompactionParams {
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

        assert_eq!(repo.boundary_count(&session_id).await.unwrap(), 1);

        let b2 = repo
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

        assert_eq!(repo.boundary_count(&session_id).await.unwrap(), 2);

        // Complete one; count stays 2.
        repo.complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &b2.id,
            schema_version: 1,
                current_context_tokens_after: None,
            first_message_id: None,
            last_compacted_message_id: None,
            first_retained_message_id: None,
            retained_tail_hash: None,
            summary_text: "done",
            marker_metadata: None,
        })
        .await
        .unwrap();

        assert_eq!(repo.boundary_count(&session_id).await.unwrap(), 2);
    }

    // ── Round-trip field preservation ─────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_row_preserves_all_fields() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        let meta = serde_json::json!({
            "marker_kind": "compaction_boundary",
            "token_count": 1234,
            "custom_key": "custom_value"
        });

        let started = repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 2,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some("first-msg"),
                last_compacted_message_id: Some("last-compacted"),
                first_retained_message_id: Some("first-retained"),
                retained_tail_hash: Some("sha256:deadbeef"),
                marker_metadata: Some(&meta),
            })
            .await
            .unwrap();

        let completed = repo
            .complete_compaction_boundary(CompleteCompactionParams {
                boundary_id: &started.id,
                schema_version: 2,
                current_context_tokens_after: None,
                first_message_id: Some("first-msg"),
                last_compacted_message_id: Some("last-compacted"),
                first_retained_message_id: Some("first-retained"),
                retained_tail_hash: Some("sha256:deadbeef"),
                summary_text: "Full summary of the compacted conversation.",
                marker_metadata: Some(&meta),
            })
            .await
            .unwrap();

        assert_eq!(completed.id, started.id);
        assert_eq!(completed.session_id, session_id);
        assert_eq!(completed.phase, CompactionPhase::Ended);
        assert_eq!(completed.schema_version, 2);
        assert_eq!(completed.first_message_id.as_deref(), Some("first-msg"));
        assert_eq!(
            completed.last_compacted_message_id.as_deref(),
            Some("last-compacted")
        );
        assert_eq!(
            completed.first_retained_message_id.as_deref(),
            Some("first-retained")
        );
        assert_eq!(
            completed.retained_tail_hash.as_deref(),
            Some("sha256:deadbeef")
        );
        assert_eq!(
            completed.summary_text.as_deref(),
            Some("Full summary of the compacted conversation.")
        );
        assert!(completed.completed_at.is_some());
        assert!(completed.completed_at.as_ref().unwrap().contains("T"));

        let meta_val = completed.marker_metadata.as_ref().unwrap();
        assert_eq!(
            meta_val["marker_kind"].as_str().unwrap(),
            "compaction_boundary"
        );
        assert_eq!(meta_val["token_count"].as_i64().unwrap(), 1234);
        assert_eq!(meta_val["custom_key"].as_str().unwrap(), "custom_value");
    }

    // ── Message persistence does NOT create boundary rows ─────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_message_does_not_create_boundary_rows() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        // Persist messages via the normal path.
        msg_repo
            .insert_message(
                &session_id,
                &task_id,
                "user",
                r#"[{"type":"text","text":"hello"}]"#,
                Some(10),
            )
            .await
            .unwrap();

        msg_repo
            .insert_message(
                &session_id,
                &task_id,
                "assistant",
                r#"[{"type":"text","text":"hi there"}]"#,
                Some(12),
            )
            .await
            .unwrap();

        // No boundary rows should exist.
        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            0,
            "insert_message must not create compaction boundary rows"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_messages_batch_does_not_create_boundary_rows() {
        let db = test_db();
        let (_project_id, task_id, session_id) = create_session(db.clone(), EventBus::noop()).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
        let boundary_repo = SessionCompactionBoundaryRepository::new(db);

        let messages = vec![
            Message::system("You are helpful."),
            Message::user("Summarize this."),
            Message::assistant("Here is the summary."),
        ];

        msg_repo
            .insert_messages_batch(&session_id, &task_id, &messages)
            .await
            .unwrap();

        assert_eq!(
            boundary_repo.boundary_count(&session_id).await.unwrap(),
            0,
            "insert_messages_batch must not create compaction boundary rows"
        );
    }

    // ── Regression: repeated failed compaction retries leave no completed
    //    boundary ────────────────────────────────────────────────────────

    /// Regression: simulates repeated reply-loop compaction retries that each
    /// record a `Started` boundary but never complete (summarizer error, early
    /// stream end, or process kill). After all retries, `latest_completed_boundary`
    /// must remain `None` and `boundary_count` must equal the number of Started
    /// rows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_failed_compactions_leave_only_started_rows() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        // Simulate 3 failed compaction attempts (like reply-loop retries).
        for i in 0..3 {
            repo.record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some(&format!("msg-{i}-first")),
                last_compacted_message_id: Some(&format!("msg-{i}-last")),
                first_retained_message_id: Some(&format!("msg-{i}-retained")),
                retained_tail_hash: Some(&format!("hash-{i}")),
                marker_metadata: None,
            })
            .await
            .unwrap();
        }

        assert_eq!(
            repo.boundary_count(&session_id).await.unwrap(),
            3,
            "three started rows should exist"
        );

        let latest = repo.latest_completed_boundary(&session_id).await.unwrap();
        assert!(
            latest.is_none(),
            "no completed boundary should exist after repeated failures"
        );
    }

    /// Regression: a completed compaction boundary followed by multiple failed
    /// retry attempts must preserve the completed boundary. `latest_completed_boundary`
    /// returns the original completed row, and the additional Started rows are
    /// ignored by projection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_boundary_preserved_after_failed_retry_attempts() {
        let db = test_db();
        let (_project_id, _task_id, session_id) =
            create_session(db.clone(), EventBus::noop()).await;

        let repo = SessionCompactionBoundaryRepository::new(db);

        // First compaction succeeds.
        let completed = repo
            .record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some("msg-1"),
                last_compacted_message_id: Some("msg-5"),
                first_retained_message_id: Some("msg-6"),
                retained_tail_hash: Some("hash-ok"),
                marker_metadata: None,
            })
            .await
            .unwrap();

        repo.complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &completed.id,
            schema_version: 1,
                current_context_tokens_after: None,
            first_message_id: Some("msg-1"),
            last_compacted_message_id: Some("msg-5"),
            first_retained_message_id: Some("msg-6"),
            retained_tail_hash: Some("hash-ok"),
            summary_text: "First successful summary.",
            marker_metadata: None,
        })
        .await
        .unwrap();

        // Two subsequent compaction attempts fail (Started-only).
        for i in 0..2 {
            repo.record_compaction_started(BeginCompactionParams {
                session_id: &session_id,
                schema_version: 1,
                trigger: None,
                current_context_tokens_before: None,
                first_message_id: Some(&format!("retry-{i}-first")),
                last_compacted_message_id: Some(&format!("retry-{i}-last")),
                first_retained_message_id: Some(&format!("retry-{i}-retained")),
                retained_tail_hash: Some(&format!("retry-hash-{i}")),
                marker_metadata: None,
            })
            .await
            .unwrap();
        }

        // 1 completed + 2 started = 3 total.
        assert_eq!(
            repo.boundary_count(&session_id).await.unwrap(),
            3,
            "1 completed + 2 started rows"
        );

        // latest_completed_boundary must return the original completed row.
        let latest = repo
            .latest_completed_boundary(&session_id)
            .await
            .unwrap()
            .expect("completed boundary should exist");
        assert_eq!(
            latest.summary_text.as_deref(),
            Some("First successful summary.")
        );
        assert_eq!(latest.id, completed.id);
    }
}

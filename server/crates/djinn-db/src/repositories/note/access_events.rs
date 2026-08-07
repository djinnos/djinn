//! Append-only note access ledger (migration 189, proposal u46i AC6).
//!
//! `notes.last_accessed` / `notes.access_count` are last-write-wins scalars and
//! carry no time series, no attribution, and no tool provenance. This module
//! owns the row-per-access ledger that makes `P(memory_read | Injected)`
//! joinable against `retrieval_traces`.
//!
//! The write is deliberately performed inside
//! [`super::NoteRepository::touch_accessed`] rather than at the call sites, so a
//! new caller cannot forget it. The `source` argument is required for the same
//! reason: `memory_search` result payloads count as an access (ADR-054) but are
//! **not** an explicit pull, and conflating the two would silently inflate the
//! metric.

use serde::{Deserialize, Serialize};

use crate::database::Database;
use crate::error::DbResult as Result;

/// Tool surface that caused a note access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteAccessSource {
    /// An explicit `memory_read` pull by the agent. This is the event the
    /// injected-pull-rate metric counts.
    MemoryRead,
    /// A note returned in a `memory_search` result set. Per ADR-054 this counts
    /// as an access for temporal/co-access scoring, but it is *not* a pull:
    /// the agent never asked for this note by name.
    MemorySearch,
}

impl NoteAccessSource {
    /// Stored spelling; must match the `chk_note_access_events_source` CHECK.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemoryRead => "memory_read",
            Self::MemorySearch => "memory_search",
        }
    }

    /// Parse a stored `source` value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "memory_read" => Some(Self::MemoryRead),
            "memory_search" => Some(Self::MemorySearch),
            _ => None,
        }
    }
}

/// Run attribution for a note access.
///
/// Both fields are optional on purpose: the MCP host surface (an operator
/// calling `memory_read` over HTTP) has no session and no task run. An
/// unattributed row is still durable evidence that the read happened, and the
/// injected-pull-rate report counts those rows explicitly instead of dropping
/// them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteAccessAttribution {
    pub session_id: Option<String>,
    pub task_run_id: Option<String>,
}

impl NoteAccessAttribution {
    /// No session and no task run — a host-side or background access.
    pub fn unattributed() -> Self {
        Self::default()
    }

    /// Attribute to a session. The task run is resolved from `sessions` at
    /// write time when it is not already known.
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            task_run_id: None,
        }
    }

    /// Attribute to an explicit session/run pair.
    pub fn for_run(session_id: Option<String>, task_run_id: Option<String>) -> Self {
        Self {
            session_id,
            task_run_id,
        }
    }

    pub fn is_attributed(&self) -> bool {
        self.session_id.is_some() || self.task_run_id.is_some()
    }
}

/// A persisted note access row.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct NoteAccessEvent {
    pub id: String,
    pub project_id: String,
    pub note_id: String,
    pub session_id: Option<String>,
    pub task_run_id: Option<String>,
    pub source: String,
    pub created_at: String,
}

/// Append one row to the ledger.
///
/// When the caller supplies a session but no task run, the run id is resolved
/// from `sessions.task_run_id` so the trace join has the same correlation key
/// on both sides. A missing session row is not an error — the event is still
/// recorded, with a NULL run.
pub(super) async fn record_note_access(
    db: &Database,
    project_id: &str,
    note_id: &str,
    source: NoteAccessSource,
    attribution: &NoteAccessAttribution,
) -> Result<()> {
    let mut task_run_id = attribution.task_run_id.clone();
    if task_run_id.is_none()
        && let Some(session_id) = attribution.session_id.as_deref()
    {
        task_run_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT task_run_id FROM sessions WHERE id = $1",
        )
        .bind(session_id)
        .fetch_optional(db.pool())
        .await?
        .flatten();
    }

    sqlx::query(
        r#"INSERT INTO note_access_events
               (id, project_id, note_id, session_id, task_run_id, source)
           VALUES ($1, $2, $3, $4, $5, $6)"#,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(project_id)
    .bind(note_id)
    .bind(attribution.session_id.as_deref())
    .bind(task_run_id.as_deref())
    .bind(source.as_str())
    .execute(db.pool())
    .await?;

    Ok(())
}

/// Read the ledger for one note, newest first. Test/diagnostic helper — the
/// production consumer is the injected-pull-rate rollup, which aggregates in
/// SQL rather than materializing rows.
pub async fn note_access_events_for_note(
    db: &Database,
    project_id: &str,
    note_id: &str,
) -> Result<Vec<NoteAccessEvent>> {
    Ok(sqlx::query_as::<_, NoteAccessEvent>(
        r#"SELECT id, project_id, note_id, session_id, task_run_id, source, created_at
             FROM note_access_events
            WHERE project_id = $1 AND note_id = $2
            ORDER BY created_at DESC, id DESC"#,
    )
    .bind(project_id)
    .bind(note_id)
    .fetch_all(db.pool())
    .await?)
}

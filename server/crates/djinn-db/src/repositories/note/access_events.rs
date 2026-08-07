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

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::database::Database;
use crate::error::DbResult as Result;

/// Maximum stored length, in characters, of a caller-supplied invocation id.
///
/// Bound to the `note_access_events.invocation_id VARCHAR(64)` column added by
/// migration 197. Callers must reject longer ids *before* note resolution
/// rather than letting Postgres truncate or error mid-transaction — a truncated
/// id would silently collide two distinct invocations into one replay key.
pub const INVOCATION_ID_MAX_CHARS: usize = 64;

/// Whether one [`super::NoteRepository::record_explicit_access`] call actually
/// counted, or was recognised as a replay of an already-counted invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplicitAccessOutcome {
    /// The append-only ledger insert won the `(invocation_id, note_id)` key, so
    /// this call incremented `access_count` and advanced `last_accessed`.
    Counted,
    /// `(invocation_id, note_id)` already existed. The caller is retrying one
    /// logical invocation; no counter and no timestamp moved.
    Replay,
}

/// The wall-clock stamp for one access event, in the exact spelling the
/// `notes.last_accessed` / `note_access_events.created_at` columns use
/// (`to_char(..., 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')`).
///
/// Callers capture this *before* opening the access transaction so the recorded
/// instant is the one at which the handler decided the read succeeded, not
/// whenever the write happened to reach the database.
pub fn access_event_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

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
    /// Caller-keyed invocation this row belongs to (migration 197).
    ///
    /// NULL for every pre-9xih row: those predate invocation keying and can
    /// never satisfy a replay probe. A non-null value is exactly what makes a
    /// row part of the invocation-keyed accounting era.
    pub invocation_id: Option<String>,
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
    let task_run_id = resolve_task_run_id(db, attribution).await?;

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

/// Resolve the run attribution for an access row, filling in the task run from
/// `sessions` when only a session is known.
async fn resolve_task_run_id(
    db: &Database,
    attribution: &NoteAccessAttribution,
) -> Result<Option<String>> {
    if attribution.task_run_id.is_some() {
        return Ok(attribution.task_run_id.clone());
    }
    let Some(session_id) = attribution.session_id.as_deref() else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar::<_, Option<String>>("SELECT task_run_id FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(db.pool())
            .await?
            .flatten(),
    )
}

/// Append one invocation-keyed explicit-read event and, only if that append
/// won, advance the note's access counters — in one transaction.
///
/// The ledger insert is the gate, not a side note. `(invocation_id, note_id)` is
/// unique (migration 197), so a conflict *is* a caller retry of one logical
/// invocation and `ON CONFLICT DO NOTHING` reports zero affected rows. The
/// counter update runs only on the winning insert, which is what makes a replay
/// leave both `access_count` and `last_accessed` untouched.
///
/// The increment is `access_count = access_count + 1` evaluated by Postgres,
/// never an application read-modify-write, so concurrent distinct invocations
/// cannot lose an increment to a lost update. `last_accessed` uses `GREATEST`
/// rather than assignment so an event that commits late but happened earlier
/// cannot rewind a newer timestamp — the columns store fixed-width ISO-8601 UTC,
/// which orders lexicographically.
pub(super) async fn record_explicit_access(
    db: &Database,
    project_id: &str,
    note_id: &str,
    invocation_id: &str,
    event_timestamp: &str,
    attribution: &NoteAccessAttribution,
) -> Result<ExplicitAccessOutcome> {
    let task_run_id = resolve_task_run_id(db, attribution).await?;

    let mut tx = db.pool().begin().await?;

    let inserted = sqlx::query(
        r#"INSERT INTO note_access_events
               (id, project_id, note_id, session_id, task_run_id, source, created_at, invocation_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (invocation_id, note_id) DO NOTHING"#,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(project_id)
    .bind(note_id)
    .bind(attribution.session_id.as_deref())
    .bind(task_run_id.as_deref())
    .bind(NoteAccessSource::MemoryRead.as_str())
    .bind(event_timestamp)
    .bind(invocation_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;

    if inserted {
        sqlx::query(
            r#"UPDATE notes
                  SET access_count  = access_count + 1,
                      last_accessed = GREATEST(last_accessed, $2)
                WHERE id = $1"#,
        )
        .bind(note_id)
        .bind(event_timestamp)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(if inserted {
        ExplicitAccessOutcome::Counted
    } else {
        ExplicitAccessOutcome::Replay
    })
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
        r#"SELECT id, project_id, note_id, session_id, task_run_id, source, created_at,
                  invocation_id
             FROM note_access_events
            WHERE project_id = $1 AND note_id = $2
            ORDER BY created_at DESC, id DESC"#,
    )
    .bind(project_id)
    .bind(note_id)
    .fetch_all(db.pool())
    .await?)
}

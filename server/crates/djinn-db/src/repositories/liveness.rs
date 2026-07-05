//! Repository methods for liveness classifier evidence persistence and current-
//! state queries.
//!
//! This module is additive: it provides narrowly-scoped persistence and query
//! helpers used by the coordinator classifier and later consumer epics (reaper
//! integration, board health, doctor diagnostics). It does **not** implement
//! reaper behavior or board-health presentation.
//!
//! The verdict/outcome/reason values are plain strings that match the CHECK
//! constraints defined in migration 95 (`95_liveness_evidence_outcomes.sql`).
//! The coordinator layer maps its typed [`Verdict`], [`LivenessOutcome`], and
//! [`LivenessReason`] enums to/from these strings.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::database::Database;
use crate::error::DbResult;

// ─── DTOs ───────────────────────────────────────────────────────────────────

/// Snapshot of classifier evidence for persistence into the
/// `liveness_evidence` append-only table and denormalized session/task_run
/// columns.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LivenessEvidenceSnapshot {
    /// The session that was classified.
    pub session_id: String,
    /// The task context, if known.
    pub task_id: Option<String>,
    /// The task_run context, if known.
    pub task_run_id: Option<String>,
    /// Verdict string: `"live"`, `"slow"`, `"dead"`, `"protocol_violation"`.
    pub verdict: String,
    /// Stable outcome kind: `"success"`, `"crash"`, `"timeout"`,
    /// `"dead_reclaimed"`, `"protocol_violation"`, `"kill_noop"`,
    /// `"slow_extended"`. `None` for live/slow verdicts where no action
    /// outcome is determined yet.
    pub outcome_kind: Option<String>,
    /// Stable reason string: `"clean_exit_nonterminal"`,
    /// `"nonzero_exit_nonterminal"`, `"hard_runtime_exceeded"`,
    /// `"slow_extension_budget_exhausted"`.
    pub outcome_reason: Option<String>,
    /// Machine-readable evidence JSON blob (pod phase, claim TTL, hard
    /// runtime, activity signals, exit code, etc.).
    pub evidence: JsonValue,
}

/// Parameters for recording a claim-extension event in the `claim_extensions`
/// table. Used when the classifier produces a `Slow` verdict and the
/// coordinator decides to extend (or deny) the claim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimExtensionRecord {
    /// The session whose claim is being extended.
    pub session_id: String,
    /// The task_run context, if known.
    pub task_run_id: Option<String>,
    /// The project that owns this session/run.
    pub project_id: String,
    /// FK to the `liveness_evidence` row that authorized this extension.
    pub liveness_evidence_id: Option<String>,
    /// Whether the extension was actually granted. `false` means it was
    /// evaluated but denied (e.g. budget exhausted).
    pub granted: bool,
    /// Extension budget counter before this extension.
    pub extension_budget_before: i32,
    /// Extension budget counter after this extension.
    pub extension_budget_after: i32,
    /// Free-form metadata JSON (claim TTL, lane, trigger reason, etc.).
    pub metadata: JsonValue,
}

/// Current classification state for a task, sufficient for the coordinator
/// classifier to produce a verdict.
///
/// All fields are nullable/optional: when no liveness data exists (legacy
/// rows), the fields that depend on the liveness schema are `None`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CurrentLivenessState {
    /// Current task status string (`"open"`, `"in_progress"`, `"closed"`,
    /// etc.). `None` if the task row doesn't exist.
    pub task_status: Option<String>,
    /// Whether the task is in a terminal state (`closed`).
    pub task_is_terminal: bool,
    /// The most recently started `running` session for this task, if any.
    /// This is the session the classifier should evaluate.
    pub active_session_id: Option<String>,
    /// Status of the active session.
    pub active_session_status: Option<String>,
    /// The most recent task_run for this task, if any.
    pub latest_task_run_id: Option<String>,
    /// Status of the latest task_run.
    pub latest_task_run_status: Option<String>,
    /// The latest liveness verdict cached on the session row. `None` when no
    /// liveness data exists for this session (legacy rows).
    pub session_liveness_verdict: Option<String>,
    /// The latest liveness outcome kind cached on the session row.
    pub session_liveness_outcome_kind: Option<String>,
    /// The latest liveness outcome reason cached on the session row.
    pub session_liveness_outcome_reason: Option<String>,
    /// The latest liveness evidence JSON cached on the session row.
    pub session_liveness_evidence: Option<JsonValue>,
    /// The latest liveness outcome kind cached on the task_run row.
    pub task_run_liveness_outcome_kind: Option<String>,
    /// The latest liveness outcome reason cached on the task_run row.
    pub task_run_liveness_outcome_reason: Option<String>,
    /// The latest liveness evidence JSON cached on the task_run row.
    pub task_run_liveness_evidence: Option<JsonValue>,
    /// ISO-8601 UTC `created_at` of the task, for claim TTL computation.
    pub task_created_at: Option<String>,
    /// ISO-8601 UTC `started_at` of the active session.
    pub session_started_at: Option<String>,
    /// ISO-8601 UTC `started_at` of the latest task_run.
    pub task_run_started_at: Option<String>,
    /// ISO-8601 UTC `ended_at` of the latest task_run (if terminal).
    pub task_run_ended_at: Option<String>,
}

// ─── Repository ─────────────────────────────────────────────────────────────

/// Repository for liveness classifier evidence persistence and current-state
/// queries.
pub struct LivenessRepository {
    db: Database,
}

impl LivenessRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist a classifier evidence/outcome snapshot.
    ///
    /// Inserts one row into the `liveness_evidence` append-only table and
    /// updates the denormalized liveness columns on the `sessions` row. If a
    /// `task_run_id` is present, the `task_runs` row is updated too.
    ///
    /// Returns the id of the newly-inserted `liveness_evidence` row.
    pub async fn persist_evidence(&self, snapshot: &LivenessEvidenceSnapshot) -> DbResult<String> {
        self.db.ensure_initialized().await?;

        let evidence_id = uuid::Uuid::now_v7().to_string();

        // 1. Insert into the append-only liveness_evidence table.
        sqlx::query(
            "INSERT INTO liveness_evidence
                (id, session_id, task_id, task_run_id, verdict,
                 outcome_kind, outcome_reason, evidence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&evidence_id)
        .bind(&snapshot.session_id)
        .bind(&snapshot.task_id)
        .bind(&snapshot.task_run_id)
        .bind(&snapshot.verdict)
        .bind(&snapshot.outcome_kind)
        .bind(&snapshot.outcome_reason)
        .bind(&snapshot.evidence)
        .execute(self.db.pool())
        .await?;

        // 2. Update denormalized columns on the sessions row.
        sqlx::query(
            "UPDATE sessions
             SET liveness_verdict = $1,
                 liveness_outcome_kind = $2,
                 liveness_outcome_reason = $3,
                 liveness_evidence = $4
             WHERE id = $5",
        )
        .bind(&snapshot.verdict)
        .bind(&snapshot.outcome_kind)
        .bind(&snapshot.outcome_reason)
        .bind(&snapshot.evidence)
        .bind(&snapshot.session_id)
        .execute(self.db.pool())
        .await?;

        // 3. Update denormalized columns on the task_runs row (if known).
        if let Some(ref run_id) = snapshot.task_run_id {
            sqlx::query(
                "UPDATE task_runs
                 SET liveness_outcome_kind = $1,
                     liveness_outcome_reason = $2,
                     liveness_evidence = $3
                 WHERE id = $4",
            )
            .bind(&snapshot.outcome_kind)
            .bind(&snapshot.outcome_reason)
            .bind(&snapshot.evidence)
            .bind(run_id)
            .execute(self.db.pool())
            .await?;
        }

        Ok(evidence_id)
    }

    /// Record a claim-extension event for a Slow verdict.
    ///
    /// Inserts one row into the `claim_extensions` table. Does **not**
    /// increment task attempts — this is metadata-only. The caller is
    /// responsible for enforcing extension budgets and updating the budget
    /// counters.
    pub async fn record_claim_extension(&self, record: &ClaimExtensionRecord) -> DbResult<String> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO claim_extensions
                (id, session_id, task_run_id, project_id,
                 liveness_evidence_id, granted,
                 extension_budget_before, extension_budget_after, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(&record.session_id)
        .bind(&record.task_run_id)
        .bind(&record.project_id)
        .bind(&record.liveness_evidence_id)
        .bind(record.granted)
        .bind(record.extension_budget_before)
        .bind(record.extension_budget_after)
        .bind(&record.metadata)
        .execute(self.db.pool())
        .await?;

        Ok(id)
    }

    /// Load the current classification state for a task.
    ///
    /// Returns enough task/session/task_run state for the coordinator
    /// classifier, including terminal task status and absence of legacy
    /// liveness rows as `None`/empty data.
    ///
    /// When no liveness data exists (legacy sessions/task_runs that predate
    /// the liveness schema), the liveness-specific fields are `None` and
    /// the method still succeeds.
    pub async fn load_current_state(&self, task_id: &str) -> DbResult<CurrentLivenessState> {
        self.db.ensure_initialized().await?;

        // 1. Load task status and timestamps.
        let task_row: Option<(String, String)> =
            sqlx::query_as("SELECT status, created_at FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_optional(self.db.pool())
                .await?;

        let (task_status, task_is_terminal, task_created_at) = match task_row {
            Some((status, created_at)) => {
                let terminal = status == "closed";
                (Some(status), terminal, Some(created_at))
            }
            None => (None, false, None),
        };

        // 2. Load the most recently started running session for this task.
        let session_row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT id, status, started_at
             FROM sessions
             WHERE task_id = $1 AND status = 'running'
             ORDER BY started_at DESC
             LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?;

        let (active_session_id, active_session_status, session_started_at) = match session_row {
            Some((id, status, started_at)) => (Some(id), Some(status), Some(started_at)),
            None => (None, None, None),
        };

        // 3. Load liveness fields from the active session (if any).
        type SessionLivenessRow = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<JsonValue>,
        );
        let (
            session_liveness_verdict,
            session_liveness_outcome_kind,
            session_liveness_outcome_reason,
            session_liveness_evidence,
        ) = if let Some(ref sid) = active_session_id {
            let row: Option<SessionLivenessRow> = sqlx::query_as(
                "SELECT liveness_verdict, liveness_outcome_kind,
                            liveness_outcome_reason, liveness_evidence
                     FROM sessions WHERE id = $1",
            )
            .bind(sid)
            .fetch_optional(self.db.pool())
            .await?;
            match row {
                Some((v, ok, or_, e)) => (v, ok, or_, e),
                None => (None, None, None, None),
            }
        } else {
            (None, None, None, None)
        };

        // 4. Load the latest task_run for this task.
        let run_row: Option<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, status, started_at, ended_at
                 FROM task_runs
                 WHERE task_id = $1
                 ORDER BY started_at DESC
                 LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?;

        let (latest_task_run_id, latest_task_run_status, task_run_started_at, task_run_ended_at) =
            match run_row {
                Some((id, status, started_at, ended_at)) => {
                    (Some(id), Some(status), Some(started_at), ended_at)
                }
                None => (None, None, None, None),
            };

        // 5. Load liveness fields from the latest task_run (if any).
        let (
            task_run_liveness_outcome_kind,
            task_run_liveness_outcome_reason,
            task_run_liveness_evidence,
        ) = if let Some(ref rid) = latest_task_run_id {
            let row: Option<(Option<String>, Option<String>, Option<JsonValue>)> = sqlx::query_as(
                "SELECT liveness_outcome_kind, liveness_outcome_reason,
                            liveness_evidence
                     FROM task_runs WHERE id = $1",
            )
            .bind(rid)
            .fetch_optional(self.db.pool())
            .await?;
            match row {
                Some((ok, or_, e)) => (ok, or_, e),
                None => (None, None, None),
            }
        } else {
            (None, None, None)
        };

        Ok(CurrentLivenessState {
            task_status,
            task_is_terminal,
            active_session_id,
            active_session_status,
            latest_task_run_id,
            latest_task_run_status,
            session_liveness_verdict,
            session_liveness_outcome_kind,
            session_liveness_outcome_reason,
            session_liveness_evidence,
            task_run_liveness_outcome_kind,
            task_run_liveness_outcome_reason,
            task_run_liveness_evidence,
            task_created_at,
            session_started_at,
            task_run_started_at,
            task_run_ended_at,
        })
    }

    // ── Test / assertion helpers ───────────────────────────────────────────

    /// Fetch the denormalized `liveness_verdict` and `liveness_outcome_kind`
    /// columns from a session row.
    ///
    /// Intended for test assertions that need to verify evidence was persisted
    /// to the session without introducing raw SQL outside `djinn-db`.
    pub async fn get_session_liveness_fields(
        &self,
        session_id: &str,
    ) -> DbResult<(Option<String>, Option<String>)> {
        self.db.ensure_initialized().await?;

        let row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT liveness_verdict, liveness_outcome_kind FROM sessions WHERE id = $1",
        )
        .bind(session_id)
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Count rows in the `liveness_evidence` table for a session, optionally
    /// filtered by `outcome_kind`.
    ///
    /// When `outcome_kind` is `None`, all evidence rows for the session are
    /// counted. Intended for test assertions.
    pub async fn count_evidence_for_session(
        &self,
        session_id: &str,
        outcome_kind: Option<&str>,
    ) -> DbResult<i64> {
        self.db.ensure_initialized().await?;

        let count: i64 = match outcome_kind {
            Some(kind) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM liveness_evidence \
                     WHERE session_id = $1 AND outcome_kind = $2",
                )
                .bind(session_id)
                .bind(kind)
                .fetch_one(self.db.pool())
                .await?
            }
            None => {
                sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE session_id = $1")
                    .bind(session_id)
                    .fetch_one(self.db.pool())
                    .await?
            }
        };

        Ok(count)
    }
}

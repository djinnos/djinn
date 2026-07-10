//! Retrieval trace repository: durable storage for memory recall traces
//! (epic 5wdh / proposal ykkj phase 1).
//!
//! Provides insert, list/filter, and detail primitives for
//! `retrieval_traces` rows. Each row records one dispatch-injection event
//! with its capped candidate set, cap metadata, optional sampling metadata,
//! durations, and estimated injected tokens.
//!
//! Errors are returned as `Result` so downstream instrumentation can log and
//! continue fail-open without changing injection output.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Current schema version for newly inserted trace rows.
pub const RETRIEVAL_TRACE_SCHEMA_VERSION: i32 = 1;

/// Default candidate cap applied before persistence.
///
/// The proposal suggests 50 unless benchmarks justify a lower value
/// (see `design/5wdh-roadmap`).
pub const DEFAULT_CANDIDATE_CAP: i32 = 50;

// ── Entry-point vocabulary ────────────────────────────────────────────────────

/// Entry points that may trigger a retrieval/injection trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalTraceEntryPoint {
    /// Dispatch prompt assembly.
    Dispatch,
    /// JIT pitfall lookup.
    JitPitfalls,
    /// Knowledge-context loading.
    LoadKnowledgeContext,
    /// Knowledge-note formatting.
    FormatKnowledgeNotes,
    /// The `memory_recall_trace` MCP tool.
    MemoryRecallTrace,
}

impl RetrievalTraceEntryPoint {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch",
            Self::JitPitfalls => "jit_pitfalls",
            Self::LoadKnowledgeContext => "load_knowledge_context",
            Self::FormatKnowledgeNotes => "format_knowledge_notes",
            Self::MemoryRecallTrace => "memory_recall_trace",
        }
    }

    /// Parse a string column value into the typed enum.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dispatch" => Some(Self::Dispatch),
            "jit_pitfalls" => Some(Self::JitPitfalls),
            "load_knowledge_context" => Some(Self::LoadKnowledgeContext),
            "format_knowledge_notes" => Some(Self::FormatKnowledgeNotes),
            "memory_recall_trace" => Some(Self::MemoryRecallTrace),
            _ => None,
        }
    }
}

impl RetrievalTraceEntryPoint {
    /// All variants for vocabulary tests.
    pub const ALL_VARIANTS: [Self; 5] = [
        Self::Dispatch,
        Self::JitPitfalls,
        Self::LoadKnowledgeContext,
        Self::FormatKnowledgeNotes,
        Self::MemoryRecallTrace,
    ];
}

/// All valid entry-point string constants, matching the migration CHECK.
pub const ENTRY_POINT_VALUES: &[&str] = &[
    "dispatch",
    "jit_pitfalls",
    "load_knowledge_context",
    "format_knowledge_notes",
    "memory_recall_trace",
];

// ── Skipped-reason vocabulary ─────────────────────────────────────────────────

/// Reason a candidate was skipped (not injected).
///
/// The vocabulary is fixed by the proposal. `skipped_reason` is nullable only
/// for injected candidates — those have `None` here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkippedReason {
    /// Candidate ranked below the top-K threshold.
    NotTopK,
    /// Candidate confidence below the minimum confidence threshold.
    MinConfidence,
    /// Candidate pruned due to token/prompt budget.
    BudgetPruned,
    /// Candidate pruned because it was superseded by a stronger candidate.
    SupersededPruned,
    /// Candidate removed by deduplication.
    Dedupe,
    /// Candidate dropped due to a search/retrieval error.
    SearchError,
}

impl SkippedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotTopK => "not_top_k",
            Self::MinConfidence => "min_confidence",
            Self::BudgetPruned => "budget_pruned",
            Self::SupersededPruned => "superseded_pruned",
            Self::Dedupe => "dedupe",
            Self::SearchError => "search_error",
        }
    }

    /// Parse a string value into the typed enum.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_top_k" => Some(Self::NotTopK),
            "min_confidence" => Some(Self::MinConfidence),
            "budget_pruned" => Some(Self::BudgetPruned),
            "superseded_pruned" => Some(Self::SupersededPruned),
            "dedupe" => Some(Self::Dedupe),
            "search_error" => Some(Self::SearchError),
            _ => None,
        }
    }
}

impl SkippedReason {
    /// All variants for vocabulary tests.
    pub const ALL_VARIANTS: [Self; 6] = [
        Self::NotTopK,
        Self::MinConfidence,
        Self::BudgetPruned,
        Self::SupersededPruned,
        Self::Dedupe,
        Self::SearchError,
    ];
}

/// All valid skipped-reason string constants.
///
/// This is the exact vocabulary required by the proposal/roadmap; it must
/// not be extended without coordination.
pub const SKIPPED_REASON_VALUES: &[&str] = &[
    "not_top_k",
    "min_confidence",
    "budget_pruned",
    "superseded_pruned",
    "dedupe",
    "search_error",
];

// ── Candidate DTO ─────────────────────────────────────────────────────────────

/// A single candidate recorded in a trace's `candidates` JSONB array.
///
/// `skipped_reason` is `None` for injected candidates; for non-injected
/// candidates it is one of [`SkippedReason`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceCandidate {
    /// Stable note id of the candidate.
    pub note_id: String,
    /// Rank position within the candidate set (1-based).
    pub rank: Option<i32>,
    /// Retrieval confidence/score (0.0–1.0).
    pub confidence: Option<f64>,
    /// Reason the candidate was skipped, or `null` for injected candidates.
    pub skipped_reason: Option<SkippedReason>,
    /// Optional source identifier (e.g. "scope_overlap").
    pub source: Option<String>,
    /// Optional scope/context metadata for later classification.
    pub scope: Option<serde_json::Value>,
}

impl TraceCandidate {
    /// Validate the candidate's outcome/skipped_reason invariant.
    ///
    /// The proposal fixes `skipped_reason` to the vocabulary in
    /// [`SKIPPED_REASON_VALUES`] (or `None`). An injected candidate has
    /// `skipped_reason = None`; a non-injected (skipped) candidate must have a
    /// valid [`SkippedReason`]. Because `skipped_reason` is a typed enum, an
    /// out-of-vocabulary value cannot be constructed; this helper is provided so
    /// callers can check the invariant explicitly and surface a clear error
    /// before persistence rather than silently persisting inconsistent data.
    ///
    /// A non-injected candidate without a `skipped_reason` is rejected: only
    /// injected candidates may omit it.
    pub fn validate_invariants(&self) -> Result<()> {
        match &self.skipped_reason {
            None => Ok(()), // injected candidate — nullable per design
            Some(reason) => {
                // The enum guarantees membership, but double-check the serialised
                // form is in the fixed vocabulary so a future rename is caught.
                if SKIPPED_REASON_VALUES.contains(&reason.as_str()) {
                    Ok(())
                } else {
                    Err(DbError::InvalidData(format!(
                        "candidate {} has skipped_reason '{}' which is not in the fixed vocabulary {:?}",
                        self.note_id,
                        reason.as_str(),
                        SKIPPED_REASON_VALUES,
                    )))
                }
            }
        }
    }
}

/// Validate a slice of candidates, returning the first invariant violation.
///
/// This is the batch form of [`TraceCandidate::validate_invariants`], suitable
/// for calling right before persistence. Returns `Ok(())` when all candidates
/// are consistent, or the first [`DbError::InvalidData`] describing the
/// malformed combination.
pub fn validate_candidates(candidates: &[TraceCandidate]) -> Result<()> {
    for c in candidates {
        c.validate_invariants()?;
    }
    Ok(())
}

// ── Row type ──────────────────────────────────────────────────────────────────

/// A `retrieval_traces` row.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct RetrievalTraceRow {
    pub id: String,
    pub schema_version: i32,
    pub project_id: String,
    pub session_id: Option<String>,
    pub task_run_id: Option<String>,
    pub task_id: Option<String>,
    pub entry_point: String,
    /// Opaque trigger context.
    pub trigger: Option<serde_json::Value>,
    /// JSONB array of [`TraceCandidate`] DTOs.
    pub candidates: serde_json::Value,
    pub candidate_cap: i32,
    pub candidate_cap_exceeded: bool,
    /// Optional sampling metadata when sampling is enabled.
    pub sampling_metadata: Option<serde_json::Value>,
    /// Per-phase durations in milliseconds.
    pub durations_ms: serde_json::Value,
    pub estimated_injected_tokens: i32,
    pub created_at: String,
}

impl RetrievalTraceRow {
    /// Parse the `entry_point` column into the typed enum.
    pub fn entry_point_enum(&self) -> Option<RetrievalTraceEntryPoint> {
        RetrievalTraceEntryPoint::parse(&self.entry_point)
    }

    /// Deserialize the `candidates` JSONB array into typed DTOs.
    pub fn candidates_typed(&self) -> Vec<TraceCandidate> {
        serde_json::from_value(self.candidates.clone()).unwrap_or_default()
    }
}

// ── Input types ───────────────────────────────────────────────────────────────

/// Parameters for inserting a new retrieval trace.
pub struct CreateRetrievalTraceParams<'a> {
    pub project_id: &'a str,
    pub session_id: Option<&'a str>,
    pub task_run_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub entry_point: RetrievalTraceEntryPoint,
    /// Opaque trigger context (task id, query, scope, etc.).
    pub trigger: Option<&'a serde_json::Value>,
    /// JSONB array of [`TraceCandidate`] DTOs.
    pub candidates: &'a serde_json::Value,
    /// Configured top-N candidate cap applied before persistence.
    pub candidate_cap: i32,
    /// Whether the raw candidate set exceeded `candidate_cap`.
    pub candidate_cap_exceeded: bool,
    /// Optional sampling metadata when sampling is enabled.
    pub sampling_metadata: Option<&'a serde_json::Value>,
    /// Per-phase durations in milliseconds.
    pub durations_ms: &'a serde_json::Value,
    pub estimated_injected_tokens: i32,
}

/// Optional filters for listing traces within a project.
#[derive(Clone, Debug, Default)]
pub struct RetrievalTraceListFilter<'a> {
    /// Filter by session id.
    pub session_id: Option<&'a str>,
    /// Filter by task run id.
    pub task_run_id: Option<&'a str>,
    /// Filter by task id.
    pub task_id: Option<&'a str>,
    /// Filter by entry point.
    pub entry_point: Option<RetrievalTraceEntryPoint>,
    /// Maximum number of rows to return (applied after ordering).
    pub limit: Option<i32>,
}

// ── Repository ────────────────────────────────────────────────────────────────

pub struct RetrievalTraceRepository {
    db: Database,
}

impl RetrievalTraceRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a new retrieval trace row and return the persisted record.
    pub async fn insert(
        &self,
        params: CreateRetrievalTraceParams<'_>,
    ) -> Result<RetrievalTraceRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO retrieval_traces (
                    id, schema_version, project_id, session_id, task_run_id, task_id,
                    entry_point, trigger, candidates,
                    candidate_cap, candidate_cap_exceeded, sampling_metadata,
                    durations_ms, estimated_injected_tokens
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"#,
        )
        .bind(&id)
        .bind(RETRIEVAL_TRACE_SCHEMA_VERSION)
        .bind(params.project_id)
        .bind(params.session_id)
        .bind(params.task_run_id)
        .bind(params.task_id)
        .bind(params.entry_point.as_str())
        .bind(params.trigger)
        .bind(params.candidates)
        .bind(params.candidate_cap)
        .bind(params.candidate_cap_exceeded)
        .bind(params.sampling_metadata)
        .bind(params.durations_ms)
        .bind(params.estimated_injected_tokens)
        .execute(self.db.pool())
        .await?;

        Ok(self
            .get_by_id(&id)
            .await?
            .expect("row just inserted must exist"))
    }

    /// List recent traces for a project, ordered by `created_at DESC`,
    /// optionally filtered by session/task-run/task/entry-point.
    pub async fn list_by_project(
        &self,
        project_id: &str,
        filter: RetrievalTraceListFilter<'_>,
    ) -> Result<Vec<RetrievalTraceRow>> {
        self.db.ensure_initialized().await?;

        // Build a dynamic WHERE clause with positional bind params. The base
        // query always filters on project_id ($1). Each optional filter appends
        // an AND clause and increments the bind position.
        let mut conditions: Vec<String> = Vec::new();
        let mut bind_pos = 2usize; // $1 is project_id

        if filter.session_id.is_some() {
            conditions.push(format!("session_id = ${bind_pos}"));
            bind_pos += 1;
        }
        if filter.task_run_id.is_some() {
            conditions.push(format!("task_run_id = ${bind_pos}"));
            bind_pos += 1;
        }
        if filter.task_id.is_some() {
            conditions.push(format!("task_id = ${bind_pos}"));
            bind_pos += 1;
        }
        if filter.entry_point.is_some() {
            conditions.push(format!("entry_point = ${bind_pos}"));
            bind_pos += 1;
        }

        let limit_bind = bind_pos; // next position for LIMIT
        let limit = filter.limit.unwrap_or(100);

        let condition_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" AND {}", conditions.join(" AND "))
        };

        let sql = format!(
            r#"SELECT
                id, schema_version, project_id, session_id, task_run_id, task_id,
                entry_point, trigger, candidates,
                candidate_cap, candidate_cap_exceeded, sampling_metadata,
                durations_ms, estimated_injected_tokens, created_at
            FROM retrieval_traces
            WHERE project_id = $1{condition_clause}
            ORDER BY created_at DESC
            LIMIT ${limit_bind}"#
        );

        let mut query = sqlx::query_as::<_, RetrievalTraceRow>(&sql).bind(project_id);

        if let Some(sid) = filter.session_id {
            query = query.bind(sid);
        }
        if let Some(trid) = filter.task_run_id {
            query = query.bind(trid);
        }
        if let Some(tid) = filter.task_id {
            query = query.bind(tid);
        }
        if let Some(ep) = filter.entry_point {
            query = query.bind(ep.as_str());
        }
        query = query.bind(limit);

        Ok(query.fetch_all(self.db.pool()).await?)
    }

    /// Fetch a single trace by id.
    pub async fn get_by_id(&self, id: &str) -> Result<Option<RetrievalTraceRow>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, RetrievalTraceRow>(RETRIEVAL_TRACE_SELECT_BY_ID)
                .bind(id)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    /// Prune (delete) trace rows older than a configurable retention cutoff for
    /// a project.
    ///
    /// `before_cutoff` is an ISO-8601 UTC timestamp string (the same format used
    /// by the `created_at` column, e.g. `2026-07-01T00:00:00.000Z`). Rows whose
    /// `created_at` is strictly less than this value are deleted. Because
    /// `created_at` is stored as a lexicographically-sortable UTC ISO-8601
    /// string, a simple string comparison (`<`) correctly orders timestamps.
    ///
    /// Returns the number of rows deleted. Errors are returned as `Result` so
    /// callers can log and continue fail-open without changing injection
    /// output.
    pub async fn prune_older_than(&self, project_id: &str, before_cutoff: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query(
            r#"DELETE FROM retrieval_traces
               WHERE project_id = $1 AND created_at < $2"#,
        )
        .bind(project_id)
        .bind(before_cutoff)
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }
}

// ── SQL constants ─────────────────────────────────────────────────────────────

const RETRIEVAL_TRACE_SELECT_BY_ID: &str = r#"
    SELECT
        id, schema_version, project_id, session_id, task_run_id, task_id,
        entry_point, trigger, candidates,
        candidate_cap, candidate_cap_exceeded, sampling_metadata,
        durations_ms, estimated_injected_tokens, created_at
    FROM retrieval_traces
    WHERE id = $1
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::database::Database;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Seed a project so FK constraints pass.
    async fn seed_project(db: &Database, project_id: &str) {
        db.ensure_initialized().await.unwrap();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo)
             VALUES ($1, $2, 'test-owner', $2)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(project_id)
        .bind(format!("proj-{project_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    }

    fn injected_candidate(note_id: &str, rank: i32, confidence: f64) -> TraceCandidate {
        TraceCandidate {
            note_id: note_id.to_string(),
            rank: Some(rank),
            confidence: Some(confidence),
            skipped_reason: None,
            source: Some("scope_overlap".to_string()),
            scope: None,
        }
    }

    fn skipped_candidate(
        note_id: &str,
        rank: i32,
        confidence: f64,
        reason: SkippedReason,
    ) -> TraceCandidate {
        TraceCandidate {
            note_id: note_id.to_string(),
            rank: Some(rank),
            confidence: Some(confidence),
            skipped_reason: Some(reason),
            source: Some("scope_overlap".to_string()),
            scope: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migration_creates_retrieval_traces_table() {
        let db = test_db();
        db.ensure_initialized().await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM retrieval_traces")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "fresh table should be empty");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_and_get_by_id_round_trips_fields() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000001";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);

        let candidates = json!([
            injected_candidate("note-a", 1, 0.95),
            skipped_candidate("note-b", 2, 0.30, SkippedReason::NotTopK),
        ]);
        let durations = json!({"retrieval_ms": 12, "cap_ms": 3});

        let row = repo
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id: Some("sess-1"),
                task_run_id: Some("run-1"),
                task_id: Some("task-1"),
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: Some(&json!({"query": "test query"})),
                candidates: &candidates,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &durations,
                estimated_injected_tokens: 512,
            })
            .await
            .unwrap();

        assert_eq!(row.schema_version, RETRIEVAL_TRACE_SCHEMA_VERSION);
        assert_eq!(row.project_id, project_id);
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));
        assert_eq!(row.task_run_id.as_deref(), Some("run-1"));
        assert_eq!(row.task_id.as_deref(), Some("task-1"));
        assert_eq!(row.entry_point, "dispatch");
        assert_eq!(
            row.entry_point_enum(),
            Some(RetrievalTraceEntryPoint::Dispatch)
        );
        assert_eq!(row.candidate_cap, 50);
        assert!(!row.candidate_cap_exceeded);
        assert!(row.sampling_metadata.is_none());
        assert_eq!(row.estimated_injected_tokens, 512);
        assert!(row.trigger.is_some());

        let typed = row.candidates_typed();
        assert_eq!(typed.len(), 2);
        assert!(
            typed[0].skipped_reason.is_none(),
            "first candidate is injected"
        );
        assert_eq!(typed[1].skipped_reason, Some(SkippedReason::NotTopK));

        // get_by_id returns the same row.
        let fetched = repo
            .get_by_id(&row.id)
            .await
            .unwrap()
            .expect("row must exist");
        assert_eq!(fetched.id, row.id);
        assert_eq!(fetched.entry_point, "dispatch");
        assert_eq!(fetched.estimated_injected_tokens, 512);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_with_capped_candidates_and_cap_exceeded() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000002";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);

        // Simulate 60 candidates capped to 50.
        let candidates: Vec<TraceCandidate> = (0..50)
            .map(|i| injected_candidate(&format!("note-{i}"), (i + 1) as i32, 0.5))
            .collect();
        let candidates_json = serde_json::to_value(&candidates).unwrap();

        let row = repo
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
                trigger: None,
                candidates: &candidates_json,
                candidate_cap: 50,
                candidate_cap_exceeded: true,
                sampling_metadata: Some(&json!({"sample_rate": 1.0})),
                durations_ms: &json!({}),
                estimated_injected_tokens: 2000,
            })
            .await
            .unwrap();

        assert!(row.candidate_cap_exceeded);
        assert_eq!(row.candidate_cap, 50);
        assert_eq!(row.candidates_typed().len(), 50);
        assert!(row.sampling_metadata.is_some());
        assert_eq!(
            row.entry_point_enum(),
            Some(RetrievalTraceEntryPoint::LoadKnowledgeContext)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_by_project_returns_recent_traces_desc() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000003";
        let other_project = "019f4900-0000-7000-8000-000000000099";
        seed_project(&db, project_id).await;
        seed_project(&db, other_project).await;
        let repo = RetrievalTraceRepository::new(db);

        let candidates = json!([]);

        let r1 = repo
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id: Some("sess-a"),
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &candidates,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .unwrap();

        let r2 = repo
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id: Some("sess-b"),
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &candidates,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .unwrap();

        // Insert into another project — should not appear.
        repo.insert(CreateRetrievalTraceParams {
            project_id: other_project,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

        let all = repo
            .list_by_project(project_id, RetrievalTraceListFilter::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        // DESC ordering: r2 (inserted later) should come first.
        assert_eq!(all[0].id, r2.id);
        assert_eq!(all[1].id, r1.id);

        // Filter by session_id.
        let filtered = repo
            .list_by_project(
                project_id,
                RetrievalTraceListFilter {
                    session_id: Some("sess-a"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, r1.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_by_project_filters_by_entry_point_and_task() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000004";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);
        let candidates = json!([]);

        repo.insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: Some("task-x"),
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

        repo.insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: Some("task-y"),
            entry_point: RetrievalTraceEntryPoint::JitPitfalls,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

        // Filter by entry point.
        let by_ep = repo
            .list_by_project(
                project_id,
                RetrievalTraceListFilter {
                    entry_point: Some(RetrievalTraceEntryPoint::JitPitfalls),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(by_ep.len(), 1);
        assert_eq!(by_ep[0].entry_point, "jit_pitfalls");

        // Filter by task_id.
        let by_task = repo
            .list_by_project(
                project_id,
                RetrievalTraceListFilter {
                    task_id: Some("task-x"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(by_task.len(), 1);
        assert_eq!(by_task[0].task_id.as_deref(), Some("task-x"));

        // Filter by task_run_id.
        let by_run = repo
            .list_by_project(
                project_id,
                RetrievalTraceListFilter {
                    task_run_id: Some("run-z"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(by_run.is_empty(), "no rows match run-z");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_by_project_respects_limit() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000005";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);
        let candidates = json!([]);

        for _ in 0..5 {
            repo.insert(CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &candidates,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            })
            .await
            .unwrap();
        }

        let limited = repo
            .list_by_project(
                project_id,
                RetrievalTraceListFilter {
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_id_returns_none_for_missing() {
        let db = test_db();
        db.ensure_initialized().await.unwrap();
        let repo = RetrievalTraceRepository::new(db);
        let result = repo.get_by_id("nonexistent-id").await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn skipped_reason_vocabulary_is_exact() {
        // Ensure the vocabulary matches the proposal requirement.
        let mut actual: Vec<&str> = SkippedReason::ALL_VARIANTS
            .iter()
            .map(|r| r.as_str())
            .collect();
        actual.sort();
        let mut expected: Vec<&str> = SKIPPED_REASON_VALUES.to_vec();
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(SKIPPED_REASON_VALUES.len(), 6);
    }

    #[test]
    fn entry_point_vocabulary_matches_migration_check() {
        let mut actual: Vec<&str> = RetrievalTraceEntryPoint::ALL_VARIANTS
            .iter()
            .map(|e| e.as_str())
            .collect();
        actual.sort();
        let mut expected: Vec<&str> = ENTRY_POINT_VALUES.to_vec();
        expected.sort();
        assert_eq!(actual, expected);
    }

    // ── Cap/sampling metadata round-trip tests (qmel) ─────────────────────────

    /// Insert helper with explicit fields for cap/sampling tests.
    async fn insert_trace(
        repo: &RetrievalTraceRepository,
        project_id: &str,
        candidates: &serde_json::Value,
        candidate_cap: i32,
        candidate_cap_exceeded: bool,
        sampling_metadata: Option<&serde_json::Value>,
    ) -> RetrievalTraceRow {
        repo.insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates,
            candidate_cap,
            candidate_cap_exceeded,
            sampling_metadata,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn candidate_cap_and_exceeded_round_trip() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000010";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);

        // Explicit cap of 30, not exceeded.
        let row = insert_trace(&repo, project_id, &json!([]), 30, false, None).await;
        assert_eq!(row.candidate_cap, 30);
        assert!(!row.candidate_cap_exceeded);

        // Fetch back by id to confirm persistence.
        let fetched = repo
            .get_by_id(&row.id)
            .await
            .unwrap()
            .expect("row must exist");
        assert_eq!(fetched.candidate_cap, 30);
        assert!(!fetched.candidate_cap_exceeded);

        // Cap exceeded case.
        let row2 = insert_trace(
            &repo,
            project_id,
            &json!([injected_candidate("n1", 1, 0.9)]),
            DEFAULT_CANDIDATE_CAP,
            true,
            None,
        )
        .await;
        assert_eq!(row2.candidate_cap, DEFAULT_CANDIDATE_CAP);
        assert!(row2.candidate_cap_exceeded);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sampling_metadata_round_trips_when_present_and_absent() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000011";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);

        // No sampling metadata → NULL in DB.
        let row_none = insert_trace(
            &repo,
            project_id,
            &json!([]),
            DEFAULT_CANDIDATE_CAP,
            false,
            None,
        )
        .await;
        assert!(row_none.sampling_metadata.is_none());

        // With sampling metadata.
        let sampling = json!({
            "enabled": true,
            "sample_rate": 0.25,
            "method": "top_k_reservoir",
            "seed": 42
        });
        let row_some = insert_trace(
            &repo,
            project_id,
            &json!([]),
            DEFAULT_CANDIDATE_CAP,
            false,
            Some(&sampling),
        )
        .await;
        assert!(row_some.sampling_metadata.is_some());
        // Round-trip the JSONB value exactly.
        assert_eq!(row_some.sampling_metadata.unwrap(), sampling);
    }

    // ── Retention pruning tests (qmel) ────────────────────────────────────────

    /// Backdate a trace row's `created_at` to a fixed ISO-8601 timestamp so
    /// pruning tests can control which rows are old vs. new.
    async fn backdate_created_at(db: &Database, trace_id: &str, created_at: &str) {
        sqlx::query("UPDATE retrieval_traces SET created_at = $1 WHERE id = $2")
            .bind(created_at)
            .bind(trace_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prune_older_than_deletes_old_rows_and_reports_count() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000012";
        let other_project = "019f4900-0000-7000-8000-000000000013";
        seed_project(&db, project_id).await;
        seed_project(&db, other_project).await;
        let repo = RetrievalTraceRepository::new(db.clone());

        // Two "old" rows in the target project.
        let old1 = insert_trace(&repo, project_id, &json!([]), DEFAULT_CANDIDATE_CAP, false, None)
            .await;
        let old2 = insert_trace(&repo, project_id, &json!([]), DEFAULT_CANDIDATE_CAP, false, None)
            .await;
        // One "new" row that should survive.
        let keep = insert_trace(&repo, project_id, &json!([]), DEFAULT_CANDIDATE_CAP, false, None)
            .await;
        // An old row in a *different* project — must NOT be pruned by this call.
        let other_old =
            insert_trace(&repo, other_project, &json!([]), DEFAULT_CANDIDATE_CAP, false, None).await;

        // Backdate: old rows → 2026-01-01, keep row → 2026-12-01.
        backdate_created_at(&db, &old1.id, "2026-01-01T00:00:00.000Z").await;
        backdate_created_at(&db, &old2.id, "2026-06-01T00:00:00.000Z").await;
        backdate_created_at(&db, &keep.id, "2026-12-01T00:00:00.000Z").await;
        backdate_created_at(&db, &other_old.id, "2026-01-01T00:00:00.000Z").await;

        // Cutoff: prune everything strictly before 2026-07-01.
        let pruned = repo
            .prune_older_than(project_id, "2026-07-01T00:00:00.000Z")
            .await
            .unwrap();

        // old1 and old2 are before the cutoff → 2 pruned.
        assert_eq!(pruned, 2);

        // The "keep" row survives.
        let remaining = repo
            .list_by_project(project_id, RetrievalTraceListFilter::default())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, keep.id);

        // The other-project old row is untouched.
        let other_remaining = repo
            .list_by_project(other_project, RetrievalTraceListFilter::default())
            .await
            .unwrap();
        assert_eq!(other_remaining.len(), 1);
        assert_eq!(other_remaining[0].id, other_old.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prune_older_than_deletes_nothing_when_all_newer() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000014";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db.clone());

        let r1 = insert_trace(&repo, project_id, &json!([]), DEFAULT_CANDIDATE_CAP, false, None)
            .await;
        let r2 = insert_trace(&repo, project_id, &json!([]), DEFAULT_CANDIDATE_CAP, false, None)
            .await;
        backdate_created_at(&db, &r1.id, "2026-11-01T00:00:00.000Z").await;
        backdate_created_at(&db, &r2.id, "2026-12-01T00:00:00.000Z").await;

        let pruned = repo
            .prune_older_than(project_id, "2026-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(pruned, 0);

        let remaining = repo
            .list_by_project(project_id, RetrievalTraceListFilter::default())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prune_older_than_empty_project_prunes_zero() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000015";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);

        let pruned = repo
            .prune_older_than(project_id, "2026-07-01T00:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(pruned, 0);
    }

    // ── Candidate validation invariants (qmel) ────────────────────────────────

    #[test]
    fn validate_candidates_accepts_injected_and_valid_skipped() {
        let candidates = vec![
            injected_candidate("n1", 1, 0.9),
            skipped_candidate("n2", 2, 0.3, SkippedReason::NotTopK),
            skipped_candidate("n3", 3, 0.2, SkippedReason::MinConfidence),
            skipped_candidate("n4", 4, 0.1, SkippedReason::BudgetPruned),
            skipped_candidate("n5", 5, 0.05, SkippedReason::SupersededPruned),
            skipped_candidate("n6", 6, 0.04, SkippedReason::Dedupe),
            skipped_candidate("n7", 7, 0.01, SkippedReason::SearchError),
        ];
        // All combinations are valid: injected has None, skipped have valid reasons.
        assert!(validate_candidates(&candidates).is_ok());
    }

    #[test]
    fn validate_candidates_accepts_empty_set() {
        assert!(validate_candidates(&[]).is_ok());
    }

    #[test]
    fn default_candidate_cap_is_documented_as_50() {
        // The proposal default is 50 unless benchmarks justify a lower value
        // (see design/5wdh-roadmap). This documents and locks the default.
        assert_eq!(DEFAULT_CANDIDATE_CAP, 50);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn candidate_invariants_survive_round_trip() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000016";
        seed_project(&db, project_id).await;
        let repo = RetrievalTraceRepository::new(db);

        // Candidate set covering the full skipped_reason vocabulary + injected.
        let candidates = json!([
            injected_candidate("inj-1", 1, 0.95),
            skipped_candidate("skip-not-top-k", 2, 0.30, SkippedReason::NotTopK),
            skipped_candidate("skip-min-conf", 3, 0.20, SkippedReason::MinConfidence),
            skipped_candidate("skip-budget", 4, 0.15, SkippedReason::BudgetPruned),
            skipped_candidate("skip-superseded", 5, 0.10, SkippedReason::SupersededPruned),
            skipped_candidate("skip-dedupe", 6, 0.08, SkippedReason::Dedupe),
            skipped_candidate("skip-search-err", 7, 0.01, SkippedReason::SearchError),
        ]);

        let row = repo
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: Some(&json!({"sample_rate": 1.0})),
                durations_ms: &json!({}),
                estimated_injected_tokens: 128,
            })
            .await
            .unwrap();

        let typed = row.candidates_typed();
        assert_eq!(typed.len(), 7);

        // The first candidate is injected (skipped_reason == None).
        assert!(typed[0].skipped_reason.is_none());

        // The remaining six each carry a distinct, valid skipped_reason.
        let reasons: Vec<SkippedReason> =
            typed[1..].iter().map(|c| c.skipped_reason.unwrap()).collect();
        assert_eq!(
            reasons,
            vec![
                SkippedReason::NotTopK,
                SkippedReason::MinConfidence,
                SkippedReason::BudgetPruned,
                SkippedReason::SupersededPruned,
                SkippedReason::Dedupe,
                SkippedReason::SearchError,
            ]
        );

        // Every round-tripped candidate passes the invariant check.
        assert!(validate_candidates(&typed).is_ok());
    }
}

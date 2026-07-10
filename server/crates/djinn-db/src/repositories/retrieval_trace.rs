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
/// This is the canonical data-layer contract consumed by downstream epics:
/// - **mwtv** (dispatch instrumentation) writes `TraceCandidate` entries with
///   `skipped_reason` set per the drop-reason taxonomy after applying
///   production top-K, min-confidence, and budget pruning.
/// - **liso** (`memory_recall_trace` MCP tool) reads persisted entries in
///   detail mode, exposing `note_id`, `rank`, `confidence`, `skipped_reason`,
///   `source`, and `scope` to the caller.
///
/// `skipped_reason` is `None` for injected candidates; for non-injected
/// candidates it is one of [`SkippedReason`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceCandidate {
    /// Stable note id of the candidate (matches `notes.id`).
    pub note_id: String,
    /// Rank position within the candidate set (1-based), derived from the
    /// same production ordering (`confidence DESC, updated_at DESC`).
    pub rank: Option<i32>,
    /// Retrieval confidence/score (0.0–1.0) persisted for downstream
    /// classification (e.g. `min_confidence` drop reason).
    pub confidence: Option<f64>,
    /// Reason the candidate was skipped, or `null` for injected candidates.
    /// Downstream dispatch instrumentation sets this to one of the
    /// fixed vocabulary values: `not_top_k`, `min_confidence`,
    /// `budget_pruned`, `superseded_pruned`, `dedupe`, `search_error`.
    pub skipped_reason: Option<SkippedReason>,
    /// Identifier of the retrieval source (e.g. `"scope_overlap"`).
    pub source: Option<String>,
    /// Scope/context metadata carried from the source query for later
    /// classification.  For scope-overlap candidates this is the note's
    /// `scope_paths` JSONB value.
    pub scope: Option<serde_json::Value>,
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
}

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

/// Maximum rows returned per `list_by_project` page when the caller does not
/// provide an explicit limit.
pub const DEFAULT_RETRIEVAL_TRACE_LIMIT: i32 = 100;

/// Maximum offset accepted by `list_by_project` to keep the bounded recent-row
/// query cheap. Larger offsets are rejected with a validation error rather than
/// being silently truncated, which is safer for operator tooling.
pub const MAX_RETRIEVAL_TRACE_OFFSET: i32 = 10_000;

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

// ── Candidate outcome ────────────────────────────────────────────────────────

/// Whether a trace candidate was injected or skipped.
///
/// Used by [`TraceCandidate::validate_invariants`] to enforce the proposal
/// rule that `skipped_reason` is nullable *only* for injected candidates.
/// The `#[serde(default)]` attribute means pre-existing JSONB rows that lack
/// an `outcome` field deserialize as `Skipped`, which preserves backward
/// compatibility while still allowing callers to mark candidates explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOutcome {
    /// The candidate was injected into the downstream prompt.
    Injected,
    /// The candidate was skipped (not injected). This is also the default
    /// when the field is absent from JSONB.
    Skipped,
}

impl CandidateOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Injected => "injected",
            Self::Skipped => "skipped",
        }
    }
}

/// All valid outcome string constants.
pub const CANDIDATE_OUTCOME_VALUES: &[&str] = &["injected", "skipped"];

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
/// This is the complete per-candidate data-layer contract shape. Every field
/// listed below is persisted in the JSONB column and must survive round-trip
/// serialization:
///
/// - **Identity:** `note_id`, `permalink`, `title` — consumed by `liso`
///   (`memory_recall_trace` tooling) for list/detail display.
/// - **Classification:** `outcome`, `skipped_reason` — populated by `mwtv`
///   (dispatch injection instrumentation). The skipped-reason vocabulary is
///   [`SKIPPED_REASON_VALUES`].
/// - **Ranking/score:** `rank` (1-based position), `confidence` (0.0–1.0).
/// - **Provenance:** `source` (e.g. `"scope_overlap"`), `scope` (JSON
///   metadata for later classification).
///
/// `outcome` distinguishes injected from skipped candidates. When absent from
/// JSONB (backward compat via `#[serde(default)]`), it defaults to
/// `Skipped`. Validation enforces:
/// - An **injected** candidate has `skipped_reason = None`.
/// - A **skipped** candidate must have a valid [`SkippedReason`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceCandidate {
    /// Stable note id of the candidate.
    pub note_id: String,
    /// Optional stable permalink of the candidate note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
    /// Optional human-readable title of the candidate note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the candidate was injected or skipped.
    ///
    /// Defaults to `Skipped` when absent from JSONB (backward compat).
    #[serde(default = "default_candidate_outcome")]
    pub outcome: CandidateOutcome,
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

/// Default outcome for deserialization when the `outcome` field is absent
/// from JSONB. `Skipped` is chosen because it is the safer assumption: a
/// missing field on a genuinely-injected candidate will cause
/// `validate_invariants` to surface a clear error (skipped candidate with no
/// reason) rather than silently accepting data that might be malformed.
fn default_candidate_outcome() -> CandidateOutcome {
    CandidateOutcome::Skipped
}

impl TraceCandidate {
    /// Validate the candidate's outcome/skipped_reason invariant.
    ///
    /// The proposal fixes `skipped_reason` to the vocabulary in
    /// [`SKIPPED_REASON_VALUES`] (or `None`). The `outcome` field determines
    /// which branch applies:
    ///
    /// - **Injected** (`outcome = Injected`): `skipped_reason` must be `None`.
    /// - **Skipped** (`outcome = Skipped`): `skipped_reason` must be `Some`
    ///   with a valid [`SkippedReason`].
    ///
    /// Because `skipped_reason` is a typed enum, an out-of-vocabulary value
    /// cannot be constructed in Rust; the vocabulary check is a defensive
    /// double-check against future renames.
    pub fn validate_invariants(&self) -> Result<()> {
        match self.outcome {
            CandidateOutcome::Injected => {
                if self.skipped_reason.is_some() {
                    Err(DbError::InvalidData(format!(
                        "candidate {} has outcome 'injected' but also has skipped_reason '{:?}' \
                         — injected candidates must not have a skipped_reason",
                        self.note_id, self.skipped_reason,
                    )))
                } else {
                    Ok(())
                }
            }
            CandidateOutcome::Skipped => {
                match &self.skipped_reason {
                    None => Err(DbError::InvalidData(format!(
                        "candidate {} has outcome 'skipped' but no skipped_reason — \
                         non-injected (skipped) candidates must have a skipped_reason from {:?}",
                        self.note_id, SKIPPED_REASON_VALUES,
                    ))),
                    Some(reason) => {
                        // Double-check the serialised form is in the fixed vocabulary.
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
    /// Filter by candidate outcome (matches any candidate in the row's JSONB
    /// array with this `outcome` value).
    pub outcome: Option<CandidateOutcome>,
    /// Filter by skipped reason (matches any candidate in the row's JSONB
    /// array with this `skipped_reason` value). Only meaningful when at least
    /// one candidate in the row is skipped, but the predicate itself is
    /// independent of the `outcome` filter so callers can compose them.
    pub skipped_reason: Option<SkippedReason>,
    /// Number of rows to skip after ordering (bounded, must be non-negative).
    pub offset: Option<i32>,
    /// Maximum number of rows to return (applied after ordering).
    pub limit: Option<i32>,
}

impl<'a> RetrievalTraceListFilter<'a> {
    /// Validate the filter bounds, returning a clear error for invalid values.
    ///
    /// `offset` must be non-negative and not exceed
    /// [`MAX_RETRIEVAL_TRACE_OFFSET`]. `limit` is capped by default behavior in
    /// the query builder; callers are not allowed to pass negative limits.
    fn validate(&self) -> Result<()> {
        if let Some(offset) = self.offset {
            if offset < 0 {
                return Err(DbError::InvalidData(
                    "retrieval trace offset must be non-negative".to_owned(),
                ));
            }
            if offset > MAX_RETRIEVAL_TRACE_OFFSET {
                return Err(DbError::InvalidData(format!(
                    "retrieval trace offset cannot exceed {MAX_RETRIEVAL_TRACE_OFFSET}"
                )));
            }
        }
        if self.limit.is_some_and(|limit| limit < 0) {
            return Err(DbError::InvalidData(
                "retrieval trace limit must be non-negative".to_owned(),
            ));
        }
        Ok(())
    }
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
    /// optionally filtered by session/task-run/task/entry-point/candidate-outcome
    /// and skipped-reason.
    pub async fn list_by_project(
        &self,
        project_id: &str,
        filter: RetrievalTraceListFilter<'_>,
    ) -> Result<Vec<RetrievalTraceRow>> {
        self.db.ensure_initialized().await?;

        filter.validate()?;

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
        // JSONB candidate predicates: because there is no GIN index on the
        // `candidates` array, these predicates are applied only inside the
        // mandatory project-scoped, bounded list query. The query remains a
        // recent-row scan (with a LIMIT), not an unbounded candidate lookup.
        if filter.outcome.is_some() {
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements(candidates) AS c WHERE (c->>'outcome')::text = ${bind_pos}::text)"
            ));
            bind_pos += 1;
        }
        if filter.skipped_reason.is_some() {
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements(candidates) AS c WHERE (c->>'skipped_reason')::text = ${bind_pos}::text)"
            ));
            bind_pos += 1;
        }

        let offset_bind = bind_pos; // next position for OFFSET
        let offset = filter.offset.unwrap_or(0);
        bind_pos += 1;
        let limit_bind = bind_pos; // next position for LIMIT
        let limit = filter.limit.unwrap_or(DEFAULT_RETRIEVAL_TRACE_LIMIT);

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
            ORDER BY created_at DESC, id DESC
            OFFSET ${offset_bind}
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
        if let Some(outcome) = filter.outcome {
            query = query.bind(outcome.as_str());
        }
        if let Some(reason) = filter.skipped_reason {
            query = query.bind(reason.as_str());
        }
        query = query.bind(offset);
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

    /// Replace the per-phase durations JSON for an existing trace row.
    ///
    /// Some instrumentation measures the awaited insert path itself before the
    /// final `durations_ms` payload is known. Keep that update inside this
    /// repository layer so application crates never issue raw SQL directly.
    pub async fn update_durations_ms(
        &self,
        id: &str,
        durations_ms: &serde_json::Value,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE retrieval_traces SET durations_ms = $2 WHERE id = $1")
            .bind(id)
            .bind(durations_ms)
            .execute(self.db.pool())
            .await?;
        Ok(())
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

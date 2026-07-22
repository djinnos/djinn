use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgTypeInfo, PgValueRef};
use sqlx::{Decode, Postgres, Type};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;
use std::collections::HashSet;

/// Maximum rows returned per `list_by_project` page when the caller does not
/// provide an explicit limit.
pub const DEFAULT_RETRIEVAL_TRACE_LIMIT: i32 = 100;

/// Maximum offset accepted by `list_by_project` to keep the bounded recent-row
/// query cheap. Larger offsets are rejected with a validation error rather than
/// being silently truncated, which is safer for operator tooling.
pub const MAX_RETRIEVAL_TRACE_OFFSET: i32 = 10_000;

/// Minimum time that retrieval traces must remain available after their
/// timestamp. This is the maximum supported lookback for the task-run outcomes
/// report owned by sibling epic `fv7a`.
///
/// The control-plane/server maintenance caller owns scheduling pruning. It must
/// supply an explicit reference time to [`RetrievalTraceRepository::prune_older_than`]
/// rather than relying on database time, so the retention boundary is
/// deterministic and testable.
pub const MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 60 * 60);

fn parse_retrieval_trace_utc_timestamp(value: &str, field: &str) -> Result<DateTime<Utc>> {
    if !value.ends_with('Z') {
        return Err(DbError::InvalidData(format!(
            "{field} must be a valid ISO-8601 UTC timestamp ending in Z: {value}"
        )));
    }

    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|err| {
            DbError::InvalidData(format!(
                "{field} must be a valid ISO-8601 UTC timestamp: {err}"
            ))
        })
}

fn format_retrieval_trace_utc_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Current schema version for newly inserted trace rows.
pub const RETRIEVAL_TRACE_SCHEMA_VERSION: i32 = 1;

/// Taxonomy version written by the additive terminal API. Legacy writers leave
/// this NULL so old candidate JSON is never inferred to be taxonomy-v1 data.
pub const KNOWLEDGE_TRACE_TAXONOMY_VERSION_V1: i32 = 1;

/// Default candidate cap applied before persistence.
///
/// The proposal suggests 50 unless benchmarks justify a lower value
/// (see `design/5wdh-roadmap`).
pub const DEFAULT_CANDIDATE_CAP: i32 = 50;

// ── Entry-point vocabulary ────────────────────────────────────────────────────

/// Entry points that may trigger a retrieval/injection trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Conservatively classify evidence written through the legacy insertion API.
/// This mirrors migration 119's invariant: an injected trace needs positive
/// token evidence and an injected candidate; reliable empty evidence needs a
/// non-empty, valid all-skipped candidate set and zero tokens. Everything else
/// is intentionally `legacy_unknown`.
pub fn classify_legacy_trace_outcome(
    candidates: &serde_json::Value,
    estimated_injected_tokens: i32,
) -> RetrievalTraceOutcome {
    let Ok(candidates) = serde_json::from_value::<Vec<TraceCandidate>>(candidates.clone()) else {
        return RetrievalTraceOutcome::LegacyUnknown;
    };
    if validate_candidates(&candidates).is_err() {
        return RetrievalTraceOutcome::LegacyUnknown;
    }

    if estimated_injected_tokens > 0
        && candidates
            .iter()
            .any(|candidate| candidate.outcome == CandidateOutcome::Injected)
    {
        RetrievalTraceOutcome::Injected
    } else if estimated_injected_tokens == 0
        && !candidates.is_empty()
        && candidates
            .iter()
            .all(|candidate| candidate.outcome == CandidateOutcome::Skipped)
    {
        RetrievalTraceOutcome::Empty
    } else {
        RetrievalTraceOutcome::LegacyUnknown
    }
}

fn validate_explicit_trace_semantics(
    candidates_json: &serde_json::Value,
    estimated_injected_tokens: i32,
    outcome: RetrievalTraceOutcome,
) -> Result<()> {
    let candidates: Vec<TraceCandidate> =
        serde_json::from_value(candidates_json.clone()).map_err(|err| {
            DbError::InvalidData(format!(
                "explicit retrieval trace candidates must be a valid array: {err}"
            ))
        })?;
    validate_candidates(&candidates)?;
    let has_injected_candidate = candidates
        .iter()
        .any(|candidate| candidate.outcome == CandidateOutcome::Injected);

    if outcome == RetrievalTraceOutcome::Injected
        && (estimated_injected_tokens <= 0 || !has_injected_candidate)
    {
        return Err(DbError::InvalidData(
            "trace outcome 'injected' requires positive estimated_injected_tokens and at least one injected candidate".to_owned(),
        ));
    }
    if matches!(
        outcome,
        RetrievalTraceOutcome::DisabledOff
            | RetrievalTraceOutcome::DisabledKillSwitch
            | RetrievalTraceOutcome::DisabledLegacy
    ) && (estimated_injected_tokens != 0 || has_injected_candidate)
    {
        return Err(DbError::InvalidData(
            "disabled trace outcomes require zero estimated_injected_tokens and no injected candidate"
                .to_owned(),
        ));
    }
    Ok(())
}

impl Type<Postgres> for RetrievalTraceOutcome {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for RetrievalTraceOutcome {
    fn decode(value: PgValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        let value = <String as Decode<Postgres>>::decode(value)?;
        Self::parse(&value).ok_or_else(|| {
            Box::new(DbError::InvalidData(format!(
                "unknown persisted retrieval trace outcome: {value}"
            ))) as sqlx::error::BoxDynError
        })
    }
}

/// Additive write input for callers that know rollout and trace outcome
/// semantics. The nested legacy parameters preserve every existing
/// `CreateRetrievalTraceParams` struct literal unchanged.
pub struct CreateRetrievalTraceWithSemanticsParams<'a> {
    pub trace: CreateRetrievalTraceParams<'a>,
    /// Persisted verbatim (for example `cohort:canary`).
    pub rollout_label: &'a str,
    pub outcome: RetrievalTraceOutcome,
}

/// Terminal result for a taxonomy-v1 retrieval attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTraceTerminalState {
    Success,
    Error,
    Cancelled,
}
impl KnowledgeTraceTerminalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeTraceDispositionCounts {
    pub confidence_filtered: i32,
    pub not_top_k: i32,
    pub oversized_skipped: i32,
    pub injected: i32,
    pub budget_pruned: i32,
}
pub struct CreateRetrievalTraceTerminalParams<'a> {
    pub trace: CreateRetrievalTraceParams<'a>,
    /// Persisted verbatim so terminal writes retain the effective rollout cohort.
    pub rollout_label: &'a str,
    /// Semantic retrieval outcome retained independently from terminal state.
    /// This keeps disabled terminals distinct from retrieval errors.
    pub outcome: RetrievalTraceOutcome,
    pub terminal_state: KnowledgeTraceTerminalState,
    pub terminal_at: &'a str,
    pub candidate_count: Option<i32>,
    pub injected_count: Option<i32>,
    pub dispositions: Option<KnowledgeTraceDispositionCounts>,
}
fn validate_terminal_trace(params: &CreateRetrievalTraceTerminalParams<'_>) -> Result<()> {
    parse_retrieval_trace_utc_timestamp(params.terminal_at, "terminal_at")?;
    match params.terminal_state {
        KnowledgeTraceTerminalState::Success => {
            if !matches!(
                params.outcome,
                RetrievalTraceOutcome::Injected | RetrievalTraceOutcome::Empty
            ) {
                return Err(DbError::InvalidData(
                    "successful taxonomy-v1 terminal requires injected or empty outcome".to_owned(),
                ));
            }
            let candidate_count = params.candidate_count.ok_or_else(|| {
                DbError::InvalidData(
                    "successful taxonomy-v1 trace requires candidate_count".to_owned(),
                )
            })?;
            let injected_count = params.injected_count.ok_or_else(|| {
                DbError::InvalidData(
                    "successful taxonomy-v1 trace requires injected_count".to_owned(),
                )
            })?;
            let counts = params.dispositions.ok_or_else(|| {
                DbError::InvalidData(
                    "successful taxonomy-v1 trace requires all disposition counts".to_owned(),
                )
            })?;
            if [
                candidate_count,
                injected_count,
                counts.confidence_filtered,
                counts.not_top_k,
                counts.oversized_skipped,
                counts.injected,
                counts.budget_pruned,
            ]
            .iter()
            .any(|n| *n < 0)
            {
                return Err(DbError::InvalidData(
                    "taxonomy-v1 candidate and disposition counts must be nonnegative".to_owned(),
                ));
            }
            if injected_count != counts.injected {
                return Err(DbError::InvalidData(
                    "taxonomy-v1 injected_count must equal injected disposition count".to_owned(),
                ));
            }
            let total = counts.confidence_filtered as i64
                + counts.not_top_k as i64
                + counts.oversized_skipped as i64
                + counts.injected as i64
                + counts.budget_pruned as i64;
            if total != candidate_count as i64 {
                return Err(DbError::InvalidData(
                    "taxonomy-v1 disposition counts must sum exactly to candidate_count".to_owned(),
                ));
            }
        }
        KnowledgeTraceTerminalState::Error | KnowledgeTraceTerminalState::Cancelled
            if params.candidate_count.is_some()
                || params.injected_count.is_some()
                || params.dispositions.is_some() =>
        {
            return Err(DbError::InvalidData(
                "error and cancelled taxonomy-v1 terminals must not include candidate dispositions"
                    .to_owned(),
            ));
        }
        KnowledgeTraceTerminalState::Error | KnowledgeTraceTerminalState::Cancelled => {}
    }
    Ok(())
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

// ── Trace outcome ────────────────────────────────────────────────────────────

/// The persisted outcome of a retrieval trace as a whole.
///
/// This deliberately has a different name from [`CandidateOutcome`]: candidate
/// outcomes describe individual JSONB candidates, while this enum records the
/// result of the trace/injection attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalTraceOutcome {
    Injected,
    Empty,
    Error,
    LegacyUnknown,
    DisabledOff,
    DisabledKillSwitch,
    DisabledLegacy,
}

impl RetrievalTraceOutcome {
    pub const ALL_VARIANTS: [Self; 7] = [
        Self::Injected,
        Self::Empty,
        Self::Error,
        Self::LegacyUnknown,
        Self::DisabledOff,
        Self::DisabledKillSwitch,
        Self::DisabledLegacy,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Injected => "injected",
            Self::Empty => "empty",
            Self::Error => "error",
            Self::LegacyUnknown => "legacy_unknown",
            Self::DisabledOff => "disabled_off",
            Self::DisabledKillSwitch => "disabled_kill_switch",
            Self::DisabledLegacy => "disabled_legacy",
        }
    }

    /// Parse a persisted outcome. Unknown values are intentionally rejected
    /// rather than silently converted to `LegacyUnknown`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "injected" => Some(Self::Injected),
            "empty" => Some(Self::Empty),
            "error" => Some(Self::Error),
            "legacy_unknown" => Some(Self::LegacyUnknown),
            "disabled_off" => Some(Self::DisabledOff),
            "disabled_kill_switch" => Some(Self::DisabledKillSwitch),
            "disabled_legacy" => Some(Self::DisabledLegacy),
            _ => None,
        }
    }
}

/// All valid trace-level outcome string constants, matching migration 119.
pub const RETRIEVAL_TRACE_OUTCOME_VALUES: &[&str] = &[
    "injected",
    "empty",
    "error",
    "legacy_unknown",
    "disabled_off",
    "disabled_kill_switch",
    "disabled_legacy",
];

// ── Skipped-reason vocabulary ─────────────────────────────────────────────────

/// Reason a candidate was skipped (not injected).
///
/// The vocabulary is fixed by the proposal. `skipped_reason` is nullable only
/// for injected candidates — those have `None` here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

// ── Health rollup workload entry points ─────────────────────────────────────────

/// Entry points that are considered workload retrieval/injection sites.
///
/// The `memory_recall_trace` MCP tool is intentionally excluded from health
/// rollups because it is operator-facing telemetry rather than production
/// dispatch/injection workload.
pub const WORKLOAD_ENTRY_POINTS: &[&str] = &[
    "dispatch",
    "jit_pitfalls",
    "load_knowledge_context",
    "format_knowledge_notes",
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
    /// Verbatim rollout cohort/flag label; unlike outcome this is intentionally
    /// not constrained to a closed vocabulary.
    pub rollout_label: String,
    /// Typed trace-level result, distinct from per-candidate outcomes.
    pub outcome: RetrievalTraceOutcome,
    pub knowledge_trace_taxonomy_version: Option<i32>,
    pub terminal_state: Option<String>,
    pub terminal_at: Option<String>,
    pub candidate_count: Option<i32>,
    pub injected_count: Option<i32>,
    pub confidence_filtered_count: Option<i32>,
    pub not_top_k_count: Option<i32>,
    pub oversized_skipped_count: Option<i32>,
    pub budget_pruned_count: Option<i32>,
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
    /// Filter by the verbatim rollout label.
    pub rollout_label: Option<&'a str>,
    /// Filter by the trace-level outcome. This is independent from the
    /// candidate-level `outcome` filter below.
    pub trace_outcome: Option<RetrievalTraceOutcome>,
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

// ── Health rollup support ───────────────────────────────────────────────────

mod retrieval_trace_health;
use retrieval_trace_health::*;
pub use retrieval_trace_health::{
    CandidateScoreSummary, DurationStageSummary, RetrievalTaxonomyValidationError,
    RetrievalTraceHealthEvidence, RetrievalTraceHealthRollup, SkipReasonCounts,
    TaxonomyV1RetrievalHealthCounts, TaxonomyV1RetrievalHealthGroup,
};

// ── Repository ────────────────────────────────────────────────────────────────

pub struct RetrievalTraceRepository {
    db: Database,
}

impl RetrievalTraceRepository {
    /// Return only positively injected candidate IDs for one exact emitting
    /// attribution tuple. Skipped candidates, including legacy candidates
    /// without an explicit outcome, are never eligibility evidence.
    pub async fn injected_candidate_note_ids_for_attribution(
        &self,
        project_id: &str,
        session_id: &str,
        task_id: &str,
        task_run_id: Option<&str>,
    ) -> Result<HashSet<String>> {
        self.db.ensure_initialized().await?;
        let rows: Vec<RetrievalTraceRow> = sqlx::query_as(
            r#"SELECT id, schema_version, project_id, session_id, task_run_id, task_id,
                entry_point, rollout_label, outcome, knowledge_trace_taxonomy_version, terminal_state, terminal_at, candidate_count, injected_count, confidence_filtered_count, not_top_k_count, oversized_skipped_count, budget_pruned_count, trigger, candidates,
                candidate_cap, candidate_cap_exceeded, sampling_metadata,
                durations_ms, estimated_injected_tokens, created_at
            FROM retrieval_traces
            WHERE project_id = $1 AND session_id = $2 AND task_id = $3
              AND task_run_id IS NOT DISTINCT FROM $4
            ORDER BY created_at DESC, id DESC
            LIMIT $5"#,
        )
        .bind(project_id)
        .bind(session_id)
        .bind(task_id)
        .bind(task_run_id)
        .bind(DEFAULT_RETRIEVAL_TRACE_LIMIT)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .flat_map(|row| row.candidates_typed())
            .filter(|candidate| candidate.outcome == CandidateOutcome::Injected)
            .map(|candidate| candidate.note_id)
            .collect())
    }

    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Insert a new retrieval trace row and return the persisted record.
    pub async fn insert(
        &self,
        params: CreateRetrievalTraceParams<'_>,
    ) -> Result<RetrievalTraceRow> {
        let outcome =
            classify_legacy_trace_outcome(params.candidates, params.estimated_injected_tokens);
        self.insert_with_values(params, "enabled", outcome).await
    }

    /// Insert a trace with caller-supplied rollout/outcome semantics.
    pub async fn insert_with_semantics(
        &self,
        params: CreateRetrievalTraceWithSemanticsParams<'_>,
    ) -> Result<RetrievalTraceRow> {
        validate_explicit_trace_semantics(
            params.trace.candidates,
            params.trace.estimated_injected_tokens,
            params.outcome,
        )?;
        self.insert_with_values(params.trace, params.rollout_label, params.outcome)
            .await
    }

    pub async fn insert_terminal(
        &self,
        params: CreateRetrievalTraceTerminalParams<'_>,
    ) -> Result<RetrievalTraceRow> {
        validate_terminal_trace(&params)?;
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let counts = params.dispositions;
        sqlx::query_as::<_, RetrievalTraceRow>(r#"INSERT INTO retrieval_traces (id,schema_version,project_id,session_id,task_run_id,task_id,entry_point,trigger,candidates,candidate_cap,candidate_cap_exceeded,sampling_metadata,durations_ms,estimated_injected_tokens,rollout_label,outcome,knowledge_trace_taxonomy_version,terminal_state,terminal_at,candidate_count,injected_count,confidence_filtered_count,not_top_k_count,oversized_skipped_count,budget_pruned_count) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25) RETURNING id,schema_version,project_id,session_id,task_run_id,task_id,entry_point,rollout_label,outcome,knowledge_trace_taxonomy_version,terminal_state,terminal_at,candidate_count,injected_count,confidence_filtered_count,not_top_k_count,oversized_skipped_count,budget_pruned_count,trigger,candidates,candidate_cap,candidate_cap_exceeded,sampling_metadata,durations_ms,estimated_injected_tokens,created_at"#)
        .bind(&id).bind(RETRIEVAL_TRACE_SCHEMA_VERSION).bind(params.trace.project_id).bind(params.trace.session_id).bind(params.trace.task_run_id).bind(params.trace.task_id).bind(params.trace.entry_point.as_str()).bind(params.trace.trigger).bind(params.trace.candidates).bind(params.trace.candidate_cap).bind(params.trace.candidate_cap_exceeded).bind(params.trace.sampling_metadata).bind(params.trace.durations_ms).bind(params.trace.estimated_injected_tokens).bind(params.rollout_label).bind(params.outcome.as_str()).bind(KNOWLEDGE_TRACE_TAXONOMY_VERSION_V1).bind(params.terminal_state.as_str()).bind(params.terminal_at).bind(params.candidate_count).bind(params.injected_count).bind(counts.map(|v|v.confidence_filtered)).bind(counts.map(|v|v.not_top_k)).bind(counts.map(|v|v.oversized_skipped)).bind(counts.map(|v|v.budget_pruned)).fetch_one(self.db.pool()).await.map_err(Into::into)
    }

    async fn insert_with_values(
        &self,
        params: CreateRetrievalTraceParams<'_>,
        rollout_label: &str,
        outcome: RetrievalTraceOutcome,
    ) -> Result<RetrievalTraceRow> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO retrieval_traces (
                    id, schema_version, project_id, session_id, task_run_id, task_id,
                    entry_point, trigger, candidates,
                    candidate_cap, candidate_cap_exceeded, sampling_metadata,
                    durations_ms, estimated_injected_tokens, rollout_label, outcome
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"#,
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
        .bind(rollout_label)
        .bind(outcome.as_str())
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
        if filter.rollout_label.is_some() {
            conditions.push(format!("rollout_label = ${bind_pos}"));
            bind_pos += 1;
        }
        if filter.trace_outcome.is_some() {
            conditions.push(format!("outcome = ${bind_pos}"));
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
                entry_point, rollout_label, outcome, knowledge_trace_taxonomy_version, terminal_state, terminal_at, candidate_count, injected_count, confidence_filtered_count, not_top_k_count, oversized_skipped_count, budget_pruned_count, trigger, candidates,
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
        if let Some(rollout_label) = filter.rollout_label {
            query = query.bind(rollout_label);
        }
        if let Some(outcome) = filter.trace_outcome {
            query = query.bind(outcome.as_str());
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

    /// Update a trace timestamp for controlled historical-data setup.
    ///
    /// Retrieval traces are normally immutable after insertion. This narrow
    /// repository operation keeps test and maintenance callers from bypassing
    /// the database boundary when they need a deterministic rollup window.
    pub async fn update_created_at(&self, id: &str, created_at: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE retrieval_traces SET created_at = $2 WHERE id = $1")
            .bind(id)
            .bind(created_at)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Corrupt a persisted taxonomy terminal's injected count for validation tests.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn corrupt_injected_count_for_test(
        &self,
        id: &str,
        injected_count: i32,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE retrieval_traces SET injected_count = $2 WHERE id = $1")
            .bind(id)
            .bind(injected_count)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Put a legacy trace in a terminal-time window for rollup contract tests.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn update_terminal_at_for_test(&self, id: &str, terminal_at: &str) -> Result<()> {
        parse_retrieval_trace_utc_timestamp(terminal_at, "terminal_at")?;
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE retrieval_traces SET terminal_at = $2 WHERE id = $1")
            .bind(id)
            .bind(terminal_at)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Return authoritative taxonomy-v1 terminal evidence for every project
    /// and supported entry point in one `[from, until)` window.
    pub async fn taxonomy_v1_health_rollup(
        &self,
        from: &str,
        until: &str,
        refreshed_at: &str,
    ) -> Result<Vec<TaxonomyV1RetrievalHealthGroup>> {
        let from_timestamp = parse_retrieval_trace_utc_timestamp(from, "from")?;
        let until_timestamp = parse_retrieval_trace_utc_timestamp(until, "until")?;
        parse_retrieval_trace_utc_timestamp(refreshed_at, "refreshed_at")?;
        if from_timestamp >= until_timestamp {
            return Err(DbError::InvalidData(
                "taxonomy-v1 health rollup requires from before until".to_owned(),
            ));
        }
        let from = format_retrieval_trace_utc_timestamp(from_timestamp);
        let until = format_retrieval_trace_utc_timestamp(until_timestamp);
        self.db.ensure_initialized().await?;
        let rows = fetch_taxonomy_v1_health_groups(self.db.pool(), &from, &until).await?;
        rows.into_iter()
            .map(|row| {
                build_taxonomy_v1_group(row, &from, &until, refreshed_at)
                    .map_err(DbError::InvalidData)
            })
            .collect()
    }

    /// Prune trace rows older than an eligible retention cutoff for one project.
    ///
    /// The control-plane/server maintenance caller owns invoking this method.
    /// `before_cutoff` and `reference_time` must be valid ISO-8601 UTC timestamp
    /// strings (for example `2026-07-01T00:00:00.000Z`). The explicit reference
    /// time avoids database `now()` and makes the protected boundary deterministic.
    ///
    /// A cutoff later than `reference_time -
    /// MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW` is rejected with
    /// [`DbError::InvalidData`]; it cannot delete report-supported traces.
    /// Eligible deletion remains project-scoped and strictly `created_at <
    /// before_cutoff`. Retained explicit outcomes, including `disabled_off` and
    /// `disabled_kill_switch`, are not modified or reclassified. The minimum
    /// window is the maximum lookback supported for the sibling `fv7a` outcomes
    /// report; a request beyond that coverage needs a truthful diagnostic.
    ///
    /// Returns the number of rows deleted. Errors are returned without imposing
    /// caller logging or fail-open policy.
    pub async fn prune_older_than(
        &self,
        project_id: &str,
        before_cutoff: &str,
        reference_time: &str,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let before_cutoff = parse_retrieval_trace_utc_timestamp(before_cutoff, "before_cutoff")?;
        let reference_time = parse_retrieval_trace_utc_timestamp(reference_time, "reference_time")?;
        let minimum_window =
            chrono::Duration::from_std(MINIMUM_RETRIEVAL_TRACE_RETENTION_WINDOW)
                .map_err(|err| DbError::InvalidData(format!("invalid retention window: {err}")))?;
        let protected_boundary = reference_time - minimum_window;
        if before_cutoff > protected_boundary {
            return Err(DbError::InvalidData(format!(
                "before_cutoff {} is inside the protected retrieval-trace retention window; latest eligible cutoff is {}",
                format_retrieval_trace_utc_timestamp(before_cutoff),
                format_retrieval_trace_utc_timestamp(protected_boundary),
            )));
        }
        let before_cutoff = format_retrieval_trace_utc_timestamp(before_cutoff);

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
        entry_point, rollout_label, outcome, knowledge_trace_taxonomy_version, terminal_state, terminal_at, candidate_count, injected_count, confidence_filtered_count, not_top_k_count, oversized_skipped_count, budget_pruned_count, trigger, candidates,
        candidate_cap, candidate_cap_exceeded, sampling_metadata,
        durations_ms, estimated_injected_tokens, created_at
    FROM retrieval_traces
    WHERE id = $1
"#;

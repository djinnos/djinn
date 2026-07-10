//! F2 — Just-in-time pitfall retrieval on the first file modification.
//!
//! Knowledge notes are injected once, statically, by path scope at session
//! start (see `prompt_context::knowledge_context`). That misses the
//! highest-leverage moment: the instant the agent first *modifies* a file.
//! This module surfaces the top scoped `pitfall`/`pattern` notes overlapping
//! the touched path the FIRST time a `write`/`edit`/`apply_patch` runs in a
//! session — transiently, appended to that one tool result, never persisted.
//!
//! ## Config gate
//! Controlled by `DJINN_JIT_PITFALLS_ROLLOUT` (default OFF for this pre-read
//! wave). Supported values are `off`/unset (default-off), `enabled`/`on`/`1`
//! (operator opt-in), `cohort`/`staging` (controlled rollout traffic), and
//! `disabled`/`kill_switch`/`0` (explicit operator kill switch). The legacy
//! `DJINN_JIT_PITFALLS=1` env var is still accepted as a migration opt-in only
//! when the rollout env var is unset. When disabled the hot path records a
//! structured default-off or kill-switch outcome but behaviour remains
//! byte-identical to the pre-F2 output: no DB search and no hint.
//!
//! ## Once-per-session
//! The "first modification" is tracked process-wide by session id (the
//! worktree path string — the same key `FileTime` uses), in a `OnceLock`-backed
//! `HashSet`. The first `write`/`edit`/`apply_patch` for a given session
//! inserts the key and runs the search; every subsequent modification in that
//! session sees the key already present and does nothing extra.
//!
//! ## Resilience
//! A search error or empty result NEVER fails the write — the hint is simply
//! skipped and the original tool result is returned unchanged.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

use crate::context::AgentContext;

const TELEMETRY_TARGET: &str = "djinn_agent::jit_pitfalls";

// ── Trace classification constants (synced with the production query) ────────
//
// These mirror the threshold/limit values passed to
// `NoteRepository::query_by_scope_overlap` below in `maybe_pitfall_hint`. They
// live here as constants so the trace classifier and helper tests can reason
// about the `min_confidence` vs `not_top_k` boundary without re-parsing the
// call site. Changing the production call MUST update these constants in
// lock-step, otherwise traces will misclassify the boundary cases.
const JIT_TRACE_PROD_MIN_CONFIDENCE: f64 = 0.3;
/// Top-K rendered into the `<relevant-pitfalls>` hint block. Trace candidates
/// ranked above this in the unfiltered universe are recorded as
/// `SkippedReason::NotTopK` when not selected for injection.
const JIT_TRACE_PROD_TOP_K: usize = 2;
/// Over-fetch multiplier used by the production `query_by_scope_overlap`
/// call. The production call asks for `top_k * overfetch` rows up front and
/// then takes the top `top_k`. We surface this in trace metadata so operators
/// can tell why some over-fetched rows were not rendered.
const JIT_TRACE_PROD_OVERFETCH: usize = 4;
/// Note type filter mirrored from the production `query_by_scope_overlap`
/// call. Trace candidate classification and trigger metadata use this list
/// verbatim so trace rows reflect exactly what the production query asked
/// for.
const JIT_TRACE_PROD_NOTE_TYPES: &[&str] = &["pitfall", "pattern"];
/// Production over-fetch upper bound — `top_k * overfetch`.
const JIT_TRACE_PROD_QUERY_LIMIT: usize = JIT_TRACE_PROD_TOP_K * JIT_TRACE_PROD_OVERFETCH;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JitPitfallOutcome {
    DisabledDefaultOff,
    DisabledKillSwitch,
    NonFirstModification,
    EligibleSearch,
    Injected,
    Empty,
    Error,
}

impl JitPitfallOutcome {
    fn label(self) -> &'static str {
        use djinn_telemetry::jit_pitfalls as telemetry;

        match self {
            Self::DisabledDefaultOff => telemetry::OUTCOME_DISABLED_DEFAULT_OFF,
            Self::DisabledKillSwitch => telemetry::OUTCOME_DISABLED_KILL_SWITCH,
            Self::NonFirstModification => telemetry::OUTCOME_NON_FIRST_MODIFICATION,
            Self::EligibleSearch => telemetry::OUTCOME_ELIGIBLE_SEARCH,
            Self::Injected => telemetry::OUTCOME_INJECTED,
            Self::Empty => telemetry::OUTCOME_EMPTY,
            Self::Error => telemetry::OUTCOME_ERROR,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JitPitfallRolloutMode {
    DefaultOff,
    Enabled,
    Cohort,
    KillSwitch,
    LegacyOptIn,
}

impl JitPitfallRolloutMode {
    fn label(self) -> &'static str {
        match self {
            Self::DefaultOff => "default_off",
            Self::Enabled => "enabled",
            Self::Cohort => "cohort",
            Self::KillSwitch => "kill_switch",
            Self::LegacyOptIn => "legacy_opt_in",
        }
    }

    fn enabled(self) -> bool {
        matches!(self, Self::Enabled | Self::Cohort | Self::LegacyOptIn)
    }

    fn disabled_outcome(self) -> JitPitfallOutcome {
        match self {
            Self::KillSwitch => JitPitfallOutcome::DisabledKillSwitch,
            _ => JitPitfallOutcome::DisabledDefaultOff,
        }
    }
}

const ROLLOUT_ENV: &str = "DJINN_JIT_PITFALLS_ROLLOUT";
const LEGACY_ENV: &str = "DJINN_JIT_PITFALLS";

#[derive(Debug, PartialEq)]
struct SafeNoteTelemetry {
    rank: usize,
    id: String,
    permalink: String,
    note_type: String,
    confidence: f64,
}

/// Resolve the F2 controlled rollout mode from env.
///
/// `DJINN_JIT_PITFALLS_ROLLOUT` is the primary operator surface. Its explicit
/// disable/kill-switch values override every other input. The legacy
/// `DJINN_JIT_PITFALLS=1` one-bit opt-in remains as migration compatibility only
/// when the primary rollout env var is unset.
fn rollout_mode_from_env() -> JitPitfallRolloutMode {
    let rollout = std::env::var(ROLLOUT_ENV).ok();
    let legacy = std::env::var(LEGACY_ENV).ok();
    rollout_mode_from_values(rollout.as_deref(), legacy.as_deref())
}

fn rollout_mode_from_values(rollout: Option<&str>, legacy: Option<&str>) -> JitPitfallRolloutMode {
    if let Some(value) = rollout.map(str::trim).filter(|value| !value.is_empty()) {
        match value.to_ascii_lowercase().replace('-', "_").as_str() {
            "enabled" | "enable" | "on" | "true" | "1" => JitPitfallRolloutMode::Enabled,
            "cohort" | "staging" | "rollout" | "controlled" => JitPitfallRolloutMode::Cohort,
            "off" => JitPitfallRolloutMode::DefaultOff,
            "disabled" | "disable" | "kill_switch" | "killswitch" | "false" | "0" => {
                JitPitfallRolloutMode::KillSwitch
            }
            _ => JitPitfallRolloutMode::DefaultOff,
        }
    } else if legacy.map(str::trim) == Some("1") {
        JitPitfallRolloutMode::LegacyOptIn
    } else {
        JitPitfallRolloutMode::DefaultOff
    }
}

/// Process-wide set of session ids that have already had their first
/// modification observed. Sessions are short-lived and keyed by worktree path
/// string, so unbounded growth is not a practical concern over a worker's
/// lifetime.
fn seen_sessions() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Returns `true` exactly once per `session_id` — on the first call. A poisoned
/// lock degrades safely to "not first" (skip the hint) rather than panicking.
fn claim_first_modification(session_id: &str) -> bool {
    match seen_sessions().lock() {
        Ok(mut set) => set.insert(session_id.to_string()),
        Err(_) => false,
    }
}

// ── Trace persistence helpers (epic 3paf) ────────────────────────────────────
//
// `maybe_pitfall_hint` calls the helpers below from every eligible search
// path (gated to non-disabled, first-modification) and never from the
// disabled/non-first shortcuts. Failures inside these helpers log a warning
// and never change the returned `Option<String>` or any Prometheus counter.
// Helpers are pure/synchronous where possible so unit tests can cover
// classification without DB setup.

/// Build the JSON trigger payload describing the trace row that
/// [`persist_jit_trace`] will persist for one eligible JIT search.
///
/// `search_error` is set only for the error path — otherwise the trace row
/// records the search that ran without leaking an outcome that has not yet
/// been observed (the search has just completed by the time we trace it).
fn build_trace_trigger(
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    rendered_note_count: usize,
    result_count: usize,
    min_confidence: f64,
    production_limit: usize,
    search_error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "rollout_mode": rollout_mode.label(),
        "touched_paths": touched_paths,
        "touched_path_count": touched_paths.len(),
        "touched_path_summary": touched_path_summary(touched_paths),
        "rendered_note_count": rendered_note_count,
        "result_count": result_count,
        "min_confidence": min_confidence,
        "production_limit": production_limit,
        "note_types": JIT_TRACE_PROD_NOTE_TYPES,
        "search_error": search_error,
    })
}

/// Build the per-phase durations JSON object for the trace row.
///
/// `search_elapsed_ms` is mandatory for every trace (it covers the production
/// search). `trace_search_elapsed_ms` and `persist_elapsed_ms` are populated
/// only when those phases actually ran and are absent (key missing) when
/// skipped, preserving a clear "did this phase run?" signal in metadata
/// rather than baking it into the durations object.
fn build_trace_durations_ms(
    search_elapsed_ms: u64,
    trace_search_elapsed_ms: Option<u64>,
    persist_elapsed_ms: Option<u64>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "search_elapsed_ms".to_owned(),
        serde_json::Value::Number(search_elapsed_ms.into()),
    );
    if let Some(trace_ms) = trace_search_elapsed_ms {
        obj.insert(
            "trace_search_elapsed_ms".to_owned(),
            serde_json::Value::Number(trace_ms.into()),
        );
    }
    if let Some(persist_ms) = persist_elapsed_ms {
        obj.insert(
            "persist_elapsed_ms".to_owned(),
            serde_json::Value::Number(persist_ms.into()),
        );
    }
    serde_json::Value::Object(obj)
}

/// Classify a single `ScopeOverlapTraceCandidate` row into a
/// [`TraceCandidate`] for JSONB persistence.
///
/// The boundary rule (mirrors the production `maybe_pitfall_hint` semantics):
///
/// 1. If `candidate.note_id` appears in `injected_note_ids`, the candidate
///    is marked `CandidateOutcome::Injected` and `skipped_reason = None`.
///    This is the deterministic "top-2 over-fetched, then take top 2" merge
///    with the production selection.
/// 2. Otherwise, if `candidate.confidence < min_confidence`, it is marked
///    `CandidateOutcome::Skipped` with `SkippedReason::MinConfidence` —
///    the deterministic production confidence floor.
/// 3. Otherwise, it is marked `CandidateOutcome::Skipped` with
///    `SkippedReason::NotTopK`.
///
/// `source` is `"scope_overlap"` to match the existing `5wdh` data-layer
/// contract; `scope` carries the `scope_paths`/`note_type`/`folder` triple
/// consumed by `liso` (`memory_recall_trace` tooling).
fn classify_trace_candidate(
    candidate: &djinn_db::repositories::note::ScopeOverlapTraceCandidate,
    injected_note_ids: &HashSet<String>,
    min_confidence: f64,
) -> djinn_db::repositories::retrieval_trace::TraceCandidate {
    use djinn_db::repositories::retrieval_trace::{
        CandidateOutcome, SkippedReason, TraceCandidate,
    };

    let rank_i32 = i32::try_from(candidate.rank).unwrap_or(i32::MAX);
    let scope_value = serde_json::from_str::<serde_json::Value>(&candidate.scope_paths)
        .unwrap_or_else(|_| serde_json::Value::String(candidate.scope_paths.clone()));
    let scope_object = serde_json::json!({
        "scope_paths": scope_value,
        "note_type": candidate.note_type,
        "folder": candidate.folder,
    });

    if injected_note_ids.contains(&candidate.id) {
        TraceCandidate {
            note_id: candidate.id.clone(),
            permalink: Some(candidate.permalink.clone()),
            title: Some(candidate.title.clone()),
            outcome: CandidateOutcome::Injected,
            rank: Some(rank_i32),
            confidence: Some(candidate.confidence),
            skipped_reason: None,
            source: Some("scope_overlap".to_owned()),
            scope: Some(scope_object),
        }
    } else if candidate.confidence < min_confidence {
        TraceCandidate {
            note_id: candidate.id.clone(),
            permalink: Some(candidate.permalink.clone()),
            title: Some(candidate.title.clone()),
            outcome: CandidateOutcome::Skipped,
            rank: Some(rank_i32),
            confidence: Some(candidate.confidence),
            skipped_reason: Some(SkippedReason::MinConfidence),
            source: Some("scope_overlap".to_owned()),
            scope: Some(scope_object),
        }
    } else {
        TraceCandidate {
            note_id: candidate.id.clone(),
            permalink: Some(candidate.permalink.clone()),
            title: Some(candidate.title.clone()),
            outcome: CandidateOutcome::Skipped,
            rank: Some(rank_i32),
            confidence: Some(candidate.confidence),
            skipped_reason: Some(SkippedReason::NotTopK),
            source: Some("scope_overlap".to_owned()),
            scope: Some(scope_object),
        }
    }
}

/// Persist a `RetrievalTraceEntryPoint::JitPitfalls` row, fail-open.
///
/// The function NEVER returns an error and NEVER changes the
/// caller-visible `Option<String>` value of `maybe_pitfall_hint`. Repository
/// insert failures log a warning and continue. The trace row's `candidates`
/// JSONB is whatever the caller passes (typically already-validated by
/// `TraceCandidate::validate_invariants`).
// Allow the wide signature: each argument is a distinct piece of trace
// metadata that should stay separate at the call sites for grep-ability.
#[allow(clippy::too_many_arguments)]
async fn persist_jit_trace(
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    trace_candidates_json: &serde_json::Value,
    durations_ms: &serde_json::Value,
    trigger: &serde_json::Value,
    estimated_injected_tokens: i32,
    candidate_cap: i32,
    candidate_cap_exceeded: bool,
) {
    use djinn_db::repositories::retrieval_trace::{
        CreateRetrievalTraceParams, RetrievalTraceEntryPoint, RetrievalTraceRepository,
    };

    let trace_repo = RetrievalTraceRepository::new(db.clone());
    let params = CreateRetrievalTraceParams {
        project_id,
        session_id: Some(session_id),
        task_run_id: None,
        task_id: None,
        entry_point: RetrievalTraceEntryPoint::JitPitfalls,
        trigger: Some(trigger),
        candidates: trace_candidates_json,
        candidate_cap,
        candidate_cap_exceeded,
        sampling_metadata: None,
        durations_ms,
        estimated_injected_tokens,
    };

    if let Err(e) = trace_repo.insert(params).await {
        tracing::warn!(
            target: TELEMETRY_TARGET,
            session_id = %session_id,
            project_id = %project_id,
            error = %e,
            "jit_pitfalls: failed to persist retrieval trace; continuing fail-open",
        );
    }
}

/// Estimate the number of tokens represented by a rendered hint block.
///
/// Approximation: `chars / 4` rounded up. Documented here as the canonical
/// JIT estimator so downstream analysis can compare measurements against a
/// single shared assumption; the column is documented as an estimate rather
/// than an exact token count.
fn estimate_injected_tokens(block_chars: usize) -> i32 {
    let chars = u32::try_from(block_chars).unwrap_or(u32::MAX);
    // Round-up division so a non-empty block always reports >= 1.
    let tokens = chars.div_ceil(4);
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

/// Persist an "empty production result" trace row mirroring the empty-path
/// semantic of `maybe_pitfall_hint`.
///
/// This helper used to skip the trace candidate universe on this path: the
/// production `query_by_scope_overlap(..., min_confidence=0.3, limit=8)`
/// filtered below-threshold notes out before they reached the classifier, so
/// a confidence-0.10 note overlapping a touched path silently disappeared
/// from the trace. The 5wdh data-layer exposes
/// [`djinn_db::NoteRepository::query_by_scope_overlap_trace_candidates`]
/// specifically so the trace universe can include below-threshold candidates
/// for deterministic `MinConfidence` classification.
///
/// This helper therefore performs the trace candidate fetch itself. The
/// production search returned empty → the injected set is empty by
/// construction → every fetched candidate is classified as either
/// `SkippedReason::MinConfidence` (confidence below
/// [`JIT_TRACE_PROD_MIN_CONFIDENCE`]) or `SkippedReason::NotTopK` (above the
/// floor but no injected candidates means the top-K is empty). Failures in
/// the universe fetch, the classification invariants, the JSON
/// serialization, or the repository insert are each fail-open — a warning is
/// logged and, where possible, a metadata-only row is still persisted so the
/// trace ledger records the attempted search.
#[allow(clippy::too_many_arguments)]
async fn persist_jit_empty_trace(
    note_repo: &djinn_db::NoteRepository,
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    search_elapsed_ms: u64,
    candidate_cap: i32,
) {
    use djinn_db::repositories::retrieval_trace::TraceCandidate;

    let persist_started = SystemClockTrait::new().now_instant();
    let trace_search_started = SystemClockTrait::new().now_instant();
    let trace_universe = note_repo
        .query_by_scope_overlap_trace_candidates(
            project_id,
            touched_paths,
            JIT_TRACE_PROD_NOTE_TYPES,
            candidate_cap as usize,
        )
        .await;
    let trace_search_elapsed_ms = elapsed_millis(trace_search_started);

    let scope_candidates = match trace_universe {
        Ok(candidates) => candidates,
        Err(trace_err) => {
            // Fail-open: log a warning and persist a metadata-only row
            // carrying the error in the trigger. Without candidate ids the
            // trace row records the attempted search but not the typed
            // `MinConfidence`/`NotTopK` classification.
            tracing::warn!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                error = %trace_err,
                "jit_pitfalls: empty-result trace candidate query failed; \
                 persisting metadata-only JitPitfalls trace (fail-open)",
            );
            let trigger = build_trace_trigger(
                rollout_mode,
                touched_paths,
                0,
                0,
                JIT_TRACE_PROD_MIN_CONFIDENCE,
                JIT_TRACE_PROD_QUERY_LIMIT,
                Some(&format!("trace_candidate_query: {trace_err}")),
            );
            let durations_ms =
                build_trace_durations_ms(search_elapsed_ms, Some(trace_search_elapsed_ms), None);
            persist_jit_trace(
                db,
                session_id,
                project_id,
                &serde_json::json!([]),
                &durations_ms,
                &trigger,
                0,
                candidate_cap,
                false,
            )
            .await;
            let persist_elapsed_ms = elapsed_millis(persist_started);
            tracing::debug!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                trace_search_elapsed_ms = trace_search_elapsed_ms,
                persist_elapsed_ms = persist_elapsed_ms,
                "jit_pitfalls: persisted empty-result search_error trace (fail-open)",
            );
            return;
        }
    };

    // Production returned no notes → the injected set is empty by
    // construction. Classify the universe against that empty set so the
    // boundary branch in `classify_trace_candidate` is exercised: anything
    // below `JIT_TRACE_PROD_MIN_CONFIDENCE` surfaces as
    // `SkippedReason::MinConfidence` (this is the deterministic
    // classification the 5wdh data-layer was extended for), and anything
    // above the floor lands in `SkippedReason::NotTopK` (no injected
    // candidates means the production top-K is empty).
    let empty_injected: HashSet<String> = HashSet::new();
    let trace_candidates: Vec<TraceCandidate> = scope_candidates
        .iter()
        .map(|candidate| {
            classify_trace_candidate(candidate, &empty_injected, JIT_TRACE_PROD_MIN_CONFIDENCE)
        })
        .collect();
    let trace_universe_count = scope_candidates.len();
    let candidate_cap_exceeded = (trace_universe_count as i64) > i64::from(candidate_cap);

    if let Err(validation_err) =
        djinn_db::repositories::retrieval_trace::validate_candidates(&trace_candidates)
    {
        // Fail-open: the production empty-result outcome is already decided;
        // skip the trace row and warn-log. The returned `Option<String>` and
        // the `Empty` telemetry outcome are unchanged.
        tracing::warn!(
            target: TELEMETRY_TARGET,
            session_id = %session_id,
            project_id = %project_id,
            error = %validation_err,
            "jit_pitfalls: empty-path trace candidates failed validation; \
             skipping trace persistence (fail-open)",
        );
        return;
    }
    let trace_candidates_json = match serde_json::to_value(&trace_candidates) {
        Ok(json) => json,
        Err(ser_err) => {
            tracing::warn!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                error = %ser_err,
                "jit_pitfalls: failed to serialize empty-path trace candidates; \
                 skipping trace persistence (fail-open)",
            );
            return;
        }
    };

    // `rendered_note_count = 0` (no notes were rendered); `result_count` is
    // the trace universe size so downstream tooling can distinguish
    // "production returned empty, typed candidates persisted" from a
    // metadata-only row.
    let trigger = build_trace_trigger(
        rollout_mode,
        touched_paths,
        0,
        trace_universe_count,
        JIT_TRACE_PROD_MIN_CONFIDENCE,
        JIT_TRACE_PROD_QUERY_LIMIT,
        None,
    );
    let durations_ms =
        build_trace_durations_ms(search_elapsed_ms, Some(trace_search_elapsed_ms), None);

    persist_jit_trace(
        db,
        session_id,
        project_id,
        &trace_candidates_json,
        &durations_ms,
        &trigger,
        0,
        candidate_cap,
        candidate_cap_exceeded,
    )
    .await;
    let persist_elapsed_ms = elapsed_millis(persist_started);
    tracing::debug!(
        target: TELEMETRY_TARGET,
        session_id = %session_id,
        project_id = %project_id,
        trace_universe_count = trace_universe_count,
        trace_search_elapsed_ms = trace_search_elapsed_ms,
        persist_elapsed_ms = persist_elapsed_ms,
        "jit_pitfalls: persisted empty-result JitPitfalls trace with typed candidates (fail-open)",
    );
}

/// Persist a search-error trace row mirroring the error-path semantic of
/// `maybe_pitfall_hint`. The trace carries an empty candidates array so the
/// row exists (the search was attempted) but the `search_error` field lives
/// in the trigger metadata. Failure path is fail-open — exactly like a
/// successful insert.
#[allow(clippy::too_many_arguments)]
async fn persist_jit_error_trace(
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    search_elapsed_ms: u64,
    error: &str,
    candidate_cap: i32,
    candidate_cap_exceeded: bool,
) {
    let trigger = build_trace_trigger(
        rollout_mode,
        touched_paths,
        0,
        0,
        JIT_TRACE_PROD_MIN_CONFIDENCE,
        JIT_TRACE_PROD_QUERY_LIMIT,
        Some(error),
    );
    let durations_ms = build_trace_durations_ms(search_elapsed_ms, None, None);
    let empty_candidates = serde_json::json!([]);
    persist_jit_trace(
        db,
        session_id,
        project_id,
        &empty_candidates,
        &durations_ms,
        &trigger,
        0,
        candidate_cap,
        candidate_cap_exceeded,
    )
    .await;
}

fn touched_path_summary(touched_paths: &[String]) -> String {
    let mut dirs = BTreeSet::new();
    let mut extensions = BTreeSet::new();

    for path in touched_paths {
        if let Some(first) = path.split('/').find(|part| !part.is_empty()) {
            dirs.insert(first.to_string());
        }
        if let Some(ext) = std::path::Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .filter(|ext| !ext.is_empty())
        {
            extensions.insert(ext.to_string());
        }
    }

    let dirs = dirs.into_iter().take(5).collect::<Vec<_>>().join(",");
    let extensions = extensions.into_iter().take(5).collect::<Vec<_>>().join(",");
    format!(
        "count={};dirs={};extensions={}",
        touched_paths.len(),
        if dirs.is_empty() {
            "none"
        } else {
            dirs.as_str()
        },
        if extensions.is_empty() {
            "none"
        } else {
            extensions.as_str()
        }
    )
}

fn safe_note_metadata(notes: &[djinn_memory::Note]) -> Vec<SafeNoteTelemetry> {
    notes
        .iter()
        .take(2)
        .enumerate()
        .map(|(idx, note)| SafeNoteTelemetry {
            rank: idx + 1,
            id: note.id.clone(),
            permalink: note.permalink.clone(),
            note_type: note.note_type.clone(),
            confidence: note.confidence,
        })
        .collect()
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn record_outcome(
    outcome: JitPitfallOutcome,
    rollout_mode: JitPitfallRolloutMode,
    session_id: &str,
    project_id: Option<&str>,
    touched_paths: &[String],
) {
    djinn_telemetry::jit_pitfalls::increment_outcome(outcome.label());
    tracing::info!(
        target: TELEMETRY_TARGET,
        outcome = outcome.label(),
        rollout_mode = rollout_mode.label(),
        session_id = %session_id,
        project_id = project_id.unwrap_or(""),
        touched_path_count = touched_paths.len(),
        touched_path_summary = %touched_path_summary(touched_paths),
        "jit_pitfalls telemetry outcome"
    );
}

/// Build the `<relevant-pitfalls>…</relevant-pitfalls>` hint block for the
/// FIRST modification of a session, if the gate is on and a scoped search
/// surfaces matching notes. Returns `None` (→ no append) when the gate is off,
/// when this is not the session's first modification, on any search error, or
/// when the search yields no matching notes.
///
/// `session_id` is the per-session key (the worktree path string).
/// `project_id` scopes the note search. `touched_paths` are repo-relative
/// paths of the files this modification touched.
pub(super) async fn maybe_pitfall_hint(
    state: &AgentContext,
    session_id: &str,
    project_id: Option<&str>,
    touched_paths: &[String],
) -> Option<String> {
    // Gate first — when OFF this is the only work done, keeping the hot path
    // byte-identical to pre-F2.
    let rollout_mode = rollout_mode_from_env();
    if !rollout_mode.enabled() {
        record_outcome(
            rollout_mode.disabled_outcome(),
            rollout_mode,
            session_id,
            project_id,
            touched_paths,
        );
        return None;
    }

    let project_id = match project_id {
        Some(project_id) => project_id,
        None => {
            record_outcome(
                JitPitfallOutcome::Error,
                rollout_mode,
                session_id,
                None,
                touched_paths,
            );
            return None;
        }
    };
    if touched_paths.is_empty() {
        record_outcome(
            JitPitfallOutcome::Error,
            rollout_mode,
            session_id,
            Some(project_id),
            touched_paths,
        );
        return None;
    }

    // Only the FIRST modification of the session does anything. Subsequent
    // writes short-circuit here. Claiming BEFORE the search means a transient
    // search failure on the first write does not re-arm the hint for later
    // writes (one shot, by design — the static knowledge block already covers
    // the steady state).
    if !claim_first_modification(session_id) {
        record_outcome(
            JitPitfallOutcome::NonFirstModification,
            rollout_mode,
            session_id,
            Some(project_id),
            touched_paths,
        );
        return None;
    }

    let note_repo = djinn_db::NoteRepository::new(state.db.clone(), state.event_bus.clone());

    record_outcome(
        JitPitfallOutcome::EligibleSearch,
        rollout_mode,
        session_id,
        Some(project_id),
        touched_paths,
    );
    let search_started = SystemClockTrait::new().now_instant();

    let notes = match note_repo
        .query_by_scope_overlap(
            project_id,
            touched_paths,
            JIT_TRACE_PROD_NOTE_TYPES,
            JIT_TRACE_PROD_MIN_CONFIDENCE,
            // Over-fetch a little, then take the top 2 below — keeps the
            // confidence-DESC ordering from the query while tolerating
            // duplicate-scope rows.
            JIT_TRACE_PROD_QUERY_LIMIT,
        )
        .await
    {
        Ok(notes) if !notes.is_empty() => notes,
        // Empty result or any error: skip the hint, never fail the write.
        Ok(_) => {
            let elapsed_ms = elapsed_millis(search_started);
            djinn_telemetry::jit_pitfalls::increment_outcome(JitPitfallOutcome::Empty.label());
            tracing::info!(
                target: TELEMETRY_TARGET,
                outcome = JitPitfallOutcome::Empty.label(),
                rollout_mode = rollout_mode.label(),
                session_id = %session_id,
                project_id = %project_id,
                touched_path_count = touched_paths.len(),
                touched_path_summary = %touched_path_summary(touched_paths),
                search_elapsed_ms = elapsed_ms,
                result_count = 0usize,
                rendered_note_count = 0usize,
                "jit_pitfalls telemetry outcome"
            );
            // Trace persistence: empty-but-eligible → the production search
            // returned empty (no notes above the confidence floor), but the
            // trace universe still contains below-threshold matches which the
            // 5wdh `query_by_scope_overlap_trace_candidates` exposes for
            // deterministic `MinConfidence` classification. The helper performs
            // the trace fetch, classify, validate, serialize, and persist —
            // every step is fail-open so the production `Empty` outcome and
            // returned `None` are unchanged on any DB/serialization error.
            persist_jit_empty_trace(
                &note_repo,
                &state.db,
                session_id,
                project_id,
                rollout_mode,
                touched_paths,
                elapsed_ms,
                djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP,
            )
            .await;
            return None;
        }
        Err(e) => {
            let elapsed_ms = elapsed_millis(search_started);
            djinn_telemetry::jit_pitfalls::increment_outcome(JitPitfallOutcome::Error.label());
            tracing::info!(
                target: TELEMETRY_TARGET,
                outcome = JitPitfallOutcome::Error.label(),
                rollout_mode = rollout_mode.label(),
                session_id = %session_id,
                project_id = %project_id,
                touched_path_count = touched_paths.len(),
                touched_path_summary = %touched_path_summary(touched_paths),
                search_elapsed_ms = elapsed_ms,
                result_count = 0usize,
                rendered_note_count = 0usize,
                error = %e,
                "jit_pitfalls: scoped note search failed; skipping hint",
            );
            // Trace persistence: search-error → fail-open row carrying the
            // error string in `trigger.search_error`. Failures inside the
            // trace row writer also stay fail-open.
            persist_jit_error_trace(
                &state.db,
                session_id,
                project_id,
                rollout_mode,
                touched_paths,
                elapsed_ms,
                &e.to_string(),
                djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP,
                false,
            )
            .await;
            return None;
        }
    };

    let elapsed_ms = elapsed_millis(search_started);
    let rendered_note_count = notes.len().min(JIT_TRACE_PROD_TOP_K);
    let note_metadata = safe_note_metadata(&notes);
    djinn_telemetry::jit_pitfalls::increment_outcome(JitPitfallOutcome::Injected.label());
    tracing::info!(
        target: TELEMETRY_TARGET,
        outcome = JitPitfallOutcome::Injected.label(),
        rollout_mode = rollout_mode.label(),
        session_id = %session_id,
        project_id = %project_id,
        touched_path_count = touched_paths.len(),
        touched_path_summary = %touched_path_summary(touched_paths),
        search_elapsed_ms = elapsed_ms,
        result_count = notes.len(),
        rendered_note_count = rendered_note_count,
        notes = ?note_metadata,
        "jit_pitfalls telemetry outcome"
    );

    // ── Trace persistence (epic 3paf) ──────────────────────────────────────
    //
    // The eligible-search path now persists a `JitPitfalls` trace row that
    // captures the unfiltered scope-overlap candidate universe alongside the
    // production selection. Trace classification uses
    // [`classify_trace_candidate`] to label injected candidates as
    // `Injected` and the remaining universe as `MinConfidence` (below
    // [`JIT_TRACE_PROD_MIN_CONFIDENCE`]) or `NotTopK` (above threshold but
    // outside the production top-K).
    //
    // Failures in either the candidate universe fetch, the classification,
    // the JSON serialization, or the repository insert are fail-open: the
    // warning log path is the only side-effect and the returned
    // `Option<String>` plus counters are unchanged.
    let trace_search_started = SystemClockTrait::new().now_instant();
    let trace_universe = note_repo
        .query_by_scope_overlap_trace_candidates(
            project_id,
            touched_paths,
            JIT_TRACE_PROD_NOTE_TYPES,
            djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP as usize,
        )
        .await;
    let trace_search_elapsed_ms = elapsed_millis(trace_search_started);

    let persist_started = SystemClockTrait::new().now_instant();
    let candidate_cap: i32 = djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP;
    let mut candidate_cap_exceeded = false;
    let mut trace_candidates_json = serde_json::json!([]);

    match trace_universe {
        Ok(scope_candidates) if !scope_candidates.is_empty() => {
            // We don't truncate to the cap inside the DB: the SQL already
            // applies LIMIT, so the cap is recorded as "configured" but never
            // "exceeded" by the raw row count. Downstream tooling reads
            // `candidate_cap_exceeded` for sampling decisions, not for
            // hard-cap enforcement — see epic 3paf design notes.
            candidate_cap_exceeded = (scope_candidates.len() as i64) > i64::from(candidate_cap);

            // The "injected" set is exactly the production-rendered notes
            // (`render_pitfall_block` takes `min(2, notes.len())` items from
            // `notes` — which is already over-fetched DESC-ordered), so the
            // first `rendered_note_count` ids form the injected set.
            let injected_note_ids: HashSet<String> = notes
                .iter()
                .take(rendered_note_count)
                .map(|note| note.id.clone())
                .collect();

            let trace_candidates: Vec<djinn_db::repositories::retrieval_trace::TraceCandidate> =
                scope_candidates
                    .iter()
                    .map(|candidate| {
                        classify_trace_candidate(
                            candidate,
                            &injected_note_ids,
                            JIT_TRACE_PROD_MIN_CONFIDENCE,
                        )
                    })
                    .collect();

            // Defensive: validate invariants before persistence so a
            // regression in the classifier surfaces as a log warning rather
            // than a database constraint violation. Failures here are
            // fail-open.
            if let Err(validation_err) =
                djinn_db::repositories::retrieval_trace::validate_candidates(&trace_candidates)
            {
                tracing::warn!(
                    target: TELEMETRY_TARGET,
                    session_id = %session_id,
                    project_id = %project_id,
                    error = %validation_err,
                    "jit_pitfalls: trace candidates failed validation; \
                     skipping trace persistence (fail-open)",
                );
            } else {
                match serde_json::to_value(&trace_candidates) {
                    Ok(json) => {
                        let candidate_count = trace_candidates.len();
                        let trace_universe_count = scope_candidates.len();

                        // Render the hint block BEFORE the async persistence
                        // path so the trace metadata refers to the same
                        // block returned to the caller (and matches the
                        // existing `result_count` / `rendered_note_count`).
                        let block = render_pitfall_block(&notes);
                        let estimated_tokens = estimate_injected_tokens(block.chars().count());

                        let trigger = build_trace_trigger(
                            rollout_mode,
                            touched_paths,
                            rendered_note_count,
                            trace_universe_count,
                            JIT_TRACE_PROD_MIN_CONFIDENCE,
                            JIT_TRACE_PROD_QUERY_LIMIT,
                            None,
                        );
                        let durations_ms = build_trace_durations_ms(
                            elapsed_ms,
                            Some(trace_search_elapsed_ms),
                            None,
                        );

                        trace_candidates_json = json;

                        persist_jit_trace(
                            &state.db,
                            session_id,
                            project_id,
                            &trace_candidates_json,
                            &durations_ms,
                            &trigger,
                            estimated_tokens,
                            candidate_cap,
                            candidate_cap_exceeded,
                        )
                        .await;
                        let persist_elapsed_ms = elapsed_millis(persist_started);
                        tracing::debug!(
                            target: TELEMETRY_TARGET,
                            session_id = %session_id,
                            project_id = %project_id,
                            candidates = candidate_count,
                            injected = injected_note_ids.len(),
                            trace_search_elapsed_ms = trace_search_elapsed_ms,
                            persist_elapsed_ms = persist_elapsed_ms,
                            estimated_injected_tokens = estimated_tokens,
                            "jit_pitfalls: persisted JitPitfalls trace (fail-open)",
                        );
                        return Some(block);
                    }
                    Err(ser_err) => {
                        tracing::warn!(
                            target: TELEMETRY_TARGET,
                            session_id = %session_id,
                            project_id = %project_id,
                            error = %ser_err,
                            "jit_pitfalls: failed to serialize trace candidates; \
                             skipping trace persistence (fail-open)",
                        );
                    }
                }
            }
            // Validation-failure fall-through: trace persistence was skipped
            // because the candidate classification failed invariants, but the
            // rendered block is still valid. Fall through to the trailing
            // return below (fail-open).
        }
        Ok(_) => {
            // Empty trace universe (production already returned some notes,
            // but the unfiltered query returned none — can happen if notes
            // were filtered out between calls or if scope_wider matches an
            // edge case we don't model). Still persist a metadata-only row
            // so the trace ledger records the attempt.
            let block = render_pitfall_block(&notes);
            let estimated_tokens = estimate_injected_tokens(block.chars().count());
            let trigger = build_trace_trigger(
                rollout_mode,
                touched_paths,
                rendered_note_count,
                0,
                JIT_TRACE_PROD_MIN_CONFIDENCE,
                JIT_TRACE_PROD_QUERY_LIMIT,
                None,
            );
            let durations_ms =
                build_trace_durations_ms(elapsed_ms, Some(trace_search_elapsed_ms), None);
            persist_jit_trace(
                &state.db,
                session_id,
                project_id,
                &trace_candidates_json,
                &durations_ms,
                &trigger,
                estimated_tokens,
                candidate_cap,
                candidate_cap_exceeded,
            )
            .await;
            let persist_elapsed_ms = elapsed_millis(persist_started);
            tracing::debug!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                persist_elapsed_ms = persist_elapsed_ms,
                estimated_injected_tokens = estimated_tokens,
                "jit_pitfalls: persisted empty-universe JitPitfalls trace (fail-open)",
            );
            return Some(block);
        }
        Err(trace_err) => {
            // The trace candidate query is best-effort: production semantics
            // are already locked in (we have the rendered block ready). Log
            // a warning and persist a metadata-only row so the trace ledger
            // records the primary outcome anyway.
            let block = render_pitfall_block(&notes);
            let estimated_tokens = estimate_injected_tokens(block.chars().count());
            let trigger = build_trace_trigger(
                rollout_mode,
                touched_paths,
                rendered_note_count,
                notes.len(),
                JIT_TRACE_PROD_MIN_CONFIDENCE,
                JIT_TRACE_PROD_QUERY_LIMIT,
                Some(&format!("trace_candidate_query: {trace_err}")),
            );
            let durations_ms =
                build_trace_durations_ms(elapsed_ms, Some(trace_search_elapsed_ms), None);
            tracing::warn!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                error = %trace_err,
                "jit_pitfalls: trace candidate query failed; \
                 persisting metadata-only JitPitfalls trace (fail-open)",
            );
            persist_jit_trace(
                &state.db,
                session_id,
                project_id,
                &trace_candidates_json,
                &durations_ms,
                &trigger,
                estimated_tokens,
                candidate_cap,
                candidate_cap_exceeded,
            )
            .await;
            return Some(block);
        }
    }

    // Reachable only on the JSON-serialization-failure or
    // validate-invariants-failure paths inside the success arm above, both
    // of which are fail-open — the rendered block is still valid and the
    // trace row is intentionally skipped.
    Some(render_pitfall_block(&notes))
}

/// Render the top-2 notes as a clearly-delimited transient hint block.
fn render_pitfall_block(notes: &[djinn_memory::Note]) -> String {
    let mut out = String::from("<relevant-pitfalls>\n");
    for note in notes.iter().take(2) {
        let label = match note.note_type.as_str() {
            "pitfall" => "Pitfall",
            "pattern" => "Pattern",
            _ => "Note",
        };
        let summary = note
            .overview
            .as_deref()
            .or(note.abstract_.as_deref())
            .unwrap_or_else(|| &note.content[..note.content.len().min(280)])
            .trim();
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            label,
            note.title.trim(),
            summary
        ));
    }
    out.push_str("</relevant-pitfalls>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_block_takes_top_two_and_delimits() {
        let mk = |title: &str, ty: &str| djinn_memory::Note {
            id: title.into(),
            project_id: "p".into(),
            permalink: title.into(),
            title: title.into(),
            file_path: String::new(),
            storage: "db".into(),
            note_type: ty.into(),
            folder: String::new(),
            status: "active".into(),
            tags: "[]".into(),
            content: format!("body of {title}"),
            retrieval_anchor: None,
            created_at: String::new(),
            updated_at: String::new(),
            last_accessed: String::new(),
            access_count: 0,
            confidence: 1.0,
            abstract_: Some(format!("abstract of {title}")),
            overview: None,
            scope_paths: "[]".into(),
        };
        let notes = vec![
            mk("one", "pitfall"),
            mk("two", "pattern"),
            mk("three", "pitfall"),
        ];
        let block = render_pitfall_block(&notes);
        assert!(block.starts_with("<relevant-pitfalls>"));
        assert!(block.ends_with("</relevant-pitfalls>"));
        assert!(block.contains("[Pitfall] one: abstract of one"));
        assert!(block.contains("[Pattern] two: abstract of two"));
        // Only top 2 — "three" must not appear.
        assert!(!block.contains("three"));
    }

    #[test]
    fn claim_first_modification_is_once_per_session() {
        let sid = format!("sess-{}", uuid::Uuid::now_v7());
        assert!(claim_first_modification(&sid), "first claim wins");
        assert!(!claim_first_modification(&sid), "second claim is a no-op");
        // A different session is independent.
        let other = format!("sess-{}", uuid::Uuid::now_v7());
        assert!(claim_first_modification(&other));
    }

    #[test]
    fn telemetry_outcome_labels_cover_rollout_taxonomy() {
        assert_eq!(
            JitPitfallOutcome::DisabledDefaultOff.label(),
            "disabled_default_off"
        );
        assert_eq!(
            JitPitfallOutcome::DisabledKillSwitch.label(),
            "disabled_kill_switch"
        );
        assert_eq!(
            JitPitfallOutcome::NonFirstModification.label(),
            "non_first_modification"
        );
        assert_eq!(JitPitfallOutcome::EligibleSearch.label(), "eligible_search");
        assert_eq!(JitPitfallOutcome::Injected.label(), "injected");
        assert_eq!(JitPitfallOutcome::Empty.label(), "empty");
        assert_eq!(JitPitfallOutcome::Error.label(), "error");
    }

    #[test]
    fn safe_note_metadata_excludes_prompt_and_hint_body_text() {
        let note = djinn_memory::Note {
            id: "note-id".into(),
            project_id: "p".into(),
            permalink: "pitfalls/example".into(),
            title: "Sensitive Title".into(),
            file_path: String::new(),
            storage: "db".into(),
            note_type: "pitfall".into(),
            folder: String::new(),
            status: "active".into(),
            tags: "[]".into(),
            content: "full rendered body must not be logged".into(),
            retrieval_anchor: None,
            created_at: String::new(),
            updated_at: String::new(),
            last_accessed: String::new(),
            access_count: 0,
            confidence: 0.75,
            abstract_: Some("abstract must not be logged".into()),
            overview: Some("overview must not be logged".into()),
            scope_paths: "[]".into(),
        };

        let metadata = safe_note_metadata(&[note]);
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].rank, 1);
        assert_eq!(metadata[0].id, "note-id");
        assert_eq!(metadata[0].permalink, "pitfalls/example");
        assert_eq!(metadata[0].note_type, "pitfall");
        assert_eq!(metadata[0].confidence, 0.75);

        let rendered = format!("{metadata:?}");
        assert!(!rendered.contains("full rendered body"));
        assert!(!rendered.contains("abstract must not be logged"));
        assert!(!rendered.contains("overview must not be logged"));
        assert!(!rendered.contains("Sensitive Title"));
    }

    #[test]
    fn disabled_by_default() {
        assert_eq!(
            rollout_mode_from_values(None, None),
            JitPitfallRolloutMode::DefaultOff
        );
        assert!(!rollout_mode_from_values(None, None).enabled());
    }

    #[test]
    fn rollout_parser_supports_enable_cohort_kill_switch_and_legacy() {
        assert_eq!(
            rollout_mode_from_values(Some("enabled"), None),
            JitPitfallRolloutMode::Enabled
        );
        assert_eq!(
            rollout_mode_from_values(Some("cohort"), None),
            JitPitfallRolloutMode::Cohort
        );
        assert_eq!(
            rollout_mode_from_values(Some("staging"), None),
            JitPitfallRolloutMode::Cohort
        );
        assert_eq!(
            rollout_mode_from_values(Some("kill-switch"), Some("1")),
            JitPitfallRolloutMode::KillSwitch
        );
        assert_eq!(
            rollout_mode_from_values(None, Some("1")),
            JitPitfallRolloutMode::LegacyOptIn
        );
        assert_eq!(
            rollout_mode_from_values(Some("unknown"), Some("1")),
            JitPitfallRolloutMode::DefaultOff
        );
    }

    // ── Trace helper tests (epic 3paf) ──────────────────────────────────────────
    //
    // These tests exercise the pure-classification helpers added by the
    // JIT-pitfalls trace instrumentation. They avoid DB setup so they
    // always run in the unit-test lane; integration-style coverage of the
    // persistence path lives in `tool_dispatch_tests.rs` and other integration
    // test files.

    fn mk_scope_trace_candidate(
        id: &str,
        rank: i64,
        confidence: f64,
        note_type: &str,
        scope_paths_json: &str,
    ) -> djinn_db::repositories::note::ScopeOverlapTraceCandidate {
        djinn_db::repositories::note::ScopeOverlapTraceCandidate {
            id: id.into(),
            permalink: format!("permalinks/{id}"),
            title: format!("Title {id}"),
            folder: String::new(),
            note_type: note_type.into(),
            scope_paths: scope_paths_json.into(),
            confidence,
            rank,
        }
    }

    #[test]
    fn classify_trace_candidate_marks_injected_note_as_injected() {
        // Candidate 1 is in the injected set → Injected, no skipped_reason.
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
        let candidate = mk_scope_trace_candidate("note-injected", 1, 0.95, "pitfall", "[]");
        let mut injected = HashSet::new();
        injected.insert("note-injected".to_owned());

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.note_id, "note-injected");
        assert_eq!(tc.outcome, CandidateOutcome::Injected);
        assert_eq!(tc.skipped_reason, None);
        assert_eq!(tc.rank, Some(1));
        assert_eq!(tc.confidence, Some(0.95));
        assert_eq!(tc.source.as_deref(), Some("scope_overlap"));
        // Injected candidates must satisfy the "Injected without skipped_reason"
        // invariant (validate_candidates is the canonical checker; here we
        // assert the same shape directly).
        let reason: Option<SkippedReason> = tc.skipped_reason;
        assert!(reason.is_none());
    }

    #[test]
    fn classify_trace_candidate_below_threshold_is_min_confidence() {
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
        let candidate = mk_scope_trace_candidate("note-below", 3, 0.10, "pattern", "[]");
        let injected = HashSet::new(); // not injected

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.outcome, CandidateOutcome::Skipped);
        assert_eq!(tc.skipped_reason, Some(SkippedReason::MinConfidence));
        assert_eq!(tc.confidence, Some(0.10));
    }

    #[test]
    fn classify_trace_candidate_above_threshold_outside_top_k_is_not_top_k() {
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
        // Confidence above the 0.3 floor but rank beyond the top-2 over-fetch
        // and the id is not in the injected set → NotTopK.
        let candidate = mk_scope_trace_candidate("note-overfetch", 4, 0.85, "pattern", "[]");
        let mut injected = HashSet::new();
        injected.insert("note-a".to_owned());
        injected.insert("note-b".to_owned());

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.outcome, CandidateOutcome::Skipped);
        assert_eq!(tc.skipped_reason, Some(SkippedReason::NotTopK));
        assert_eq!(tc.confidence, Some(0.85));
    }

    #[test]
    fn classify_trace_candidate_at_threshold_boundary_is_over_floor() {
        // Confidence equal to the floor (0.3) is NOT below it — should land in
        // the NotTopK branch (since the id is not in the injected set).
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
        let candidate = mk_scope_trace_candidate("note-edge", 5, 0.30, "pattern", "[]");
        let injected = HashSet::new();

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.outcome, CandidateOutcome::Skipped);
        assert_eq!(tc.skipped_reason, Some(SkippedReason::NotTopK));
    }

    #[test]
    fn classify_trace_candidate_preserves_identity_fields() {
        // Identity fields (note_id/permalink/title) and provenance (source,
        // scope) must round-trip so `liso` tooling can list/detail the row.
        use djinn_db::repositories::retrieval_trace::SkippedReason;
        let candidate = mk_scope_trace_candidate("n1", 2, 0.42, "pitfall", r#"["src/handlers"]"#);
        let injected = HashSet::new();

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.note_id, "n1");
        assert_eq!(tc.permalink.as_deref(), Some("permalinks/n1"));
        assert_eq!(tc.title.as_deref(), Some("Title n1"));
        assert_eq!(tc.skipped_reason, Some(SkippedReason::NotTopK));
        assert_eq!(tc.source.as_deref(), Some("scope_overlap"));
        let scope = tc.scope.as_ref().expect("scope must be set");
        assert_eq!(scope["note_type"].as_str(), Some("pitfall"));
        assert_eq!(
            scope["scope_paths"],
            serde_json::json!(["src/handlers"]),
            "scope_paths must parse to a JSON array when the source row is valid JSON"
        );
    }

    #[test]
    fn build_trace_trigger_includes_rollout_paths_and_optional_search_error() {
        // Success path: search_error key is present but null so the shape is
        // stable for downstream readers.
        let touched = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        let trigger = build_trace_trigger(
            JitPitfallRolloutMode::Cohort,
            &touched,
            2,
            5,
            JIT_TRACE_PROD_MIN_CONFIDENCE,
            JIT_TRACE_PROD_QUERY_LIMIT,
            None,
        );
        assert_eq!(trigger["rollout_mode"], "cohort");
        assert_eq!(trigger["touched_paths"][0], "src/a.rs");
        assert_eq!(trigger["touched_path_count"], 2);
        assert_eq!(trigger["rendered_note_count"], 2);
        assert_eq!(trigger["result_count"], 5);
        assert_eq!(trigger["min_confidence"], 0.3);
        assert_eq!(trigger["production_limit"], 8);
        assert_eq!(
            trigger["note_types"],
            serde_json::json!(["pitfall", "pattern"])
        );
        assert!(trigger["search_error"].is_null());

        // Error path: search_error is set.
        let trigger_err = build_trace_trigger(
            JitPitfallRolloutMode::Enabled,
            &touched,
            0,
            0,
            JIT_TRACE_PROD_MIN_CONFIDENCE,
            JIT_TRACE_PROD_QUERY_LIMIT,
            Some("scoped note search failed"),
        );
        assert_eq!(
            trigger_err["search_error"].as_str(),
            Some("scoped note search failed"),
        );
        assert_eq!(trigger_err["rollout_mode"], "enabled");
    }

    #[test]
    fn build_trace_durations_ms_only_includes_phases_that_actually_ran() {
        // Only `search_elapsed_ms` is set → only that key is present.
        let d = build_trace_durations_ms(123, None, None);
        let obj = d.as_object().expect("durations must be an object");
        assert_eq!(obj.len(), 1);
        assert_eq!(obj["search_elapsed_ms"], serde_json::json!(123));

        // Both phases ran → all three keys present.
        let d2 = build_trace_durations_ms(10, Some(20), Some(30));
        let obj2 = d2.as_object().expect("durations must be an object");
        assert_eq!(obj2.len(), 3);
        assert_eq!(obj2["search_elapsed_ms"], serde_json::json!(10));
        assert_eq!(obj2["trace_search_elapsed_ms"], serde_json::json!(20));
        assert_eq!(obj2["persist_elapsed_ms"], serde_json::json!(30));

        // Only trace-search ran → persist_elapsed_ms is absent.
        let d3 = build_trace_durations_ms(0, Some(7), None);
        let obj3 = d3.as_object().expect("durations must be an object");
        assert_eq!(obj3.len(), 2);
        assert!(obj3.get("persist_elapsed_ms").is_none());
    }

    #[test]
    fn estimate_injected_tokens_uses_chars_per_4_with_round_up() {
        // Empty block → 0 tokens.
        assert_eq!(estimate_injected_tokens(0), 0);
        // 1 char → ceil(1/4) = 1.
        assert_eq!(estimate_injected_tokens(1), 1);
        // Exactly 4 chars → 1 token.
        assert_eq!(estimate_injected_tokens(4), 1);
        // 5 chars → ceil(5/4) = 2 tokens.
        assert_eq!(estimate_injected_tokens(5), 2);
        // A large block scales proportionally to its size and remains strictly
        // positive for any non-zero character count.
        let big = usize::from(u16::MAX) * 4;
        let tokens = estimate_injected_tokens(big);
        assert!(tokens > 0, "non-empty block must report positive tokens");
        assert!(
            tokens > estimate_injected_tokens(4),
            "larger blocks must report strictly more tokens than 4-char blocks"
        );
    }

    #[test]
    fn trace_constants_lock_step_with_production_call() {
        // These constants must match the production `query_by_scope_overlap`
        // call in `maybe_pitfall_hint` and the over-fetch heuristic in
        // `render_pitfall_block`. Changing them should fail loudly here so
        // the trace classifier and the production path stay aligned.
        assert_eq!(JIT_TRACE_PROD_MIN_CONFIDENCE, 0.3);
        assert_eq!(JIT_TRACE_PROD_TOP_K, 2);
        assert_eq!(JIT_TRACE_PROD_OVERFETCH, 4);
        assert_eq!(JIT_TRACE_PROD_QUERY_LIMIT, 8);
        assert_eq!(
            JIT_TRACE_PROD_NOTE_TYPES,
            &["pitfall", "pattern"],
            "trace classification must mirror the production note-type filter"
        );
    }

    /// Regression coverage for the empty-production-result trace path.
    ///
    /// Reviewer scenario (epic 3paf round 1):
    /// `query_by_scope_overlap(..., min_confidence=0.3, limit=8)` returns
    /// empty for a touched path that overlaps an active pitfall/pattern note
    /// with confidence 0.10. Before the fix, `persist_jit_empty_trace`
    /// short-circuited with an empty candidates array, so the below-threshold
    /// note disappeared from the trace instead of being persisted as a typed
    /// `MinConfidence` candidate. After the fix, `persist_jit_empty_trace`
    /// fetches the unfiltered trace universe and classifies each candidate
    /// against an empty `injected_note_ids` set — this test pins that
    /// classification contract.
    ///
    /// Two candidates are exercised:
    ///   * `note-below` (confidence 0.10) → `Skipped(MinConfidence)` — the
    ///     deterministic classification that the empty path now produces.
    ///   * `note-above` (confidence 0.85) → `Skipped(NotTopK)` — no injected
    ///     candidates exist on this path, so every above-threshold row is
    ///     outside the production top-K.
    #[test]
    fn empty_result_trace_path_classifies_below_threshold_as_min_confidence() {
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};

        let candidates = vec![
            mk_scope_trace_candidate("note-below", 1, 0.10, "pitfall", "[]"),
            mk_scope_trace_candidate("note-above", 2, 0.85, "pattern", "[]"),
        ];
        // The empty-path always classifies against an empty injected set
        // because the production `query_by_scope_overlap` returned no notes
        // to inject into the `<relevant-pitfalls>` block.
        let empty_injected: HashSet<String> = HashSet::new();

        let classified: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                classify_trace_candidate(candidate, &empty_injected, JIT_TRACE_PROD_MIN_CONFIDENCE)
            })
            .collect();

        assert_eq!(classified.len(), 2);

        // Below-threshold row → typed as `MinConfidence` (was silently
        // dropped before the fix).
        let below = classified
            .iter()
            .find(|c| c.note_id == "note-below")
            .expect("below-threshold candidate must be classified");
        assert_eq!(below.outcome, CandidateOutcome::Skipped);
        assert_eq!(below.skipped_reason, Some(SkippedReason::MinConfidence));
        assert_eq!(below.confidence, Some(0.10));

        // Above-threshold row → typed as `NotTopK` (empty injected set means
        // the production top-K is empty on this path).
        let above = classified
            .iter()
            .find(|c| c.note_id == "note-above")
            .expect("above-threshold candidate must be classified");
        assert_eq!(above.outcome, CandidateOutcome::Skipped);
        assert_eq!(above.skipped_reason, Some(SkippedReason::NotTopK));
        assert_eq!(above.confidence, Some(0.85));

        // Every classified candidate must satisfy the
        // `Skipped has skipped_reason` invariant — this is the same shape
        // check `persist_jit_empty_trace` runs before serialization, so a
        // regression in the classifier surfaces here before the helper is
        // reached.
        assert!(
            djinn_db::repositories::retrieval_trace::validate_candidates(&classified).is_ok(),
            "empty-path classification must satisfy TraceCandidate invariants",
        );
    }
}

//! JIT pitfall retrieval trace persistence helpers (epic 3paf).
//!
//! [`maybe_pitfall_hint`](super::maybe_pitfall_hint) calls the helpers below
//! from every eligible search path (gated to non-disabled, first-modification)
//! and never from the disabled/non-first shortcuts. Failures inside these
//! helpers log a warning and never change the returned `Option<String>` or any
//! Prometheus counter.
//!
//! Helpers are pure/synchronous where possible so unit tests can cover
//! classification without DB setup.

use std::collections::HashSet;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

use super::{JitPitfallRolloutMode, TELEMETRY_TARGET, elapsed_millis, touched_path_summary};

// ── Trace classification constants (synced with the production query) ────────
//
// These mirror the threshold/limit values passed to
// `NoteRepository::query_by_scope_overlap` in [`super::maybe_pitfall_hint`].
// They live here as constants so the trace classifier and helper tests can
// reason about the `min_confidence` vs `not_top_k` boundary without re-parsing
// the call site. Changing the production call MUST update these constants in
// lock-step, otherwise traces will misclassify the boundary cases.
pub(super) const JIT_TRACE_PROD_MIN_CONFIDENCE: f64 = 0.3;
/// Top-K rendered into the `<relevant-pitfalls>` hint block. Trace candidates
/// ranked above this in the unfiltered universe are recorded as
/// `SkippedReason::NotTopK` when not selected for injection.
pub(super) const JIT_TRACE_PROD_TOP_K: usize = 2;
/// Over-fetch multiplier used by the production `query_by_scope_overlap`
/// call. The production call asks for `top_k * overfetch` rows up front and
/// then takes the top `top_k`. We surface this in trace metadata so operators
/// can tell why some over-fetched rows were not rendered.
const JIT_TRACE_PROD_OVERFETCH: usize = 4;
/// Note type filter mirrored from the production `query_by_scope_overlap`
/// call. Trace candidate classification and trigger metadata use this list
/// verbatim so trace rows reflect exactly what the production query asked for.
pub(super) const JIT_TRACE_PROD_NOTE_TYPES: &[&str] = &["pitfall", "pattern"];
/// Production over-fetch upper bound — `top_k * overfetch`.
pub(super) const JIT_TRACE_PROD_QUERY_LIMIT: usize =
    JIT_TRACE_PROD_TOP_K * JIT_TRACE_PROD_OVERFETCH;

/// Build the JSON trigger payload describing the trace row that
/// [`persist_jit_trace`] will persist for one eligible JIT search.
///
/// `search_error` is set only for the error path — otherwise the trace row
/// records the search that ran without leaking an outcome that has not yet
/// been observed (the search has just completed by the time we trace it).
pub(super) fn build_trace_trigger(
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    rendered_note_count: usize,
    result_count: usize,
    min_confidence: f64,
    production_limit: usize,
    search_error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "shape": "touched_file",
        "rollout_mode": rollout_mode.label(),
        "touched_paths": touched_paths,
        "touched_path_count": touched_paths.len(),
        "touched_path_summary": touched_path_summary(touched_paths),
        "rendered_note_count": rendered_note_count,
        "result_count": result_count,
        "min_confidence": min_confidence,
        "production_limit": production_limit,
        "candidate_cap": djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP,
        "candidate_cap_source": "DEFAULT_CANDIDATE_CAP",
        "note_types": JIT_TRACE_PROD_NOTE_TYPES,
        "search_error": search_error,
    })
}

/// Build the pre-insert per-phase durations JSON object for the trace row.
///
/// `search_elapsed_ms` is mandatory for every trace (it covers the production
/// search). `trace_search_elapsed_ms` is populated only when that phase ran and
/// is absent (key missing) when skipped. `persist_elapsed_ms` is intentionally
/// added inside [`persist_jit_trace`] after measuring the awaited repository
/// insert path, so callers cannot accidentally persist a row that only logs the
/// insert duration after the fact.
pub(super) fn build_trace_durations_ms(
    search_elapsed_ms: u64,
    trace_search_elapsed_ms: Option<u64>,
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
    serde_json::Value::Object(obj)
}

fn with_persist_elapsed_ms(
    durations_ms: &serde_json::Value,
    persist_elapsed_ms: u64,
) -> serde_json::Value {
    let mut obj = durations_ms.as_object().cloned().unwrap_or_default();
    obj.insert(
        "persist_elapsed_ms".to_owned(),
        serde_json::Value::Number(persist_elapsed_ms.into()),
    );
    serde_json::Value::Object(obj)
}

/// Classify a single `ScopeOverlapTraceCandidate` row into a
/// [`TraceCandidate`] for JSONB persistence.
///
/// The boundary rule (mirrors the production `maybe_pitfall_hint` semantics):
///
/// 1. If `candidate.note_id` appears in `injected_note_ids`, the candidate
///    is marked `CandidateOutcome::Injected` and `skipped_reason = None`.
/// 2. Otherwise, if `candidate.confidence < min_confidence`, it is marked
///    `CandidateOutcome::Skipped` with `SkippedReason::MinConfidence`.
/// 3. Otherwise, it is marked `CandidateOutcome::Skipped` with
///    `SkippedReason::NotTopK`.
///
/// `source` is `"scope_overlap"` to match the existing `5wdh` data-layer
/// contract; `scope` carries the `scope_paths`/`note_type`/`folder` triple
/// consumed by `liso` (`memory_recall_trace` tooling).
pub(super) fn classify_trace_candidate(
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
/// insert failures log a warning and continue.
#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_jit_trace(
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    trace_candidates_json: &serde_json::Value,
    durations_ms: &serde_json::Value,
    trigger: &serde_json::Value,
    estimated_injected_tokens: i32,
    candidate_cap: i32,
    candidate_cap_exceeded: bool,
) -> Option<u64> {
    use djinn_db::repositories::retrieval_trace::{
        CreateRetrievalTraceParams, RetrievalTraceEntryPoint, RetrievalTraceRepository,
    };

    let trace_repo = RetrievalTraceRepository::new(db.clone());
    let params = CreateRetrievalTraceParams {
        project_id,
        // JIT currently keys its once-per-session guard with a worktree path,
        // not a database session UUID. The trace schema stores IDs in
        // VARCHAR(36), so retain an actual compatible ID when provided and
        // omit path-shaped keys rather than turning trace persistence into a
        // fail-open insert failure.
        session_id: (session_id.len() <= 36).then_some(session_id),
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

    let insert_started = SystemClockTrait::new().now_instant();
    let inserted = trace_repo.insert(params).await;
    let persist_elapsed_ms = elapsed_millis(insert_started);

    match inserted {
        Ok(row) => {
            let persisted_durations_ms = with_persist_elapsed_ms(durations_ms, persist_elapsed_ms);
            if let Err(e) = trace_repo
                .update_durations_ms(&row.id, &persisted_durations_ms)
                .await
            {
                tracing::warn!(
                    target: TELEMETRY_TARGET,
                    session_id = %session_id,
                    project_id = %project_id,
                    trace_id = %row.id,
                    error = %e,
                    "jit_pitfalls: failed to store measured retrieval-trace insert duration; continuing fail-open",
                );
            }
            Some(persist_elapsed_ms)
        }
        Err(e) => {
            tracing::warn!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                persist_elapsed_ms = ?persist_elapsed_ms,
                error = %e,
                "jit_pitfalls: failed to persist retrieval trace; continuing fail-open",
            );
            None
        }
    }
}

/// Estimate the number of tokens represented by a rendered hint block.
///
/// Approximation: `chars / 4` rounded up.
pub(super) fn estimate_injected_tokens(block_chars: usize) -> i32 {
    let chars = u32::try_from(block_chars).unwrap_or(u32::MAX);
    let tokens = chars.div_ceil(4);
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

/// Persist an "empty production result" trace row mirroring the empty-path
/// semantic of `maybe_pitfall_hint`.
///
/// This helper fetches the unfiltered trace candidate universe via
/// [`djinn_db::NoteRepository::query_by_scope_overlap_trace_candidates`] so
/// below-threshold candidates are classified as `MinConfidence` rather than
/// silently disappearing from the trace. The production search returned empty
/// → the injected set is empty by construction → every fetched candidate is
/// classified as either `SkippedReason::MinConfidence` (confidence below
/// [`JIT_TRACE_PROD_MIN_CONFIDENCE`]) or `SkippedReason::NotTopK` (above the
/// floor but no injected candidates means the top-K is empty). Failures in
/// the universe fetch, classification, JSON serialization, or repository
/// insert are each fail-open.
#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_jit_empty_trace(
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
            // carrying the error in the trigger.
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
                build_trace_durations_ms(search_elapsed_ms, Some(trace_search_elapsed_ms));
            let persist_elapsed_ms = persist_jit_trace(
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
            tracing::debug!(
                target: TELEMETRY_TARGET,
                session_id = %session_id,
                project_id = %project_id,
                trace_search_elapsed_ms = trace_search_elapsed_ms,
                persist_elapsed_ms = ?persist_elapsed_ms,
                "jit_pitfalls: persisted empty-result search_error trace (fail-open)",
            );
            return;
        }
    };

    // Production returned no notes → the injected set is empty by
    // construction. Classify the universe against that empty set so the
    // boundary branch in `classify_trace_candidate` is exercised: anything
    // below `JIT_TRACE_PROD_MIN_CONFIDENCE` surfaces as
    // `SkippedReason::MinConfidence`, and anything above the floor lands
    // in `SkippedReason::NotTopK`.
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
    let durations_ms = build_trace_durations_ms(search_elapsed_ms, Some(trace_search_elapsed_ms));

    let persist_elapsed_ms = persist_jit_trace(
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
    tracing::debug!(
        target: TELEMETRY_TARGET,
        session_id = %session_id,
        project_id = %project_id,
        trace_universe_count = trace_universe_count,
        trace_search_elapsed_ms = trace_search_elapsed_ms,
        persist_elapsed_ms = ?persist_elapsed_ms,
        "jit_pitfalls: persisted empty-result JitPitfalls trace with typed candidates (fail-open)",
    );
}

/// Persist a search-error trace row mirroring the error-path semantic of
/// `maybe_pitfall_hint`. The trace carries an empty candidates array so the
/// row exists (the search was attempted) but the `search_error` field lives
/// in the trigger metadata. Failure path is fail-open.
#[allow(clippy::too_many_arguments)]
pub(super) async fn persist_jit_error_trace(
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
    let durations_ms = build_trace_durations_ms(search_elapsed_ms, None);
    let empty_candidates = serde_json::json!([]);
    let _persist_elapsed_ms = persist_jit_trace(
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let reason: Option<SkippedReason> = tc.skipped_reason;
        assert!(reason.is_none());
    }

    #[test]
    fn classify_trace_candidate_below_threshold_is_min_confidence() {
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
        let candidate = mk_scope_trace_candidate("note-below", 3, 0.10, "pattern", "[]");
        let injected = HashSet::new();

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.outcome, CandidateOutcome::Skipped);
        assert_eq!(tc.skipped_reason, Some(SkippedReason::MinConfidence));
        assert_eq!(tc.confidence, Some(0.10));
    }

    #[test]
    fn classify_trace_candidate_above_threshold_outside_top_k_is_not_top_k() {
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
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
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};
        let candidate = mk_scope_trace_candidate("note-edge", 5, 0.30, "pattern", "[]");
        let injected = HashSet::new();

        let tc = classify_trace_candidate(&candidate, &injected, 0.3);
        assert_eq!(tc.outcome, CandidateOutcome::Skipped);
        assert_eq!(tc.skipped_reason, Some(SkippedReason::NotTopK));
    }

    #[test]
    fn classify_trace_candidate_preserves_identity_fields() {
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
        let touched = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        let trigger = build_trace_trigger(
            JitPitfallRolloutMode::Cohort("cohort".to_owned()),
            &touched,
            2,
            5,
            JIT_TRACE_PROD_MIN_CONFIDENCE,
            JIT_TRACE_PROD_QUERY_LIMIT,
            None,
        );
        assert_eq!(trigger["shape"], "touched_file");
        assert_eq!(trigger["rollout_mode"], "cohort");
        assert_eq!(trigger["touched_paths"][0], "src/a.rs");
        assert_eq!(trigger["touched_path_count"], 2);
        assert_eq!(trigger["rendered_note_count"], 2);
        assert_eq!(trigger["result_count"], 5);
        assert_eq!(trigger["min_confidence"], 0.3);
        assert_eq!(trigger["production_limit"], 8);
        assert_eq!(
            trigger["candidate_cap"],
            djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP
        );
        assert_eq!(trigger["candidate_cap_source"], "DEFAULT_CANDIDATE_CAP");
        assert_eq!(
            trigger["note_types"],
            serde_json::json!(["pitfall", "pattern"])
        );
        assert!(trigger["search_error"].is_null());

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
        let d = build_trace_durations_ms(123, None);
        let obj = d.as_object().expect("durations must be an object");
        assert_eq!(obj.len(), 1);
        assert_eq!(obj["search_elapsed_ms"], serde_json::json!(123));

        let d2 = build_trace_durations_ms(10, Some(20));
        let obj2 = d2.as_object().expect("durations must be an object");
        assert_eq!(obj2.len(), 2);
        assert_eq!(obj2["search_elapsed_ms"], serde_json::json!(10));
        assert_eq!(obj2["trace_search_elapsed_ms"], serde_json::json!(20));

        let d3 = build_trace_durations_ms(0, Some(7));
        let obj3 = d3.as_object().expect("durations must be an object");
        assert_eq!(obj3.len(), 2);
        assert!(obj3.get("persist_elapsed_ms").is_none());
    }

    #[test]
    fn with_persist_elapsed_ms_adds_measured_insert_duration() {
        let d = build_trace_durations_ms(10, Some(20));
        let persisted = with_persist_elapsed_ms(&d, 30);
        let obj = persisted.as_object().expect("durations must be an object");
        assert_eq!(obj["search_elapsed_ms"], serde_json::json!(10));
        assert_eq!(obj["trace_search_elapsed_ms"], serde_json::json!(20));
        assert_eq!(obj["persist_elapsed_ms"], serde_json::json!(30));
    }

    #[test]
    fn estimate_injected_tokens_uses_chars_per_4_with_round_up() {
        assert_eq!(estimate_injected_tokens(0), 0);
        assert_eq!(estimate_injected_tokens(1), 1);
        assert_eq!(estimate_injected_tokens(4), 1);
        assert_eq!(estimate_injected_tokens(5), 2);
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

    /// Regression: empty-production-result path classifies below-threshold
    /// candidates as `MinConfidence` (reviewer scenario from epic 3paf round 1).
    #[test]
    fn empty_result_trace_path_classifies_below_threshold_as_min_confidence() {
        use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};

        let candidates = [
            mk_scope_trace_candidate("note-below", 1, 0.10, "pitfall", "[]"),
            mk_scope_trace_candidate("note-above", 2, 0.85, "pattern", "[]"),
        ];
        let empty_injected: HashSet<String> = HashSet::new();

        let classified: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                classify_trace_candidate(candidate, &empty_injected, JIT_TRACE_PROD_MIN_CONFIDENCE)
            })
            .collect();

        assert_eq!(classified.len(), 2);

        let below = classified
            .iter()
            .find(|c| c.note_id == "note-below")
            .expect("below-threshold candidate must be classified");
        assert_eq!(below.outcome, CandidateOutcome::Skipped);
        assert_eq!(below.skipped_reason, Some(SkippedReason::MinConfidence));
        assert_eq!(below.confidence, Some(0.10));

        let above = classified
            .iter()
            .find(|c| c.note_id == "note-above")
            .expect("above-threshold candidate must be classified");
        assert_eq!(above.outcome, CandidateOutcome::Skipped);
        assert_eq!(above.skipped_reason, Some(SkippedReason::NotTopK));
        assert_eq!(above.confidence, Some(0.85));

        assert!(
            djinn_db::repositories::retrieval_trace::validate_candidates(&classified).is_ok(),
            "empty-path classification must satisfy TraceCandidate invariants",
        );
    }

    /// AC5: the default candidate cap is shared between the static
    /// `load_knowledge_context` and JIT `jit_pitfalls` entry points. Both use
    /// `DEFAULT_CANDIDATE_CAP` from the 5wdh data-layer foundation. This test
    /// pins the value and confirms the trigger builder embeds it as
    /// `candidate_cap` with the source label `"DEFAULT_CANDIDATE_CAP"`.
    #[test]
    fn default_candidate_cap_is_consistent_across_entry_points() {
        use djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP;

        // The foundation constant is 50 (documented in the 5wdh roadmap).
        assert_eq!(DEFAULT_CANDIDATE_CAP, 50);

        // The JIT trigger builder embeds DEFAULT_CANDIDATE_CAP as the cap value.
        let trigger = build_trace_trigger(
            JitPitfallRolloutMode::Enabled,
            &["src/a.rs".to_string()],
            1,
            1,
            JIT_TRACE_PROD_MIN_CONFIDENCE,
            JIT_TRACE_PROD_QUERY_LIMIT,
            None,
        );
        assert_eq!(trigger["candidate_cap"], DEFAULT_CANDIDATE_CAP);
        assert_eq!(
            trigger["candidate_cap_source"], "DEFAULT_CANDIDATE_CAP",
            "the cap source label must be the foundation constant name"
        );
    }
}

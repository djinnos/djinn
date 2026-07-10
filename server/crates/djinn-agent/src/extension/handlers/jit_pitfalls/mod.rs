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

mod trace;

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

use crate::context::AgentContext;

use trace::{
    JIT_TRACE_PROD_MIN_CONFIDENCE, JIT_TRACE_PROD_NOTE_TYPES, JIT_TRACE_PROD_QUERY_LIMIT,
    JIT_TRACE_PROD_TOP_K, build_trace_durations_ms, build_trace_trigger, classify_trace_candidate,
    estimate_injected_tokens, persist_jit_empty_trace, persist_jit_error_trace, persist_jit_trace,
};

const TELEMETRY_TARGET: &str = "djinn_agent::jit_pitfalls";

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
/// modification observed.
fn seen_sessions() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Returns `true` exactly once per `session_id` — on the first call.
fn claim_first_modification(session_id: &str) -> bool {
    match seen_sessions().lock() {
        Ok(mut set) => set.insert(session_id.to_string()),
        Err(_) => false,
    }
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
pub(super) async fn maybe_pitfall_hint(
    state: &AgentContext,
    session_id: &str,
    project_id: Option<&str>,
    touched_paths: &[String],
) -> Option<String> {
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
            JIT_TRACE_PROD_QUERY_LIMIT,
        )
        .await
    {
        Ok(notes) if !notes.is_empty() => notes,
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
    // The eligible-search path persists a `JitPitfalls` trace row that
    // captures the unfiltered scope-overlap candidate universe alongside the
    // production selection. Failures in candidate universe fetch,
    // classification, JSON serialization, or repository insert are fail-open.
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
            candidate_cap_exceeded = (scope_candidates.len() as i64) > i64::from(candidate_cap);

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
            // Validation/serialization failure fall-through: trace persistence
            // was skipped but the rendered block is still valid.
        }
        Ok(_) => {
            // Empty trace universe — production already returned notes but the
            // unfiltered query returned none. Persist a metadata-only row.
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
            // Trace candidate query is best-effort: production semantics are
            // already locked in. Log a warning and persist a metadata-only row.
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
    // validate-invariants-failure paths inside the success arm above.
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
        assert!(!block.contains("three"));
    }

    #[test]
    fn claim_first_modification_is_once_per_session() {
        let sid = format!("sess-{}", uuid::Uuid::now_v7());
        assert!(claim_first_modification(&sid), "first claim wins");
        assert!(!claim_first_modification(&sid), "second claim is a no-op");
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
}

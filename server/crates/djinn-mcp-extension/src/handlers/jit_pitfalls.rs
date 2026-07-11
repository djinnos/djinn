//! JIT pitfall hint injection on first file modification.
//!
//! See the original `djinn-agent::extension::handlers::jit_pitfalls` for the
//! full design rationale. This module operates through [`crate::ExtensionContext`].

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

use crate::context::ExtensionContext;

const TELEMETRY_TARGET: &str = "djinn_agent::jit_pitfalls";
const JIT_MIN_CONFIDENCE: f64 = 0.3;
const JIT_TOP_K: usize = 2;
const JIT_QUERY_LIMIT: usize = 8;
const JIT_NOTE_TYPES: &[&str] = &["pitfall", "pattern"];

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

fn seen_sessions() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

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

/// Build the `<relevant-pitfalls>…</relevant-pitfalls>` hint block.
pub(crate) async fn maybe_pitfall_hint(
    ctx: &dyn ExtensionContext,
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

    let note_repo = djinn_db::NoteRepository::new(ctx.db(), ctx.event_bus());

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
            JIT_NOTE_TYPES,
            JIT_MIN_CONFIDENCE,
            JIT_QUERY_LIMIT,
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
            persist_empty_trace(
                &note_repo,
                &ctx.db(),
                session_id,
                project_id,
                rollout_mode,
                touched_paths,
                elapsed_ms,
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
            persist_trace(
                &ctx.db(),
                session_id,
                project_id,
                &serde_json::json!([]),
                &trace_trigger(rollout_mode, touched_paths, 0, 0, Some(&e.to_string())),
                elapsed_ms,
                None,
                0,
            )
            .await;
            return None;
        }
    };

    let elapsed_ms = elapsed_millis(search_started);
    let rendered_note_count = notes.len().min(JIT_TOP_K);
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

    let block = render_pitfall_block(&notes);
    // This observational lookup follows all production logging and selection.
    // Any trace failure is logged in its helper and cannot alter the MCP hint.
    persist_injected_trace(
        &note_repo,
        &ctx.db(),
        session_id,
        project_id,
        rollout_mode,
        touched_paths,
        elapsed_ms,
        &notes,
        rendered_note_count,
        estimate_tokens(block.chars().count()),
    )
    .await;

    Some(block)
}

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

/// Build the stable, detail-ready trigger. `shape` explicitly identifies the
/// touched-file trigger while retaining all touched paths used by the query.
fn trace_trigger(
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    rendered_note_count: usize,
    result_count: usize,
    search_error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "shape": "touched_file",
        "touched_paths": touched_paths,
        "touched_path_count": touched_paths.len(),
        "touched_path_summary": touched_path_summary(touched_paths),
        "rollout_mode": rollout_mode.label(),
        "note_types": JIT_NOTE_TYPES,
        "min_confidence": JIT_MIN_CONFIDENCE,
        "production_limit": JIT_QUERY_LIMIT,
        "rendered_note_count": rendered_note_count,
        "result_count": result_count,
        "search_error": search_error,
    })
}

fn classify_candidate(
    candidate: &djinn_db::repositories::note::ScopeOverlapTraceCandidate,
    injected_ids: &HashSet<String>,
) -> djinn_db::repositories::retrieval_trace::TraceCandidate {
    use djinn_db::repositories::retrieval_trace::{
        CandidateOutcome, SkippedReason, TraceCandidate,
    };

    let scope_paths = serde_json::from_str::<serde_json::Value>(&candidate.scope_paths)
        .unwrap_or_else(|_| serde_json::Value::String(candidate.scope_paths.clone()));
    let (outcome, skipped_reason) = if injected_ids.contains(&candidate.id) {
        (CandidateOutcome::Injected, None)
    } else if candidate.confidence < JIT_MIN_CONFIDENCE {
        (
            CandidateOutcome::Skipped,
            Some(SkippedReason::MinConfidence),
        )
    } else {
        (CandidateOutcome::Skipped, Some(SkippedReason::NotTopK))
    };
    TraceCandidate {
        note_id: candidate.id.clone(),
        permalink: Some(candidate.permalink.clone()),
        title: Some(candidate.title.clone()),
        outcome,
        rank: Some(i32::try_from(candidate.rank).unwrap_or(i32::MAX)),
        confidence: Some(candidate.confidence),
        skipped_reason,
        source: Some("scope_overlap".to_owned()),
        scope: Some(serde_json::json!({
            "scope_paths": scope_paths,
            "note_type": candidate.note_type,
            "folder": candidate.folder,
        })),
    }
}

fn estimate_tokens(chars: usize) -> i32 {
    i32::try_from(u32::try_from(chars).unwrap_or(u32::MAX).div_ceil(4)).unwrap_or(i32::MAX)
}

/// Insert a JIT trace without exposing any data-layer error to the MCP handler.
#[allow(clippy::too_many_arguments)]
async fn persist_trace(
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    candidates: &serde_json::Value,
    trigger: &serde_json::Value,
    search_elapsed_ms: u64,
    candidate_fetch_elapsed_ms: Option<u64>,
    estimated_injected_tokens: i32,
) {
    use djinn_db::repositories::retrieval_trace::{
        CreateRetrievalTraceParams, DEFAULT_CANDIDATE_CAP, RetrievalTraceEntryPoint,
        RetrievalTraceRepository,
    };

    let mut durations = serde_json::Map::new();
    durations.insert("search_elapsed_ms".into(), search_elapsed_ms.into());
    if let Some(elapsed) = candidate_fetch_elapsed_ms {
        durations.insert("trace_candidate_fetch_elapsed_ms".into(), elapsed.into());
    }
    let durations = serde_json::Value::Object(durations);
    if let Err(error) = RetrievalTraceRepository::new(db.clone())
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: Some(session_id),
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::JitPitfalls,
            trigger: Some(trigger),
            candidates,
            candidate_cap: DEFAULT_CANDIDATE_CAP,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &durations,
            estimated_injected_tokens,
        })
        .await
    {
        tracing::warn!(target: TELEMETRY_TARGET, session_id = %session_id,
            project_id = %project_id, error = %error,
            "jit_pitfalls: failed to persist retrieval trace; continuing fail-open");
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_injected_trace(
    note_repo: &djinn_db::NoteRepository,
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    search_elapsed_ms: u64,
    notes: &[djinn_memory::Note],
    rendered_note_count: usize,
    estimated_injected_tokens: i32,
) {
    let started = SystemClockTrait::new().now_instant();
    let universe = note_repo
        .query_by_scope_overlap_trace_candidates(
            project_id,
            touched_paths,
            JIT_NOTE_TYPES,
            djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP as usize,
        )
        .await;
    let fetch_elapsed = elapsed_millis(started);
    let (candidates, result_count, error) = match universe {
        Ok(universe) => {
            let injected_ids = notes
                .iter()
                .take(rendered_note_count)
                .map(|note| note.id.clone())
                .collect();
            let typed = universe
                .iter()
                .map(|candidate| classify_candidate(candidate, &injected_ids))
                .collect::<Vec<_>>();
            if let Err(error) = djinn_db::repositories::retrieval_trace::validate_candidates(&typed)
            {
                tracing::warn!(target: TELEMETRY_TARGET, error = %error,
                    "jit_pitfalls: invalid trace candidates; skipping trace persistence");
                return;
            }
            match serde_json::to_value(typed) {
                Ok(value) => (value, universe.len(), None),
                Err(error) => {
                    tracing::warn!(target: TELEMETRY_TARGET, error = %error,
                        "jit_pitfalls: failed to serialize trace candidates; skipping trace persistence");
                    return;
                }
            }
        }
        Err(error) => {
            tracing::warn!(target: TELEMETRY_TARGET, error = %error,
                "jit_pitfalls: trace candidate query failed; persisting metadata-only trace");
            (
                serde_json::json!([]),
                notes.len(),
                Some(format!("trace_candidate_query: {error}")),
            )
        }
    };
    persist_trace(
        db,
        session_id,
        project_id,
        &candidates,
        &trace_trigger(
            rollout_mode,
            touched_paths,
            rendered_note_count,
            result_count,
            error.as_deref(),
        ),
        search_elapsed_ms,
        Some(fetch_elapsed),
        estimated_injected_tokens,
    )
    .await;
}

async fn persist_empty_trace(
    note_repo: &djinn_db::NoteRepository,
    db: &djinn_db::Database,
    session_id: &str,
    project_id: &str,
    rollout_mode: JitPitfallRolloutMode,
    touched_paths: &[String],
    search_elapsed_ms: u64,
) {
    // The unfiltered universe keeps a miss explainable: candidates below the
    // production floor are `min_confidence`; the remaining candidates are
    // deterministically `not_top_k` because nothing was injected.
    persist_injected_trace(
        note_repo,
        db,
        session_id,
        project_id,
        rollout_mode,
        touched_paths,
        search_elapsed_ms,
        &[],
        0,
        0,
    )
    .await;
}

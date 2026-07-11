//! JIT pitfall hint injection on first file modification.
//!
//! See the original `djinn-agent::extension::handlers::jit_pitfalls` for the
//! full design rationale. This module operates through [`crate::ExtensionContext`].
//!
//! ## Inactive extraction mirror
//!
//! This handler is compiled as part of the incremental extension extraction,
//! but it is not an MCP dispatch entry point: `dispatch.rs` deliberately does
//! not route JIT post-write enrichment through `ExtensionContext`. The live
//! write/edit/apply-patch hook remains
//! `djinn-agent::extension::handlers::workspace::maybe_append_pitfall_hint`,
//! which invokes the agent-local JIT handler. Keep retrieval-trace
//! instrumentation there; adding it here would create unreachable duplicate
//! traces. `inactive_mirror_is_not_registered_with_extension_dispatch` guards
//! this boundary until the post-write hook is migrated.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

use crate::context::ExtensionContext;

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
        .query_by_scope_overlap(project_id, touched_paths, &["pitfall", "pattern"], 0.3, 8)
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
            return None;
        }
    };

    let elapsed_ms = elapsed_millis(search_started);
    let rendered_note_count = notes.len().min(2);
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

    Some(render_pitfall_block(&notes))
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

#[cfg(test)]
mod inactive_mirror_tests {
    /// The extension dispatcher does not own post-write enrichment. This is a
    /// deliberate extraction boundary: registering this mirror would make it a
    /// second JIT dispatch injection entry point and require trace parity.
    #[test]
    fn inactive_mirror_is_not_registered_with_extension_dispatch() {
        let dispatch_source = include_str!("../dispatch.rs");

        assert!(
            !dispatch_source.contains("jit_pitfalls"),
            "registering the mirror with extension dispatch requires activating \
             retrieval-trace instrumentation and parity coverage"
        );
        assert!(
            dispatch_source.contains("DispatchResult::Unhandled"),
            "the extension must preserve the agent facade's ownership of \
             post-write enrichment until that hook is explicitly migrated"
        );
    }
}

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
//! Entirely gated behind the env var `DJINN_JIT_PITFALLS=1` (default OFF). When
//! OFF the hot path records a structured disabled outcome but behaviour remains
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

use crate::context::AgentContext;

const TELEMETRY_TARGET: &str = "djinn_agent::jit_pitfalls";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JitPitfallOutcome {
    Disabled,
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
            Self::Disabled => telemetry::OUTCOME_DISABLED,
            Self::NonFirstModification => telemetry::OUTCOME_NON_FIRST_MODIFICATION,
            Self::EligibleSearch => telemetry::OUTCOME_ELIGIBLE_SEARCH,
            Self::Injected => telemetry::OUTCOME_INJECTED,
            Self::Empty => telemetry::OUTCOME_EMPTY,
            Self::Error => telemetry::OUTCOME_ERROR,
        }
    }
}

#[derive(Debug, PartialEq)]
struct SafeNoteTelemetry {
    rank: usize,
    id: String,
    permalink: String,
    note_type: String,
    confidence: f64,
}

/// Env-gate for F2. Default OFF: only `DJINN_JIT_PITFALLS=1` enables it.
fn enabled() -> bool {
    std::env::var("DJINN_JIT_PITFALLS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
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
    session_id: &str,
    project_id: Option<&str>,
    touched_paths: &[String],
) {
    djinn_telemetry::jit_pitfalls::increment_outcome(outcome.label());
    tracing::info!(
        target: TELEMETRY_TARGET,
        outcome = outcome.label(),
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
    if !enabled() {
        record_outcome(
            JitPitfallOutcome::Disabled,
            session_id,
            project_id,
            touched_paths,
        );
        return None;
    }

    let project_id = match project_id {
        Some(project_id) => project_id,
        None => {
            record_outcome(JitPitfallOutcome::Error, session_id, None, touched_paths);
            return None;
        }
    };
    if touched_paths.is_empty() {
        record_outcome(
            JitPitfallOutcome::Error,
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
            session_id,
            Some(project_id),
            touched_paths,
        );
        return None;
    }

    let note_repo = djinn_db::NoteRepository::new(state.db.clone(), state.event_bus.clone());

    record_outcome(
        JitPitfallOutcome::EligibleSearch,
        session_id,
        Some(project_id),
        touched_paths,
    );
    let search_started = Instant::now();

    let notes = match note_repo
        .query_by_scope_overlap(
            project_id,
            touched_paths,
            &["pitfall", "pattern"],
            0.3,
            // Over-fetch a little, then take the top 2 below — keeps the
            // confidence-DESC ordering from the query while tolerating
            // duplicate-scope rows.
            8,
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
        assert_eq!(JitPitfallOutcome::Disabled.label(), "disabled");
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
        // Note: this reads the ambient env. In the default test environment
        // the var is unset → disabled.
        if std::env::var("DJINN_JIT_PITFALLS").is_err() {
            assert!(!enabled());
        }
    }
}

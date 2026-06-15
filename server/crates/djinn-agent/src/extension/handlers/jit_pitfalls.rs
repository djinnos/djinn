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
//! OFF the hot path is a single cheap env read and behaviour is byte-identical
//! to the pre-F2 output: no DB search, no hint, zero cost.
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

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use crate::context::AgentContext;

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
        return None;
    }

    let project_id = project_id?;
    if touched_paths.is_empty() {
        return None;
    }

    // Only the FIRST modification of the session does anything. Subsequent
    // writes short-circuit here. Claiming BEFORE the search means a transient
    // search failure on the first write does not re-arm the hint for later
    // writes (one shot, by design — the static knowledge block already covers
    // the steady state).
    if !claim_first_modification(session_id) {
        return None;
    }

    let note_repo = djinn_db::NoteRepository::new(state.db.clone(), state.event_bus.clone());

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
        Ok(_) => return None,
        Err(e) => {
            tracing::debug!(
                project_id = %project_id,
                error = %e,
                "jit_pitfalls: scoped note search failed; skipping hint",
            );
            return None;
        }
    };

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
    fn disabled_by_default() {
        // Note: this reads the ambient env. In the default test environment
        // the var is unset → disabled.
        if std::env::var("DJINN_JIT_PITFALLS").is_err() {
            assert!(!enabled());
        }
    }
}

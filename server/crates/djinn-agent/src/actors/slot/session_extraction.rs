//! Structural session extraction: co-access flush + event taxonomy capture.
//!
//! After a session completes, this module parses the session's conversation to:
//! 1. Collect which notes were read (via `memory_read` tool calls).
//! 2. Determine which notes were read but never subsequently referenced in tool
//!    call arguments (staleness signal).
//! 3. Build an event taxonomy: counts of files_changed, errors, git_ops,
//!    tools_used, notes_read, notes_written, and tasks_transitioned.
//! 4. Store the taxonomy as JSON on the session record.
//! 5. Flush co-access pairs from the read notes to `note_associations`.
//!
//! No LLM calls are made here — this is purely structural parsing.

use std::collections::{HashMap, HashSet};

use djinn_core::message::{ContentBlock, Message, Role};
use serde::{Deserialize, Serialize};

use crate::context::AgentContext;

// ── Event taxonomy ────────────────────────────────────────────────────────────

/// Aggregated event counts extracted from a completed session's tool log.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTaxonomy {
    /// Number of distinct files mentioned in tool calls (write_file, edit_file, etc.)
    pub files_changed: u32,
    /// Number of tool call errors (tool results with `is_error: true`)
    pub errors: u32,
    /// Number of git operation tool calls (git_commit, git_push, git_status, etc.)
    pub git_ops: u32,
    /// Total number of unique tool names invoked
    pub tools_used: u32,
    /// Number of notes read via memory_read
    pub notes_read: u32,
    /// Number of notes written via memory_write / memory_edit
    pub notes_written: u32,
    /// Number of task state transitions triggered via task_transition
    pub tasks_transitioned: u32,
    /// Extraction-quality counters persisted for this session.
    #[serde(default)]
    pub extraction_quality: ExtractionQuality,
}

/// Extraction quality counters persisted alongside session taxonomy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionQuality {
    pub extracted: u32,
    pub dedup_skipped: u32,
    pub novelty_skipped: u32,
    pub written: u32,
}

// ── Tool name classification ──────────────────────────────────────────────────

fn is_git_tool(name: &str) -> bool {
    matches!(
        name,
        "git_commit"
            | "git_push"
            | "git_status"
            | "git_diff"
            | "git_log"
            | "git_checkout"
            | "git_branch"
            | "git_add"
            | "git_rebase"
            | "git_merge"
            | "git_stash"
            | "git_reset"
    )
}

fn is_memory_write_tool(name: &str) -> bool {
    matches!(name, "memory_write" | "memory_edit" | "memory_move")
}

fn is_file_change_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "edit_file"
            | "create_file"
            | "delete_file"
            | "patch_file"
            | "str_replace_editor"
            | "text_editor"
    )
}

// ── Extraction logic ──────────────────────────────────────────────────────────

/// Extract the note identifier from a `memory_read` tool call input.
///
/// The agent's `memory_read` tool takes `identifier` (permalink or title) and
/// `project` (path). We return the `identifier` value if present.
fn note_id_from_memory_read(input: &serde_json::Value) -> Option<String> {
    input
        .get("identifier")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Collect all string values from a JSON object recursively (to detect note
/// references in subsequent tool calls after a memory_read).
fn collect_string_values(val: &serde_json::Value, out: &mut Vec<String>) {
    match val {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(m) => {
            for v in m.values() {
                collect_string_values(v, out);
            }
        }
        serde_json::Value::Array(a) => {
            for v in a {
                collect_string_values(v, out);
            }
        }
        _ => {}
    }
}

/// Parse a completed session's conversation messages and return:
/// - `SessionTaxonomy` with event counts
/// - `notes_read_ids`: ordered list of note identifiers from `memory_read` calls
/// - `stale_note_ids`: notes read but not mentioned in any later tool arg
pub fn extract_session_signals(
    messages: &[Message],
) -> (SessionTaxonomy, Vec<String>, Vec<String>) {
    let mut taxonomy = SessionTaxonomy::default();
    let mut unique_tools: HashSet<String> = HashSet::new();
    let mut files_changed_set: HashSet<String> = HashSet::new();
    let mut notes_read_ordered: Vec<String> = Vec::new();
    let mut notes_read_set: HashSet<String> = HashSet::new();
    // Map from note identifier → index in conversation (used to detect staleness)
    let mut note_first_read_order: HashMap<String, usize> = HashMap::new();
    // For each message, track tool call inputs after a read (for staleness check)
    let mut tool_call_index: usize = 0;
    // Collect all subsequent tool inputs for staleness analysis
    let mut tool_inputs_after: Vec<(usize, Vec<String>)> = Vec::new(); // (call_index, string_values)

    for msg in messages {
        match msg.role {
            Role::Assistant => {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { name, input, .. } = block {
                        unique_tools.insert(name.clone());
                        let current_index = tool_call_index;
                        tool_call_index += 1;

                        if name == "memory_read" {
                            if let Some(note_id) = note_id_from_memory_read(input)
                                && notes_read_set.insert(note_id.clone())
                            {
                                notes_read_ordered.push(note_id.clone());
                                note_first_read_order.insert(note_id.clone(), current_index);
                                taxonomy.notes_read += 1;
                            }
                        } else if is_memory_write_tool(name) {
                            taxonomy.notes_written += 1;
                        } else if is_git_tool(name) {
                            taxonomy.git_ops += 1;
                        } else if name == "task_transition" {
                            taxonomy.tasks_transitioned += 1;
                        } else if is_file_change_tool(name) {
                            // Extract file path from input
                            if let Some(path) = input
                                .get("path")
                                .or_else(|| input.get("file_path"))
                                .or_else(|| input.get("filename"))
                                .and_then(|v| v.as_str())
                            {
                                files_changed_set.insert(path.to_string());
                            }
                        }

                        // Collect all string values for staleness analysis
                        let mut vals = Vec::new();
                        collect_string_values(input, &mut vals);
                        tool_inputs_after.push((current_index, vals));
                    }
                }
            }
            Role::User => {
                // ToolResult blocks carry error signals
                for block in &msg.content {
                    if let ContentBlock::ToolResult { is_error, .. } = block
                        && *is_error
                    {
                        taxonomy.errors += 1;
                    }
                }
            }
            Role::System => {}
        }
    }

    taxonomy.files_changed = files_changed_set.len() as u32;
    taxonomy.tools_used = unique_tools.len() as u32;

    // Staleness analysis: notes read but never mentioned in a subsequent tool call
    let stale_note_ids: Vec<String> = notes_read_ordered
        .iter()
        .filter(|note_id| {
            let read_at = *note_first_read_order.get(*note_id).unwrap_or(&0);
            // Check if this note identifier appears in any tool input *after* it was read
            let referenced_later = tool_inputs_after.iter().any(|(call_idx, strings)| {
                *call_idx > read_at && strings.iter().any(|s| s.contains(note_id.as_str()))
            });
            !referenced_later
        })
        .cloned()
        .collect();

    (taxonomy, notes_read_ordered, stale_note_ids)
}

// ── Top-level entry point ─────────────────────────────────────────────────────

/// Run structural extraction for a completed session in the background.
///
/// Parses the conversation messages to build an event taxonomy and flush
/// co-access associations to `note_associations`. The taxonomy is stored as
/// JSON on the session record.
///
/// Returns the extracted `SessionTaxonomy` on success so that callers can
/// chain LLM extraction without a round-trip DB read. Returns `None` when
/// extraction is skipped (e.g. no messages) or when the taxonomy cannot be
/// serialised.
///
/// All errors are logged as warnings; nothing propagates back to the caller.
pub(crate) async fn run_structural_extraction(
    session_id: String,
    messages: Vec<Message>,
    app_state: AgentContext,
) -> Option<SessionTaxonomy> {
    if messages.is_empty() {
        tracing::debug!(session_id = %session_id, "structural_extraction: no messages; skipping");
        return None;
    }

    let (taxonomy, notes_read, stale_notes) = extract_session_signals(&messages);

    // ── Log staleness signals ──────────────────────────────────────────────
    for stale_id in &stale_notes {
        tracing::debug!(
            session_id = %session_id,
            note_identifier = %stale_id,
            "structural_extraction: note read but never referenced again (staleness signal)"
        );
    }

    tracing::debug!(
        session_id = %session_id,
        notes_read = notes_read.len(),
        stale_notes = stale_notes.len(),
        files_changed = taxonomy.files_changed,
        errors = taxonomy.errors,
        git_ops = taxonomy.git_ops,
        tools_used = taxonomy.tools_used,
        notes_written = taxonomy.notes_written,
        tasks_transitioned = taxonomy.tasks_transitioned,
        "structural_extraction: taxonomy built"
    );

    // ── Store taxonomy on session record ───────────────────────────────────
    let taxonomy_json = match serde_json::to_string(&taxonomy) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e, "structural_extraction: failed to serialize taxonomy");
            return None;
        }
    };

    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = session_repo
        .set_event_taxonomy(&session_id, &taxonomy_json)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "structural_extraction: failed to store event taxonomy"
        );
    }

    // ── Flush co-access pairs ──────────────────────────────────────────────
    // We need note DB IDs, but the conversation only has the note identifiers
    // (permalink or title strings) from memory_read. We can flush them as a
    // batch using upsert_association once we resolve to IDs. However, the
    // conversation does not give us project_id for the note lookup; we'll use
    // the session's task_id → project_id. Since notes_read can span multiple
    // projects (edge case), we do a best-effort resolution.
    //
    // Because we don't have project_id here (it's available on the session
    // record), we look up the session to find project_id, then resolve notes.
    flush_co_access(&session_id, &notes_read, &app_state).await;

    Some(taxonomy)
}

/// Resolve note identifiers to DB IDs via project context, then flush all
/// co-access pairs to `note_associations`.
async fn flush_co_access(session_id: &str, notes_read: &[String], app_state: &AgentContext) {
    if notes_read.len() < 2 {
        tracing::debug!(
            session_id = %session_id,
            "structural_extraction: fewer than 2 notes read; skipping co-access flush"
        );
        return;
    }

    // Load the session to find project_id
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let session = match session_repo.get(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(
                session_id = %session_id,
                "structural_extraction: session not found; skipping co-access flush"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "structural_extraction: failed to load session; skipping co-access flush"
            );
            return;
        }
    };

    let note_repo =
        djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Resolve note identifiers → note IDs (UUID strings)
    let mut resolved_ids: Vec<String> = Vec::new();
    for identifier in notes_read {
        match note_repo.resolve(&session.project_id, identifier).await {
            Ok(Some(note)) => resolved_ids.push(note.id),
            Ok(None) => {
                tracing::debug!(
                    session_id = %session_id,
                    identifier = %identifier,
                    "structural_extraction: note identifier did not resolve; skipping"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    identifier = %identifier,
                    error = %e,
                    "structural_extraction: error resolving note identifier"
                );
            }
        }
    }

    if resolved_ids.len() < 2 {
        tracing::debug!(
            session_id = %session_id,
            resolved = resolved_ids.len(),
            "structural_extraction: fewer than 2 notes resolved; skipping co-access flush"
        );
        return;
    }

    // Flush all (i, j) pairs
    let mut pairs_flushed: u32 = 0;
    for (i, note_a) in resolved_ids.iter().enumerate() {
        for note_b in resolved_ids.iter().skip(i + 1) {
            if let Err(e) = note_repo.upsert_association(note_a, note_b, 1).await {
                tracing::warn!(
                    session_id = %session_id,
                    note_a = %note_a,
                    note_b = %note_b,
                    error = %e,
                    "structural_extraction: failed to flush co-access pair"
                );
            } else {
                pairs_flushed += 1;
            }
        }
    }

    tracing::debug!(
        session_id = %session_id,
        pairs_flushed,
        notes_resolved = resolved_ids.len(),
        "structural_extraction: co-access flush complete"
    );
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use djinn_core::message::{ContentBlock, Message};

    use super::*;

    fn tool_use(name: &str, input: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "test-id".to_string(),
                name: name.to_string(),
                input,
            }],
            metadata: None,
        }
    }

    fn tool_result_error(tool_use_id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: vec![ContentBlock::text("error occurred")],
                is_error: true,
            }],
            metadata: None,
        }
    }

    fn tool_result_ok(tool_use_id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: vec![ContentBlock::text("ok")],
                is_error: false,
            }],
            metadata: None,
        }
    }

    #[test]
    fn empty_messages_returns_zero_taxonomy() {
        let (taxonomy, notes, stale) = extract_session_signals(&[]);
        assert_eq!(taxonomy, SessionTaxonomy::default());
        assert!(notes.is_empty());
        assert!(stale.is_empty());
    }

    #[test]
    fn memory_read_increments_notes_read() {
        let msgs = vec![tool_use(
            "memory_read",
            serde_json::json!({"identifier": "decisions/my-adr", "project": "/tmp/proj"}),
        )];
        let (taxonomy, notes, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.notes_read, 1);
        assert_eq!(notes, vec!["decisions/my-adr"]);
    }

    #[test]
    fn memory_read_deduplication() {
        let msgs = vec![
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "decisions/adr-1", "project": "/tmp/proj"}),
            ),
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "decisions/adr-1", "project": "/tmp/proj"}),
            ),
        ];
        let (taxonomy, notes, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.notes_read, 1);
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn git_tool_counted() {
        let msgs = vec![
            tool_use("git_commit", serde_json::json!({"message": "fix"})),
            tool_use("git_push", serde_json::json!({})),
        ];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.git_ops, 2);
    }

    #[test]
    fn task_transition_counted() {
        let msgs = vec![tool_use(
            "task_transition",
            serde_json::json!({"task_id": "abc", "action": "done"}),
        )];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.tasks_transitioned, 1);
    }

    #[test]
    fn error_tool_result_increments_errors() {
        let msgs = vec![
            tool_use("write_file", serde_json::json!({"path": "src/main.rs"})),
            tool_result_error("test-id"),
        ];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.errors, 1);
    }

    #[test]
    fn ok_tool_result_does_not_increment_errors() {
        let msgs = vec![
            tool_use("write_file", serde_json::json!({"path": "src/main.rs"})),
            tool_result_ok("test-id"),
        ];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.errors, 0);
    }

    #[test]
    fn files_changed_deduplication() {
        let msgs = vec![
            tool_use(
                "write_file",
                serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
            ),
            tool_use(
                "edit_file",
                serde_json::json!({"path": "src/main.rs", "diff": "..."}),
            ),
        ];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.files_changed, 1); // same file edited twice
    }

    #[test]
    fn notes_written_counted() {
        let msgs = vec![
            tool_use(
                "memory_write",
                serde_json::json!({"identifier": "research/new-note", "project": "/tmp"}),
            ),
            tool_use(
                "memory_edit",
                serde_json::json!({"identifier": "research/another", "project": "/tmp"}),
            ),
        ];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.notes_written, 2);
    }

    #[test]
    fn tools_used_counts_unique_tool_names() {
        let msgs = vec![
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "x", "project": "/tmp"}),
            ),
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "y", "project": "/tmp"}),
            ),
            tool_use("git_commit", serde_json::json!({"message": "msg"})),
            tool_use("write_file", serde_json::json!({"path": "a.rs"})),
        ];
        let (taxonomy, _, _) = extract_session_signals(&msgs);
        assert_eq!(taxonomy.tools_used, 3); // memory_read, git_commit, write_file
    }

    #[test]
    fn stale_note_detection_when_not_referenced_later() {
        let msgs = vec![
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "decisions/adr-unused", "project": "/tmp"}),
            ),
            tool_use("git_commit", serde_json::json!({"message": "done"})),
        ];
        let (_, notes, stale) = extract_session_signals(&msgs);
        assert_eq!(notes, vec!["decisions/adr-unused"]);
        assert_eq!(stale, vec!["decisions/adr-unused"]);
    }

    #[test]
    fn note_not_stale_when_referenced_in_later_tool() {
        let msgs = vec![
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "decisions/adr-used", "project": "/tmp"}),
            ),
            tool_use(
                "memory_edit",
                serde_json::json!({"identifier": "decisions/adr-used", "project": "/tmp"}),
            ),
        ];
        let (_, _, stale) = extract_session_signals(&msgs);
        assert!(stale.is_empty(), "note was referenced in later tool call");
    }

    #[test]
    fn taxonomy_serializes_round_trips() {
        let tax = SessionTaxonomy {
            files_changed: 3,
            errors: 1,
            git_ops: 2,
            tools_used: 5,
            notes_read: 4,
            notes_written: 1,
            tasks_transitioned: 1,
            extraction_quality: ExtractionQuality {
                extracted: 2,
                dedup_skipped: 1,
                novelty_skipped: 0,
                written: 1,
            },
        };
        let json = serde_json::to_string(&tax).unwrap();
        let parsed: SessionTaxonomy = serde_json::from_str(&json).unwrap();
        assert_eq!(tax, parsed);
    }

    #[test]
    fn taxonomy_deserializes_without_extraction_quality_field() {
        let json = serde_json::json!({
            "files_changed": 1,
            "errors": 0,
            "git_ops": 0,
            "tools_used": 1,
            "notes_read": 0,
            "notes_written": 0,
            "tasks_transitioned": 0
        })
        .to_string();

        let parsed: SessionTaxonomy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.extraction_quality, ExtractionQuality::default());
    }
}

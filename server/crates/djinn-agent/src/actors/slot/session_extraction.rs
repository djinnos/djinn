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

/// Result of parsing a completed session's conversation messages.
pub struct SessionSignals {
    pub taxonomy: SessionTaxonomy,
    /// Ordered list of note identifiers from `memory_read` calls.
    pub notes_read_ids: Vec<String>,
    /// Notes read but not mentioned in any later tool argument.
    pub stale_note_ids: Vec<String>,
    /// Deduplicated list of canonical note permalinks created or modified during
    /// the session via `memory_write`, `memory_edit`, or `memory_move` tool calls.
    /// Extracted from successful (non-error) tool results.
    pub notes_written_permalinks: Vec<String>,
}

/// Extract the canonical note permalink from a memory_write / memory_edit /
/// memory_move tool result. The result text is JSON-serialised
/// `MemoryNoteResponse`; we extract the `permalink` field.
fn permalink_from_tool_result(content: &[ContentBlock]) -> Option<String> {
    for block in content {
        if let ContentBlock::Text { text } = block
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(text)
            && let Some(permalink) = val.get("permalink").and_then(|v| v.as_str())
            && !permalink.is_empty()
        {
            return Some(permalink.to_string());
        }
    }
    None
}

/// Parse a completed session's conversation messages and return a
/// [`SessionSignals`] containing event counts, note-read identifiers,
/// staleness signals, and note-written permalinks.
pub fn extract_session_signals(messages: &[Message]) -> SessionSignals {
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

    // Track tool_use_ids belonging to memory_write/edit/move calls so we can
    // extract the canonical permalink from their corresponding ToolResult.
    let mut memory_write_tool_use_ids: HashSet<String> = HashSet::new();
    // Deduplicated, ordered list of note permalinks written during this session.
    let mut notes_written_permalinks: Vec<String> = Vec::new();
    let mut notes_written_set: HashSet<String> = HashSet::new();

    for msg in messages {
        match msg.role {
            Role::Assistant => {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
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
                            memory_write_tool_use_ids.insert(id.clone());
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
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                    {
                        if *is_error {
                            taxonomy.errors += 1;
                        } else if memory_write_tool_use_ids.contains(tool_use_id) {
                            // Extract canonical permalink from successful memory write result
                            if let Some(permalink) = permalink_from_tool_result(content)
                                && notes_written_set.insert(permalink.clone())
                            {
                                notes_written_permalinks.push(permalink);
                            }
                        }
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

    SessionSignals {
        taxonomy,
        notes_read_ids: notes_read_ordered,
        stale_note_ids,
        notes_written_permalinks,
    }
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

    let signals = extract_session_signals(&messages);

    // ── Log staleness signals ──────────────────────────────────────────────
    for stale_id in &signals.stale_note_ids {
        tracing::debug!(
            session_id = %session_id,
            note_identifier = %stale_id,
            "structural_extraction: note read but never referenced again (staleness signal)"
        );
    }

    tracing::debug!(
        session_id = %session_id,
        notes_read = signals.notes_read_ids.len(),
        stale_notes = signals.stale_note_ids.len(),
        notes_written_permalinks = signals.notes_written_permalinks.len(),
        files_changed = signals.taxonomy.files_changed,
        errors = signals.taxonomy.errors,
        git_ops = signals.taxonomy.git_ops,
        tools_used = signals.taxonomy.tools_used,
        notes_written = signals.taxonomy.notes_written,
        tasks_transitioned = signals.taxonomy.tasks_transitioned,
        "structural_extraction: taxonomy built"
    );

    // ── Store taxonomy on session record ───────────────────────────────────
    let taxonomy_json = match serde_json::to_string(&signals.taxonomy) {
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
    flush_co_access(&session_id, &signals.notes_read_ids, &app_state).await;

    // ── Auto-link written notes to task and epic memory_refs ───────────────
    if !signals.notes_written_permalinks.is_empty() {
        autolink_memory_refs(&session_id, &signals.notes_written_permalinks, &app_state).await;
    }

    Some(signals.taxonomy)
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

/// Deduplicate-append `new_permalinks` into a JSON array string, returning the
/// updated JSON. Preserves existing entries and only adds new ones.
fn dedup_append_memory_refs(existing_json: &str, new_permalinks: &[String]) -> String {
    let mut refs: Vec<String> = serde_json::from_str(existing_json).unwrap_or_default();
    let existing_set: HashSet<String> = refs.iter().cloned().collect();
    for permalink in new_permalinks {
        if !existing_set.contains(permalink) {
            refs.push(permalink.clone());
        }
    }
    serde_json::to_string(&refs).unwrap_or_else(|_| "[]".to_string())
}

/// Look up the session's task, deduplicate-append written note permalinks to the
/// task's `memory_refs`, and propagate to the parent epic's `memory_refs`.
async fn autolink_memory_refs(session_id: &str, permalinks: &[String], app_state: &AgentContext) {
    // Load the session to find task_id
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let session = match session_repo.get(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(
                session_id = %session_id,
                "autolink_memory_refs: session not found"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "autolink_memory_refs: failed to load session"
            );
            return;
        }
    };

    let Some(task_id) = session.task_id.as_deref() else {
        tracing::debug!(
            session_id = %session_id,
            "autolink_memory_refs: session has no task_id; skipping"
        );
        return;
    };

    // ── Update task memory_refs ───────────────────────────────────────────
    let task_repo =
        djinn_db::TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match task_repo.get(task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!(
                session_id = %session_id,
                task_id = %task_id,
                "autolink_memory_refs: task not found"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                task_id = %task_id,
                error = %e,
                "autolink_memory_refs: failed to load task"
            );
            return;
        }
    };

    let updated_task_refs = dedup_append_memory_refs(&task.memory_refs, permalinks);
    if updated_task_refs != task.memory_refs {
        if let Err(e) = task_repo
            .update_memory_refs(task_id, &updated_task_refs)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                task_id = %task_id,
                error = %e,
                "autolink_memory_refs: failed to update task memory_refs"
            );
        } else {
            tracing::debug!(
                session_id = %session_id,
                task_id = %task_id,
                new_refs = %updated_task_refs,
                "autolink_memory_refs: updated task memory_refs"
            );
        }
    }

    // ── Propagate to parent epic memory_refs ──────────────────────────────
    let Some(epic_id) = task.epic_id.as_deref() else {
        tracing::debug!(
            session_id = %session_id,
            task_id = %task_id,
            "autolink_memory_refs: task has no epic_id; skipping epic propagation"
        );
        return;
    };

    let epic_repo =
        djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let epic = match epic_repo.get(epic_id).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            tracing::warn!(
                session_id = %session_id,
                epic_id = %epic_id,
                "autolink_memory_refs: epic not found"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                epic_id = %epic_id,
                error = %e,
                "autolink_memory_refs: failed to load epic"
            );
            return;
        }
    };

    let updated_epic_refs = dedup_append_memory_refs(&epic.memory_refs, permalinks);
    if updated_epic_refs != epic.memory_refs {
        if let Err(e) = epic_repo
            .update_memory_refs(epic_id, &updated_epic_refs)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                epic_id = %epic_id,
                error = %e,
                "autolink_memory_refs: failed to update epic memory_refs"
            );
        } else {
            tracing::debug!(
                session_id = %session_id,
                epic_id = %epic_id,
                new_refs = %updated_epic_refs,
                "autolink_memory_refs: updated epic memory_refs"
            );
        }
    }
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
        let signals = extract_session_signals(&[]);
        assert_eq!(signals.taxonomy, SessionTaxonomy::default());
        assert!(signals.notes_read_ids.is_empty());
        assert!(signals.stale_note_ids.is_empty());
        assert!(signals.notes_written_permalinks.is_empty());
    }

    #[test]
    fn memory_read_increments_notes_read() {
        let msgs = vec![tool_use(
            "memory_read",
            serde_json::json!({"identifier": "decisions/my-adr", "project": "/tmp/proj"}),
        )];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_read, 1);
        assert_eq!(signals.notes_read_ids, vec!["decisions/my-adr"]);
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
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_read, 1);
        assert_eq!(signals.notes_read_ids.len(), 1);
    }

    #[test]
    fn git_tool_counted() {
        let msgs = vec![
            tool_use("git_commit", serde_json::json!({"message": "fix"})),
            tool_use("git_push", serde_json::json!({})),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.git_ops, 2);
    }

    #[test]
    fn task_transition_counted() {
        let msgs = vec![tool_use(
            "task_transition",
            serde_json::json!({"task_id": "abc", "action": "done"}),
        )];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.tasks_transitioned, 1);
    }

    #[test]
    fn error_tool_result_increments_errors() {
        let msgs = vec![
            tool_use("write_file", serde_json::json!({"path": "src/main.rs"})),
            tool_result_error("test-id"),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.errors, 1);
    }

    #[test]
    fn ok_tool_result_does_not_increment_errors() {
        let msgs = vec![
            tool_use("write_file", serde_json::json!({"path": "src/main.rs"})),
            tool_result_ok("test-id"),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.errors, 0);
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
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.files_changed, 1); // same file edited twice
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
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_written, 2);
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
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.tools_used, 3); // memory_read, git_commit, write_file
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
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.notes_read_ids, vec!["decisions/adr-unused"]);
        assert_eq!(signals.stale_note_ids, vec!["decisions/adr-unused"]);
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
        let signals = extract_session_signals(&msgs);
        assert!(
            signals.stale_note_ids.is_empty(),
            "note was referenced in later tool call"
        );
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

    // ── Tool-use + tool-result helper with custom id ──────────────────────

    fn tool_use_with_id(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            metadata: None,
        }
    }

    fn tool_result_with_json(tool_use_id: &str, json_text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: vec![ContentBlock::text(json_text)],
                is_error: false,
            }],
            metadata: None,
        }
    }

    // ── Note auto-linking extraction tests ─────────────────────────────────

    #[test]
    fn memory_write_extracts_permalink_from_result() {
        let msgs = vec![
            tool_use_with_id(
                "call-1",
                "memory_write",
                serde_json::json!({"title": "My Research", "type": "research", "project": "/tmp", "content": "findings"}),
            ),
            tool_result_with_json(
                "call-1",
                &serde_json::json!({
                    "id": "note-uuid-1",
                    "permalink": "research/my-research",
                    "title": "My Research",
                    "note_type": "research"
                })
                .to_string(),
            ),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_written, 1);
        assert_eq!(
            signals.notes_written_permalinks,
            vec!["research/my-research"]
        );
    }

    #[test]
    fn memory_edit_extracts_permalink_from_result() {
        let msgs = vec![
            tool_use_with_id(
                "call-2",
                "memory_edit",
                serde_json::json!({"identifier": "decisions/adr-1", "operation": "append", "content": "update", "project": "/tmp"}),
            ),
            tool_result_with_json(
                "call-2",
                &serde_json::json!({
                    "id": "note-uuid-2",
                    "permalink": "decisions/adr-1",
                    "title": "ADR 1"
                })
                .to_string(),
            ),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_written, 1);
        assert_eq!(signals.notes_written_permalinks, vec!["decisions/adr-1"]);
    }

    #[test]
    fn memory_move_extracts_canonical_permalink() {
        let msgs = vec![
            tool_use_with_id(
                "call-3",
                "memory_move",
                serde_json::json!({"identifier": "research/old-name", "type": "decisions", "project": "/tmp"}),
            ),
            tool_result_with_json(
                "call-3",
                &serde_json::json!({
                    "id": "note-uuid-3",
                    "permalink": "decisions/old-name",
                    "title": "Old Name"
                })
                .to_string(),
            ),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(
            signals.notes_written_permalinks,
            vec!["decisions/old-name"],
            "should use canonical permalink from result, not input identifier"
        );
    }

    #[test]
    fn written_permalinks_deduplication() {
        let msgs = vec![
            // First write
            tool_use_with_id(
                "call-a",
                "memory_write",
                serde_json::json!({"title": "Note", "type": "research", "project": "/tmp", "content": "v1"}),
            ),
            tool_result_with_json(
                "call-a",
                &serde_json::json!({"permalink": "research/note"}).to_string(),
            ),
            // Edit same note (same permalink in result)
            tool_use_with_id(
                "call-b",
                "memory_edit",
                serde_json::json!({"identifier": "research/note", "operation": "append", "content": "v2", "project": "/tmp"}),
            ),
            tool_result_with_json(
                "call-b",
                &serde_json::json!({"permalink": "research/note"}).to_string(),
            ),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_written, 2);
        assert_eq!(
            signals.notes_written_permalinks,
            vec!["research/note"],
            "duplicate permalinks should be deduplicated"
        );
    }

    #[test]
    fn error_write_result_not_included_in_permalinks() {
        let msgs = vec![
            tool_use_with_id(
                "call-err",
                "memory_write",
                serde_json::json!({"title": "Fail", "type": "research", "project": "/tmp", "content": "x"}),
            ),
            // Error result
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-err".to_string(),
                    content: vec![ContentBlock::text(r#"{"error": "something went wrong"}"#)],
                    is_error: true,
                }],
                metadata: None,
            },
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_written, 1);
        assert!(
            signals.notes_written_permalinks.is_empty(),
            "error results should not produce permalinks"
        );
    }

    #[test]
    fn multiple_writes_collect_all_permalinks() {
        let msgs = vec![
            tool_use_with_id(
                "w1",
                "memory_write",
                serde_json::json!({"title": "A", "type": "research", "project": "/tmp", "content": "a"}),
            ),
            tool_result_with_json(
                "w1",
                &serde_json::json!({"permalink": "research/a"}).to_string(),
            ),
            tool_use_with_id(
                "w2",
                "memory_write",
                serde_json::json!({"title": "B", "type": "decisions", "project": "/tmp", "content": "b"}),
            ),
            tool_result_with_json(
                "w2",
                &serde_json::json!({"permalink": "decisions/b"}).to_string(),
            ),
            tool_use_with_id(
                "w3",
                "memory_edit",
                serde_json::json!({"identifier": "patterns/c", "operation": "append", "content": "c", "project": "/tmp"}),
            ),
            tool_result_with_json(
                "w3",
                &serde_json::json!({"permalink": "patterns/c"}).to_string(),
            ),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.notes_written, 3);
        assert_eq!(
            signals.notes_written_permalinks,
            vec!["research/a", "decisions/b", "patterns/c"]
        );
    }

    #[test]
    fn dedup_append_memory_refs_adds_new_and_skips_existing() {
        let existing = r#"["research/old", "decisions/adr-1"]"#;
        let new = vec![
            "decisions/adr-1".to_string(), // duplicate
            "research/new".to_string(),    // new
        ];
        let result = dedup_append_memory_refs(existing, &new);
        let parsed: Vec<String> = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed,
            vec!["research/old", "decisions/adr-1", "research/new"]
        );
    }

    #[test]
    fn dedup_append_memory_refs_empty_existing() {
        let result = dedup_append_memory_refs("[]", &["research/a".to_string()]);
        let parsed: Vec<String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed, vec!["research/a"]);
    }

    #[test]
    fn dedup_append_memory_refs_malformed_json_recovers() {
        let result = dedup_append_memory_refs("not-json", &["research/a".to_string()]);
        let parsed: Vec<String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed, vec!["research/a"]);
    }

    #[test]
    fn permalink_from_tool_result_extracts_from_json_text() {
        let content = vec![ContentBlock::text(
            serde_json::json!({"id": "x", "permalink": "research/note", "title": "T"}).to_string(),
        )];
        assert_eq!(
            permalink_from_tool_result(&content),
            Some("research/note".to_string())
        );
    }

    #[test]
    fn permalink_from_tool_result_returns_none_for_missing_field() {
        let content = vec![ContentBlock::text(
            serde_json::json!({"id": "x", "title": "T"}).to_string(),
        )];
        assert_eq!(permalink_from_tool_result(&content), None);
    }

    #[test]
    fn permalink_from_tool_result_returns_none_for_empty_permalink() {
        let content = vec![ContentBlock::text(
            serde_json::json!({"permalink": ""}).to_string(),
        )];
        assert_eq!(permalink_from_tool_result(&content), None);
    }

    #[test]
    fn permalink_from_tool_result_returns_none_for_non_json() {
        let content = vec![ContentBlock::text("not json at all")];
        assert_eq!(permalink_from_tool_result(&content), None);
    }
}

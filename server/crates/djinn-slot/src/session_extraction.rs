// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
//! Structural session extraction: co-access flush + event taxonomy capture.
//!
//! After a session completes, this module parses the session's conversation to:
//! 1. Collect which notes were read (via `memory_read` tool calls).
//! 2. Determine which notes were read but never subsequently referenced in tool
//!    call arguments (staleness signal).
//! 3. Build an event taxonomy: counts of files_changed, errors,
//!    tools_used, notes_read, notes_written, and tasks_transitioned.
//! 4. Store the taxonomy as JSON on the session record.
//! 5. Flush co-access pairs from the read notes to `note_associations`.
//!
//! No LLM calls are made here — this is purely structural parsing.
//!
//! # Wiring (Phase 2.2)
//!
//! [`run_post_session_extraction`] is the production entry point, called
//! (fire-and-forget) from `supervisor_runner` when a task-run completes on
//! the **server** — the long-lived process that owns the embedding model +
//! Qdrant, so notes created here get embedded. Sessions run on ephemeral
//! worker pods, so extraction must NOT run there. The file-level
//! `#[allow(dead_code)]` is retained only to cover helpers that are still
//! exercised solely by unit tests.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use djinn_core::message::{ContentBlock, Message, Role};
use serde::{Deserialize, Serialize};

use crate::host::SlotContext;

/// Server-side post-task-run knowledge extraction (Phase 2.2 wiring).
///
/// Called fire-and-forget from `supervisor_runner` once a task-run completes
/// with real work. For each session of THIS run (matched by `task_run_id`)
/// that hasn't already been extracted, run structural extraction and then the
/// LLM distillation that writes `case`/`pattern`/`pitfall` notes. Idempotent:
/// a session whose `event_taxonomy` is already set is skipped (so retries /
/// re-dispatches don't double-extract). All failures are best-effort logged —
/// extraction must never affect task-run outcomes.
pub async fn run_post_session_extraction(
    task_id: String,
    task_run_id: String,
    app_state: SlotContext,
) {
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let msg_repo =
        djinn_db::SessionMessageRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let sessions = match session_repo.list_for_task(&task_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "post_session_extraction: list_for_task failed");
            return;
        }
    };
    for session in sessions {
        // Only sessions produced by this task-run.
        if session.task_run_id.as_deref() != Some(task_run_id.as_str()) {
            continue;
        }
        // Idempotency: a set event_taxonomy means this session was already
        // extracted (structural extraction writes it).
        if matches!(
            session_repo.get_event_taxonomy_json(&session.id).await,
            Ok(Some(_))
        ) {
            continue;
        }
        let messages = match msg_repo.load_conversation(&session.id).await {
            Ok(conv) => conv.messages,
            Err(e) => {
                tracing::warn!(session_id = %session.id, error = %e, "post_session_extraction: load_conversation failed");
                continue;
            }
        };
        // Trivial sessions aren't worth an LLM call.
        if messages.len() < 2 {
            continue;
        }
        tracing::info!(
            task_id = %task_id,
            session_id = %session.id,
            agent_type = %session.agent_type,
            messages = messages.len(),
            "post_session_extraction: extracting knowledge from session"
        );
        if let Some(taxonomy) =
            run_structural_extraction(session.id.clone(), messages, app_state.clone()).await
        {
            super::llm_extraction::run_llm_extraction(session.id, taxonomy, app_state.clone())
                .await;
        }
    }
}

/// One-shot recovery sweep: run post-session extraction over every COMPLETED
/// task-run whose sessions were never extracted (`event_taxonomy IS NULL`).
///
/// This backfills runs that completed *before* the streamed-report fix wired
/// extraction up (and any run whose worker died before streaming a report, so
/// the live trigger never fired). It reuses the live server's fully-wired
/// `SlotContext` — same provider/catalog/db the per-run trigger uses — so it
/// must run inside the server process, not a separate binary (a standalone
/// boot would also `interrupt_stale_sessions`, clobbering the live server).
///
/// Idempotent: [`run_post_session_extraction`] skips any session whose
/// `event_taxonomy` is already set, so re-running this is safe and cheap.
/// Sequential by design — extraction makes an LLM call per session, and we
/// don't want a backfill to stampede the provider.
///
/// Selector follows the agreed policy: ALL completed runs with unextracted
/// sessions (not just the latest), so retries / manually-repaired older runs
/// aren't starved.
pub async fn run_extraction_backfill(app_state: SlotContext) {
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let candidates = match session_repo.list_unextracted_completed_candidates().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "extraction_backfill: candidate query failed; aborting sweep");
            return;
        }
    };
    let total = candidates.len();
    tracing::info!(
        task_runs = total,
        "extraction_backfill: starting one-shot sweep over completed, unextracted task-runs"
    );
    for (idx, candidate) in candidates.into_iter().enumerate() {
        tracing::info!(
            task_id = %candidate.task_id,
            task_run_id = %candidate.task_run_id,
            progress = format!("{}/{}", idx + 1, total),
            "extraction_backfill: extracting task-run"
        );
        run_post_session_extraction(candidate.task_id, candidate.task_run_id, app_state.clone())
            .await;
    }
    tracing::info!(task_runs = total, "extraction_backfill: sweep complete");
}

/// Aggregated event counts extracted from a completed session's tool log.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTaxonomy {
    /// Number of distinct files mentioned in tool calls (write, edit, apply_patch)
    pub files_changed: u32,
    /// Number of tool call errors (tool results with `is_error: true`)
    pub errors: u32,
    /// Total number of unique tool names invoked
    pub tools_used: u32,
    /// Number of notes read via memory_read
    pub notes_read: u32,
    /// Number of notes written via memory_write / memory_edit
    pub notes_written: u32,
    /// Number of task state transitions triggered via task_transition
    pub tasks_transitioned: u32,
    /// Deduplicated list of file paths changed during the session.
    #[serde(default)]
    pub changed_file_paths: Vec<String>,
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
    #[serde(default)]
    pub merged: u32,
    /// `AlreadyKnown` decisions whose attributed evidence content was persisted
    /// before applying the duplicate confidence signal.
    #[serde(default)]
    pub evidence_merged: u32,
    /// `AlreadyKnown` decisions that retained the historical confidence-only
    /// behavior because evidence merging was ineligible or degraded.
    #[serde(default)]
    pub boost_fallback: u32,
    #[serde(default)]
    pub downgraded: u32,
    #[serde(default)]
    pub discarded: u32,
    /// Number of candidates dropped at the ADR-054 admission gate (post-dedup,
    /// pre-novelty) because `assess_note_quality` reported `is_underspecified`.
    /// The admission gate lives inside `run_llm_extraction_inner` and never
    /// affects human-authored memory writes.
    #[serde(default)]
    pub admission_dropped: u32,
}

fn is_memory_write_tool(name: &str) -> bool {
    matches!(name, "memory_write" | "memory_edit" | "memory_move")
}

fn is_file_change_tool(name: &str) -> bool {
    matches!(name, "write" | "edit" | "apply_patch")
}

/// Parse an `apply_patch` tool input and extract all affected file paths from
/// the embedded multi-file patch blob.
fn extract_apply_patch_paths(input: &serde_json::Value) -> Vec<String> {
    let Some(patch) = input.get("patch").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim_end();
        for prefix in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty() {
                    paths.push(path.to_string());
                }
                break;
            }
        }
    }
    paths
}

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
                        } else if name == "task_transition" {
                            taxonomy.tasks_transitioned += 1;
                        } else if is_file_change_tool(name) {
                            if name == "apply_patch" {
                                for p in extract_apply_patch_paths(input) {
                                    files_changed_set.insert(p);
                                }
                            } else if let Some(path) = input
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
    taxonomy.changed_file_paths = files_changed_set.into_iter().collect();
    taxonomy.changed_file_paths.sort();
    taxonomy.files_changed = taxonomy.changed_file_paths.len() as u32;
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
pub async fn run_structural_extraction(
    session_id: String,
    messages: Vec<Message>,
    app_state: SlotContext,
) -> Option<SessionTaxonomy> {
    if messages.is_empty() {
        tracing::debug!(session_id = %session_id, "structural_extraction: no messages; skipping");
        return None;
    }
    let signals = extract_session_signals(&messages);
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
        tools_used = signals.taxonomy.tools_used,
        notes_written = signals.taxonomy.notes_written,
        tasks_transitioned = signals.taxonomy.tasks_transitioned,
        "structural_extraction: taxonomy built"
    );
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
    flush_co_access(&session_id, &signals.notes_read_ids, &app_state).await;
    if !signals.notes_written_permalinks.is_empty() {
        autolink_memory_refs(&session_id, &signals.notes_written_permalinks, &app_state).await;
    }
    emit_proposal_derived_from_edges(
        &session_id,
        &signals.notes_read_ids,
        &signals.notes_written_permalinks,
        &app_state,
    )
    .await;
    Some(signals.taxonomy)
}

/// Resolve note identifiers to DB IDs via project context, then flush all
/// co-access pairs to `note_associations`.
async fn flush_co_access(session_id: &str, notes_read: &[String], app_state: &SlotContext) {
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
    // Chat sessions (migration 14) carry no project_id, so they can't
    // participate in project-scoped note resolution. Skip cleanly.
    let session_project_id = match session.project_id.as_deref() {
        Some(p) => p,
        None => {
            tracing::debug!(
                session_id = %session_id,
                "structural_extraction: session has no project_id; skipping co-access flush"
            );
            return;
        }
    };
    let note_repo =
        djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    // Resolve note identifiers → note IDs (UUID strings)
    let mut resolved_ids: Vec<String> = Vec::new();
    for identifier in notes_read {
        match note_repo.resolve(session_project_id, identifier).await {
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
///
/// The final serialization uses a non-panicking fallback: if `to_string` fails
/// the caller receives an empty JSON array (`"[]"`) rather than a panic.
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
async fn autolink_memory_refs(session_id: &str, permalinks: &[String], app_state: &SlotContext) {
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

/// Emit `derived_from` typed entity edges from a proposal to notes read or
/// written during a session.
///
/// Walks the `session → task → epic → proposal` chain via `proposal_for_epic`,
/// or the initial graduation `session → breakdown task → proposal` chain via
/// `proposal_for_breakdown_task` before child epics exist. When a proposal is
/// found, records a
/// `proposal → note, kind=derived_from` edge for every note in the session
/// (both read and written) using the heterogeneous `memory_entity_associations`
/// substrate.
///
/// Notes are resolved to DB IDs via the session's `project_id`. Both
/// `notes_read` (identifiers from `memory_read`) and `notes_written`
/// (permalinks from `memory_write`/`memory_edit`/`memory_move`) are included
/// so the proposal captures the full provenance of its execution.
///
/// Best-effort: failures are logged and never block session extraction.
async fn emit_proposal_derived_from_edges(
    session_id: &str,
    notes_read: &[String],
    notes_written_permalinks: &[String],
    app_state: &SlotContext,
) {
    if notes_read.is_empty() && notes_written_permalinks.is_empty() {
        return;
    }
    // Load the session to find task_id and project_id.
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let session = match session_repo.get(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::debug!(
                session_id = %session_id,
                "emit_proposal_derived_from: session not found; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "emit_proposal_derived_from: failed to load session; skipping"
            );
            return;
        }
    };
    let Some(task_id) = session.task_id.as_deref() else {
        tracing::debug!(
            session_id = %session_id,
            "emit_proposal_derived_from: session has no task_id; skipping"
        );
        return;
    };
    let session_project_id = session.project_id.as_deref();
    // Load the task so we can route either through its epic or, for the initial
    // proposal breakdown Planner, through proposals.build_breakdown_task_id.
    let task_repo =
        djinn_db::TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match task_repo.get(task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::debug!(
                session_id = %session_id,
                task_id = %task_id,
                "emit_proposal_derived_from: task not found; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                task_id = %task_id,
                error = %e,
                "emit_proposal_derived_from: failed to load task; skipping"
            );
            return;
        }
    };
    // Look up the proposal linked to this task. Worker tasks flow through their
    // parent epic's proposal_epics edge. The initial proposal-decomposition
    // Planner task has no epic yet, so fall back to proposals.build_breakdown_task_id
    // to capture graduation-time memory reads.
    let proposal_repo =
        djinn_db::ProposalRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let proposal = if let Some(epic_id) = task.epic_id.as_deref() {
        match proposal_repo.proposal_for_epic(epic_id).await {
            Ok(Some(p)) => Some(p),
            Ok(None) => {
                tracing::debug!(
                    session_id = %session_id,
                    epic_id = %epic_id,
                    "emit_proposal_derived_from: epic has no linked proposal; trying breakdown task lookup"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    epic_id = %epic_id,
                    error = %e,
                    "emit_proposal_derived_from: failed to look up proposal for epic; trying breakdown task lookup"
                );
                None
            }
        }
    } else {
        None
    };
    let proposal = match proposal {
        Some(p) => p,
        None => match proposal_repo.proposal_for_breakdown_task(task_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::debug!(
                    session_id = %session_id,
                    task_id = %task_id,
                    "emit_proposal_derived_from: task is not linked to a proposal; skipping"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    task_id = %task_id,
                    error = %e,
                    "emit_proposal_derived_from: failed to look up proposal for breakdown task; skipping"
                );
                return;
            }
        },
    };
    let proposal_ref = djinn_db::MemoryEntityRef::proposal(&proposal.id);
    let note_repo =
        djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    // Collect all note identifiers that need resolution. Written notes use
    // permalinks (from tool results); read notes use identifiers (from tool
    // inputs). Both are resolved the same way via `note_repo.resolve`.
    let mut all_note_identifiers: Vec<&str> = Vec::new();
    for id in notes_read {
        all_note_identifiers.push(id.as_str());
    }
    for permalink in notes_written_permalinks {
        // Avoid double-processing notes that were both read and written.
        if !notes_read.contains(permalink) {
            all_note_identifiers.push(permalink.as_str());
        }
    }
    let mut edges_recorded: u32 = 0;
    for identifier in &all_note_identifiers {
        // We need a project_id to resolve the identifier. If the session has
        // no project_id (e.g. chat sessions), skip resolution.
        let Some(pid) = session_project_id else {
            tracing::debug!(
                session_id = %session_id,
                identifier = %identifier,
                "emit_proposal_derived_from: session has no project_id; skipping resolution"
            );
            break;
        };
        let note = match note_repo.resolve(pid, identifier).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                tracing::debug!(
                    session_id = %session_id,
                    identifier = %identifier,
                    "emit_proposal_derived_from: note did not resolve; skipping"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    identifier = %identifier,
                    error = %e,
                    "emit_proposal_derived_from: error resolving note"
                );
                continue;
            }
        };
        let note_ref = djinn_db::MemoryEntityRef::note(&note.id);
        if let Err(e) = note_repo
            .upsert_typed_entity_association(
                proposal_ref.clone(),
                note_ref,
                djinn_db::MemoryEntityKind::DerivedFrom,
                0.8,
            )
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                proposal_id = %proposal.id,
                note_id = %note.id,
                error = %e,
                "emit_proposal_derived_from: failed to record derived_from edge"
            );
        } else {
            edges_recorded += 1;
        }
    }
    if edges_recorded > 0 {
        tracing::debug!(
            session_id = %session_id,
            proposal_id = %proposal.id,
            edges_recorded,
            "emit_proposal_derived_from: recorded proposal derived_from edges"
        );
    }
}

/// Derive scope paths from a list of changed file paths.
///
/// For each file path, strips the project root prefix and takes the parent
/// directory. This is language-agnostic: works for Rust, Go, Python, JS, etc.
///
/// Files at the project root produce the scope `"."`, which is the canonical
/// marker for the root scope.
///
/// Examples:
/// - `server/crates/djinn-db/src/repositories/agent.rs` → `server/crates/djinn-db/src/repositories`
/// - `internal/auth/login/handler.go` → `internal/auth/login`
/// - `packages/ui/src/Button.tsx` → `packages/ui/src`
/// - `README.md` → `.`
pub fn derive_scope_paths(file_paths: &[String], project_root: &str) -> Vec<String> {
    let mut scopes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let root_prefix = project_root.trim_end_matches('/');
    for path in file_paths {
        // Strip project root prefix if present
        let relative = path
            .strip_prefix(root_prefix)
            .unwrap_or(path)
            .trim_start_matches('/');
        if relative.is_empty() {
            continue;
        }
        if let Some(idx) = relative.rfind('/') {
            let dir = &relative[..idx];
            if !dir.is_empty() {
                scopes.insert(dir.to_string());
            } else {
                scopes.insert(".".to_string());
            }
        } else {
            scopes.insert(".".to_string());
        }
    }
    let mut result: Vec<String> = scopes.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::message::{ContentBlock, Message};
    use djinn_db::{
        CreateSessionParams, EpicCreateInput, EpicRepository, MemoryEntityKind, MemoryEntityRef,
        MemoryEntityType, NoteRepository, ProposalCreateInput, ProposalRepository,
        SessionRepository, TaskRepository,
    };
    use tokio_util::sync::CancellationToken;
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
    fn memory_write_result(tool_use_id: &str, permalink: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: vec![ContentBlock::text(
                    serde_json::json!({"permalink": permalink}).to_string(),
                )],
                is_error: false,
            }],
            metadata: None,
        }
    }
    fn has_proposal_derived_from_edge(
        edges: &[djinn_db::MemoryEntityAssociation],
        proposal_id: &str,
        note_id: &str,
    ) -> bool {
        edges.iter().any(|edge| {
            edge.source.entity_type == MemoryEntityType::Proposal
                && edge.source.id == proposal_id
                && edge.target.entity_type == MemoryEntityType::Note
                && edge.target.id == note_id
                && edge.kind == MemoryEntityKind::DerivedFrom
        })
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
            tool_use("write", serde_json::json!({"path": "src/main.rs"})),
            tool_result_error("test-id"),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.errors, 1);
    }
    #[test]
    fn ok_tool_result_does_not_increment_errors() {
        let msgs = vec![
            tool_use("write", serde_json::json!({"path": "src/main.rs"})),
            tool_result_ok("test-id"),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.errors, 0);
    }
    #[test]
    fn files_changed_deduplication() {
        let msgs = vec![
            tool_use(
                "write",
                serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
            ),
            tool_use(
                "edit",
                serde_json::json!({"path": "src/main.rs", "diff": "..."}),
            ),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.files_changed, 1); // same file edited twice
    }
    #[test]
    fn apply_patch_collects_all_paths() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n context\n-old\n+new\n*** Add File: src/b.rs\n+content\n*** Delete File: src/c.rs\n*** End Patch\n";
        let msgs = vec![tool_use("apply_patch", serde_json::json!({"patch": patch}))];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.files_changed, 3);
        assert_eq!(
            signals.taxonomy.changed_file_paths,
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string(),
            ]
        );
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
            tool_use(
                "task_transition",
                serde_json::json!({"task_id": "abc", "action": "done"}),
            ),
            tool_use("write", serde_json::json!({"path": "a.rs"})),
        ];
        let signals = extract_session_signals(&msgs);
        assert_eq!(signals.taxonomy.tools_used, 3); // memory_read, task_transition, write
    }
    #[test]
    fn stale_note_detection_when_not_referenced_later() {
        let msgs = vec![
            tool_use(
                "memory_read",
                serde_json::json!({"identifier": "decisions/adr-unused", "project": "/tmp"}),
            ),
            tool_use(
                "task_transition",
                serde_json::json!({"task_id": "abc", "action": "done"}),
            ),
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_epic_task_written_notes_emit_derived_from_edges_after_autolink() {
        let db = crate::test_helpers::create_test_db();
        let ctx = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        let events = djinn_core::events::EventBus::noop();
        let project = crate::test_helpers::create_test_project(&db).await;
        let proposal_repo = ProposalRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events.clone());
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let session_repo = SessionRepository::new(db.clone(), events.clone());
        let note_repo = NoteRepository::new(db.clone(), events.clone());
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Derived provenance proposal",
                body: "Build the fixture path",
                acceptance_criteria: Some("[]"),
                status: Some("building"),
                body_format: None,
            })
            .await
            .expect("create proposal");
        let epic = epic_repo
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "proposal epic",
                    description: "desc",
                    emoji: "🧪",
                    color: "blue",
                    owner: "planner",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .expect("create epic");
        proposal_repo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .expect("link proposal epic");
        let task = task_repo
            .create_in_project(
                &project.id,
                Some(&epic.id),
                "worker task",
                "desc",
                "design",
                "task",
                1,
                "worker",
                None,
                None,
            )
            .await
            .expect("create task");
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");
        let written = note_repo
            .create(&project.id, "Written Case", "body", "case", "[]")
            .await
            .expect("create written note");
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "write-note".to_string(),
                    name: "memory_write".to_string(),
                    input: serde_json::json!({"title": "Written Case", "type": "case"}),
                }],
                metadata: None,
            },
            memory_write_result("write-note", &written.permalink),
        ];
        let taxonomy = run_structural_extraction(session.id.clone(), messages, ctx).await;
        assert!(taxonomy.is_some());
        let task_after = task_repo
            .get(&task.id)
            .await
            .expect("load task")
            .expect("task exists");
        assert!(task_after.memory_refs.contains(&written.permalink));
        let epic_after = epic_repo
            .get(&epic.id)
            .await
            .expect("load epic")
            .expect("epic exists");
        assert!(epic_after.memory_refs.contains(&written.permalink));
        let edges = note_repo
            .list_typed_entity_associations_for(MemoryEntityRef::proposal(&proposal.id), 0.0, 10)
            .await
            .expect("list proposal associations");
        assert!(has_proposal_derived_from_edge(
            &edges,
            &proposal.id,
            &written.id
        ));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_breakdown_task_read_notes_emit_derived_from_edges() {
        let db = crate::test_helpers::create_test_db();
        let ctx = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        let events = djinn_core::events::EventBus::noop();
        let project = crate::test_helpers::create_test_project(&db).await;
        let proposal_repo = ProposalRepository::new(db.clone(), events.clone());
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let session_repo = SessionRepository::new(db.clone(), events.clone());
        let note_repo = NoteRepository::new(db.clone(), events.clone());
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Graduation provenance proposal",
                body: "Read planning context",
                acceptance_criteria: Some("[]"),
                status: Some("approved"),
                body_format: None,
            })
            .await
            .expect("create proposal");
        let breakdown_task = task_repo
            .create_in_project(
                &project.id,
                None,
                "Break down proposal",
                "desc",
                "design",
                "epic_breakdown",
                10,
                "planner",
                None,
                None,
            )
            .await
            .expect("create breakdown task");
        proposal_repo
            .set_breakdown_task(&proposal.id, &breakdown_task.id)
            .await
            .expect("link breakdown task");
        let read_note = note_repo
            .create(&project.id, "Planner Read Note", "body", "reference", "[]")
            .await
            .expect("create read note");
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&breakdown_task.id),
                model: "test-model",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");
        let messages = vec![tool_use(
            "memory_read",
            serde_json::json!({"identifier": read_note.permalink, "project": project.slug()}),
        )];
        let taxonomy = run_structural_extraction(session.id.clone(), messages, ctx).await;
        assert!(taxonomy.is_some());
        let edges = note_repo
            .list_typed_entity_associations_for(MemoryEntityRef::proposal(&proposal.id), 0.0, 10)
            .await
            .expect("list proposal associations");
        assert!(has_proposal_derived_from_edge(
            &edges,
            &proposal.id,
            &read_note.id
        ));
    }
    #[test]
    fn taxonomy_serializes_round_trips() {
        let tax = SessionTaxonomy {
            files_changed: 3,
            errors: 1,
            tools_used: 5,
            notes_read: 4,
            notes_written: 1,
            tasks_transitioned: 1,
            changed_file_paths: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            extraction_quality: ExtractionQuality {
                extracted: 2,
                dedup_skipped: 1,
                novelty_skipped: 0,
                written: 1,
                merged: 0,
                evidence_merged: 0,
                boost_fallback: 0,
                downgraded: 0,
                discarded: 0,
                admission_dropped: 0,
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

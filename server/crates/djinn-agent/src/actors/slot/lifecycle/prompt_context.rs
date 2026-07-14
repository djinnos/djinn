//! Role-specific prompt-context assembly: conflict, activity, epic, knowledge,
//! code-graph, and CI directives → rendered system prompt with extensions + skills.

use std::path::Path;

use djinn_core::models::Task;

use crate::actors::slot::MergeConflictMetadata;
use crate::actors::slot::helpers::{
    COMBINED_BRIEF_TOTAL_CHARS, NotePackDisposition, build_reviewer_diff_context,
    build_role_code_graph_context, derive_task_scope_paths, extract_worker_context,
    format_attempt_history, pack_knowledge_notes, recent_feedback,
};
use crate::actors::slot::lifecycle::attempt_context;
use crate::actors::slot::lifecycle::memory_intent_planner::{PlannedNoteType, PlannedQuery};
use crate::context::AgentContext;
use crate::prompts::{TaskContext, apply_role_extensions, apply_skills};
use crate::skills::ResolvedSkill;
use djinn_db::{NoteRepository, ProposalRepository, TaskRepository};
use tracing::Instrument;

/// Test-only observation point at the real typed-search boundary. It keeps
/// production repository semantics intact while smoke tests assert concurrency.
#[cfg(test)]
pub(super) struct PlannedSearchObserver {
    pub entered: std::sync::atomic::AtomicUsize,
    pub barrier: std::sync::Arc<tokio::sync::Barrier>,
    pub ready: std::sync::Arc<tokio::sync::Notify>,
    pub release: std::sync::Arc<tokio::sync::Notify>,
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static PLANNED_SEARCH_OBSERVER: std::sync::Arc<PlannedSearchObserver>;
}

mod types;
pub(crate) use types::{
    KnowledgeContextIdentity, PromptContext, PromptContextInputs, ReadSourceInfo,
};

/// Append read-only sibling repo section to prompt. No-op when no read sources.
fn append_read_sources_prompt(prompt: &str, read_sources: &[ReadSourceInfo]) -> String {
    if read_sources.is_empty() {
        return prompt.to_string();
    }
    let mut s = String::from(prompt);
    s.push_str("\n\n## Related repositories (read-only)\n");
    s.push_str(
        "These sibling repos are flagged as relevant to this task. Read any file with \
         `read(project=\"<slug>\", file_path=...)`, search them with \
         `code_search(query=..., project=\"<slug>\")` (omit `project` to search ALL \
         registered repos at once), and for full shell/build use \
         `shell(project=\"<slug>\", ...)`. You can reach ANY registered repo this way — \
         these are just the ones called out as relevant. You MUST NOT write to, commit \
         to, or open a PR against them — every write goes to THIS task's own project.\n\n",
    );
    for rs in read_sources {
        s.push_str(&format!("- **{}** ({})\n", rs.slug, rs.name));
    }
    s
}

/// Format conflicting files as a `- <path>` markdown list.
fn format_conflict_files(conflict_ctx: Option<&MergeConflictMetadata>) -> Option<String> {
    conflict_ctx.map(|m| {
        m.conflicting_files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Build activity-log digest with recent feedback and per-role counts. Returns None when empty.
fn format_activity_text(
    activity_entries: &Option<Vec<djinn_core::models::ActivityEntry>>,
    max_feedback: usize,
) -> Option<String> {
    match activity_entries {
        Some(entries) if !entries.is_empty() => {
            let feedback = recent_feedback(entries, max_feedback);
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for e in entries {
                if e.event_type == "comment" {
                    *counts.entry(e.actor_role.as_str()).or_default() += 1;
                }
            }
            let count_summary: String = counts
                .iter()
                .map(|(role, n)| format!("{n} {role}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut parts = Vec::new();
            if !feedback.is_empty() {
                parts.push(format!(
                    "**Recent feedback (newest last):**\n{}",
                    feedback.join("\n\n---\n")
                ));
            }
            if !count_summary.is_empty() {
                parts.push(format!(
                    "**Activity totals:** {count_summary} comments. Use `task_activity_list` with `actor_role` filter for full history."
                ));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    }
}

/// Format MCP server instructions as a deterministic `mcp_instructions` prompt
/// section. Returns `None` when the map is empty (no server provided instructions).
///
/// Each server gets its own subsection, sorted by server name. The section is
/// only included when at least one connected server returned non-empty
/// instructions.
fn format_mcp_instructions(
    mcp_server_instructions: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if mcp_server_instructions.is_empty() {
        return None;
    }
    let mut sections = Vec::new();
    for (server_name, instruction) in mcp_server_instructions {
        sections.push(format!("### {server_name}\n{instruction}"));
    }
    Some(format!(
        "## MCP Server Instructions\n{}",
        sections.join("\n\n")
    ))
}

/// Apply extensions, skills, read sources, and MCP instructions to base prompt
/// in canonical order.
fn apply_prompt_sections(
    base_system_prompt: &str,
    system_prompt_extensions: &str,
    resolved_skills: &[ResolvedSkill],
    read_sources: &[ReadSourceInfo],
    mcp_server_instructions: &std::collections::BTreeMap<String, String>,
) -> String {
    let with_extensions = apply_role_extensions(base_system_prompt, system_prompt_extensions);
    let with_skills = apply_skills(&with_extensions, resolved_skills);
    let with_read_sources = append_read_sources_prompt(&with_skills, read_sources);
    match format_mcp_instructions(mcp_server_instructions) {
        Some(section) => format!("{with_read_sources}\n\n{section}"),
        None => with_read_sources,
    }
}

/// Append sibling task summary lines to `ctx_lines` for the given epic.
async fn load_sibling_tasks(
    epic_id: &str,
    task_repo: &TaskRepository,
    ctx_lines: &mut Vec<String>,
) {
    if let Ok(result) = task_repo
        .list_filtered(djinn_db::ListQuery {
            parent: Some(epic_id.to_string()),
            limit: 50,
            ..Default::default()
        })
        .await
    {
        let open = result.tasks.iter().filter(|t| t.status != "closed").count();
        let closed = result.tasks.iter().filter(|t| t.status == "closed").count();
        ctx_lines.push(format!(
            "\n### Sibling Tasks ({open} open, {closed} closed)"
        ));
        for t in &result.tasks {
            let status_marker = if t.status == "closed" {
                "closed"
            } else {
                &t.status
            };
            ctx_lines.push(format!("- [{}] {}: {}", status_marker, t.short_id, t.title));
        }
    }
}

/// Append delivered-task sub-bullets for a blocker's closed tasks.
async fn append_blocker_deliveries(
    epic_id: &str,
    blocker: &djinn_db::EpicBlockerRef,
    task_repo: &TaskRepository,
    ctx_lines: &mut Vec<String>,
) {
    let closed_tasks = match task_repo
        .list_filtered(djinn_db::ListQuery {
            parent: Some(blocker.epic_id.clone()),
            status: Some("closed".to_string()),
            limit: 20,
            ..Default::default()
        })
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::debug!(
                epic_id = %epic_id,
                blocking_epic_id = %blocker.epic_id,
                error = %e,
                "Lifecycle: failed to list closed tasks for blocking epic"
            );
            return;
        }
    };
    for t in &closed_tasks.tasks {
        ctx_lines.push(format!("  - Delivered: {}", t.title));
    }
}

/// Append blocking-epic lines and delivered-task sub-bullets.
async fn load_blocking_epics(
    epic_id: &str,
    epic_repo: &djinn_db::EpicRepository,
    task_repo: &TaskRepository,
    ctx_lines: &mut Vec<String>,
) {
    let blockers = match epic_repo.list_blockers(epic_id).await {
        Ok(b) if !b.is_empty() => b,
        _ => return,
    };
    ctx_lines.push("\n### Blocking Epics".to_string());
    for blocker in &blockers {
        ctx_lines.push(format!(
            "- **{}** ({}) — {}",
            blocker.title, blocker.short_id, blocker.status
        ));
        append_blocker_deliveries(epic_id, blocker, task_repo, ctx_lines).await;
    }
}

/// Append proposal-sibling-epic lines if epic belongs to a multi-epic proposal.
async fn load_proposal_sibling_epics(
    epic_id: &str,
    epic_repo: &djinn_db::EpicRepository,
    proposal_repo: &ProposalRepository,
    ctx_lines: &mut Vec<String>,
) {
    let proposal = match proposal_repo.proposal_for_epic(epic_id).await {
        Ok(Some(p)) => p,
        _ => return,
    };
    let siblings = match proposal_repo.graduated_epics(&proposal.id).await {
        Ok(s) => s,
        _ => return,
    };
    let sibling_ids: Vec<String> = siblings
        .into_iter()
        .filter(|(sid, _)| sid != epic_id)
        .map(|(sid, _)| sid)
        .collect();
    if sibling_ids.is_empty() {
        return;
    }
    ctx_lines.push(format!("\n### Proposal Sibling Epics ({})", proposal.title));
    for sid in &sibling_ids {
        append_proposal_sibling_epic(epic_id, sid, epic_repo, ctx_lines).await;
    }
}

/// Append a single proposal-sibling-epic bullet; skip on error.
async fn append_proposal_sibling_epic(
    epic_id: &str,
    sibling_id: &str,
    epic_repo: &djinn_db::EpicRepository,
    ctx_lines: &mut Vec<String>,
) {
    match epic_repo.get(sibling_id).await {
        Ok(Some(sibling_epic)) => {
            ctx_lines.push(format!(
                "- **{}** ({}) — {}",
                sibling_epic.title, sibling_epic.short_id, sibling_epic.status
            ));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::debug!(
                epic_id = %epic_id,
                sibling_epic_id = %sibling_id,
                error = %e,
                "Lifecycle: failed to load proposal sibling epic for prompt context"
            );
        }
    }
}

/// Load epic context block; returns None when not needed or on error.
async fn load_epic_context(
    task: &Task,
    needs_epic_context: bool,
    app_state: &AgentContext,
) -> Option<String> {
    if !needs_epic_context {
        return None;
    }
    let epic_id = task.epic_id.as_deref()?;
    let epic_repo =
        djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let epic = epic_repo.get(epic_id).await.ok()??;
    let mut ctx_lines = vec![
        format!("**Epic:** {} ({})", epic.title, epic.short_id),
        format!("**Description:** {}", epic.description),
        format!(
            "**Memory refs:** call `epic_show({})` then `memory_read(identifier=<ref>)` for each — use the memory_* MCP tools; do not read `.djinn/memory/` files from the worker filesystem.",
            epic.short_id
        ),
    ];
    load_sibling_tasks(epic_id, &task_repo, &mut ctx_lines).await;
    load_blocking_epics(epic_id, &epic_repo, &task_repo, &mut ctx_lines).await;
    let proposal_repo = ProposalRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    load_proposal_sibling_epics(epic_id, &epic_repo, &proposal_repo, &mut ctx_lines).await;
    Some(ctx_lines.join("\n"))
}

/// Production confidence threshold for knowledge-note injection.
const KNOWLEDGE_MIN_CONFIDENCE: f64 = 0.3;

/// Production injection limit (top-K) for knowledge notes.
const KNOWLEDGE_INJECTION_LIMIT: usize = 10;

/// Character budget for the rendered knowledge-notes prompt section.
const KNOWLEDGE_BUDGET_CHARS: usize = 2000;

/// Note types queried for knowledge-context injection.
const KNOWLEDGE_NOTE_TYPES: &[&str] = &["pattern", "pitfall", "case"];
const PLANNER_NOTES_PER_QUERY: usize = 2;
const PLANNER_NOTES_GLOBAL: usize = 6;

fn planned_note_type_name(kind: PlannedNoteType) -> &'static str {
    match kind {
        PlannedNoteType::Pitfall => "pitfall",
        PlannedNoteType::Pattern => "pattern",
        PlannedNoteType::Case => "case",
        PlannedNoteType::Reference => "reference",
    }
}

async fn load_planned_knowledge(
    note_repo: &NoteRepository,
    task: &Task,
    queries: &[PlannedQuery],
    scope_notes: &[djinn_memory::Note],
    scope_used: usize,
) -> Option<String> {
    if scope_used >= KNOWLEDGE_BUDGET_CHARS {
        return None;
    }
    let entities = vec!["note".to_string()];
    let buckets = futures::future::join_all(queries.iter().map(|q| {
        async {
            // Keep the test observation immediately adjacent to the production
            // repository call so smoke tests exercise the real search boundary.
            #[cfg(test)]
            if let Ok(observer) = PLANNED_SEARCH_OBSERVER.try_with(Clone::clone) {
                observer
                    .entered
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                observer.barrier.wait().await;
                observer.ready.notify_waiters();
                observer.release.notified().await;
            }
            note_repo
                .search(djinn_db::NoteSearchParams {
                    project_id: &task.project_id,
                    query: &q.query,
                    task_id: Some(&task.id),
                    folder: None,
                    note_type: Some(planned_note_type_name(q.note_type)),
                    limit: PLANNER_NOTES_PER_QUERY,
                    semantic_scores: None,
                    edge_kinds: None,
                    entity_types: Some(&entities),
                })
                .await
        }
    }))
    .await;
    if buckets.iter().any(Result::is_err) {
        return None;
    }
    Some(render_planned_knowledge(
        buckets.into_iter().map(Result::unwrap).collect(),
        scope_notes,
        scope_used,
    ))
}

/// Deterministically pack already-ranked planner buckets after the authoritative
/// scope result. Keeping this separate from repository I/O makes the scope-first
/// budget, dedupe, cap, and ordering invariants directly regression-testable.
fn render_planned_knowledge(
    buckets: Vec<Vec<djinn_memory::MemorySearchEntityRow>>,
    scope_notes: &[djinn_memory::Note],
    scope_used: usize,
) -> String {
    let mut ids: std::collections::HashSet<String> =
        scope_notes.iter().map(|n| n.id.clone()).collect();
    let mut links: std::collections::HashSet<String> =
        scope_notes.iter().map(|n| n.permalink.clone()).collect();
    let (mut used, mut lines) = (scope_used, Vec::new());
    for bucket in buckets {
        for row in bucket.into_iter().take(PLANNER_NOTES_PER_QUERY) {
            if !ids.insert(row.id.clone()) || !links.insert(row.permalink.clone()) {
                continue;
            }
            if lines.len() == PLANNER_NOTES_GLOBAL {
                return lines.join("\n");
            }
            let label = match row.note_type.as_str() {
                "pitfall" => "Pitfall",
                "pattern" => "Pattern",
                "case" => "Case",
                "reference" => "Reference",
                _ => "Note",
            };
            let line = format!(
                "- **[{}] {}**: {} (permalink: {})",
                label, row.title, row.snippet, row.permalink
            );
            if used + line.len() > KNOWLEDGE_BUDGET_CHARS {
                return lines.join("\n");
            }
            used += line.len() + 1;
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Load knowledge context from scope-matched notes. Returns None on error/empty.
///
/// Instruments retrieval with a fail-open `LoadKnowledgeContext` trace row. The
/// production query is authoritative for prompt output; the trace-candidate query
/// provides the full universe for classification.
///
/// ## Trace contract (epic 3paf; consumed by sibling `liso` MCP tooling)
///
/// - **Entry point:** `LoadKnowledgeContext` → `"load_knowledge_context"`.
/// - **Trigger:** `{ "shape": "scope_paths", "task_paths": [...] }`.
/// - **Outcomes** (`TraceCandidate`): `injected` (top-K, survived budget — no
///   reason), `min_confidence` (<0.3), `not_top_k`, `budget_pruned`,
///   `dedupe`, `search_error`.
/// - **Durations:** `candidate_fetch_ms`, `classify_ms`, `prompt_pack_ms`, `persist_ms`.
/// - **Tokens:** `ceil(injected_chars/4)`. **Cap:** `DEFAULT_CANDIDATE_CAP`;
///   `exceeded`=`len>=cap`.
/// - **Fail-open:** trace errors are logged and swallowed; the rendered context
///   is produced from the production query alone.
pub(crate) async fn load_knowledge_context(
    task: &Task,
    epic_context: Option<&str>,
    app_state: &AgentContext,
    identity: Option<KnowledgeContextIdentity<'_>>,
    planned_queries: Option<&[PlannedQuery]>,
) -> Option<String> {
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_paths = derive_task_scope_paths(task, epic_context);

    let fetch_start = tokio::time::Instant::now();

    // Fetch the production result set (unchanged semantics) and the capped trace
    // candidate universe concurrently. The production query is authoritative for
    // prompt output; the trace candidate query provides the full universe for
    // classification.
    let (production_result, trace_candidates_result) = tokio::join!(
        note_repo.query_by_scope_overlap(
            &task.project_id,
            &task_paths,
            KNOWLEDGE_NOTE_TYPES,
            KNOWLEDGE_MIN_CONFIDENCE,
            KNOWLEDGE_INJECTION_LIMIT,
        ),
        note_repo.query_by_scope_overlap_trace_candidates(
            &task.project_id,
            &task_paths,
            KNOWLEDGE_NOTE_TYPES,
            djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP as usize,
        ),
    );
    let candidate_fetch_ms = fetch_start.elapsed().as_millis() as i64;

    let notes = match production_result {
        Ok(notes) => notes,
        Err(e) => {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to query knowledge context"
            );
            // Even on production-query error, attempt to persist a trace with the
            // candidates we have (if any) classifying them as search_error.
            if let Ok(ref candidates) = trace_candidates_result {
                let error_candidates = classify_knowledge_candidates_for_error(candidates);
                let cap_exceeded = candidates.len()
                    >= djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP as usize;
                persist_knowledge_trace(
                    task,
                    &task_paths,
                    &error_candidates,
                    0,
                    KnowledgeTraceDurations {
                        candidate_fetch_ms,
                        classify_ms: 0,
                        prompt_pack_ms: 0,
                        persist_ms: 0,
                    },
                    cap_exceeded,
                    &app_state.db,
                    identity,
                )
                .await;
            }
            return None;
        }
    };

    let classification_start = tokio::time::Instant::now();
    let trace_candidates = trace_candidates_result.unwrap_or_default();
    let candidate_cap_exceeded = trace_candidates.len()
        >= djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP as usize;

    // Build the set of production note IDs (those that passed confidence + top-K)
    // for fast membership lookup during classification.
    let production_ids: std::collections::HashSet<&str> =
        notes.iter().map(|n| n.id.as_str()).collect();

    // Classify each trace candidate deterministically.
    let classified = classify_knowledge_candidates(&trace_candidates, &production_ids);
    let classification_ms = classification_start.elapsed().as_millis() as i64;

    // Render the prompt using the packed API (byte-identical to format_knowledge_notes).
    let pack_start = tokio::time::Instant::now();
    let packed = pack_knowledge_notes(&notes, KNOWLEDGE_BUDGET_CHARS);
    let pack_ms = pack_start.elapsed().as_millis() as i64;

    // Apply budget-pruned classification from packing outcomes.
    let trace_candidates_final = apply_budget_outcomes(classified, &packed, &notes);
    let estimated_injected_tokens = packed.total_injected_tokens as i32;

    let scope_rendered = if notes.is_empty() {
        None
    } else {
        Some(packed.rendered)
    };

    // Persist the trace (fail-open). Measure the persist phase separately.
    let persist_start = tokio::time::Instant::now();
    persist_knowledge_trace(
        task,
        &task_paths,
        &trace_candidates_final,
        estimated_injected_tokens,
        KnowledgeTraceDurations {
            candidate_fetch_ms,
            classify_ms: classification_ms,
            prompt_pack_ms: pack_ms,
            persist_ms: persist_start.elapsed().as_millis() as i64,
        },
        candidate_cap_exceeded,
        &app_state.db,
        identity,
    )
    .await;

    match planned_queries {
        None => scope_rendered,
        Some(queries) => match load_planned_knowledge(
            &note_repo,
            task,
            queries,
            &notes,
            packed.total_injected_chars,
        )
        .await
        {
            Some(extra) if !extra.is_empty() => Some(match scope_rendered {
                Some(scope) => format!("{scope}\n{extra}"),
                None => extra,
            }),
            _ => scope_rendered,
        },
    }
}

/// Classify trace candidates into `TraceCandidate` DTOs with deterministic outcomes.
///
/// Classification rules (applied in priority order):
/// 1. Confidence below `KNOWLEDGE_MIN_CONFIDENCE` → `min_confidence`.
/// 2. Passed confidence but outside production top-K → `not_top_k`.
/// 3. In the production set → `Injected` (pending budget classification).
///
/// Budget pruning and dedupe are applied later by [`apply_budget_outcomes`] once
/// the packed-note outcomes are available.
fn classify_knowledge_candidates(
    candidates: &[djinn_db::repositories::note::ScopeOverlapTraceCandidate],
    production_ids: &std::collections::HashSet<&str>,
) -> Vec<djinn_db::repositories::retrieval_trace::TraceCandidate> {
    use djinn_db::repositories::retrieval_trace::{
        CandidateOutcome, SkippedReason, TraceCandidate,
    };

    candidates
        .iter()
        .map(|c| {
            let (outcome, skipped_reason) = if c.confidence < KNOWLEDGE_MIN_CONFIDENCE {
                (
                    CandidateOutcome::Skipped,
                    Some(SkippedReason::MinConfidence),
                )
            } else if !production_ids.contains(c.id.as_str()) {
                (CandidateOutcome::Skipped, Some(SkippedReason::NotTopK))
            } else {
                (CandidateOutcome::Injected, None)
            };
            TraceCandidate {
                note_id: c.id.clone(),
                permalink: Some(c.permalink.clone()),
                title: Some(c.title.clone()),
                outcome,
                rank: Some(c.rank as i32),
                confidence: Some(c.confidence),
                skipped_reason,
                source: Some("scope_overlap".to_string()),
                scope: Some(serde_json::json!({
                    "folder": c.folder,
                    "note_type": c.note_type,
                    "scope_paths": c.scope_paths,
                })),
            }
        })
        .collect()
}

/// Classify all candidates as `search_error` — used when the production query fails
/// but the trace candidate fetch succeeded.
fn classify_knowledge_candidates_for_error(
    candidates: &[djinn_db::repositories::note::ScopeOverlapTraceCandidate],
) -> Vec<djinn_db::repositories::retrieval_trace::TraceCandidate> {
    use djinn_db::repositories::retrieval_trace::{
        CandidateOutcome, SkippedReason, TraceCandidate,
    };

    candidates
        .iter()
        .map(|c| TraceCandidate {
            note_id: c.id.clone(),
            permalink: Some(c.permalink.clone()),
            title: Some(c.title.clone()),
            outcome: CandidateOutcome::Skipped,
            rank: Some(c.rank as i32),
            confidence: Some(c.confidence),
            skipped_reason: Some(SkippedReason::SearchError),
            source: Some("scope_overlap".to_string()),
            scope: Some(serde_json::json!({
                "folder": c.folder,
                "note_type": c.note_type,
                "scope_paths": c.scope_paths,
            })),
        })
        .collect()
}

/// Apply budget-pruned outcomes from the packed notes to the classified candidates.
///
/// Candidates initially classified as `Injected` are reclassified to `BudgetPruned`
/// if the packing outcome for the corresponding note is `BudgetPruned`.
/// Deduplication: if multiple injected candidates resolve to the same permalink
/// (shouldn't happen in practice but handled defensively), the first wins and
/// subsequent ones become `Dedupe`.
fn apply_budget_outcomes(
    mut candidates: Vec<djinn_db::repositories::retrieval_trace::TraceCandidate>,
    packed: &crate::actors::slot::helpers::PackedKnowledgeNotes,
    notes: &[djinn_memory::Note],
) -> Vec<djinn_db::repositories::retrieval_trace::TraceCandidate> {
    use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};

    // Build a lookup from note permalink → pack disposition.
    let mut disposition_by_permalink: std::collections::HashMap<&str, NotePackDisposition> =
        std::collections::HashMap::new();
    for outcome in &packed.outcomes {
        disposition_by_permalink
            .entry(&outcome.permalink)
            .or_insert(outcome.disposition.clone());
    }

    // Build a set of note IDs → permalink for lookup.
    let id_to_permalink: std::collections::HashMap<&str, &str> = notes
        .iter()
        .map(|n| (n.id.as_str(), n.permalink.as_str()))
        .collect();

    // Track seen permalinks among injected candidates for dedupe.
    let mut seen_injected: std::collections::HashSet<String> = std::collections::HashSet::new();

    for candidate in &mut candidates {
        if candidate.outcome != CandidateOutcome::Injected {
            continue;
        }
        // Look up the permalink for this note ID.
        let permalink = candidate.permalink.as_deref().unwrap_or_else(|| {
            id_to_permalink
                .get(candidate.note_id.as_str())
                .copied()
                .unwrap_or("")
        });

        // Check dedupe first.
        if !permalink.is_empty() && !seen_injected.insert(permalink.to_string()) {
            candidate.outcome = CandidateOutcome::Skipped;
            candidate.skipped_reason = Some(SkippedReason::Dedupe);
            continue;
        }

        // Check budget pruning.
        if let Some(&NotePackDisposition::BudgetPruned) = disposition_by_permalink.get(permalink) {
            candidate.outcome = CandidateOutcome::Skipped;
            candidate.skipped_reason = Some(SkippedReason::BudgetPruned);
        }
    }

    candidates
}

/// Per-phase durations (milliseconds) for the knowledge-context retrieval trace.
struct KnowledgeTraceDurations {
    candidate_fetch_ms: i64,
    classify_ms: i64,
    prompt_pack_ms: i64,
    persist_ms: i64,
}

/// Persist a `LoadKnowledgeContext` retrieval trace row. Fail-open: logs and
/// swallows all errors, never propagating them to the caller.
async fn persist_knowledge_trace(
    task: &Task,
    task_paths: &[String],
    candidates: &[djinn_db::repositories::retrieval_trace::TraceCandidate],
    estimated_injected_tokens: i32,
    durations: KnowledgeTraceDurations,
    candidate_cap_exceeded: bool,
    db: &djinn_db::Database,
    identity: Option<KnowledgeContextIdentity<'_>>,
) {
    use djinn_db::repositories::retrieval_trace::{
        CreateRetrievalTraceParams, RetrievalTraceEntryPoint, RetrievalTraceRepository,
        validate_candidates,
    };

    if candidates.is_empty() {
        return;
    }

    // Validate candidate invariants before serialization.
    if let Err(e) = validate_candidates(candidates) {
        tracing::debug!(
            task_id = %task.short_id,
            error = %e,
            "Lifecycle: retrieval trace candidate validation failed; skipping trace persistence"
        );
        return;
    }

    let candidates_json = match serde_json::to_value(candidates) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to serialize retrieval trace candidates; skipping trace persistence"
            );
            return;
        }
    };

    let trigger = serde_json::json!({
        "shape": "scope_paths",
        "task_paths": task_paths,
    });
    let durations_ms = serde_json::json!({
        "candidate_fetch_ms": durations.candidate_fetch_ms,
        "classify_ms": durations.classify_ms,
        "prompt_pack_ms": durations.prompt_pack_ms,
        "persist_ms": durations.persist_ms,
    });

    let cap = djinn_db::repositories::retrieval_trace::DEFAULT_CANDIDATE_CAP;

    let repo = RetrievalTraceRepository::new(db.clone());
    let params = CreateRetrievalTraceParams {
        project_id: &task.project_id,
        session_id: identity.map(|identity| identity.session_id),
        task_run_id: identity.map(|identity| identity.task_run_id),
        task_id: Some(&task.id),
        entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
        trigger: Some(&trigger),
        candidates: &candidates_json,
        candidate_cap: cap,
        candidate_cap_exceeded,
        sampling_metadata: None,
        durations_ms: &durations_ms,
        estimated_injected_tokens,
    };

    if let Err(e) = repo.insert(params).await {
        tracing::debug!(
            task_id = %task.short_id,
            error = %e,
            "Lifecycle: failed to persist retrieval trace for knowledge context; continuing (fail-open)"
        );
    }
}

/// Build full prompt context for one role session. Non-fatal: DB queries fall back to None.
///
/// Independent work phases are overlapped with `tokio::join!` to reduce wall-clock
/// latency while preserving the documented dependency chain:
///
/// 1. Synchronous setup (conflict files, CI directive, role flags).
/// 2. **Concurrent:** activity-text fetch ‖ epic-context fetch.
/// 3. **Concurrent** (after epic_context): knowledge lookup ‖ attempt-history
///    lookups+formatting ‖ code-graph context ‖ reviewer-diff/git setup.
/// 4. Prompt rendering (depends on all prior results).
pub(crate) async fn assemble_prompt_context(inputs: PromptContextInputs<'_>) -> PromptContext {
    let total_start = tokio::time::Instant::now();
    let PromptContextInputs {
        task,
        runtime_role,
        role_for_epic_check,
        project_path,
        worktree_path,
        conflict_ctx,
        merge_validation_ctx,
        prompt_setup_commands,
        system_prompt_extensions,
        resolved_skills,
        app_state,
        knowledge_identity,
        planned_queries,
        read_sources,
        worker_resume_note,
        arbiter_directive,
        mcp_server_instructions,
        extension_diagnostics,
    } = inputs;

    // ── Phase 0: synchronous work with no data dependencies ──
    let conflict_files = format_conflict_files(conflict_ctx);
    let ci_blocking_directive = build_ci_blocking_directive(task);
    let needs_epic_context = role_for_epic_check.needs_epic_context();
    let role_name = runtime_role.config().name;

    // ── Phase 1: activity + epic context concurrently ──
    // Each child measures its own wall-clock time so the child-span
    // metric reports per-child duration, not the phase aggregate.
    let (
        ((activity_text, worker_summary, worker_concerns), _activity_elapsed),
        (epic_context, _epic_elapsed),
    ) = tokio::join!(
        {
            let span = tracing::info_span!(
                "prompt_ctx::activity_db",
                task_id = %task.short_id,
            );
            async {
                let child_start = tokio::time::Instant::now();
                let task_repo =
                    TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                let activity_entries = task_repo.list_activity(&task.id).await.ok();
                let activity_text = format_activity_text(&activity_entries, 3);
                let (worker_summary, worker_concerns) = extract_worker_context(&activity_entries);
                (
                    (activity_text, worker_summary, worker_concerns),
                    child_start.elapsed(),
                )
            }
            .instrument(span)
        },
        {
            let span = tracing::info_span!(
                "prompt_ctx::epic_context",
                task_id = %task.short_id,
            );
            async move {
                let child_start = tokio::time::Instant::now();
                let result = load_epic_context(task, needs_epic_context, app_state).await;
                (result, child_start.elapsed())
            }
            .instrument(span)
        }
    );
    djinn_telemetry::prompt_context_metrics::record_child_span(
        djinn_telemetry::prompt_context_metrics::SPAN_ACTIVITY_DB,
        _activity_elapsed,
    );
    djinn_telemetry::prompt_context_metrics::record_child_span(
        djinn_telemetry::prompt_context_metrics::SPAN_EPIC_CONTEXT,
        _epic_elapsed,
    );

    // ── Phase 2: knowledge, attempt history, code-graph, and reviewer-diff
    //    concurrently.  Knowledge and code-graph depend on epic_context (available
    //    from phase 1).  Attempt-history lookups are independent of epic_context
    //    but must complete before prompt rendering.  Reviewer-diff is fully
    //    independent once role_name is known. ──
    // Each child measures its own wall-clock time so the child-span
    // metric reports per-child duration, not the phase aggregate.
    let epic_context_ref = epic_context.as_deref();
    let task_paths_for_code_graph = derive_task_scope_paths(task, epic_context_ref);
    let (
        (knowledge_context, _knowledge_elapsed),
        ((prior_attempts, completed_dependency_parents, activity_text), _attempt_elapsed),
        (code_graph_context, _code_graph_elapsed),
        (reviewer_diff_context, _reviewer_elapsed),
    ) = tokio::join!(
        // Knowledge context (depends on epic_context)
        {
            let span = tracing::info_span!(
                "prompt_ctx::knowledge_context",
                task_id = %task.short_id,
            );
            async move {
                let child_start = tokio::time::Instant::now();
                let result = load_knowledge_context(
                    task,
                    epic_context_ref,
                    app_state,
                    knowledge_identity,
                    planned_queries,
                )
                .await;
                (result, child_start.elapsed())
            }
            .instrument(span)
        },
        // Attempt-history lookups + formatting (depends on activity_text for budget)
        {
            let span = tracing::info_span!(
                "prompt_ctx::attempt_history",
                task_id = %task.short_id,
            );
            async {
                let child_start = tokio::time::Instant::now();
                let task_attempt_repo = djinn_db::TaskAttemptRepository::new(app_state.db.clone());
                let prior_attempts =
                    attempt_context::load_prior_attempts(task, &task_attempt_repo).await;
                let completed_dependency_parents =
                    attempt_context::load_completed_dependency_parents(task, &task_attempt_repo)
                        .await;
                // Append attempt history to the existing activity text so it renders
                // inside the Activity Log section, not as a new competing top-level
                // prompt section.  The attempt entries share the
                // COMBINED_BRIEF_TOTAL_CHARS budget with existing feedback.
                // Deduplication prevents rendering the same rejection text twice;
                // over-budget output drops oldest attempt entries first, then oldest
                // dependency-parent entries, with a deterministic truncation note.
                let activity_text = {
                    let existing_len = activity_text.as_deref().map_or(0, |s| s.len());
                    let remaining_budget = COMBINED_BRIEF_TOTAL_CHARS.saturating_sub(existing_len);
                    let attempt_history_text = format_attempt_history(
                        prior_attempts.as_deref().unwrap_or(&[]),
                        completed_dependency_parents.as_deref().unwrap_or(&[]),
                        activity_text.as_deref().unwrap_or(""),
                        remaining_budget,
                    );
                    match (activity_text, attempt_history_text) {
                        (Some(activity), Some(history)) => {
                            Some(format!("{activity}\n\n---\n\n{history}"))
                        }
                        (activity @ Some(_), None) => activity,
                        (None, history @ Some(_)) => history,
                        (None, None) => None,
                    }
                };
                (
                    (prior_attempts, completed_dependency_parents, activity_text),
                    child_start.elapsed(),
                )
            }
            .instrument(span)
        },
        // Code-graph context (depends on epic_context)
        {
            let span = tracing::info_span!(
                "prompt_ctx::code_graph",
                task_id = %task.short_id,
            );
            async move {
                let child_start = tokio::time::Instant::now();
                let result = build_role_code_graph_context(
                    role_name,
                    task,
                    app_state,
                    project_path,
                    &task_paths_for_code_graph,
                )
                .await;
                (result, child_start.elapsed())
            }
            .instrument(span)
        },
        // Reviewer-diff / git context (depends only on role_name)
        {
            let span = tracing::info_span!(
                "prompt_ctx::reviewer_diff",
                task_id = %task.short_id,
            );
            async move {
                let child_start = tokio::time::Instant::now();
                let result =
                    if crate::actors::slot::helpers::is_role_auto_code_context_enabled(role_name) {
                        let (from_sha, to_sha) =
                            resolve_reviewer_diff_shas(worktree_path, &task.project_id, app_state)
                                .await;
                        if from_sha.is_some() || to_sha.is_some() {
                            build_reviewer_diff_context(
                                role_name,
                                task,
                                app_state,
                                project_path,
                                from_sha.as_deref(),
                                to_sha.as_deref(),
                            )
                            .await
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                (result, child_start.elapsed())
            }
            .instrument(span)
        }
    );
    djinn_telemetry::prompt_context_metrics::record_child_span(
        djinn_telemetry::prompt_context_metrics::SPAN_KNOWLEDGE_CONTEXT,
        _knowledge_elapsed,
    );
    djinn_telemetry::prompt_context_metrics::record_child_span(
        djinn_telemetry::prompt_context_metrics::SPAN_ATTEMPT_HISTORY,
        _attempt_elapsed,
    );
    djinn_telemetry::prompt_context_metrics::record_child_span(
        djinn_telemetry::prompt_context_metrics::SPAN_CODE_GRAPH,
        _code_graph_elapsed,
    );
    djinn_telemetry::prompt_context_metrics::record_child_span(
        djinn_telemetry::prompt_context_metrics::SPAN_REVIEWER_DIFF,
        _reviewer_elapsed,
    );

    // ── Phase 3: prompt rendering (depends on all prior results) ──
    let base_system_prompt = runtime_role.render_prompt(
        task,
        &TaskContext {
            project_path: project_path.to_string(),
            workspace_path: worktree_path.display().to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files: conflict_files.clone(),
            merge_base_branch: conflict_ctx.map(|m| m.base_branch.clone()),
            merge_target_branch: conflict_ctx.map(|m| m.merge_target.clone()),
            merge_failure_context: merge_validation_ctx.clone(),
            setup_commands: prompt_setup_commands.clone(),
            activity: activity_text.clone(),
            worker_summary: worker_summary.clone(),
            worker_concerns: worker_concerns.clone(),
            epic_context: epic_context.clone(),
            knowledge_context: knowledge_context.clone(),
            code_graph_context: code_graph_context.clone(),
            reviewer_diff_context: reviewer_diff_context.clone(),
            ci_blocking_directive: ci_blocking_directive.clone(),
            worker_resume_note: worker_resume_note.map(str::to_string),
            arbiter_directive: arbiter_directive.map(str::to_string),
        },
    );
    let system_prompt_with_extensions =
        apply_role_extensions(&base_system_prompt, system_prompt_extensions);
    let system_prompt = apply_prompt_sections(
        &base_system_prompt,
        system_prompt_extensions,
        resolved_skills,
        read_sources,
        mcp_server_instructions,
    );
    // 7ry9: Hash the final provider-facing system prompt *after* all
    // extensions, skills, read sources, MCP instructions, and truncation.
    // The hash is over the exact bytes supplied to the provider.
    let system_prompt_hash = djinn_roles::prompts::rendered_system_prompt_hash(&system_prompt);
    djinn_telemetry::prompt_context_metrics::record_total(total_start.elapsed());
    PromptContext {
        conflict_files,
        activity_text,
        worker_summary,
        worker_concerns,
        epic_context,
        knowledge_context,
        code_graph_context,
        reviewer_diff_context,
        ci_blocking_directive,
        worker_resume_note: worker_resume_note.map(str::to_string),
        arbiter_directive: arbiter_directive.map(str::to_string),
        prior_attempts,
        completed_dependency_parents,
        base_system_prompt,
        system_prompt_with_extensions,
        system_prompt,
        system_prompt_hash,
        prompt_setup_commands,
        extension_diagnostics: extension_diagnostics.to_vec(),
    }
}

/// Build BLOCKING directive for red required CI. Returns None for passing/advisory states.
fn build_ci_blocking_directive(task: &Task) -> Option<String> {
    if task.ci_status != "failing" {
        return None;
    }
    let base_sha = task.ci_last_remediation_base_sha.as_deref()?;
    let head_sha = task.ci_head_sha.as_deref().unwrap_or("unknown");
    let pr_number = task.ci_pr_number.unwrap_or(0);
    let check_names: Vec<String> =
        serde_json::from_str(&task.ci_blocking_required_check_names).unwrap_or_default();
    let checks_display = if check_names.is_empty() {
        "unknown".to_string()
    } else {
        check_names.join(", ")
    };
    let fingerprint_line = match &task.ci_failure_fingerprint {
        Some(fp) => format!("**Failure fingerprint:** `{fp}`\n"),
        None => String::new(),
    };
    Some(format!(
        "**PR:** #{pr_number}\\\n\
         **Failing head SHA:** `{head_sha}`\\\n\
         **Blocking checks:** {checks_display}\\\n\
         {fingerprint_line}\
         **Remediation baseline SHA:** `{base_sha}`\n\n\
         > REQUIRED CI is failing on the current PR head. You MUST fix the \
         failing required checks listed above before this task can proceed. \
         The task will remain in remediation until all blocking checks pass \
         on a new commit pushed to the PR branch."
    ))
}

/// Resolve (from_sha, to_sha) for reviewer diff context via git. Best-effort.
async fn resolve_reviewer_diff_shas(
    worktree_path: &Path,
    project_id: &str,
    app_state: &AgentContext,
) -> (Option<String>, Option<String>) {
    let target_branch = {
        let repo =
            djinn_db::ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        match repo.get_config(project_id).await {
            Ok(Some(config)) => config.target_branch,
            _ => "main".to_string(),
        }
    };
    let head_sha = djinn_git::rev_parse(worktree_path, "HEAD").await.ok();
    let base_sha = djinn_git::merge_base(worktree_path, &target_branch, "HEAD")
        .await
        .ok();
    (base_sha, head_sha)
}

/// Only the worker role receives resume context.
pub(crate) fn role_receives_worker_resume(role_name: &str) -> bool {
    role_name == "worker"
}

/// Only the worker role receives the arbiter directive.
pub(crate) fn role_receives_arbiter_directive(role_name: &str) -> bool {
    role_name == "worker"
}

/// Load the arbiter directive for a monitored reopen. Returns `None` for
/// non-worker roles or when no monitored reopen is in progress.
///
/// The directive is loaded from the latest unconsumed arbitration row only
/// when `monitored_reopen_count >= 1` AND the directive has not yet been
/// injected (`directive_injected == false`). The one-shot guard is enforced
/// atomically: `mark_directive_injected` flips `directive_injected` from
/// `false` to `true` with a conditional `WHERE directive_injected = false`
/// clause. Only the first worker prompt wins the race; any second worker
/// prompt (re-entry) will see `directive_injected == true` and return `None`.
pub(crate) async fn load_arbiter_directive(
    role_name: &str,
    task_id: &str,
    app_state: &AgentContext,
) -> Option<String> {
    if !role_receives_arbiter_directive(role_name) {
        return None;
    }
    use djinn_db::repositories::task_arbitration::TaskArbitrationRepository;
    let arb_repo = TaskArbitrationRepository::new(app_state.db.clone());
    let (_hold_cycle, unconsumed_record) =
        arb_repo.resolve_current_hold_cycle(task_id).await.ok()?;
    let record = unconsumed_record?;
    // Only inject when a monitored reopen attempt is in progress.
    if record.monitored_reopen_count < 1 {
        return None;
    }
    // One-shot guard: if the directive was already injected for this monitored
    // reopen, do not inject it again.
    if record.directive_injected {
        return None;
    }
    // Atomically claim the injection.  If the UPDATE affects zero rows the
    // directive was already injected by a concurrent prompt; return None.
    let claimed = arb_repo
        .mark_directive_injected(task_id, record.hold_cycle)
        .await
        .ok()?;
    if !claimed {
        return None;
    }
    // Extract the directive text from the structured JSON payload.
    record
        .directive
        .as_ref()
        .and_then(|d| d.get("directive"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Build one-line worker resume note. Returns None when not applicable.
pub(crate) fn build_worker_resume_note(
    role_name: &str,
    metadata: Option<&djinn_runtime::ResumeLifecycleMetadata>,
) -> Option<String> {
    if !role_receives_worker_resume(role_name) {
        return None;
    }
    let metadata = metadata?;
    if !metadata.considered {
        return None;
    }
    let has_checkpoint = metadata.commit_sha.is_some();
    let has_submit_or_review = metadata
        .submit_or_review_id
        .as_ref()
        .is_some_and(|id| !id.trim().is_empty());
    let has_prior_session = metadata
        .prior_session_lineage
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    let has_failover_context = metadata.new_model.is_some() || metadata.failover_reason.is_some();
    if !has_checkpoint && !has_submit_or_review && !has_prior_session && !has_failover_context {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    // ── Resume source details ──────────────────────────────────────────
    if let Some(session) = &metadata.prior_session_lineage
        && !session.trim().is_empty()
    {
        parts.push(format!("prior session `{session}`"));
    }
    if let Some(source_kind) = &metadata.source_kind {
        parts.push(format!("source: {}", source_kind_label(*source_kind)));
    }
    if let Some(sha) = &metadata.commit_sha
        && !sha.trim().is_empty()
    {
        parts.push(format!("checkpoint `{sha}`"));
    } else if let Some(id) = &metadata.submit_or_review_id
        && !id.trim().is_empty()
    {
        parts.push(format!("submit/review `{id}`"));
    }
    if let Some(target_ref) = &metadata.target_ref
        && !target_ref.trim().is_empty()
    {
        parts.push(format!("target ref `{target_ref}`"));
    }
    // ── Termination / failover context ─────────────────────────────────
    if let Some(reason) = metadata.selection_reason {
        parts.push(format!("terminated: {}", termination_label(reason)));
    }
    if let Some(failover_reason) = &metadata.failover_reason
        && !failover_reason.trim().is_empty()
    {
        parts.push(format!("failover reason: {failover_reason}"));
    }
    // ── Model context ──────────────────────────────────────────────────
    if let Some(prev_model) = &metadata.previous_model
        && !prev_model.trim().is_empty()
    {
        parts.push(format!("prev model `{prev_model}`"));
    }
    if let Some(new_model) = &metadata.new_model
        && !new_model.trim().is_empty()
    {
        parts.push(format!("current model `{new_model}`"));
    }
    // ── Progress / verification ────────────────────────────────────────
    if let Some(summary) = &metadata.last_durable_progress_summary
        && !summary.trim().is_empty()
    {
        // Cut on a char boundary: a byte-index slice panics when the cut
        // lands inside a multi-byte char in the free-text summary.
        let truncated = match summary.char_indices().nth(117) {
            Some((byte_idx, _)) => format!("{}…", &summary[..byte_idx]),
            None => summary.clone(),
        };
        parts.push(format!("last progress: {truncated}"));
    }
    if let Some(cmd) = &metadata.verification_command
        && !cmd.trim().is_empty()
    {
        parts.push(format!("verify: `{cmd}`"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "**Resuming from prior session.** {}",
        parts.join("; ")
    ))
}

/// Map a [`ResumeSelectionReason`] to a human-readable termination label.
fn termination_label(reason: djinn_runtime::ResumeSelectionReason) -> &'static str {
    use djinn_runtime::ResumeSelectionReason as R;
    match reason {
        R::AutoSubmitAccepted => "auto-submit accepted",
        R::LatestSafeCheckpoint => "no-progress checkpoint",
        R::AlternateCheckpointRef => "alternate checkpoint ref",
        R::CleanTaskBranchFallback => "clean fallback",
        R::NewerTaskBranch => "newer task branch",
        R::CheckpointMissing => "checkpoint missing",
        R::CheckpointUnsafe => "checkpoint unsafe",
        R::MergeConflict => "merge conflict",
        R::Disabled => "resume disabled",
    }
}

/// Map a [`ResumeSourceKind`] to a concise human-readable label.
fn source_kind_label(kind: djinn_runtime::ResumeSourceKind) -> &'static str {
    use djinn_runtime::ResumeSourceKind as K;
    match kind {
        K::AutoSubmit => "auto-submit",
        K::TaskBranchCheckpoint => "task-branch checkpoint",
        K::AlternateCheckpointRef => "alternate checkpoint ref",
        K::CleanTaskBranch => "clean task branch",
    }
}

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "prompt_context_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "attempt_history_prompt_tests.rs"]
mod attempt_history_tests;

#[cfg(test)]
#[path = "ci_directive_tests.rs"]
mod ci_directive_tests;

#[cfg(test)]
#[path = "knowledge_trace_tests.rs"]
mod knowledge_trace_tests;

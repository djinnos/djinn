// djinn:allow-oversize — cohesive prompt assembly; follow-up modularization is out of scope.
//! Role-specific prompt-context assembly: conflict, activity, epic, knowledge,
//! code-graph, and CI directives → rendered system prompt with extensions + skills.

use std::path::{Path, PathBuf};

use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_core::models::Task;

use crate::actors::slot::MergeConflictMetadata;
use crate::actors::slot::helpers::{
    BaseTreeProvider, COMBINED_BRIEF_TOTAL_CHARS, KnowledgePackConfig, ListedBaseTree,
    NotePackDisposition, ScopeFallbackReason, build_reviewer_diff_context,
    build_role_code_graph_context, derive_task_scope_path_tokens, derive_task_scope_paths,
    extract_worker_context, format_attempt_history, pack_ranked_knowledge_notes, recent_feedback,
};
use crate::actors::slot::lifecycle::attempt_context;
use crate::context::AgentContext;
use crate::prompts::{TaskContext, apply_role_extensions, apply_skills};
use crate::rollout::{DefaultPolicy, RolloutMode, parse as parse_rollout};
use crate::skills::ResolvedSkill;
use djinn_db::{NoteRepository, ProposalRepository, TaskRepository};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

// Environment variables are process-global. Keep the test guard here, rather
// than in an individual test module, so every knowledge-context test that
// reads or changes the rollout configuration serializes with assembly tests.
#[cfg(any(test, feature = "test-support"))]
pub(crate) static KNOWLEDGE_CONTEXT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct KnowledgeContextTestEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    rollout: Option<std::ffi::OsString>,
    legacy: Option<std::ffi::OsString>,
    mirror_root: Option<std::ffi::OsString>,
}

// Only in-crate unit tests mutate the rollout variables. Test-support callers
// still use the guard to serialize production reads, but do not need setters.
#[cfg(test)]
impl KnowledgeContextTestEnvGuard {
    pub(super) fn clear(&mut self) {
        // SAFETY: this guard serializes all knowledge-context rollout tests.
        unsafe {
            std::env::remove_var(KNOWLEDGE_CONTEXT_ROLLOUT_ENV);
            std::env::remove_var(KNOWLEDGE_CONTEXT_LEGACY_ENV);
        }
    }

    pub(super) fn set_rollout(&mut self, value: &str) {
        // SAFETY: this guard serializes all knowledge-context rollout tests.
        unsafe { std::env::set_var(KNOWLEDGE_CONTEXT_ROLLOUT_ENV, value) }
    }

    pub(super) fn set_legacy(&mut self, value: &str) {
        // SAFETY: this guard serializes all knowledge-context rollout tests.
        unsafe { std::env::set_var(KNOWLEDGE_CONTEXT_LEGACY_ENV, value) }
    }

    /// Point bare-mirror resolution at `root` for the life of this guard, so a
    /// test can stand up the repository that `resolve_base_tree_root` reads.
    pub(super) fn set_mirror_root(&mut self, root: &std::path::Path) {
        // SAFETY: this guard serializes all knowledge-context rollout tests.
        unsafe { std::env::set_var(MIRROR_ROOT_ENV, root) }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for KnowledgeContextTestEnvGuard {
    fn drop(&mut self) {
        // SAFETY: the guard is still held while the original process environment
        // is restored, including during unwinding from a failed assertion.
        unsafe {
            match &self.rollout {
                Some(value) => std::env::set_var(KNOWLEDGE_CONTEXT_ROLLOUT_ENV, value),
                None => std::env::remove_var(KNOWLEDGE_CONTEXT_ROLLOUT_ENV),
            }
            match &self.legacy {
                Some(value) => std::env::set_var(KNOWLEDGE_CONTEXT_LEGACY_ENV, value),
                None => std::env::remove_var(KNOWLEDGE_CONTEXT_LEGACY_ENV),
            }
            match &self.mirror_root {
                Some(value) => std::env::set_var(MIRROR_ROOT_ENV, value),
                None => std::env::remove_var(MIRROR_ROOT_ENV),
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn knowledge_context_test_env_guard() -> KnowledgeContextTestEnvGuard {
    let lock = KNOWLEDGE_CONTEXT_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    KnowledgeContextTestEnvGuard {
        rollout: std::env::var_os(KNOWLEDGE_CONTEXT_ROLLOUT_ENV),
        legacy: std::env::var_os(KNOWLEDGE_CONTEXT_LEGACY_ENV),
        mirror_root: std::env::var_os(MIRROR_ROOT_ENV),
        _lock: lock,
    }
}

/// Mirror-root override honoured by [`crate::repo_access::mirror_root`], saved
/// and restored by [`KnowledgeContextTestEnvGuard`] because base-tree
/// resolution reads it.
#[cfg(any(test, feature = "test-support"))]
const MIRROR_ROOT_ENV: &str = "DJINN_MIRROR_ROOT";

mod ci_directive;
mod diagnostics;
mod planner_enrichment;
mod types;
use ci_directive::build_ci_blocking_directive;
use planner_enrichment::merge_planned_knowledge;
#[allow(unused_imports)] // Lifecycle seams are consumed by stage wiring and test modules.
pub(crate) use types::{
    MemoryIntentPlannerHost, MemoryIntentPlannerInvocation, PlannedNoteSearch, PromptContext,
    PromptContextInputs, ReadSourceInfo, SupervisorPlannerHost,
};
// Re-export for `use super::*` in test modules.
#[allow(unused_imports)]
pub(super) use diagnostics::{
    EXTENSION_DIAGNOSTICS_HEADING, MAX_EXTENSION_DIAGNOSTIC_RECORDS,
    MAX_EXTENSION_DIAGNOSTIC_SECTION_BYTES, insert_diagnostics_before_task,
    render_extension_diagnostics,
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

/// Apply extensions, diagnostics, skills, read sources, and MCP instructions
/// in canonical order.
fn apply_prompt_sections(
    base_system_prompt: &str,
    system_prompt_extensions: &str,
    resolved_skills: &[ResolvedSkill],
    read_sources: &[ReadSourceInfo],
    mcp_server_instructions: &std::collections::BTreeMap<String, String>,
    extension_diagnostics: &[ExtensionLoadDiagnosticV1],
) -> String {
    // Always split at the task boundary so platform and task bytes are
    // byte-identical with and without diagnostics.
    let with_extensions = match render_extension_diagnostics(extension_diagnostics) {
        Some(diagnostics) => insert_diagnostics_before_task(
            base_system_prompt,
            system_prompt_extensions,
            &diagnostics,
        ),
        None => insert_diagnostics_before_task(base_system_prompt, system_prompt_extensions, ""),
    };
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
            tracing::warn!(
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
            tracing::warn!(
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
            "**Memory refs:** call `epic_show({})` then `memory_read(identifier=<ref>)` for each — notes are stored in the project database and accessed through the memory_* MCP tools.",
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

/// Note types queried for knowledge-context injection.
const KNOWLEDGE_NOTE_TYPES: &[&str] = &["pattern", "pitfall", "case"];

const KNOWLEDGE_CONTEXT_ROLLOUT_ENV: &str = "DJINN_KNOWLEDGE_CONTEXT_ROLLOUT";
const KNOWLEDGE_CONTEXT_LEGACY_ENV: &str = "DJINN_KNOWLEDGE_CONTEXT";

/// Parse the operator-owned knowledge-context gate once at the assembly boundary.
/// Cohorts are deployment labels only; no session assignment occurs here.
fn knowledge_context_rollout_from_env() -> RolloutMode {
    let rollout = std::env::var(KNOWLEDGE_CONTEXT_ROLLOUT_ENV).ok();
    let legacy = std::env::var(KNOWLEDGE_CONTEXT_LEGACY_ENV).ok();
    parse_rollout(
        rollout.as_deref(),
        legacy.as_deref(),
        DefaultPolicy::Enabled,
    )
}

fn disabled_knowledge_outcome(
    rollout: &RolloutMode,
) -> djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome {
    use djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome;

    match rollout {
        RolloutMode::Off => RetrievalTraceOutcome::DisabledOff,
        RolloutMode::KillSwitch => RetrievalTraceOutcome::DisabledKillSwitch,
        RolloutMode::LegacyDisabled => RetrievalTraceOutcome::DisabledLegacy,
        _ => unreachable!("only disabled rollout modes request a suppression trace"),
    }
}

/// Load knowledge context through the ranked RRF retrieval path.
///
/// Instruments retrieval with a fail-open `LoadKnowledgeContext` trace row.
///
/// ## Trace contract (epic 3paf; extended by proposal `5205`)
///
/// - **Entry point:** `LoadKnowledgeContext` → `"load_knowledge_context"`.
/// - **Trigger:** `{ "shape": "ranked_injection_v1", "strategy",
///   "ranking_profile", "task_paths", "scope_fallback_reason",
///   "candidate_window", "rrf_k", "search_error" }`.
/// - **Outcomes** (`TraceCandidate`): `injected` (top-K, survived budget — no
///   reason), `min_confidence` (<0.3), `not_top_k`, `budget_pruned`,
///   `oversized_skipped`. Each candidate's `scope` additionally carries its
///   per-signal ranks and its fused rank/score.
/// - **Durations:** `candidate_fetch_ms`, `classify_ms`, `prompt_pack_ms`, `persist_ms`.
/// - **Tokens:** `ceil(injected_chars/4)`.
/// - **Fail-open:** trace errors are logged and swallowed. A *retrieval* error
///   injects no knowledge, records `search_error`, and still lets the rest of
///   the prompt render.
#[allow(dead_code)] // Scope-only entry point remains available to focused lifecycle tests.
pub(crate) async fn load_knowledge_context(
    task: &Task,
    epic_context: Option<&str>,
    app_state: &AgentContext,
    base_tree: Option<&ListedBaseTree>,
) -> Option<String> {
    let rollout = knowledge_context_rollout_from_env();
    let cancellation = CancellationToken::new();
    load_knowledge_context_with_planner(
        task,
        epic_context,
        app_state,
        None,
        &rollout,
        &cancellation,
        base_tree,
    )
    .await
}

/// The project's configured target branch, defaulting to `main`.
pub(crate) async fn project_target_branch(task: &Task, app_state: &AgentContext) -> String {
    let repo = djinn_db::ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match repo.get_config(&task.project_id).await {
        Ok(Some(config)) => config.target_branch,
        _ => "main".to_owned(),
    }
}

/// Resolve the filesystem root whose Git history holds the attempt's base
/// revision.
///
/// **This is not `project_path`.** `PromptContextInputs::project_path` is the
/// value the prompt hands to MCP tools as `project=…`, and dispatch sets it to
/// `task.project_id` — a UUID, deliberately, because `ProjectRepository::resolve`
/// accepts ids and `owner/repo` slugs but not filesystem paths
/// (`supervisor_impl::stage`). Handing that UUID to `git` made
/// [`load_base_tree`] return `None` for *every* production dispatch, so
/// `derive_task_scope_paths` recorded `tree_provider_unavailable`, the validated
/// scope was always empty, and knowledge injection degenerated to its lexical
/// signal alone.
///
/// The server and every task-run pod carry the project's **bare mirror** at
/// `{mirror_root}/{project_id}.git`, which holds exactly the revision an attempt
/// branches from, so that is the primary source. A `project_path` that really is
/// a directory (local runs, worker worktrees, tests) is the fallback. Returning
/// `None` keeps the specified degradation: empty scope with a typed reason.
pub(crate) fn resolve_base_tree_root(project_id: &str, project_path: &str) -> Option<PathBuf> {
    let mirror = crate::repo_access::mirror_path(project_id);
    if mirror.is_dir() {
        return Some(mirror);
    }
    let direct = Path::new(project_path);
    direct.is_dir().then(|| direct.to_path_buf())
}

/// Build the base-revision tree provider for `task`'s attempt.
///
/// Resolution order is `origin/<target>`, then `<target>`, then `HEAD`. A bare
/// mirror publishes the upstream branch as a local head (`refs/heads/main`, no
/// `origin/` remote-tracking ref), which is why plain `<target>` must stay in
/// the ladder. Every failure mode returns `None`, which
/// [`derive_task_scope_paths`] turns into an explicit empty scope with a typed
/// fallback reason: provider unavailability is a specified degradation, never a
/// hard error and never a licence to trust unvalidated prose tokens.
pub(crate) async fn load_base_tree(
    repo_root: &Path,
    target_branch: &str,
) -> Option<ListedBaseTree> {
    if !repo_root.exists() {
        return None;
    }
    let candidates = [
        format!("origin/{target_branch}"),
        target_branch.to_owned(),
        "HEAD".to_owned(),
    ];
    for revision in candidates {
        match djinn_git::list_tracked_paths(repo_root, &revision).await {
            Ok(paths) if !paths.is_empty() => {
                return Some(ListedBaseTree::from_tracked_files(paths));
            }
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(
                    revision = %revision,
                    error = %error,
                    "load_base_tree: revision unavailable; trying the next candidate"
                );
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn load_knowledge_context_with_planner(
    task: &Task,
    epic_context: Option<&str>,
    app_state: &AgentContext,
    planner: Option<&MemoryIntentPlannerInvocation<'_>>,
    rollout: &RolloutMode,
    cancellation: &CancellationToken,
    base_tree: Option<&ListedBaseTree>,
) -> Option<String> {
    // Proposal 5205: prose tokens are validated against the task attempt's
    // base-revision Git tree. With no provider this is an explicit *empty*
    // scope plus a typed reason — never the old unvalidated regex output.
    let derived_scope = derive_task_scope_paths(
        task,
        epic_context,
        base_tree.map(|tree| tree as &dyn BaseTreeProvider),
    );
    let task_paths = derived_scope.paths.clone();
    let scope_fallback_reason = derived_scope.fallback_reason;
    if cancellation.is_cancelled() {
        persist_cancelled_knowledge_trace(task, &task_paths, app_state, planner, rollout).await;
        return None;
    }
    if !rollout.enabled() {
        persist_knowledge_trace(
            task,
            &task_paths,
            &[],
            0,
            KnowledgeTraceDurations::default(),
            false,
            &app_state.db,
            planner.map(|p| (p.session_id, p.task_run_id)),
            rollout,
            disabled_knowledge_outcome(rollout),
            false,
            None,
            &KnowledgeTraceStrategy {
                scope_fallback_reason: scope_fallback_reason.map(ScopeFallbackReason::as_str),
                ..KnowledgeTraceStrategy::default()
            },
        )
        .await;
        return None;
    }
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let top_k = app_state.knowledge_injection.knowledge_injection_limit as usize;
    let query = knowledge_injection_query(task, epic_context);
    let mut strategy = KnowledgeTraceStrategy {
        scope_fallback_reason: scope_fallback_reason.map(ScopeFallbackReason::as_str),
        ..KnowledgeTraceStrategy::default()
    };

    let fetch_start = tokio::time::Instant::now();

    // Proposal 5205: candidates come *only* from the RRF search path under
    // `KnowledgeInjectionV1`. The recency-ordered `query_by_scope_overlap`
    // query is retired for this entry point; the JIT pitfalls call site keeps
    // using it. There is no separate trace-candidate universe any more — the
    // single fused list is both the prompt input and the trace universe.
    let retrieval = tokio::select! {
        _ = cancellation.cancelled() => {
            persist_cancelled_knowledge_trace(task, &task_paths, app_state, planner, rollout).await;
            return None;
        }
        result = note_repo.search_knowledge_injection_candidates(
            djinn_db::repositories::note::KnowledgeInjectionSearchParams {
                project_id: &task.project_id,
                query: &query,
                task_id: Some(&task.id),
                note_types: KNOWLEDGE_NOTE_TYPES,
                task_paths: &task_paths,
                top_k,
                semantic_scores: None,
            },
        ) => result,
    };
    let candidate_fetch_ms = fetch_start.elapsed().as_millis() as i64;
    if cancellation.is_cancelled() {
        persist_cancelled_knowledge_trace(task, &task_paths, app_state, planner, rollout).await;
        return None;
    }

    let search = match retrieval {
        Ok(search) => search,
        Err(e) => {
            // Ranked-search failure is non-fatal to prompt construction: no
            // knowledge is injected, the trace records `search_error`, and the
            // caller still renders the rest of the prompt. Stale unranked notes
            // are never injected as an error fallback.
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: ranked knowledge retrieval failed; injecting no knowledge"
            );
            strategy.search_error = Some(e.to_string());
            persist_knowledge_trace(
                task,
                &task_paths,
                &[],
                0,
                KnowledgeTraceDurations {
                    candidate_fetch_ms,
                    classify_ms: 0,
                    prompt_pack_ms: 0,
                    persist_ms: 0,
                },
                false,
                &app_state.db,
                planner.map(|p| (p.session_id, p.task_run_id)),
                rollout,
                djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome::Error,
                false,
                None,
                &strategy,
            )
            .await;
            return None;
        }
    };

    strategy.ranking_profile = search.profile.as_str();
    strategy.candidate_window = search.candidate_window;
    strategy.rrf_k = Some(search.rrf_k);

    let classification_start = tokio::time::Instant::now();
    let notes: Vec<djinn_memory::Note> = search
        .candidates
        .iter()
        .map(|candidate| candidate.note.clone())
        .collect();
    // `search_knowledge_injection_candidates` already truncates to the fixed
    // 50-note window, so packing sees at most that many candidates.
    let candidate_cap_exceeded = notes.len() >= search.candidate_window;

    // Exactly one ordered list is packed exactly once. Confidence floor, top-k,
    // and byte budget are applied here and nowhere else; no unranked scope
    // block is prepended and no note-type quota exists.
    let pack_start = tokio::time::Instant::now();
    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            minimum_confidence: KNOWLEDGE_MIN_CONFIDENCE,
            top_k,
            total_byte_budget: app_state
                .knowledge_injection
                .knowledge_injection_budget_bytes as usize,
            line_byte_cap: app_state
                .knowledge_injection
                .knowledge_injection_line_cap_bytes as usize,
        },
    );
    let pack_ms = pack_start.elapsed().as_millis() as i64;
    if cancellation.is_cancelled() {
        persist_cancelled_knowledge_trace(task, &task_paths, app_state, planner, rollout).await;
        return None;
    }
    let trace_candidates_final =
        injection_trace_candidates(&search.candidates, &packed, search.profile.as_str());
    let terminal_dispositions = pack_disposition_counts(&packed);
    let classification_ms = classification_start.elapsed().as_millis() as i64;
    let estimated_injected_tokens = packed.total_injected_tokens as i32;

    // Planner duplicate suppression must see only the notes that actually
    // reached the prompt. A candidate dropped by the confidence floor, top-k,
    // or the byte budget is *not* in the prompt, so it must not suppress a
    // matching planned-search result — passing the whole ranked list here would
    // silently hide planner enrichment behind notes the agent never sees.
    let injected_notes: Vec<djinn_memory::Note> = notes
        .iter()
        .zip(&packed.outcomes)
        .filter(|(_, outcome)| outcome.disposition == NotePackDisposition::Injected)
        .map(|(note, _)| note.clone())
        .collect();

    let rendered = (!packed.rendered.is_empty()).then_some(packed.rendered.clone());
    let rendered = merge_planned_knowledge(
        rendered,
        &injected_notes,
        &note_repo,
        task,
        planner,
        app_state.knowledge_injection,
    )
    .await;
    if cancellation.is_cancelled() {
        persist_cancelled_knowledge_trace(task, &task_paths, app_state, planner, rollout).await;
        return None;
    }

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
        planner.map(|p| (p.session_id, p.task_run_id)),
        rollout,
        if estimated_injected_tokens > 0
            && trace_candidates_final.iter().any(|c| {
                c.outcome == djinn_db::repositories::retrieval_trace::CandidateOutcome::Injected
            })
        {
            djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome::Injected
        } else {
            djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome::Empty
        },
        false,
        Some(terminal_dispositions),
        &strategy,
    )
    .await;

    rendered
}

/// Write the exceptional cancellation terminal without fabricating a candidate
/// universe or disposition histogram.
async fn persist_cancelled_knowledge_trace(
    task: &Task,
    task_paths: &[String],
    app_state: &AgentContext,
    planner: Option<&MemoryIntentPlannerInvocation<'_>>,
    rollout: &RolloutMode,
) {
    persist_knowledge_trace(
        task,
        task_paths,
        &[],
        0,
        KnowledgeTraceDurations::default(),
        false,
        &app_state.db,
        planner.map(|p| (p.session_id, p.task_run_id)),
        rollout,
        djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome::Error,
        true,
        None,
        &KnowledgeTraceStrategy::default(),
    )
    .await;
}

/// The lexical query knowledge injection retrieves with: the task's own title
/// and description.
///
/// This is what makes ranking task-relative at all — the retired path had no
/// task-to-note relevance input beyond boolean scope eligibility.
///
/// # Why not the design body or the epic context
///
/// `sanitize_postgres_tsquery` **AND**-joins terms and keeps only the first
/// **12**. Every additional word therefore makes the query strictly harder to
/// satisfy, and past the twelfth it silently displaces a more topical earlier
/// term. Concatenating the design body or the epic blob does not broaden
/// recall — it reliably drives the lexical signal to zero matches, because no
/// single note contains all twelve leading terms of a task *and* its epic.
///
/// The title plus description is the shortest text that still identifies the
/// task. The remaining five signals (embedding, temporal, graph, task-affinity,
/// validated scope) carry the context this deliberately omits; the epic in
/// particular already reaches fusion through task affinity, which scores notes
/// from the epic's own `memory_refs`.
///
/// `epic_context` is accepted so the call site stays honest about what it has
/// and this decision stays visible at the boundary rather than at the caller.
pub(crate) fn knowledge_injection_query(task: &Task, _epic_context: Option<&str>) -> String {
    let mut parts: Vec<&str> = vec![task.title.as_str()];
    if !task.description.trim().is_empty() {
        parts.push(task.description.as_str());
    }
    parts.join("\n")
}

/// Test-only: a note body the task's own text will retrieve.
///
/// Proposal `5205` replaced boolean scope-overlap eligibility with relevance
/// ranking, so a note now has to be *about the task* to be retrieved at all.
/// The retired query returned every global note above the confidence floor
/// regardless of content; tests that need a note injected must therefore give
/// it a genuine lexical relationship to the task instead of relying on that.
///
/// [`knowledge_injection_query`] builds the query from the task's title,
/// description, and design, so echoing those is what makes the note reachable.
#[cfg(test)]
pub(crate) fn related_content(task: &Task, extra: &str) -> String {
    format!(
        "{} {} {} {extra}",
        task.title, task.description, task.design
    )
}

/// Ranking identity and fallback reasons recorded on the retrieval trace.
#[derive(Debug, Clone, Default)]
struct KnowledgeTraceStrategy {
    ranking_profile: &'static str,
    scope_fallback_reason: Option<&'static str>,
    candidate_window: usize,
    rrf_k: Option<f64>,
    search_error: Option<String>,
}

impl KnowledgeTraceStrategy {
    /// Stable strategy version so a trace can be attributed to this ranker.
    const STRATEGY: &'static str = "ranked_injection_v1";
}

/// Build one candidate's trace `scope` payload.
///
/// Records the ranking identity, the per-signal ranks, and the fused
/// rank/score so a reviewer can reconstruct *why* a note reached (or missed)
/// the prompt without re-running retrieval.
fn injection_candidate_scope(
    candidate: &djinn_db::repositories::note::KnowledgeInjectionCandidate,
    profile: &str,
) -> serde_json::Value {
    serde_json::json!({
        "folder": candidate.note.folder,
        "note_type": candidate.note.note_type,
        "scope_paths": candidate.note.scope_paths,
        "ranking_profile": profile,
        "fused_rank": candidate.fused_rank,
        "fused_score": candidate.fused_score,
        "signal_ranks": candidate.signal_ranks,
    })
}

/// Map the single fused candidate list plus its packing outcomes onto trace
/// candidates, preserving per-signal ranks, fused rank/score, and exactly one
/// terminal disposition per candidate.
fn injection_trace_candidates(
    candidates: &[djinn_db::repositories::note::KnowledgeInjectionCandidate],
    packed: &crate::actors::slot::helpers::PackedKnowledgeNotes,
    profile: &str,
) -> Vec<djinn_db::repositories::retrieval_trace::TraceCandidate> {
    use djinn_db::repositories::retrieval_trace::{
        CandidateOutcome, SkippedReason, TraceCandidate,
    };
    candidates
        .iter()
        .zip(&packed.outcomes)
        .map(|(candidate, outcome)| {
            let (outcome_kind, skipped_reason) = match outcome.disposition {
                NotePackDisposition::Injected => (CandidateOutcome::Injected, None),
                NotePackDisposition::ConfidenceFiltered => (
                    CandidateOutcome::Skipped,
                    Some(SkippedReason::MinConfidence),
                ),
                NotePackDisposition::NotTopK => {
                    (CandidateOutcome::Skipped, Some(SkippedReason::NotTopK))
                }
                NotePackDisposition::OversizedSkipped => (
                    CandidateOutcome::Skipped,
                    Some(SkippedReason::OversizedSkipped),
                ),
                NotePackDisposition::BudgetPruned => {
                    (CandidateOutcome::Skipped, Some(SkippedReason::BudgetPruned))
                }
            };
            TraceCandidate {
                note_id: candidate.note.id.clone(),
                permalink: Some(candidate.note.permalink.clone()),
                title: Some(candidate.note.title.clone()),
                outcome: outcome_kind,
                rank: Some(candidate.fused_rank as i32),
                confidence: Some(candidate.note.confidence),
                skipped_reason,
                source: Some(KnowledgeTraceStrategy::STRATEGY.to_owned()),
                scope: Some(injection_candidate_scope(candidate, profile)),
            }
        })
        .collect()
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
#[cfg(test)]
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
#[cfg(test)]
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

/// Apply pack-time drop outcomes from the packed notes to the classified candidates.
///
/// Candidates initially classified as `Injected` are reclassified to
/// `BudgetPruned` or `OversizedSkipped` if the packing outcome for the
/// corresponding note is `BudgetPruned` or `OversizedSkipped` respectively.
/// Deduplication: if multiple injected candidates resolve to the same permalink
/// (shouldn't happen in practice but handled defensively), the first wins and
/// subsequent ones become `Dedupe`.
#[cfg(test)]
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

        // Check pack-time drops. `OversizedSkipped` and `BudgetPruned` are
        // both non-injections and must be reported distinctly: the former is
        // a whole-note drop that no budget could have saved.
        match disposition_by_permalink.get(permalink) {
            Some(NotePackDisposition::BudgetPruned) => {
                candidate.outcome = CandidateOutcome::Skipped;
                candidate.skipped_reason = Some(SkippedReason::BudgetPruned);
            }
            Some(NotePackDisposition::OversizedSkipped) => {
                candidate.outcome = CandidateOutcome::Skipped;
                candidate.skipped_reason = Some(SkippedReason::OversizedSkipped);
            }
            _ => {}
        }
    }

    candidates
}

/// Convert exact pack outcomes into detailed candidate records.
#[cfg(test)]
fn trace_candidates_from_pack(
    notes: &[djinn_memory::Note],
    packed: &crate::actors::slot::helpers::PackedKnowledgeNotes,
) -> Vec<djinn_db::repositories::retrieval_trace::TraceCandidate> {
    use djinn_db::repositories::retrieval_trace::{
        CandidateOutcome, SkippedReason, TraceCandidate,
    };
    notes.iter().zip(&packed.outcomes).enumerate().map(|(index, (note, outcome))| {
        let (outcome, skipped_reason) = match outcome.disposition {
            NotePackDisposition::Injected => (CandidateOutcome::Injected, None),
            NotePackDisposition::ConfidenceFiltered => (CandidateOutcome::Skipped, Some(SkippedReason::MinConfidence)),
            NotePackDisposition::NotTopK => (CandidateOutcome::Skipped, Some(SkippedReason::NotTopK)),
            // `OversizedSkipped` is a whole-note DROP (the fixed per-line
            // overhead alone exceeds the line cap, so nothing renders) while
            // `BudgetPruned` merely lost a competition for remaining space.
            // They are reported distinctly so a silently deleted note stays
            // visible to the operator (proposal u46i AC4).
            NotePackDisposition::OversizedSkipped => (CandidateOutcome::Skipped, Some(SkippedReason::OversizedSkipped)),
            NotePackDisposition::BudgetPruned => (CandidateOutcome::Skipped, Some(SkippedReason::BudgetPruned)),
        };
        TraceCandidate {
            note_id: note.id.clone(), permalink: Some(note.permalink.clone()), title: Some(note.title.clone()),
            outcome, rank: Some((index + 1) as i32), confidence: Some(note.confidence), skipped_reason,
            source: Some("scope_overlap".to_owned()),
            scope: Some(serde_json::json!({"folder": note.folder, "note_type": note.note_type, "scope_paths": note.scope_paths})),
        }
    }).collect()
}

fn pack_disposition_counts(
    packed: &crate::actors::slot::helpers::PackedKnowledgeNotes,
) -> djinn_db::repositories::retrieval_trace::KnowledgeTraceDispositionCounts {
    use djinn_db::repositories::retrieval_trace::KnowledgeTraceDispositionCounts;
    let mut counts = KnowledgeTraceDispositionCounts {
        confidence_filtered: 0,
        not_top_k: 0,
        oversized_skipped: 0,
        injected: 0,
        budget_pruned: 0,
    };
    for outcome in &packed.outcomes {
        match outcome.disposition {
            NotePackDisposition::ConfidenceFiltered => counts.confidence_filtered += 1,
            NotePackDisposition::NotTopK => counts.not_top_k += 1,
            NotePackDisposition::OversizedSkipped => counts.oversized_skipped += 1,
            NotePackDisposition::Injected => counts.injected += 1,
            NotePackDisposition::BudgetPruned => counts.budget_pruned += 1,
        }
    }
    counts
}

/// Per-phase durations (milliseconds) for the knowledge-context retrieval trace.
#[derive(Default)]
struct KnowledgeTraceDurations {
    candidate_fetch_ms: i64,
    classify_ms: i64,
    prompt_pack_ms: i64,
    persist_ms: i64,
}

/// Persist a `LoadKnowledgeContext` retrieval trace row. Fail-open: logs and
/// swallows all errors, never propagating them to the caller.
#[allow(clippy::too_many_arguments)] // Trace fields stay explicit at this boundary.
async fn persist_knowledge_trace(
    task: &Task,
    task_paths: &[String],
    candidates: &[djinn_db::repositories::retrieval_trace::TraceCandidate],
    estimated_injected_tokens: i32,
    durations: KnowledgeTraceDurations,
    candidate_cap_exceeded: bool,
    db: &djinn_db::Database,
    attribution: Option<(&str, &str)>,
    rollout: &RolloutMode,
    outcome: djinn_db::repositories::retrieval_trace::RetrievalTraceOutcome,
    cancelled: bool,
    terminal_dispositions: Option<
        djinn_db::repositories::retrieval_trace::KnowledgeTraceDispositionCounts,
    >,
    strategy: &KnowledgeTraceStrategy,
) {
    use djinn_db::repositories::retrieval_trace::{
        CreateRetrievalTraceParams, CreateRetrievalTraceTerminalParams,
        KnowledgeTraceTerminalState, RetrievalTraceEntryPoint, RetrievalTraceRepository,
        validate_candidates,
    };

    // Validate candidate invariants before serialization.
    if let Err(e) = validate_candidates(candidates) {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "Lifecycle: retrieval trace candidate validation failed; skipping trace persistence"
        );
        return;
    }

    let candidates_json = match serde_json::to_value(candidates) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "Lifecycle: failed to serialize retrieval trace candidates; skipping trace persistence"
            );
            return;
        }
    };

    let trigger = serde_json::json!({
        // Proposal 5205: the trigger carries the retrieval strategy version,
        // the ranking profile, the validated scope (or the typed reason it is
        // empty), the fixed candidate window, and the `rrf_k` actually used.
        // `search_error` is present only when ranked retrieval itself failed.
        "shape": "ranked_injection_v1",
        "strategy": KnowledgeTraceStrategy::STRATEGY,
        "ranking_profile": strategy.ranking_profile,
        "task_paths": task_paths,
        "scope_fallback_reason": strategy.scope_fallback_reason,
        "candidate_window": strategy.candidate_window,
        "rrf_k": strategy.rrf_k,
        "search_error": strategy.search_error,
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
        session_id: attribution.map(|(session_id, _)| session_id),
        task_run_id: attribution.map(|(_, task_run_id)| task_run_id),
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
    let terminal_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 timestamp");
    let write = match terminal_dispositions {
        Some(dispositions) => {
            repo.insert_terminal(CreateRetrievalTraceTerminalParams {
                trace: params,
                rollout_label: rollout.label(),
                outcome,
                terminal_state: KnowledgeTraceTerminalState::Success,
                terminal_at: &terminal_at,
                candidate_count: Some(candidates.len() as i32),
                injected_count: Some(dispositions.injected),
                dispositions: Some(dispositions),
            })
            .await
        }
        None => {
            repo.insert_terminal(CreateRetrievalTraceTerminalParams {
                trace: params,
                rollout_label: rollout.label(),
                outcome,
                terminal_state: if cancelled {
                    KnowledgeTraceTerminalState::Cancelled
                } else {
                    KnowledgeTraceTerminalState::Error
                },
                terminal_at: &terminal_at,
                candidate_count: None,
                injected_count: None,
                dispositions: None,
            })
            .await
        }
    };
    if let Err(e) = write {
        tracing::warn!(task_id = %task.short_id, error = %e, "Lifecycle: failed to persist retrieval trace for knowledge context; continuing (fail-open)");
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
        read_sources,
        worker_resume_note,
        arbiter_directive,
        mcp_server_instructions,
        extension_diagnostics,
        cancellation,
        memory_intent_planner,
    } = inputs;
    let uncancelled = CancellationToken::new();
    let cancellation = cancellation.unwrap_or(&uncancelled);

    // ── Phase 0: synchronous work with no data dependencies ──
    let conflict_files = format_conflict_files(conflict_ctx);
    let ci_blocking_directive = build_ci_blocking_directive(task);
    let needs_epic_context = role_for_epic_check.needs_epic_context();
    let role_name = runtime_role.config().name;
    let knowledge_rollout = knowledge_context_rollout_from_env();

    // ── Phase 1: activity + epic context + base tree concurrently ──
    // Each child measures its own wall-clock time so the child-span
    // metric reports per-child duration, not the phase aggregate.
    //
    // The base-revision tree shells out to `git ls-tree` and depends only on
    // the project path and the configured target branch, so it belongs here
    // rather than serially in front of phase 2 where it would add its whole
    // latency to every prompt assembly.
    let (
        ((activity_text, worker_summary, worker_concerns), _activity_elapsed),
        (epic_context, _epic_elapsed),
        base_tree,
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
        },
        {
            let span = tracing::info_span!(
                "prompt_ctx::base_tree",
                task_id = %task.short_id,
            );
            async move {
                // The base tree comes from the project's own repository, which
                // `project_path` does not name (see `resolve_base_tree_root`).
                let root = resolve_base_tree_root(&task.project_id, project_path)?;
                load_base_tree(&root, &project_target_branch(task, app_state).await).await
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
    // The code-graph block filters symbol file paths by directory prefix and is
    // outside proposal 5205's scope; it keeps the unvalidated token extractor.
    let task_paths_for_code_graph = derive_task_scope_path_tokens(task, epic_context_ref);
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
                let result = load_knowledge_context_with_planner(
                    task,
                    epic_context_ref,
                    app_state,
                    memory_intent_planner.as_ref(),
                    &knowledge_rollout,
                    cancellation,
                    base_tree.as_ref(),
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
        extension_diagnostics,
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
        K::TaskBranchCheckpoint => "task-branch checkpoint",
        K::AlternateCheckpointRef => "alternate checkpoint ref",
        K::CleanTaskBranch => "clean task branch",
    }
}

#[cfg(test)]
#[path = "test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "prompt_context_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "rendered_surface_guard_tests.rs"]
mod rendered_surface_guard_tests;

#[cfg(test)]
#[path = "attempt_history_prompt_tests.rs"]
mod attempt_history_tests;

#[cfg(test)]
#[path = "ci_directive_tests.rs"]
mod ci_directive_tests;

#[cfg(test)]
#[path = "knowledge_trace_tests.rs"]
mod knowledge_trace_tests;

#[cfg(test)]
#[path = "injection_wiring_tests.rs"]
mod injection_wiring_tests;

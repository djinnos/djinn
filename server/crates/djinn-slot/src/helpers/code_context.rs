use super::*;

/// Approximate tokens-per-character ratio used for simple token estimation
/// when no shared estimator is available. 4 chars ≈ 1 token is the standard
/// heuristic for English-like prompt text.
const APPROX_CHARS_PER_TOKEN: f64 = 4.0;

/// Outcome classification for a single note candidate during prompt packing.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotePackDisposition {
    /// The note was rendered into the prompt text within the character budget.
    Injected,
    /// The note was dropped because adding its rendered line would exceed the
    /// remaining character budget. Once a note is budget-pruned, all subsequent
    /// notes (which have equal or lower confidence) are also pruned.
    BudgetPruned,
}

/// Per-note packing outcome for trace instrumentation.
///
/// Captures the decision and metadata for each input candidate so downstream
/// callers (e.g. retrieval trace persistence) can classify prompt-budget drops
/// without re-parsing the rendered text.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NotePackOutcome {
    /// The note's stable permalink identifier (e.g. `"pitfalls/refinement-target-less"`).
    pub permalink: String,
    /// The note's human-readable title.
    pub title: String,
    /// How this note was dispositioned during packing.
    pub disposition: NotePackDisposition,
    /// Estimated rendered character count for this note's line (including the
    /// permalink suffix). `None` for budget-pruned notes where the line was
    /// never rendered.
    pub estimated_rendered_chars: Option<usize>,
    /// Simple token estimate for this note's rendered line, computed as
    /// `ceil(estimated_rendered_chars / 4.0)`. `None` for budget-pruned notes.
    /// This is a rough heuristic; swap in a shared estimator if one becomes
    /// available.
    pub estimated_rendered_tokens: Option<usize>,
}

/// Result of packing knowledge notes into a budget-capped prompt string.
///
/// The `rendered` text is byte-identical to what [`format_knowledge_notes`]
/// would produce for the same inputs. The `outcomes` vector contains one
/// entry per input note, in input order, classifying each as injected or
/// budget-pruned with associated metadata for trace persistence.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PackedKnowledgeNotes {
    /// The rendered prompt text, byte-identical to `format_knowledge_notes` output.
    pub rendered: String,
    /// Per-note packing outcomes, one per input note in input order.
    pub outcomes: Vec<NotePackOutcome>,
    /// Total estimated characters consumed by all injected notes (including
    /// inter-note newlines).
    pub total_injected_chars: usize,
    /// Simple token estimate for all injected content, computed as
    /// `ceil(total_injected_chars / 4.0)`.
    pub total_injected_tokens: usize,
}

/// Compute a simple token estimate from a character count.
///
/// Uses the standard heuristic of ~4 characters per token, rounding up so
/// callers get a conservative budget estimate.
fn token_estimate(chars: usize) -> usize {
    ((chars as f64) / APPROX_CHARS_PER_TOKEN).ceil() as usize
}

/// Pack knowledge notes into a budget-capped prompt string and return
/// per-note packing outcomes for trace instrumentation.
///
/// This is the core implementation that [`format_knowledge_notes`] delegates
/// to.  It uses the exact same line-rendering and budget accounting as the
/// original formatter, but additionally classifies every input note as
/// injected or budget-pruned and returns metadata suitable for retrieval
/// trace persistence.
///
/// Notes are processed in input order (which the caller controls; typically
/// highest-confidence first).  The first note that would overflow the budget
/// triggers pruning of that note and **all** remaining notes.
///
/// Once the budget is exhausted, subsequent notes are classified as
/// `BudgetPruned` **without** computing their label, summary, or rendered
/// line content.  This preserves the original formatter's break semantics:
/// the old `format_knowledge_notes` loop broke on the first overflow and
/// never inspected later notes, so any content slicing (e.g. fallback
/// summary at a non-UTF-8 boundary) was unreachable.  Skipping content
/// computation for exhausted-budget notes maintains that invariant and
/// avoids panics on notes whose `content[..min(100)]` would land on a
/// non-byte-boundary.
pub fn pack_knowledge_notes(
    notes: &[djinn_memory::Note],
    budget_chars: usize,
) -> PackedKnowledgeNotes {
    let mut lines = Vec::new();
    let mut outcomes = Vec::with_capacity(notes.len());
    let mut used: usize = 0;
    let mut budget_exhausted = false;

    for note in notes {
        // Once the budget is exhausted, push a budget-pruned outcome
        // immediately without computing label/summary/line content.
        // This preserves the original formatter's break semantics: the old
        // loop broke on the first overflow and never inspected later notes,
        // so any content slicing (e.g. fallback summary at a non-UTF-8
        // boundary) was unreachable.
        if budget_exhausted {
            outcomes.push(NotePackOutcome {
                permalink: note.permalink.clone(),
                title: note.title.clone(),
                disposition: NotePackDisposition::BudgetPruned,
                estimated_rendered_chars: None,
                estimated_rendered_tokens: None,
            });
            continue;
        }

        let label = match note.note_type.as_str() {
            "pitfall" => "Pitfall",
            "pattern" => "Pattern",
            "case" => "Case",
            _ => "Note",
        };
        let summary = if note.confidence > 0.8 {
            // High confidence: use overview (L1) if available
            note.overview
                .as_deref()
                .or(note.abstract_.as_deref())
                .unwrap_or_else(|| &note.content[..note.content.len().min(200)])
        } else {
            // Lower confidence: use abstract (L0) if available
            note.abstract_
                .as_deref()
                .unwrap_or_else(|| &note.content[..note.content.len().min(100)])
        };
        // Append the permalink on the same rendered line so callers can
        // resolve the note via `memory_read(identifier=<permalink>)`.
        // The permalink suffix length is counted against `budget_chars` below,
        // so the truncation logic is identical to a non-permalink line of
        // the same total length.
        let line = format!(
            "- **[{}] {}**: {} (permalink: {})",
            label, note.title, summary, note.permalink
        );

        if used + line.len() > budget_chars {
            budget_exhausted = true;
            outcomes.push(NotePackOutcome {
                permalink: note.permalink.clone(),
                title: note.title.clone(),
                disposition: NotePackDisposition::BudgetPruned,
                estimated_rendered_chars: None,
                estimated_rendered_tokens: None,
            });
        } else {
            let line_chars = line.len();
            used += line_chars + 1; // +1 for newline
            lines.push(line);
            outcomes.push(NotePackOutcome {
                permalink: note.permalink.clone(),
                title: note.title.clone(),
                disposition: NotePackDisposition::Injected,
                estimated_rendered_chars: Some(line_chars),
                estimated_rendered_tokens: Some(token_estimate(line_chars)),
            });
        }
    }

    PackedKnowledgeNotes {
        rendered: lines.join("\n"),
        outcomes,
        total_injected_chars: used,
        total_injected_tokens: token_estimate(used),
    }
}

/// Format knowledge notes for injection into the system prompt.
///
/// Each rendered line preserves the note's type, title, and summary content
/// and appends the note's `permalink` (e.g. `pitfalls/refinement-target-less`)
/// so callers can resolve the note via
/// `memory_read(identifier=<permalink>)` with an exact identifier instead of
/// relying on title matching.
///
/// Uses L0 (abstract) for most notes, L1 (overview) for high-confidence ones.
/// Budget-capped at `budget_chars`, dropping lowest-confidence notes first;
/// the appended `permalink` is included in that budget accounting rather than
/// being layered on after truncation.
///
/// Now implemented as a compatibility wrapper around [`pack_knowledge_notes`].
pub fn format_knowledge_notes(notes: &[djinn_memory::Note], budget_chars: usize) -> String {
    pack_knowledge_notes(notes, budget_chars).rendered
}

/// Returns true when the given role name is opted-in to auto-injected
/// `code_graph context` blocks via `DJINN_AUTO_CODE_CONTEXT_ROLES`.
///
/// Empty / unset env var → false for every role (the safe default).
/// Whitespace + case are tolerated so `Worker, REVIEWER` works as expected.
pub fn is_role_auto_code_context_enabled(role_name: &str) -> bool {
    let Ok(raw) = std::env::var(AUTO_CODE_CONTEXT_ROLES_ENV) else {
        return false;
    };
    let target = role_name.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .any(|s| s == target)
}

/// Map a PageRank value to a human-readable tier string. Thresholds align
/// with the rough buckets used elsewhere in the code-graph UI: top 5%
/// hotspots are `high`, next 20% `medium`, the long tail `low`.
fn pagerank_tier(pagerank: f64) -> &'static str {
    if pagerank >= 0.5 {
        "high"
    } else if pagerank >= 0.1 {
        "medium"
    } else {
        "low"
    }
}

/// Returns true when `path` is equal to or nested under any directory in
/// `scope_paths`. The `scope_paths` here come from
/// [`derive_task_scope_paths`] and are already directory-prefix shapes
/// (no trailing slashes).
fn path_under_any_scope(path: &str, scope_paths: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    scope_paths
        .iter()
        .any(|scope| path == scope || path.starts_with(&format!("{scope}/")))
}

/// Format the inline `calls: a, b, c` style sub-bullet from a
/// `RelatedSymbol` bucket. Returns `"none"` when empty so the prompt is
/// readable instead of trailing whitespace.
fn format_related_names(syms: &[djinn_control_plane::bridge::RelatedSymbol], max: usize) -> String {
    if syms.is_empty() {
        return "none".to_string();
    }
    syms.iter()
        .take(max)
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Extract crate/module path prefixes from a task's description, design, and epic context.
pub fn derive_task_scope_paths(
    task: &djinn_core::models::Task,
    epic_context: Option<&str>,
) -> Vec<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    // Match paths like: crates/foo, src/bar/baz, server/crates/djinn-db
    // Looking for patterns with at least 2 slash-separated segments
    static TASK_SCOPE_PATH_RE: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    let re = match TASK_SCOPE_PATH_RE.get_or_init(|| {
        Regex::new(r#"(?:^|[\s`"(])([a-zA-Z0-9_-]+(?:/[a-zA-Z0-9_.-]+){1,6})(?:[\s`")\.,:]|$)"#)
    }) {
        Ok(re) => re,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to compile task-scope path regex; skipping scope derivation"
            );
            return Vec::new();
        }
    };
    let mut paths = std::collections::HashSet::new();
    for text in [&task.description, &task.design] {
        for cap in re.captures_iter(text) {
            if let Some(m) = cap.get(1) {
                let path = m.as_str();
                // Filter to paths that look like code paths (not URLs, not short fragments)
                if path.contains('/') && !path.starts_with("http") && !path.starts_with("//") {
                    // Derive scope: split on /src/ or take up to 3 components
                    if let Some(idx) = path.find("/src/") {
                        paths.insert(path[..idx].to_string());
                    } else {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }
    if let Some(epic) = epic_context {
        for cap in re.captures_iter(epic) {
            if let Some(m) = cap.get(1) {
                let path = m.as_str();
                if path.contains('/') && !path.starts_with("http") && !path.starts_with("//") {
                    if let Some(idx) = path.find("/src/") {
                        paths.insert(path[..idx].to_string());
                    } else {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }
    paths.into_iter().collect()
}

/// Build the auto-injected `code_graph context` block for `role_name`.
///
/// Returns `None` when the role is not enabled, no scope paths could be
/// inferred, the canonical graph is unavailable, or no high-PageRank
/// symbol's `file_path` falls under one of the task's scope paths.
///
/// Emits one bullet per selected symbol. Per file we take up to
/// [`AUTO_CODE_CONTEXT_PER_FILE`] symbols, in PageRank order. The whole
/// block is truncated via `truncate::smart_truncate` to
/// [`AUTO_CODE_CONTEXT_BUDGET_CHARS`].
pub async fn build_role_code_graph_context(
    role_name: &str,
    task: &Task,
    app_state: &SlotContext,
    project_path: &str,
    task_paths: &[String],
) -> Option<String> {
    if !is_role_auto_code_context_enabled(role_name) {
        return None;
    }
    if task_paths.is_empty() {
        return None;
    }
    let graph_ops = app_state.repo_graph_ops.clone()?;
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: task.project_id.clone(),
        clone_path: project_path.to_string(),
        workspace: None,
        sub_path: None,
    };
    let ranked = match graph_ops
        .ranked(
            &ctx,
            ctx.workspace.as_deref(),
            Some("symbol"),
            Some("pagerank"),
            AUTO_CODE_CONTEXT_RANKED_POOL,
        )
        .await
    {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::debug!(
                role = role_name,
                task_id = %task.short_id,
                error = %e,
                "build_role_code_graph_context: ranked() failed; skipping auto-injection"
            );
            return None;
        }
    };
    if ranked.is_empty() {
        return None;
    }
    let mut per_file_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut bullets: Vec<String> = Vec::new();
    for node in ranked {
        if bullets.len() >= AUTO_CODE_CONTEXT_MAX_BULLETS {
            break;
        }
        let symbol_ctx = match graph_ops.context(&ctx, &node.key, false).await {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(
                    role = role_name,
                    key = %node.key,
                    error = %e,
                    "build_role_code_graph_context: context() failed; skipping symbol"
                );
                continue;
            }
        };
        let file_path = symbol_ctx.symbol.file_path.clone();
        if !path_under_any_scope(&file_path, task_paths) {
            continue;
        }
        let count = per_file_count.entry(file_path.clone()).or_insert(0);
        if *count >= AUTO_CODE_CONTEXT_PER_FILE {
            continue;
        }
        *count += 1;
        let callers: usize = symbol_ctx.incoming.values().map(|v| v.len()).sum();
        let callees: usize = symbol_ctx.outgoing.values().map(|v| v.len()).sum();
        let tier = pagerank_tier(node.page_rank);
        // Outgoing calls (function-like edges) and writes give the worker
        // a quick read on what this symbol *does*; reads give a quick read
        // on its inputs.
        use djinn_control_plane::bridge::EdgeCategory;
        let calls = symbol_ctx
            .outgoing
            .get(&EdgeCategory::Calls)
            .map(|v| format_related_names(v, 5))
            .unwrap_or_else(|| "none".to_string());
        let reads = symbol_ctx
            .outgoing
            .get(&EdgeCategory::Reads)
            .map(|v| format_related_names(v, 5))
            .unwrap_or_else(|| "none".to_string());
        bullets.push(format!(
            "- `{file_path}::{name}` (callers: {callers}, callees: {callees}, pagerank-tier: {tier})\n  - calls: {calls}\n  - reads: {reads}",
            name = symbol_ctx.symbol.name,
        ));
    }
    if bullets.is_empty() {
        return None;
    }
    let body = bullets.join("\n");
    Some(crate::truncate::smart_truncate(
        &body,
        AUTO_CODE_CONTEXT_BUDGET_CHARS,
    ))
}

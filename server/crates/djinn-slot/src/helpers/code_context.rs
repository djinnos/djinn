use super::*;

/// Approximate tokens-per-byte ratio used for telemetry only. It never gates
/// packing, which is governed solely by exact UTF-8 byte limits.
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Outcome classification for a single note candidate during prompt packing.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotePackDisposition {
    ConfidenceFiltered,
    NotTopK,
    OversizedSkipped,
    Injected,
    BudgetPruned,
}

/// Inputs to deterministic packing of the complete ranked candidate universe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnowledgePackConfig {
    pub minimum_confidence: f64,
    pub top_k: usize,
    pub total_byte_budget: usize,
    pub line_byte_cap: usize,
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
    /// Exact rendered UTF-8 byte count for this line. The legacy field name is
    /// retained for downstream compatibility.
    pub estimated_rendered_chars: Option<usize>,
    /// Simple token estimate for this note's rendered line, computed as
    /// `ceil(estimated_rendered_chars / 4)`, used only as telemetry.
    pub estimated_rendered_tokens: Option<usize>,
}

/// Result of packing knowledge notes into a budget-capped prompt string.
///
/// The `rendered` text is byte-identical to what [`format_knowledge_notes`]
/// would produce for the same inputs. The `outcomes` vector contains one
/// entry per input note, in input order, with one terminal disposition.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PackedKnowledgeNotes {
    /// The rendered prompt text, byte-identical to `format_knowledge_notes` output.
    pub rendered: String,
    /// Per-note packing outcomes, one per input note in input order.
    pub outcomes: Vec<NotePackOutcome>,
    /// Total exact UTF-8 bytes consumed by injected notes, including only
    /// newlines actually placed between lines. Legacy field name retained.
    pub total_injected_chars: usize,
    /// Simple token estimate for all injected content, computed as
    /// `ceil(total_injected_chars / 4)`.
    pub total_injected_tokens: usize,
}

fn token_estimate(bytes: usize) -> usize {
    bytes.div_ceil(APPROX_BYTES_PER_TOKEN)
}

fn note_label(note: &djinn_memory::Note) -> &'static str {
    match note.note_type.as_str() {
        "pitfall" => "Pitfall",
        "pattern" => "Pattern",
        "case" => "Case",
        _ => "Note",
    }
}

/// Returns an L0 summary, never consulting the L1 `overview` field.
fn l0_summary(note: &djinn_memory::Note) -> &str {
    note.abstract_
        .as_deref()
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .or_else(|| {
            let content = note.content.trim();
            (!content.is_empty()).then_some(content)
        })
        .unwrap_or("(no abstract)")
}

/// Truncate only summary text at a valid UTF-8 boundary. An ellipsis is added
/// only when all three of its bytes fit.
fn truncate_summary(summary: &str, max_bytes: usize) -> String {
    if summary.len() <= max_bytes {
        return summary.to_owned();
    }
    let (content_limit, ellipsis) = if max_bytes >= '…'.len_utf8() {
        (max_bytes - '…'.len_utf8(), "…")
    } else {
        (max_bytes, "")
    };
    let boundary = summary
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|&index| index <= content_limit)
        .last()
        .unwrap_or(0);
    format!("{}{}", &summary[..boundary], ellipsis)
}

fn rendered_line(note: &djinn_memory::Note, line_byte_cap: usize) -> Option<String> {
    let prefix = format!("- **[{}] {}**: ", note_label(note), note.title);
    let suffix = format!(" (permalink: {})", note.permalink);
    let minimum_summary = "(no abstract)";
    let metadata_bytes = prefix.len() + suffix.len();
    if metadata_bytes + minimum_summary.len() > line_byte_cap {
        return None;
    }
    Some(format!(
        "{prefix}{}{suffix}",
        truncate_summary(l0_summary(note), line_byte_cap - metadata_bytes)
    ))
}

/// Pack the complete ranked universe under the deterministic five-way byte
/// contract. Candidates are inspected in rank order even after oversized and
/// total-budget misses.
pub fn pack_ranked_knowledge_notes(
    notes: &[djinn_memory::Note],
    config: KnowledgePackConfig,
) -> PackedKnowledgeNotes {
    let mut lines = Vec::new();
    let mut outcomes = Vec::with_capacity(notes.len());
    let mut used_bytes = 0usize;
    let mut confidence_eligible = 0usize;

    for note in notes {
        let disposition = if note.confidence < config.minimum_confidence {
            NotePackDisposition::ConfidenceFiltered
        } else {
            let eligible_rank = confidence_eligible;
            confidence_eligible += 1;
            if eligible_rank >= config.top_k {
                NotePackDisposition::NotTopK
            } else if let Some(line) = rendered_line(note, config.line_byte_cap) {
                let separator_bytes = usize::from(!lines.is_empty());
                if used_bytes + separator_bytes + line.len() <= config.total_byte_budget {
                    used_bytes += separator_bytes + line.len();
                    outcomes.push(NotePackOutcome {
                        permalink: note.permalink.clone(),
                        title: note.title.clone(),
                        disposition: NotePackDisposition::Injected,
                        estimated_rendered_chars: Some(line.len()),
                        estimated_rendered_tokens: Some(token_estimate(line.len())),
                    });
                    lines.push(line);
                    continue;
                }
                NotePackDisposition::BudgetPruned
            } else {
                NotePackDisposition::OversizedSkipped
            }
        };
        outcomes.push(NotePackOutcome {
            permalink: note.permalink.clone(),
            title: note.title.clone(),
            disposition,
            estimated_rendered_chars: None,
            estimated_rendered_tokens: None,
        });
    }

    PackedKnowledgeNotes {
        rendered: lines.join("\n"),
        outcomes,
        total_injected_chars: used_bytes,
        total_injected_tokens: token_estimate(used_bytes),
    }
}

/// Compatibility entry point for existing callers that already filter and
/// limit candidates. Full-universe callers should use
/// [`pack_ranked_knowledge_notes`].
pub fn pack_knowledge_notes(
    notes: &[djinn_memory::Note],
    budget_chars: usize,
) -> PackedKnowledgeNotes {
    pack_ranked_knowledge_notes(
        notes,
        KnowledgePackConfig {
            minimum_confidence: f64::NEG_INFINITY,
            top_k: notes.len(),
            total_byte_budget: budget_chars,
            line_byte_cap: budget_chars,
        },
    )
}

/// Format knowledge notes for injection into the system prompt.
///
/// Each rendered line preserves the note's type, title, and summary content
/// and appends the note's `permalink` (e.g. `pitfalls/refinement-target-less`)
/// so callers can resolve the note via
/// `memory_read(identifier=<permalink>)` with an exact identifier instead of
/// relying on title matching.
///
/// Uses only L0: trimmed nonblank `abstract_`, then trimmed nonblank content,
/// then `(no abstract)`. It never reads L1 `overview`. Budget-capped at exact UTF-8
/// bytes, with later candidates still considered after a miss;
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

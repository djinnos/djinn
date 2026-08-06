use super::*;

/// Approximate tokens-per-byte ratio used for telemetry only. It never gates
/// packing, which is governed solely by exact UTF-8 byte limits.
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Exact byte allocation available to one entry's action excerpt block.
///
/// The cap owns every byte *after* the summary line's terminating newline:
/// the `  action: ` / continuation prefixes, the excerpt content, the
/// newlines placed *between* action lines, and the optional truncation
/// marker. The summary line keeps its own separate
/// [`KnowledgePackConfig::line_byte_cap`] allowance.
///
/// Deliberately **not** configurable (proposal `u46i`).
pub const ACTION_EXCERPT_CAP: usize = 640;

/// Prefix for the first physical action line of an entry.
const ACTION_FIRST_PREFIX: &str = "  action: ";

/// Prefix for every continuation action line. Same byte width as
/// [`ACTION_FIRST_PREFIX`] so the excerpt renders as an aligned block.
const ACTION_CONT_PREFIX: &str = "          ";

/// Level-2 headings (ASCII case-folded) whose section body is eligible to be
/// rendered as the entry's action excerpt, in precedence order.
const ACTION_HEADINGS: [&str; 2] = ["prevention", "recommended approach"];

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

/// Trace *detail* describing how much of a note's actionable section reached
/// the prompt. This is deliberately **not** a disposition: every candidate
/// still records exactly one [`NotePackDisposition`]. It is `None` when the
/// note has no eligible actionable section at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionExcerptDetail {
    /// The whole actionable section fit inline.
    Full,
    /// Part of the section fit inline; the last action line is the marker.
    Truncated,
    /// Nothing of the section fit inline; only the pull marker (or, when even
    /// the marker cannot obey `line_byte_cap`, nothing) was rendered.
    Omitted,
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
    /// How much of the note's actionable section reached the prompt. Trace
    /// detail only — never a second disposition. `None` when the note has no
    /// eligible `Prevention` / `Recommended approach` section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_excerpt: Option<ActionExcerptDetail>,
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

/// Trim a candidate summary field and discard whitespace-only values.
fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// Returns an L0 summary, never consulting the L1 `overview` field.
///
/// Field precedence (proposal `u46i`): the first non-empty value of
/// `retrieval_anchor` (the note's applicability condition), then `abstract`,
/// then `content`. Whitespace-only values count as empty.
pub(super) fn l0_summary(note: &djinn_memory::Note) -> &str {
    non_blank(note.retrieval_anchor.as_deref())
        .or_else(|| non_blank(note.abstract_.as_deref()))
        .or_else(|| non_blank(Some(note.content.as_str())))
        .unwrap_or("(no abstract)")
}

// ── Structural Markdown scan for the actionable section ────────────────────

/// A fenced-code delimiter kind. Backtick and tilde fences never close each
/// other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FenceKind {
    Backtick,
    Tilde,
}

/// Leading-space count of a line, capped so the caller can cheaply reject
/// indented code blocks (4+ spaces) without scanning the whole line.
fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Recognize a fenced-code delimiter: up to three leading spaces followed by a
/// run of three or more backticks or tildes. Returns the kind and run length.
fn fence_delimiter(line: &str) -> Option<(FenceKind, usize)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    let kind = match marker {
        '`' => FenceKind::Backtick,
        '~' => FenceKind::Tilde,
        _ => return None,
    };
    let run = rest.chars().take_while(|&c| c == marker).count();
    (run >= 3).then_some((kind, run))
}

/// True when `line` closes an open fence of `kind` with `open_run` markers: a
/// run of at least `open_run` identical markers and nothing but whitespace
/// after it.
fn closes_fence(line: &str, kind: FenceKind, open_run: usize) -> bool {
    let Some((line_kind, run)) = fence_delimiter(line) else {
        return false;
    };
    if line_kind != kind || run < open_run {
        return false;
    }
    let indent = leading_spaces(line);
    let marker = if kind == FenceKind::Backtick {
        '`'
    } else {
        '~'
    };
    line[indent..].trim_start_matches(marker).trim().is_empty()
}

/// Recognize an ATX heading outside any fenced block, returning its level and
/// trimmed text. Lines indented four or more spaces are indented code, and
/// block-quote lines start with `>`, so neither can produce a heading here.
fn atx_heading(line: &str) -> Option<(usize, &str)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let mut text = after.trim();
    // Strip an optional ATX closing sequence (`## Prevention ##`).
    let without_closers = text.trim_end_matches('#');
    if without_closers.len() != text.len()
        && (without_closers.is_empty() || without_closers.ends_with([' ', '\t']))
    {
        text = without_closers.trim_end();
    }
    Some((level, text))
}

/// True when `heading_text` names an eligible action heading, compared with
/// ASCII case folding against an exact match.
fn action_heading_rank(heading_text: &str) -> Option<usize> {
    let folded = heading_text.trim().to_ascii_lowercase();
    ACTION_HEADINGS.iter().position(|name| *name == folded)
}

/// Split `content` into normalized source logical lines: CRLF is folded to LF
/// and trailing whitespace is trimmed from each line.
fn normalized_lines(content: &str) -> Vec<&str> {
    content
        .split('\n')
        .map(|line| line.trim_end_matches('\r').trim_end())
        .collect()
}

/// One indivisible rendering unit: either a single source line, or a complete
/// fenced-code block from its opening through its closing fence.
pub(super) type ActionUnit<'a> = Vec<&'a str>;

/// Extract the note body's actionable section as indivisible units.
///
/// Parses structurally: heading recognition is suppressed inside fenced code
/// (so a fenced or quoted `## Prevention` can never open a section) and the
/// scan never performs a raw substring search for a heading. Returns `None`
/// for a missing, empty, or malformed section — the caller then renders a
/// summary-only entry rather than dropping the candidate.
pub(super) fn extract_action_units(content: &str) -> Option<Vec<ActionUnit<'_>>> {
    let lines = normalized_lines(content);

    // Pass 1: classify every line while tracking fenced-code state, recording
    // the (rank, body-range) of each eligible level-2 section.
    let mut open_fence: Option<(FenceKind, usize)> = None;
    let mut sections: Vec<(usize, usize, usize)> = Vec::new(); // (rank, start, end)
    let mut pending: Option<(usize, usize)> = None; // (rank, body start)
    for (index, line) in lines.iter().enumerate() {
        if let Some((kind, run)) = open_fence {
            if closes_fence(line, kind, run) {
                open_fence = None;
            }
            continue;
        }
        if let Some((kind, run)) = fence_delimiter(line) {
            open_fence = Some((kind, run));
            continue;
        }
        let Some((level, text)) = atx_heading(line) else {
            continue;
        };
        if level > 2 {
            // Level-3 and deeper headings belong to the enclosing section.
            continue;
        }
        if let Some((rank, start)) = pending.take() {
            sections.push((rank, start, index));
        }
        // Only level-2 headings are eligible; a level-1 `# Prevention` closes
        // the previous section without opening a new one.
        if level == 2
            && let Some(rank) = action_heading_rank(text)
        {
            pending = Some((rank, index + 1));
        }
    }
    if let Some((rank, start)) = pending.take() {
        sections.push((rank, start, lines.len()));
    }

    // Pass 2: precedence is the first non-empty `Prevention` section, then the
    // first non-empty `Recommended approach` section.
    let mut chosen: Option<(usize, usize)> = None;
    for rank in 0..ACTION_HEADINGS.len() {
        chosen = sections
            .iter()
            .find(|(section_rank, start, end)| {
                *section_rank == rank
                    && lines[*start..*end]
                        .iter()
                        .any(|line| !line.trim().is_empty())
            })
            .map(|(_, start, end)| (*start, *end));
        if chosen.is_some() {
            break;
        }
    }
    let (start, end) = chosen?;

    // Discard leading and trailing blank lines from the selected body.
    let body = &lines[start..end];
    let first = body.iter().position(|line| !line.trim().is_empty())?;
    let last = body.iter().rposition(|line| !line.trim().is_empty())?;
    let body = &body[first..=last];

    // Pass 3: group into indivisible units. An unclosed fence inside the
    // section is malformed and yields no excerpt at all.
    let mut units: Vec<ActionUnit<'_>> = Vec::new();
    let mut index = 0usize;
    while index < body.len() {
        let line = body[index];
        if let Some((kind, run)) = fence_delimiter(line) {
            let mut unit = vec![line];
            let mut cursor = index + 1;
            let mut closed = false;
            while cursor < body.len() {
                unit.push(body[cursor]);
                if closes_fence(body[cursor], kind, run) {
                    closed = true;
                    break;
                }
                cursor += 1;
            }
            if !closed {
                return None;
            }
            units.push(unit);
            index = cursor + 1;
        } else {
            units.push(vec![line]);
            index += 1;
        }
    }
    (!units.is_empty()).then_some(units)
}

// ── Bounded action-excerpt rendering ───────────────────────────────────────

/// Rendered action block for one entry: the physical lines to place under the
/// summary, plus the trace detail describing how complete they are.
pub(super) struct ActionBlock {
    pub(super) lines: Vec<String>,
    pub(super) detail: ActionExcerptDetail,
}

/// Bytes an action block of `lines` consumes against [`ACTION_EXCERPT_CAP`]:
/// every rendered byte plus the newlines placed *between* action lines. The
/// newline that terminates the summary line is not part of this allocation.
fn action_block_bytes(lines: &[String]) -> usize {
    if lines.is_empty() {
        return 0;
    }
    lines.iter().map(String::len).sum::<usize>() + lines.len() - 1
}

/// Prefix one source line for its position in the action block. Blank source
/// lines render as empty physical lines rather than a run of trailing spaces.
fn render_action_line(source: &str, is_first: bool) -> String {
    if source.trim().is_empty() {
        String::new()
    } else if is_first {
        format!("{ACTION_FIRST_PREFIX}{source}")
    } else {
        format!("{ACTION_CONT_PREFIX}{source}")
    }
}

/// Render the bounded action block for `units`.
///
/// Adds only complete units while both the 640-byte allocation and the
/// per-physical-line cap hold, so no source line, Unicode scalar, inline-code
/// span, or fenced block is ever byte-sliced. When content remains, the last
/// physical line becomes the deterministic pull marker, dropping already
/// included units if that is what it takes to make the marker fit. Returns
/// `None` when not even the marker can be rendered — the entry then degrades
/// to a summary-only line.
pub(super) fn render_action_block(
    units: &[ActionUnit<'_>],
    permalink: &str,
    line_byte_cap: usize,
) -> Option<ActionBlock> {
    let mut lines: Vec<String> = Vec::new();
    let mut unit_line_counts: Vec<usize> = Vec::new();
    let mut truncated = false;

    for unit in units {
        let mut candidate: Vec<String> = Vec::new();
        let mut used = action_block_bytes(&lines);
        let mut fits = true;
        for source in unit {
            let rendered = render_action_line(source, lines.is_empty() && candidate.is_empty());
            if rendered.len() > line_byte_cap {
                fits = false;
                break;
            }
            // Every line after the first also costs the newline before it.
            used = if lines.is_empty() && candidate.is_empty() {
                rendered.len()
            } else {
                used + 1 + rendered.len()
            };
            if used > ACTION_EXCERPT_CAP {
                fits = false;
                break;
            }
            candidate.push(rendered);
        }
        if !fits {
            truncated = true;
            break;
        }
        unit_line_counts.push(candidate.len());
        lines.extend(candidate);
    }

    if !truncated {
        if lines.is_empty() {
            return None;
        }
        return Some(ActionBlock {
            lines,
            detail: ActionExcerptDetail::Full,
        });
    }

    // Content remains: the block must end with the pull marker.
    let marker = format!("{ACTION_FIRST_PREFIX}… truncated; memory_read({permalink})");
    if marker.len() > line_byte_cap {
        return None;
    }
    loop {
        // A blank line immediately before the marker is noise. Blank source
        // lines are always single-line units, so units stay in lockstep.
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
            unit_line_counts.pop();
        }
        let with_marker = if lines.is_empty() {
            marker.len()
        } else {
            action_block_bytes(&lines) + 1 + marker.len()
        };
        if with_marker <= ACTION_EXCERPT_CAP {
            let detail = if lines.is_empty() {
                ActionExcerptDetail::Omitted
            } else {
                ActionExcerptDetail::Truncated
            };
            lines.push(marker);
            return Some(ActionBlock { lines, detail });
        }
        // Drop the last included logical unit and try again.
        let count = unit_line_counts.pop()?;
        lines.truncate(lines.len().saturating_sub(count));
    }
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

/// One atomically packable entry: the summary line plus zero or more bounded
/// action lines, already joined with the newlines that separate them.
struct RenderedEntry {
    text: String,
    action_excerpt: Option<ActionExcerptDetail>,
}

/// Render the complete entry for `note`, or `None` when even the summary line
/// cannot obey `line_byte_cap` (the caller then reports `OversizedSkipped`).
fn rendered_entry(note: &djinn_memory::Note, line_byte_cap: usize) -> Option<RenderedEntry> {
    let summary = rendered_line(note, line_byte_cap)?;
    let Some(units) = extract_action_units(&note.content) else {
        // No eligible section: summary-only entry, no action trace detail.
        return Some(RenderedEntry {
            text: summary,
            action_excerpt: None,
        });
    };
    // A section that exists but cannot render even its pull marker still
    // degrades to a summary-only entry, reported as `omitted`.
    let Some(block) = render_action_block(&units, &note.permalink, line_byte_cap) else {
        return Some(RenderedEntry {
            text: summary,
            action_excerpt: Some(ActionExcerptDetail::Omitted),
        });
    };
    let mut text = String::with_capacity(summary.len() + ACTION_EXCERPT_CAP + 1);
    text.push_str(&summary);
    for line in &block.lines {
        text.push('\n');
        text.push_str(line);
    }
    Some(RenderedEntry {
        text,
        action_excerpt: Some(block.detail),
    })
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
    let mut pruned_detail: Option<ActionExcerptDetail> = None;

    for note in notes {
        let disposition = if note.confidence < config.minimum_confidence {
            NotePackDisposition::ConfidenceFiltered
        } else {
            let eligible_rank = confidence_eligible;
            confidence_eligible += 1;
            if eligible_rank >= config.top_k {
                NotePackDisposition::NotTopK
            } else if let Some(entry) = rendered_entry(note, config.line_byte_cap) {
                // Entry atomicity: the summary line and its action lines are
                // injected together or not at all. A candidate that does not
                // fit is wholly `BudgetPruned` and scanning continues, so a
                // later smaller candidate can still be injected.
                let separator_bytes = usize::from(!lines.is_empty());
                if used_bytes + separator_bytes + entry.text.len() <= config.total_byte_budget {
                    used_bytes += separator_bytes + entry.text.len();
                    outcomes.push(NotePackOutcome {
                        permalink: note.permalink.clone(),
                        title: note.title.clone(),
                        disposition: NotePackDisposition::Injected,
                        estimated_rendered_chars: Some(entry.text.len()),
                        estimated_rendered_tokens: Some(token_estimate(entry.text.len())),
                        action_excerpt: entry.action_excerpt,
                    });
                    lines.push(entry.text);
                    continue;
                }
                pruned_detail = entry.action_excerpt;
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
            action_excerpt: pruned_detail.take(),
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
/// Uses only L0 for the summary payload: trimmed nonblank `retrieval_anchor`
/// (the note's applicability condition), then trimmed nonblank `abstract_`,
/// then trimmed nonblank content, then `(no abstract)`. It never reads L1
/// `overview`. Budget-capped at exact UTF-8 bytes, with later candidates still
/// considered after a miss; the appended `permalink` is included in that
/// budget accounting rather than being layered on after truncation.
///
/// Each entry may additionally carry a bounded action excerpt drawn from the
/// note's `Prevention` (else `Recommended approach`) section, capped at
/// [`ACTION_EXCERPT_CAP`] bytes and ending in an explicit `memory_read`
/// pull marker when it had to be truncated.
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

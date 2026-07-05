//! Typed fuzzy-match strategy core for the edit tool.
//!
//! This module exposes a lower-level, typed matcher (`find_match`) that
//! locates the single best candidate for `old_text` in file `content` using a
//! strict→loose strategy chain, returning rich metadata (strategy, outcome,
//! byte/line range, reindentation flag, and placeholder fields for future
//! nearest-miss scoring, guard rejection, and Unicode-splice status). The
//! matcher never writes files or performs the replacement — that is the job of
//! the compatibility wrapper `fuzzy_replace`, which translates typed metadata
//! into the existing `(String, Option<String>)` surface consumed by
//! `call_edit`.
//!
//! Conceptual design only — no third-party source (e.g. Hermes) is vendored or
//! copied. Strategy ordering and guard concepts are original to this project;
//! see [[design/c77e-roadmap]].

use std::path::Path;

// ════════════════════════════════════════════════════════════════════════════
// Typed strategy identification
// ════════════════════════════════════════════════════════════════════════════

/// Matching strategies, ordered strict → loose. The matcher consults them in
/// `STRATEGY_ORDER` and returns the result of the first strategy that produces
/// any candidate (unique match or ambiguity). Future waves will extend this
/// enum with `escape_normalized`, `trimmed_boundary`, `unicode_normalized`,
/// `block_anchor`, and `context_aware` — see [[design/c77e-roadmap]].
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MatchStrategy {
    /// Exact byte-for-byte match.
    Exact,
    /// Match after trimming trailing whitespace per line.
    LineTrimmed,
    /// Match after collapsing runs of spaces/tabs to a single space.
    WhitespaceNormalized,
    /// Match after normalizing common string-literal escaping differences
    /// (notably quote and backslash escaping) while rejecting unsafe candidates.
    EscapeNormalized,
    /// Match after allowing extra leading/trailing blank or whitespace-only
    /// boundary lines; replacement is applied only to the intended candidate.
    TrimmedBoundary,
    /// Match after Unicode NFKC/confusables normalization; replacement is
    /// byte-preserving for unchanged original graphemes.
    UnicodeNormalized,
    /// Match using a unique surrounding block anchor; rejects non-unique or
    /// overly broad block candidates.
    BlockAnchor,
    /// Match with context-aware threshold and tie-rejection for loose matches.
    ContextAware,
    /// Match after stripping leading whitespace per line; the replacement is
    /// reindented to the matched block's base indentation.
    IndentationFlexible,
}

impl MatchStrategy {
    /// Stable machine-readable identifier for telemetry/schema use.
    #[allow(dead_code)]
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::LineTrimmed => "line_trimmed",
            Self::WhitespaceNormalized => "whitespace_normalized",
            Self::EscapeNormalized => "escape_normalized",
            Self::TrimmedBoundary => "trimmed_boundary",
            Self::UnicodeNormalized => "unicode_normalized",
            Self::BlockAnchor => "block_anchor",
            Self::ContextAware => "context_aware",
            Self::IndentationFlexible => "indentation_flexible",
        }
    }
}

/// The ordered strategy chain. First-match-wins: a strategy earlier in the
/// list is only skipped when it finds zero candidates. Exposed as a `const` so
/// the ordering is documented and verifiable independently of the dispatch
/// logic.
const STRATEGY_ORDER: &[MatchStrategy] = &[
    MatchStrategy::Exact,
    MatchStrategy::LineTrimmed,
    MatchStrategy::WhitespaceNormalized,
    MatchStrategy::IndentationFlexible,
    MatchStrategy::EscapeNormalized,
    MatchStrategy::TrimmedBoundary,
];

// ════════════════════════════════════════════════════════════════════════════
// Typed outcome & metadata
// ════════════════════════════════════════════════════════════════════════════

/// The outcome of a strategy's candidate search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MatchOutcome {
    /// Exactly one candidate found and all guards passed.
    Success,
    /// Multiple candidates found — caller must disambiguate.
    Ambiguous,
    /// No candidate found by this strategy (or by any strategy in the chain).
    NoMatch,
    /// Candidate found but a safety guard rejected it. Reserved for future
    /// guard work (UTF-8 boundary, CRLF preservation, escape balance, etc.);
    /// Wave 1 strategies never produce this outcome.
    #[allow(dead_code)]
    GuardRejected,
}

/// Placeholder for grapheme-safe Unicode splice status. Populated only by the
/// Unicode-normalized strategy in a later wave; Wave 1 strategies always leave
/// this as `None`, signalling no Unicode splice was performed. Conceptual
/// design only — no third-party source (e.g. Hermes) is vendored or copied.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnicodeSpliceStatus {
    /// Splice landed on a valid UTF-8 boundary; original non-ASCII bytes
    /// preserved byte-for-byte.
    Clean,
    /// Splice required grapheme-boundary adjustment.
    Adjusted,
}

/// Candidate byte range, half-open `[start, end)` into the original content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ByteRange {
    pub start: usize,
    pub end: usize,
}

/// Candidate line range, 1-based, inclusive `[start, end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LineRange {
    pub start: usize,
    pub end: usize,
}

/// Typed metadata produced by the matcher for a single consultation of the
/// strategy chain. The compatibility wrapper `fuzzy_replace` translates this
/// into the existing string notes/errors so `call_edit` is unchanged until
/// sibling handler/schema epics consume richer metadata.
//
// Several fields are intentional placeholders for future-wave strategies and
// guards (nearest-miss scoring, guard rejection reason, Unicode splice
// status). They are never read on the Wave 1 path; the `#[allow(dead_code)]`
// silences the `-D warnings` gate until sibling tasks consume them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(super) struct MatchMetadata {
    /// Which strategy produced this result. For `NoMatch` this is the last
    /// strategy consulted in the chain (conventional; no strategy decided).
    pub strategy: MatchStrategy,
    /// Outcome for the deciding strategy.
    pub outcome: MatchOutcome,
    /// How many candidates the deciding strategy found.
    pub candidate_count: usize,
    /// Byte range of the unique candidate in the original content.
    /// `None` unless `outcome == Success`.
    pub byte_range: Option<ByteRange>,
    /// 1-based inclusive line range of the unique candidate.
    /// `None` unless `outcome == Success`.
    pub line_range: Option<LineRange>,
    /// Nearest-miss score placeholder for future fuzzy scoring (0.0..=1.0).
    /// Always `None` for Wave 1 strategies, which have no partial scoring.
    pub nearest_miss: Option<f64>,
    /// `true` when the replacement must be reindented to the matched block's
    /// base indentation (indentation_flexible strategy only).
    pub reindented: bool,
    /// Guard-rejection reason placeholder. `None` unless a future guard
    /// rejects a candidate. Wave 1 never rejects.
    pub guard_rejected_reason: Option<&'static str>,
    /// Unicode splice status placeholder. `None` for Wave 1.
    pub unicode_splice: Option<UnicodeSpliceStatus>,
}

// ════════════════════════════════════════════════════════════════════════════
// Lower-level typed matcher
// ════════════════════════════════════════════════════════════════════════════

/// Lower-level typed matcher. Locates the single best candidate for
/// `old_text` in `content` using the strict→loose strategy chain and returns
/// typed metadata. Does NOT write files or perform the replacement — the
/// compatibility wrapper `fuzzy_replace` applies `new_text` to the returned
/// byte range.
///
/// First-match-wins: the first strategy that produces any candidate (unique
/// or ambiguous) determines the outcome and stops the chain. A strategy with
/// zero candidates is skipped. If no strategy produces any candidate, the
/// outcome is `NoMatch`.
pub(super) fn find_match(content: &str, old_text: &str) -> MatchMetadata {
    for &strategy in STRATEGY_ORDER {
        if let Some(metadata) = run_strategy(strategy, content, old_text) {
            return metadata;
        }
    }
    // No strategy found any candidate.
    no_match_metadata()
}

/// Dispatch a single strategy. Returns `None` when the strategy found zero
/// candidates (so the chain falls through to the next strategy), or
/// `Some(metadata)` when it found one (Success) or many (Ambiguous).
fn run_strategy(strategy: MatchStrategy, content: &str, old_text: &str) -> Option<MatchMetadata> {
    match strategy {
        MatchStrategy::Exact => try_exact(content, old_text),
        MatchStrategy::LineTrimmed => try_line_trimmed(content, old_text),
        MatchStrategy::WhitespaceNormalized => try_whitespace_normalized(content, old_text),
        MatchStrategy::IndentationFlexible => try_indentation_flexible(content, old_text),
        MatchStrategy::EscapeNormalized => try_escape_normalized(content, old_text),
        MatchStrategy::TrimmedBoundary => try_trimmed_boundary(content, old_text),
        MatchStrategy::UnicodeNormalized
        | MatchStrategy::BlockAnchor
        | MatchStrategy::ContextAware => None,
    }
}

// ── Metadata constructors ───────────────────────────────────────────────────

fn success_metadata(
    strategy: MatchStrategy,
    byte_range: ByteRange,
    line_range: LineRange,
) -> MatchMetadata {
    MatchMetadata {
        strategy,
        outcome: MatchOutcome::Success,
        candidate_count: 1,
        byte_range: Some(byte_range),
        line_range: Some(line_range),
        nearest_miss: None,
        reindented: false,
        guard_rejected_reason: None,
        unicode_splice: None,
    }
}

fn success_metadata_reindented(
    strategy: MatchStrategy,
    byte_range: ByteRange,
    line_range: LineRange,
) -> MatchMetadata {
    MatchMetadata {
        strategy,
        outcome: MatchOutcome::Success,
        candidate_count: 1,
        byte_range: Some(byte_range),
        line_range: Some(line_range),
        nearest_miss: None,
        reindented: true,
        guard_rejected_reason: None,
        unicode_splice: None,
    }
}

fn ambiguous_metadata(strategy: MatchStrategy, count: usize) -> MatchMetadata {
    MatchMetadata {
        strategy,
        outcome: MatchOutcome::Ambiguous,
        candidate_count: count,
        byte_range: None,
        line_range: None,
        nearest_miss: None,
        reindented: false,
        guard_rejected_reason: None,
        unicode_splice: None,
    }
}

/// Metadata for when the entire strategy chain produced no candidate. The
/// `strategy` field is set to the last strategy consulted (conventional).
fn no_match_metadata() -> MatchMetadata {
    MatchMetadata {
        strategy: *STRATEGY_ORDER.last().expect("strategy chain is non-empty"),
        outcome: MatchOutcome::NoMatch,
        candidate_count: 0,
        byte_range: None,
        line_range: None,
        nearest_miss: None,
        reindented: false,
        guard_rejected_reason: None,
        unicode_splice: None,
    }
}

/// Compute the 1-based inclusive line range for a half-open byte range
/// `[start, end)` in `content`. Line numbers count `\n` separators. If `end`
/// lands immediately after a newline (the byte at `end-1` is `\n`), that
/// trailing newline does not open a new matched line — the last matched line
/// is the one the newline terminates.
fn line_range_for(content: &str, start: usize, end: usize) -> LineRange {
    let line_start = content[..start].matches('\n').count() + 1;
    // For the end line, count newlines strictly inside [start, end-1): a
    // trailing `\n` at byte end-1 closes the last matched line rather than
    // opening a new one.
    let last_byte = end.max(start).saturating_sub(1);
    let prefix = &content[..last_byte];
    let line_end = prefix.matches('\n').count() + 1;
    LineRange {
        start: line_start,
        end: line_end,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Individual strategies
// ════════════════════════════════════════════════════════════════════════════

/// Exact byte-for-byte match. Duplicate occurrences are an ambiguity failure.
fn try_exact(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let count = content.matches(old_text).count();
    if count == 0 {
        return None;
    }
    if count == 1 {
        let start = content.find(old_text).unwrap_or(0);
        let end = start + old_text.len();
        return Some(success_metadata(
            MatchStrategy::Exact,
            ByteRange { start, end },
            line_range_for(content, start, end),
        ));
    }
    Some(ambiguous_metadata(MatchStrategy::Exact, count))
}

/// Trim trailing whitespace from each line, then match. Duplicates are an
/// ambiguity failure.
fn try_line_trimmed(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let trimmed_content: String = content
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed_old: String = old_text
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    let count = trimmed_content.matches(&trimmed_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(MatchStrategy::LineTrimmed, count));
    }

    let start = trimmed_content.find(&trimmed_old)?;
    let end = start + trimmed_old.len();

    let (orig_start, orig_end) = map_trimmed_to_original(content, &trimmed_content, start, end);
    Some(success_metadata(
        MatchStrategy::LineTrimmed,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
}

/// Collapse all runs of spaces/tabs to a single space, then match. Duplicates
/// are an ambiguity failure.
fn try_whitespace_normalized(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let (norm_content, content_map) = normalize_whitespace_with_map(content);
    let (norm_old, _) = normalize_whitespace_with_map(old_text);

    let count = norm_content.matches(&norm_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(
            MatchStrategy::WhitespaceNormalized,
            count,
        ));
    }

    let norm_start = norm_content.find(&norm_old)?;
    let norm_end = norm_start + norm_old.len();

    let orig_start = content_map[norm_start];
    let orig_end = if norm_end >= content_map.len() {
        content.len()
    } else {
        content_map[norm_end]
    };

    Some(success_metadata(
        MatchStrategy::WhitespaceNormalized,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
}

/// Strip leading whitespace from each line, match, then report the candidate
/// range with a reindentation flag so the wrapper can reindent the
/// replacement. Duplicates are an ambiguity failure.
fn try_indentation_flexible(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let stripped_content: String = content
        .lines()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    let stripped_old: String = old_text
        .lines()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("\n");

    if stripped_old.is_empty() {
        return None;
    }

    let count = stripped_content.matches(&stripped_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(
            MatchStrategy::IndentationFlexible,
            count,
        ));
    }

    let stripped_start = stripped_content.find(&stripped_old)?;

    let match_start_line = stripped_content[..stripped_start]
        .chars()
        .filter(|&c| c == '\n')
        .count();
    let old_line_count = stripped_old.chars().filter(|&c| c == '\n').count() + 1;

    let content_lines: Vec<&str> = content.lines().collect();

    let mut orig_start = 0usize;
    for line in &content_lines[..match_start_line] {
        orig_start += line.len() + 1;
    }
    let mut orig_end = orig_start;
    for (i, line) in content_lines[match_start_line..]
        .iter()
        .enumerate()
        .take(old_line_count)
    {
        orig_end += line.len();
        if match_start_line + i + 1 < content_lines.len() {
            orig_end += 1;
        }
    }
    orig_end = orig_end.min(content.len());

    Some(success_metadata_reindented(
        MatchStrategy::IndentationFlexible,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
}

/// Normalize string-literal escaping: treat backslash-escaped quotes and
/// backslash-escaped backslashes as equivalent to their literal counterparts
/// for matching purposes only. The original content's byte range is preserved
/// for the replacement, so no actual escaping is changed in the output.
///
/// The escape map maps each byte in the normalized string back to the original
/// byte index.
fn normalize_escapes_with_map(s: &str) -> (String, Vec<usize>) {
    let mut normalized = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip a backslash that precedes a quote or another backslash, but emit
        // the escaped character to the normalized view.
        if bytes[i] == b'\\'
            && i + 1 < bytes.len()
            && (bytes[i + 1] == b'\\' || bytes[i + 1] == b'\'' || bytes[i + 1] == b'"')
        {
            normalized.push(bytes[i + 1] as char);
            map.push(i + 1);
            i += 2;
        } else {
            normalized.push(bytes[i] as char);
            map.push(i);
            i += 1;
        }
    }
    (normalized, map)
}

/// Returns true if `candidate` contains an unescaped single or double quote.
/// Such a candidate would cross or corrupt a string-literal boundary, so the
/// escape-normalized strategy rejects it.
fn candidate_crosses_quote_boundary(candidate: &str) -> bool {
    let mut escaped = false;
    for c in candidate.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '\'' || c == '"' {
            return true;
        }
    }
    false
}

/// Returns true if byte position `pos` in `content` is inside a backslash escape
/// sequence (i.e., an odd number of consecutive backslashes immediately precede
/// `pos`). A candidate that starts or ends inside an escape sequence could
/// leave a partial escape prefix after replacement, so it is rejected.
fn position_inside_escape_sequence(content: &str, pos: usize) -> bool {
    let mut count = 0usize;
    for c in content[..pos].chars().rev() {
        if c == '\\' {
            count += 1;
        } else {
            break;
        }
    }
    count % 2 == 1
}

/// Escape-normalized match. Differences limited to quote/backslash escaping
/// between `old_text` and `content` are allowed. The replacement is applied to
/// the original byte range. Guard rejection occurs when the candidate's
/// quote/backslash balance differs from the surrounding context in a way that
/// would indicate crossing a literal boundary.
fn try_escape_normalized(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let (norm_content, content_map) = normalize_escapes_with_map(content);
    let (norm_old, _) = normalize_escapes_with_map(old_text);

    if norm_old.is_empty() {
        return None;
    }

    let count = norm_content.matches(&norm_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(MatchStrategy::EscapeNormalized, count));
    }

    let norm_start = norm_content.find(&norm_old)?;
    let norm_end = norm_start + norm_old.len();

    let orig_start = content_map[norm_start];
    let orig_end = if norm_end >= content_map.len() {
        content.len()
    } else {
        content_map[norm_end]
    };

    // Guard: the candidate must not cross a quote boundary, and the candidate
    // boundaries must not split an escape sequence in the original content.
    let candidate = &content[orig_start..orig_end];
    let quote_reason = guard_reason_if(
        candidate_crosses_quote_boundary(candidate),
        "escape quote balance mismatch",
    );
    let backslash_reason = guard_reason_if(
        position_inside_escape_sequence(content, orig_start)
            || position_inside_escape_sequence(content, orig_end),
        "escape backslash balance mismatch",
    );
    if let Some(reason) = quote_reason.or(backslash_reason) {
        return Some(guard_rejected_metadata(
            MatchStrategy::EscapeNormalized,
            reason,
        ));
    }

    Some(success_metadata(
        MatchStrategy::EscapeNormalized,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
}

/// Return `Some(reason)` if `condition` is true, else `None`.
fn guard_reason_if(condition: bool, reason: &'static str) -> Option<&'static str> {
    if condition { Some(reason) } else { None }
}

/// Trim leading/trailing blank or whitespace-only lines from `old_text` and
/// `content`, locate the candidate, then report the original byte range of the
/// inner content so the replacement is applied only to the intended candidate.
fn try_trimmed_boundary(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let trimmed_old = trim_boundary_lines(old_text);
    if trimmed_old.is_empty() {
        return None;
    }

    // Candidate content may have extra leading/trailing whitespace-only
    // boundary lines. Strip them and look for a unique match.
    let trimmed_content = trim_boundary_lines(content);
    let count = trimmed_content.matches(&trimmed_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(MatchStrategy::TrimmedBoundary, count));
    }

    let trimmed_start = trimmed_content.find(&trimmed_old)?;
    let trimmed_end = trimmed_start + trimmed_old.len();

    // Map the trimmed positions back to the original content.
    let (orig_start, orig_end) =
        map_trimmed_to_original(content, &trimmed_content, trimmed_start, trimmed_end);

    Some(success_metadata(
        MatchStrategy::TrimmedBoundary,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
}

/// Remove leading/trailing lines that are empty or contain only whitespace.
fn trim_boundary_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let first_content = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(lines.len());
    let last_content = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map(|idx| idx + 1)
        .unwrap_or(lines.len());
    lines[first_content..last_content].join("\n")
}

/// Metadata constructor for guard-rejected outcomes.
fn guard_rejected_metadata(strategy: MatchStrategy, reason: &'static str) -> MatchMetadata {
    MatchMetadata {
        strategy,
        outcome: MatchOutcome::GuardRejected,
        candidate_count: 1,
        byte_range: None,
        line_range: None,
        nearest_miss: None,
        reindented: false,
        guard_rejected_reason: Some(reason),
        unicode_splice: None,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Compatibility wrapper (preserves the call_edit contract)
// ════════════════════════════════════════════════════════════════════════════

/// Multi-layer fuzzy string replacement for the edit tool.
///
/// Tries matching strategies in order of strictness:
/// 1. Exact match
/// 2. Line-trimmed match (trailing whitespace stripped per line)
/// 3. Whitespace-normalized match (runs of whitespace collapsed to single space)
/// 4. Indentation-flexible match (leading whitespace stripped per line)
/// 5. Escape-normalized match (quote/backslash escaping normalized)
/// 6. Trimmed-boundary match (extra blank boundary lines ignored)
///
/// Returns `(new_content, optional_match_note)`. Internally delegates to the
/// typed `find_match` core and translates its metadata into the existing
/// string surface so `call_edit` continues to behave as before.
pub(super) fn fuzzy_replace(
    content: &str,
    old_text: &str,
    new_text: &str,
    path: &Path,
) -> Result<(String, Option<String>), String> {
    let metadata = find_match(content, old_text);

    match metadata.outcome {
        MatchOutcome::Success => {
            let byte_range = metadata
                .byte_range
                .expect("success metadata must carry a byte range");
            let start = byte_range.start;
            let end = byte_range.end;

            let replacement = if metadata.reindented {
                let matched_block = &content[start..end];
                let reindented = reindent_replacement(matched_block, new_text);
                let needs_trailing_newline =
                    content[..end].ends_with('\n') && !reindented.ends_with('\n');
                let mut r = reindented;
                if needs_trailing_newline {
                    r.push('\n');
                }
                r
            } else {
                new_text.to_string()
            };

            let mut result = String::with_capacity(content.len());
            result.push_str(&content[..start]);
            result.push_str(&replacement);
            result.push_str(&content[end..]);

            Ok((result, match_note_for(metadata.strategy)))
        }
        MatchOutcome::Ambiguous => Err(format!(
            "old_text appears {} times {} (must be unique): {}",
            metadata.candidate_count,
            ambiguity_phrase(metadata.strategy),
            path.display()
        )),
        MatchOutcome::NoMatch => Err(format!("old_text not found in file: {}", path.display())),
        MatchOutcome::GuardRejected => Err(format!(
            "old_text match rejected by safety guard{}: {}",
            metadata
                .guard_rejected_reason
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
            path.display()
        )),
    }
}

/// Human-readable match note for a successful strategy, or `None` for exact
/// (which needs no note). Matches the pre-refactor user-visible strings.
fn match_note_for(strategy: MatchStrategy) -> Option<String> {
    match strategy {
        MatchStrategy::Exact => None,
        MatchStrategy::LineTrimmed => {
            Some("(matched after trimming trailing whitespace)".to_string())
        }
        MatchStrategy::WhitespaceNormalized => {
            Some("(matched with whitespace normalization)".to_string())
        }
        MatchStrategy::IndentationFlexible => {
            Some("(matched with flexible indentation)".to_string())
        }
        MatchStrategy::EscapeNormalized => Some("(matched with escape normalization)".to_string()),
        MatchStrategy::TrimmedBoundary => Some("(matched with trimmed boundary lines)".to_string()),
        MatchStrategy::UnicodeNormalized
        | MatchStrategy::BlockAnchor
        | MatchStrategy::ContextAware => None,
    }
}

/// Ambiguity qualifier used in the error message. Matches the pre-refactor
/// user-visible phrasing.
fn ambiguity_phrase(strategy: MatchStrategy) -> &'static str {
    match strategy {
        MatchStrategy::Exact => "in file",
        MatchStrategy::LineTrimmed => "after trimming trailing whitespace",
        MatchStrategy::WhitespaceNormalized => "after whitespace normalization",
        MatchStrategy::IndentationFlexible => "after stripping indentation",
        MatchStrategy::EscapeNormalized => "after escape normalization",
        MatchStrategy::TrimmedBoundary => "after trimming boundary lines",
        MatchStrategy::UnicodeNormalized
        | MatchStrategy::BlockAnchor
        | MatchStrategy::ContextAware => "with future strategy",
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Reindentation helper
// ════════════════════════════════════════════════════════════════════════════

pub(super) fn reindent_replacement(matched_block: &str, replacement: &str) -> String {
    let matched_lines: Vec<&str> = matched_block.split('\n').collect();
    let replacement_lines: Vec<&str> = replacement.split('\n').collect();

    if replacement_lines.is_empty() {
        return String::new();
    }

    let matched_base_indent = matched_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or("", |line| leading_whitespace(line));

    let replacement_base_indent = replacement_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or("", |line| leading_whitespace(line));

    replacement_lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                return String::new();
            }

            let replacement_indent = leading_whitespace(line);
            let relative_indent = replacement_indent
                .strip_prefix(replacement_base_indent)
                .unwrap_or(replacement_indent);

            format!(
                "{matched_base_indent}{relative_indent}{}",
                &line[replacement_indent.len()..]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Map byte positions from a trimmed version back to the original content.
fn map_trimmed_to_original(
    original: &str,
    trimmed: &str,
    trimmed_start: usize,
    trimmed_end: usize,
) -> (usize, usize) {
    let orig_lines: Vec<&str> = original.split('\n').collect();
    let trimmed_lines: Vec<&str> = trimmed.split('\n').collect();

    let mut orig_offset = 0usize;
    let mut trimmed_offset = 0usize;
    let mut result_start = 0usize;
    let mut result_end = 0usize;
    let mut found_start = false;
    let mut found_end = false;

    for (i, (orig_line, trimmed_line)) in orig_lines.iter().zip(trimmed_lines.iter()).enumerate() {
        let newline: usize = usize::from(i < orig_lines.len() - 1);

        if !found_start && trimmed_start < trimmed_offset + trimmed_line.len() + newline {
            let offset_in_line = trimmed_start - trimmed_offset;
            result_start = orig_offset + offset_in_line;
            found_start = true;
        }

        if !found_end && trimmed_end <= trimmed_offset + trimmed_line.len() + newline {
            let offset_in_line = trimmed_end - trimmed_offset;
            let clamped = offset_in_line.min(orig_line.len() + newline);
            result_end = orig_offset + clamped;
            found_end = true;
        }

        orig_offset += orig_line.len() + newline;
        trimmed_offset += trimmed_line.len() + newline;

        if found_start && found_end {
            break;
        }
    }

    (result_start, result_end)
}

/// Normalize whitespace: collapse runs of spaces/tabs to a single space.
/// Returns (normalized_string, map from normalized byte index to original byte
/// index).
fn normalize_whitespace_with_map(s: &str) -> (String, Vec<usize>) {
    let mut normalized = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len());
    let mut in_ws = false;
    let bytes = s.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' || b == b'\r' {
            in_ws = false;
            normalized.push(b as char);
            map.push(i);
        } else if b == b' ' || b == b'\t' {
            if !in_ws {
                normalized.push(' ');
                map.push(i);
                in_ws = true;
            }
        } else {
            in_ws = false;
            normalized.push(b as char);
            map.push(i);
        }
    }

    (normalized, map)
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::{
        MatchOutcome, MatchStrategy, ambiguity_phrase, find_match, fuzzy_replace, line_range_for,
        match_note_for, reindent_replacement,
    };
    use std::path::Path;

    // ── Existing wrapper-level tests (unchanged behaviour) ──────────────────

    #[test]
    fn rebases_multiline_replacement_using_matched_indentation() {
        let content = "fn main() {\n    match value {\n        Some(x) => {\n            process(x);\n        }\n    }\n}\n";
        let old_text = "match value {\n    Some(x) => {\n        process(x);\n    }\n}";
        let new_text = "match value {\n    Some(x) => {\n        if ready {\n            process(x);\n        }\n    }\n}";

        let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
            .expect("fuzzy replace should succeed");

        assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
        assert!(updated.contains(
            "    match value {\n        Some(x) => {\n            if ready {\n                process(x);\n            }\n        }\n    }"
        ));
    }

    #[test]
    fn preserves_later_nested_indent_when_first_replacement_line_is_less_indented() {
        let content = "impl Example {\n        if condition {\n            run();\n        }\n}\n";
        let old_text = "if condition {\n    run();\n}";
        let new_text =
            "if condition {\n    let nested = || {\n        run();\n    };\n    nested();\n}";

        let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
            .expect("fuzzy replace should succeed");

        assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
        assert!(updated.contains(
            "        if condition {\n            let nested = || {\n                run();\n            };\n            nested();\n        }"
        ));
    }

    #[test]
    fn reindent_replacement_preserves_internal_relative_indentation() {
        let matched_block = "        if ready {\n            execute();\n        }";
        let replacement =
            "if ready {\n    let nested = || {\n        execute();\n    };\n    nested();\n}";

        assert_eq!(
            reindent_replacement(matched_block, replacement),
            "        if ready {\n            let nested = || {\n                execute();\n            };\n            nested();\n        }"
        );
    }

    // ── Typed metadata tests ───────────────────────────────────────────────

    #[test]
    fn exact_match_returns_success_metadata() {
        let content = "hello world\nfoo bar\n";
        let old_text = "world";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::Exact);
        assert_eq!(m.outcome, MatchOutcome::Success);
        assert_eq!(m.candidate_count, 1);
        let br = m.byte_range.expect("exact success has byte range");
        assert_eq!(br.start, 6);
        assert_eq!(br.end, 11);
        let lr = m.line_range.expect("exact success has line range");
        assert_eq!(lr.start, 1);
        assert_eq!(lr.end, 1);
        assert!(!m.reindented);
        assert!(m.nearest_miss.is_none());
        assert!(m.guard_rejected_reason.is_none());
        assert!(m.unicode_splice.is_none());
    }

    #[test]
    fn exact_match_note_is_none() {
        assert!(match_note_for(MatchStrategy::Exact).is_none());
    }

    #[test]
    fn line_trimmed_match_returns_success_metadata() {
        // Content has trailing spaces; old_text does not.
        let content = "let x = 1;   \nlet y = 2;\n";
        let old_text = "let x = 1;\nlet y = 2;";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::LineTrimmed);
        assert_eq!(m.outcome, MatchOutcome::Success);
        assert_eq!(m.candidate_count, 1);
        assert!(m.byte_range.is_some());
        assert!(m.line_range.is_some());
        assert!(!m.reindented);
    }

    #[test]
    fn whitespace_normalized_match_returns_success_metadata() {
        // Content has extra spaces between tokens; old_text has single spaces.
        let content = "fn    main()   {  }";
        let old_text = "fn main() { }";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::WhitespaceNormalized);
        assert_eq!(m.outcome, MatchOutcome::Success);
        assert_eq!(m.candidate_count, 1);
        let br = m.byte_range.expect("whitespace success has byte range");
        // Byte range should cover the matched span in original content.
        assert_eq!(&content[br.start..br.end], "fn    main()   {  }");
        assert!(!m.reindented);
    }

    #[test]
    fn indentation_flexible_match_returns_success_with_reindentation_metadata() {
        // Content is indented more than old_text.
        let content = "fn outer() {\n    if ready {\n        do_thing();\n    }\n}\n";
        let old_text = "if ready {\n    do_thing();\n}";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::IndentationFlexible);
        assert_eq!(m.outcome, MatchOutcome::Success);
        assert_eq!(m.candidate_count, 1);
        let br = m.byte_range.expect("indentation success has byte range");
        // The matched block in the original includes the file's indentation.
        // The byte range also includes the trailing newline of the matched
        // region (the wrapper compensates via `needs_trailing_newline`), so we
        // assert the block up to the closing brace and separately confirm the
        // range extends to cover the last matched line.
        assert!(content[br.start..br.end].starts_with("    if ready {"));
        assert!(
            content[br.start..br.end].contains("do_thing();"),
            "byte range must cover the matched block: {:?}",
            &content[br.start..br.end]
        );
        let lr = m.line_range.expect("indentation success has line range");
        assert_eq!(lr.start, 2);
        assert_eq!(lr.end, 4);
        assert!(
            m.reindented,
            "indentation_flexible success must set the reindentation flag"
        );
    }

    #[test]
    fn ambiguous_match_returns_ambiguity_metadata() {
        let content = "foo\nbar\nfoo\nbaz\n";
        let old_text = "foo";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::Exact);
        assert_eq!(m.outcome, MatchOutcome::Ambiguous);
        assert_eq!(m.candidate_count, 2);
        assert!(m.byte_range.is_none());
        assert!(m.line_range.is_none());
    }

    #[test]
    fn no_match_returns_no_match_metadata() {
        let content = "hello world\n";
        let old_text = "this text does not exist anywhere";

        let m = find_match(content, old_text);

        assert_eq!(m.outcome, MatchOutcome::NoMatch);
        assert_eq!(m.candidate_count, 0);
        assert!(m.byte_range.is_none());
        assert!(m.line_range.is_none());
    }

    #[test]
    fn strategy_as_str_returns_stable_identifiers() {
        assert_eq!(MatchStrategy::Exact.as_str(), "exact");
        assert_eq!(MatchStrategy::LineTrimmed.as_str(), "line_trimmed");
        assert_eq!(
            MatchStrategy::WhitespaceNormalized.as_str(),
            "whitespace_normalized"
        );
        assert_eq!(
            MatchStrategy::IndentationFlexible.as_str(),
            "indentation_flexible"
        );
        assert_eq!(
            MatchStrategy::EscapeNormalized.as_str(),
            "escape_normalized"
        );
        assert_eq!(MatchStrategy::TrimmedBoundary.as_str(), "trimmed_boundary");
    }

    #[test]
    fn ambiguity_phrases_match_legacy_wording() {
        assert_eq!(ambiguity_phrase(MatchStrategy::Exact), "in file");
        assert_eq!(
            ambiguity_phrase(MatchStrategy::LineTrimmed),
            "after trimming trailing whitespace"
        );
        assert_eq!(
            ambiguity_phrase(MatchStrategy::WhitespaceNormalized),
            "after whitespace normalization"
        );
        assert_eq!(
            ambiguity_phrase(MatchStrategy::IndentationFlexible),
            "after stripping indentation"
        );
        assert_eq!(
            ambiguity_phrase(MatchStrategy::EscapeNormalized),
            "after escape normalization"
        );
        assert_eq!(
            ambiguity_phrase(MatchStrategy::TrimmedBoundary),
            "after trimming boundary lines"
        );
    }

    #[test]
    fn strategy_order_includes_new_strategies_after_indentation_flexible() {
        let expected: &[MatchStrategy] = &[
            MatchStrategy::Exact,
            MatchStrategy::LineTrimmed,
            MatchStrategy::WhitespaceNormalized,
            MatchStrategy::IndentationFlexible,
            MatchStrategy::EscapeNormalized,
            MatchStrategy::TrimmedBoundary,
        ];
        assert_eq!(
            super::STRATEGY_ORDER,
            expected,
            "escape_normalized and trimmed_boundary must follow indentation_flexible and precede later strategies"
        );
    }

    #[test]
    fn line_range_for_single_line() {
        let content = "aaa\nbbb\nccc";
        let lr = line_range_for(content, 4, 7);
        assert_eq!(lr.start, 2);
        assert_eq!(lr.end, 2);
    }

    #[test]
    fn line_range_for_multiline() {
        let content = "aaa\nbbb\nccc";
        let lr = line_range_for(content, 4, 11);
        assert_eq!(lr.start, 2);
        assert_eq!(lr.end, 3);
    }

    // ── New strategy tests: escape_normalized and trimmed_boundary ───────────

    #[test]
    fn escape_normalized_match_handles_escaped_quotes() {
        // File contains a single-quoted string with escaped quotes; the old_text
        // uses a different escaping style but is escape-equivalent.
        let content = "let s = \"He said \\\"hello\\\"\";";
        let old_text = "He said \"hello\"";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
        assert_eq!(m.outcome, MatchOutcome::Success);
        let br = m.byte_range.expect("escape success has byte range");
        assert_eq!(&content[br.start..br.end], "He said \\\"hello\\\"");
    }

    #[test]
    fn escape_normalized_match_handles_escaped_backslashes() {
        let content = "let path = \"C:\\\\Users\\\\foo\";";
        let old_text = "C:\\\\Users\\\\foo";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
        assert_eq!(m.outcome, MatchOutcome::Success);
        let br = m.byte_range.expect("escape success has byte range");
        assert_eq!(&content[br.start..br.end], old_text);
    }

    #[test]
    fn escape_normalized_rejects_quote_imbalance_guard() {
        // The candidate ("x"; let b = "x") contains unescaped quotes, so it
        // crosses a literal boundary. The normalized view matches old_text but
        // the guard rejects it.
        let content = "let a = \"x\"; let b = \"x\";";
        let old_text = "x\"; let b = \"x";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
        assert_eq!(m.outcome, MatchOutcome::GuardRejected);
        assert_eq!(
            m.guard_rejected_reason,
            Some("escape quote balance mismatch")
        );
    }

    #[test]
    fn escape_normalized_rejects_backslash_imbalance_guard() {
        // The candidate starts immediately after an opening backslash escape and
        // ends inside another, so the boundaries split escape sequences.
        let content = "let a = \\\"x\\\";";
        let old_text = "x";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
        assert_eq!(m.outcome, MatchOutcome::GuardRejected);
        assert_eq!(
            m.guard_rejected_reason,
            Some("escape backslash balance mismatch")
        );
    }

    #[test]
    fn escape_normalized_ambiguity_requires_disambiguation() {
        let content = "x\\\" y x\\\" y";
        let old_text = "x\" y";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
        assert_eq!(m.outcome, MatchOutcome::Ambiguous);
        assert_eq!(m.candidate_count, 2);
    }

    #[test]
    fn trimmed_boundary_ignores_leading_blank_lines() {
        let content = "\n\nlet x = 1;\nlet y = 2;\n";
        let old_text = "let x = 1;\nlet y = 2;";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::TrimmedBoundary);
        assert_eq!(m.outcome, MatchOutcome::Success);
        let br = m
            .byte_range
            .expect("trimmed boundary success has byte range");
        assert_eq!(&content[br.start..br.end], "let x = 1;\nlet y = 2;\n");
    }

    #[test]
    fn trimmed_boundary_ignores_trailing_whitespace_lines() {
        let content = "let x = 1;\nlet y = 2;\n   \n\n";
        let old_text = "let x = 1;\nlet y = 2;";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::TrimmedBoundary);
        assert_eq!(m.outcome, MatchOutcome::Success);
        let br = m
            .byte_range
            .expect("trimmed boundary success has byte range");
        assert_eq!(&content[br.start..br.end], "let x = 1;\nlet y = 2;\n");
    }

    #[test]
    fn trimmed_boundary_does_not_replace_surrounding_whitespace_lines() {
        let content = "header\n\nlet x = 1;\nlet y = 2;\n\nfooter\n";
        let old_text = "\n\nlet x = 1;\nlet y = 2;\n\n";
        let new_text = "let a = 9;\nlet b = 8;";

        let (updated, note) =
            fuzzy_replace(content, old_text, new_text, Path::new("test.rs")).unwrap();

        assert_eq!(
            note.as_deref(),
            Some("(matched with trimmed boundary lines)")
        );
        assert!(updated.contains("header\n"));
        assert!(updated.contains("let a = 9;\nlet b = 8;"));
        assert!(updated.contains("footer\n"));
    }

    #[test]
    fn trimmed_boundary_ambiguity_requires_disambiguation() {
        let content = "let x = 1;\n\nlet x = 1;";
        let old_text = "\n\nlet x = 1;\n";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::TrimmedBoundary);
        assert_eq!(m.outcome, MatchOutcome::Ambiguous);
        assert_eq!(m.candidate_count, 2);
    }

    #[test]
    fn first_match_wins_escape_before_trimmed_boundary() {
        // A case that could be matched by both escape_normalized and
        // trimmed_boundary: the earlier escape strategy should win.
        let content = "let s = \"x\\\" y\";";
        let old_text = "x\" y";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
        assert_eq!(m.outcome, MatchOutcome::Success);
    }

    #[test]
    fn first_match_wins_indentation_before_escape() {
        // A case that indentation_flexible can match; escape_normalized should not
        // shadow it because indentation_flexible is earlier in the chain.
        let content = "    if ready {\n        do_thing();\n    }\n";
        let old_text = "if ready {\n    do_thing();\n}";

        let m = find_match(content, old_text);

        assert_eq!(m.strategy, MatchStrategy::IndentationFlexible);
        assert_eq!(m.outcome, MatchOutcome::Success);
    }

    // ── Wrapper tests ─────────────────────────────────────────────────────

    #[test]
    fn wrapper_preserves_exact_match_note_absence() {
        let content = "hello world";
        let (updated, note) =
            fuzzy_replace(content, "world", "universe", Path::new("test.rs")).unwrap();
        assert_eq!(updated, "hello universe");
        assert!(note.is_none(), "exact match should not produce a note");
    }

    #[test]
    fn wrapper_line_trimmed_uses_typed_core() {
        // Multiline: the content's first line has trailing spaces, so the exact
        // two-line substring is NOT present, but line-trimmed matching finds it.
        let content = "let x = 1;   \nlet y = 2;\n";
        let (updated, note) = fuzzy_replace(
            content,
            "let x = 1;\nlet y = 2;",
            "let x = 3;\nlet y = 4;",
            Path::new("test.rs"),
        )
        .unwrap();
        assert_eq!(updated, "let x = 3;\nlet y = 4;\n");
        assert_eq!(
            note.as_deref(),
            Some("(matched after trimming trailing whitespace)")
        );
    }

    #[test]
    fn wrapper_reports_ambiguity_with_strategy_phrase() {
        let content = "foo bar\nfoo bar\n";
        let err = fuzzy_replace(content, "foo", "baz", Path::new("test.rs")).unwrap_err();
        assert!(err.contains("appears 2 times in file"), "got: {err}");
        assert!(err.contains("test.rs"), "got: {err}");
    }

    #[test]
    fn wrapper_reports_no_match() {
        let content = "hello\n";
        let err = fuzzy_replace(content, "missing", "x", Path::new("f.rs")).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
        assert!(err.contains("f.rs"), "got: {err}");
    }
}

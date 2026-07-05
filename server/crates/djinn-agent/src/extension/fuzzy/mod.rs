//! Typed fuzzy-match strategy core for the edit tool.
//!
//! This module exposes a lower-level, typed matcher (`find_match`) that
//! locates the single best candidate for `old_text` in file `content` using a
//! strict→loose strategy chain, returning rich metadata (strategy, outcome,
//! byte/line range, reindentation flag, nearest-miss scoring, guard rejection
//! reason, and placeholder fields for future Unicode-splice status). The
//! matcher never writes files or performs the replacement — that is the job of
//! the compatibility wrapper `fuzzy_replace`, which translates typed metadata
//! into the existing `(String, Option<String>)` surface consumed by
//! `call_edit`.
//!
//! Conceptual design only — no third-party source (e.g. Hermes) is vendored or
//! copied. Strategy ordering and guard concepts are original to this project;
//! see [[design/c77e-roadmap]].

use std::path::Path;

mod util;
use util::*;

// ════════════════════════════════════════════════════════════════════════════
// Typed strategy identification
// ════════════════════════════════════════════════════════════════════════════

/// Matching strategies, ordered strict → loose. The matcher consults them in
/// `STRATEGY_ORDER` and returns the result of the first strategy that produces
/// any candidate (unique match or ambiguity).
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
    MatchStrategy::UnicodeNormalized,
    MatchStrategy::BlockAnchor,
    MatchStrategy::ContextAware,
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
    /// Candidate found but a safety guard rejected it (UTF-8 boundary, CRLF
    /// preservation, line-boundary, or escape balance guard).
    GuardRejected,
}

/// Grapheme-safe Unicode splice status.
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

/// Typed metadata from a single strategy-chain run. `fuzzy_replace` translates
/// this into the existing string surface for `call_edit`.
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
    /// Nearest-miss score for no-match outcomes (0.0..=1.0). `None` unless
    /// `outcome == NoMatch`. Computed as the longest normalized substring of
    /// `old_text` found in `content`, divided by the length of normalized
    /// `old_text`.
    pub nearest_miss: Option<f64>,
    /// `true` when the replacement must be reindented to the matched block's
    /// base indentation (indentation_flexible strategy only).
    pub reindented: bool,
    /// Guard-rejection reason. `None` unless `outcome == GuardRejected`.
    /// Set when a safety guard (UTF-8 boundary, line-boundary, or CRLF
    /// preservation) rejects a candidate that would silently corrupt the
    /// file content.
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
    no_match_metadata(content, old_text)
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
        MatchStrategy::UnicodeNormalized => try_unicode_normalized(content, old_text),
        MatchStrategy::BlockAnchor => try_block_anchor(content, old_text),
        MatchStrategy::ContextAware => try_context_aware(content, old_text),
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
/// Computes a nearest-miss score indicating the best partial substring match.
fn no_match_metadata(content: &str, old_text: &str) -> MatchMetadata {
    MatchMetadata {
        strategy: *STRATEGY_ORDER.last().expect("strategy chain is non-empty"),
        outcome: MatchOutcome::NoMatch,
        candidate_count: 0,
        byte_range: None,
        line_range: None,
        nearest_miss: Some(nearest_miss_score(content, old_text)),
        reindented: false,
        guard_rejected_reason: None,
        unicode_splice: None,
    }
}

/// Metadata constructor for guard-rejection outcomes.
fn reject_metadata(strategy: MatchStrategy, reason: &'static str) -> MatchMetadata {
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
        // Safety guards.
        if let Err(reason) = guard_utf8_boundary(content, start, end) {
            return Some(reject_metadata(MatchStrategy::Exact, reason));
        }
        if let Err(reason) = guard_crlf_preservation(content, start, end) {
            return Some(reject_metadata(MatchStrategy::Exact, reason));
        }
        if let Err(reason) = guard_line_boundary(content, start, end, old_text) {
            return Some(reject_metadata(MatchStrategy::Exact, reason));
        }
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
    // Safety guards on the mapped original byte range.
    if let Err(reason) = guard_utf8_boundary(content, orig_start, orig_end) {
        return Some(reject_metadata(MatchStrategy::LineTrimmed, reason));
    }
    if let Err(reason) = guard_crlf_preservation(content, orig_start, orig_end) {
        return Some(reject_metadata(MatchStrategy::LineTrimmed, reason));
    }
    if let Err(reason) = guard_line_boundary(content, orig_start, orig_end, &trimmed_old) {
        return Some(reject_metadata(MatchStrategy::LineTrimmed, reason));
    }
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
    // Safety guards on the mapped original byte range.
    if let Err(reason) = guard_utf8_boundary(content, orig_start, orig_end) {
        return Some(reject_metadata(MatchStrategy::WhitespaceNormalized, reason));
    }
    if let Err(reason) = guard_crlf_preservation(content, orig_start, orig_end) {
        return Some(reject_metadata(MatchStrategy::WhitespaceNormalized, reason));
    }
    if let Err(reason) = guard_line_boundary(content, orig_start, orig_end, &norm_old) {
        return Some(reject_metadata(MatchStrategy::WhitespaceNormalized, reason));
    }
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
    // Safety guards on the mapped original byte range.
    if let Err(reason) = guard_utf8_boundary(content, orig_start, orig_end) {
        return Some(reject_metadata(MatchStrategy::IndentationFlexible, reason));
    }
    if let Err(reason) = guard_crlf_preservation(content, orig_start, orig_end) {
        return Some(reject_metadata(MatchStrategy::IndentationFlexible, reason));
    }
    if let Err(reason) = guard_line_boundary(content, orig_start, orig_end, &stripped_old) {
        return Some(reject_metadata(MatchStrategy::IndentationFlexible, reason));
    }
    Some(success_metadata_reindented(
        MatchStrategy::IndentationFlexible,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
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
        return Some(reject_metadata(MatchStrategy::EscapeNormalized, reason));
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

/// Trim leading/trailing blank or whitespace-only lines from `old_text` and
/// `content`, locate the candidate, then report the original byte range of the
/// inner content so the replacement is applied only to the intended candidate.
fn try_trimmed_boundary(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let trimmed_old = trim_boundary_lines(old_text);
    if trimmed_old.is_empty() || trimmed_old == old_text {
        return None;
    }

    // The caller may include extra leading/trailing blank or whitespace-only
    // boundary lines for context. Match only the trimmed inner text against the
    // original content so the replacement range excludes those boundary lines.
    let count = content.matches(&trimmed_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(MatchStrategy::TrimmedBoundary, count));
    }

    let orig_start = content.find(&trimmed_old)?;
    let orig_end = orig_start + trimmed_old.len();

    Some(success_metadata(
        MatchStrategy::TrimmedBoundary,
        ByteRange {
            start: orig_start,
            end: orig_end,
        },
        line_range_for(content, orig_start, orig_end),
    ))
}

/// Unicode-normalised match. Differences limited to NFKC-equivalent text and
/// explicit confusables (smart quotes, typographic dashes, Latin ligatures,
/// fullwidth ASCII, and superscript/subscript digits) are tolerated. The
/// replacement is spliced at grapheme-safe boundaries and unchanged original
/// Unicode bytes outside the edited range are preserved byte-for-byte.
fn try_unicode_normalized(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let (norm_content, content_map) = normalize_with_confusables(content);
    let (norm_old, _) = normalize_with_confusables(old_text);

    if norm_old.is_empty() {
        return None;
    }

    let count = norm_content.matches(norm_old.as_str()).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(ambiguous_metadata(MatchStrategy::UnicodeNormalized, count));
    }

    let norm_start = norm_content.find(norm_old.as_str())?;
    let norm_end = norm_start + norm_old.len();

    // Map normalised byte range back to original byte range.
    let orig_start = content_map[norm_start];
    let orig_end = if norm_end >= content_map.len() {
        content.len()
    } else {
        content_map[norm_end]
    };

    // Grapheme-boundary adjustment: snap both boundaries to grapheme clusters.
    let adj_start = adjusted_grapheme_start(content, orig_start);
    let adj_end = adjusted_grapheme_end(content, orig_end);
    let clean = adj_start == orig_start && adj_end == orig_end;
    let splice_status = if clean {
        UnicodeSpliceStatus::Clean
    } else {
        UnicodeSpliceStatus::Adjusted
    };

    // Safety guard: UTF-8 boundaries.
    if let Err(reason) = guard_utf8_boundary(content, adj_start, adj_end) {
        return Some(reject_metadata(MatchStrategy::UnicodeNormalized, reason));
    }
    // Safety guard: CRLF preservation.
    if let Err(reason) = guard_crlf_preservation(content, adj_start, adj_end) {
        return Some(reject_metadata(MatchStrategy::UnicodeNormalized, reason));
    }
    // Safety guard: line boundaries for multi-line matches.
    // Use the *original* normalised match text (not the adjusted range) for
    // the multi-line check, because the grapheme adjustment may extend the
    // range by combining marks rather than newlines.
    let matched_text = &content[adj_start..adj_end];
    if let Err(reason) = guard_line_boundary(content, adj_start, adj_end, matched_text) {
        return Some(reject_metadata(MatchStrategy::UnicodeNormalized, reason));
    }

    Some(success_metadata_unicode(
        ByteRange {
            start: adj_start,
            end: adj_end,
        },
        line_range_for(content, adj_start, adj_end),
        splice_status,
    ))
}

/// Success metadata with Unicode splice status populated.
fn success_metadata_unicode(
    byte_range: ByteRange,
    line_range: LineRange,
    splice: UnicodeSpliceStatus,
) -> MatchMetadata {
    MatchMetadata {
        strategy: MatchStrategy::UnicodeNormalized,
        outcome: MatchOutcome::Success,
        candidate_count: 1,
        byte_range: Some(byte_range),
        line_range: Some(line_range),
        nearest_miss: None,
        reindented: false,
        guard_rejected_reason: None,
        unicode_splice: Some(splice),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Block-anchor & context-aware strategy constants
// ════════════════════════════════════════════════════════════════════════════

/// Maximum number of lines allowed in a block-anchor span. Exceeding this
/// triggers a guard rejection to prevent overly broad matches.
const BLOCK_ANCHOR_MAX_SPAN_LINES: usize = 100;

/// Minimum context-aware score (0.0..=1.0) for a candidate to be accepted.
const CONTEXT_AWARE_MIN_SCORE: f64 = 0.7;

/// Margin within which two candidates are considered a tie (rejected as
/// ambiguous).
const CONTEXT_AWARE_TIE_MARGIN: f64 = 0.05;

/// Block-anchor strategy: use the first and last non-empty lines of `old_text`
/// as "anchor" lines. Find each anchor in `content`; when both anchors appear
/// exactly once and bound a span of at most `BLOCK_ANCHOR_MAX_SPAN_LINES`
/// lines, the inner content between them is the candidate. The full candidate
/// range is then compared against the full normalized `old_text` so that
/// whitespace/indentation differences within the block do not prevent a match.
fn try_block_anchor(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let start_anchor = first_non_empty_line(old_text);
    let end_anchor = last_non_empty_line(old_text);

    if start_anchor.is_empty() {
        return None;
    }

    // Single-line old_text: both anchors are the same line. The block
    // approach adds no value because there is no interior to span. Skip
    // so the later context_aware strategy (or earlier strategies) can
    // handle it.
    if start_anchor == end_anchor {
        return None;
    }

    // Find start anchor (exact substring in content).
    let start_positions: Vec<usize> = content
        .match_indices(start_anchor)
        .map(|(pos, _)| pos)
        .collect();

    if start_positions.is_empty() {
        return None;
    }
    if start_positions.len() > 1 {
        return Some(ambiguous_metadata(
            MatchStrategy::BlockAnchor,
            start_positions.len(),
        ));
    }
    let block_start = start_positions[0];

    // Find end anchor (exact substring in content).
    let end_positions: Vec<usize> = content
        .match_indices(end_anchor)
        .map(|(pos, _)| pos)
        .collect();

    if end_positions.is_empty() {
        return None;
    }
    if end_positions.len() > 1 {
        return Some(ambiguous_metadata(
            MatchStrategy::BlockAnchor,
            end_positions.len(),
        ));
    }
    let end_anchor_start = end_positions[0];
    let block_end = end_anchor_start + end_anchor.len();

    // The end anchor must appear after the start anchor.
    if block_end <= block_start {
        return None;
    }

    // Guard: block span must not exceed the maximum line limit.
    let span_lines = content[block_start..block_end].matches('\n').count();
    if span_lines > BLOCK_ANCHOR_MAX_SPAN_LINES {
        return Some(reject_metadata(
            MatchStrategy::BlockAnchor,
            "block anchor span exceeds maximum line limit",
        ));
    }

    // The candidate text in content is the block between (inclusive of) the
    // two anchors.
    let candidate = &content[block_start..block_end];

    // Compare normalized forms: collapse whitespace runs to single spaces for
    // both the candidate and old_text so minor indentation/spacing differences
    // do not prevent a match.
    let (norm_candidate, _) = normalize_whitespace_with_map(candidate);
    let (norm_old, _) = normalize_whitespace_with_map(old_text);

    if norm_candidate != norm_old {
        return None;
    }

    // Safety guards on the original byte range.
    if let Err(reason) = guard_utf8_boundary(content, block_start, block_end) {
        return Some(reject_metadata(MatchStrategy::BlockAnchor, reason));
    }
    if let Err(reason) = guard_crlf_preservation(content, block_start, block_end) {
        return Some(reject_metadata(MatchStrategy::BlockAnchor, reason));
    }
    if let Err(reason) = guard_line_boundary(content, block_start, block_end, candidate) {
        return Some(reject_metadata(MatchStrategy::BlockAnchor, reason));
    }

    Some(success_metadata(
        MatchStrategy::BlockAnchor,
        ByteRange {
            start: block_start,
            end: block_end,
        },
        line_range_for(content, block_start, block_end),
    ))
}

/// Context-aware strategy: locate candidate regions by searching for the first
/// non-empty line of `old_text` as a substring in `content`. For each match
/// position, score the surrounding context by comparing subsequent normalized
/// lines of `old_text` against the corresponding lines in `content` at that
/// position. Accept only when:
///
/// - The best score meets `CONTEXT_AWARE_MIN_SCORE`.
/// - The best score exceeds the second-best score by at least
///   `CONTEXT_AWARE_TIE_MARGIN`.
///
/// This strategy is deliberately the last in the chain and is more permissive
/// than earlier strategies — it accepts partial line-level matches — but its
/// scoring and tie-rejection keep false positives low.
fn try_context_aware(content: &str, old_text: &str) -> Option<MatchMetadata> {
    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();

    if old_lines.is_empty() {
        return None;
    }

    // Use the first non-empty trimmed line as an anchor to locate candidate
    // regions in the content.
    let anchor = old_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        .unwrap_or("");
    if anchor.is_empty() {
        return None;
    }

    // Find every occurrence of the anchor line (whitespace-trimmed) in the
    // content. We work at the line level so we can score surrounding context.
    let anchor_normalized = collapse_whitespace(anchor);
    let mut candidate_starts: Vec<usize> = Vec::new();
    for (i, line) in content_lines.iter().enumerate() {
        if collapse_whitespace(line.trim()) == anchor_normalized {
            candidate_starts.push(i);
        }
    }

    if candidate_starts.is_empty() {
        return None;
    }

    // Score each candidate: count the number of consecutive old_lines that
    // match (after whitespace normalization) at the corresponding position in
    // content_lines.
    let old_normalized: Vec<String> = old_lines
        .iter()
        .map(|l| collapse_whitespace(l.trim()))
        .collect();

    struct Candidate {
        content_line_start: usize,
        matched_lines: usize,
        score: f64,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for &line_start in &candidate_starts {
        let remaining = content_lines.len().saturating_sub(line_start);
        let compare_count = old_lines.len().min(remaining);
        let mut matched = 0usize;
        for j in 0..compare_count {
            let content_norm = collapse_whitespace(content_lines[line_start + j].trim());
            if content_norm == old_normalized[j] {
                matched += 1;
            } else {
                break; // Stop at first mismatch — sequential match only.
            }
        }
        if matched == 0 {
            continue;
        }
        let score = matched as f64 / old_lines.len() as f64;
        candidates.push(Candidate {
            content_line_start: line_start,
            matched_lines: matched,
            score,
        });
    }

    if candidates.is_empty() {
        return None;
    }

    // Sort by score descending, then by matched_lines descending.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.matched_lines.cmp(&a.matched_lines))
    });

    let best = &candidates[0];

    // Reject if below the minimum score threshold.
    if best.score < CONTEXT_AWARE_MIN_SCORE {
        return Some(reject_metadata(
            MatchStrategy::ContextAware,
            "context-aware score below threshold",
        ));
    }

    // Tie rejection: if the second-best score is within the tie margin of the
    // best, reject as ambiguous.
    if candidates.len() > 1 {
        let second = &candidates[1];
        if (best.score - second.score).abs() < CONTEXT_AWARE_TIE_MARGIN {
            return Some(ambiguous_metadata(
                MatchStrategy::ContextAware,
                candidates.len(),
            ));
        }
    }

    // Compute the byte range for the best candidate.
    let mut byte_start = 0usize;
    for line in &content_lines[..best.content_line_start] {
        byte_start += line.len() + 1; // +1 for '\n'
    }
    let mut byte_end = byte_start;
    for (i, line) in content_lines[best.content_line_start..]
        .iter()
        .enumerate()
        .take(old_lines.len())
    {
        byte_end += line.len();
        if best.content_line_start + i + 1 < content_lines.len() {
            byte_end += 1; // +1 for '\n'
        }
    }
    byte_end = byte_end.min(content.len());

    // Safety guards on the byte range.
    if let Err(reason) = guard_utf8_boundary(content, byte_start, byte_end) {
        return Some(reject_metadata(MatchStrategy::ContextAware, reason));
    }
    if let Err(reason) = guard_crlf_preservation(content, byte_start, byte_end) {
        return Some(reject_metadata(MatchStrategy::ContextAware, reason));
    }
    // Note: context_aware intentionally allows partial-line matches (unlike
    // earlier strategies), so we do NOT apply the line-boundary guard here.

    Some(success_metadata(
        MatchStrategy::ContextAware,
        ByteRange {
            start: byte_start,
            end: byte_end,
        },
        line_range_for(content, byte_start, byte_end),
    ))
}

// ════════════════════════════════════════════════════════════════════════════
// Typed match application (crate-internal helper for call_edit)
// ════════════════════════════════════════════════════════════════════════════

/// Apply a successful typed match to produce the new file content.
///
/// Extracts the byte range from `metadata` (which must be `Success`), applies
/// reindentation when `metadata.reindented` is set, and splices `new_text` into
/// `content` at the matched range.  Panics if `metadata.outcome` is not
/// `Success` — callers must match on the outcome first.
pub(super) fn apply_match(content: &str, new_text: &str, metadata: &MatchMetadata) -> String {
    assert!(
        metadata.outcome == MatchOutcome::Success,
        "apply_match called with non-Success outcome: {:?}",
        metadata.outcome
    );
    let byte_range = metadata
        .byte_range
        .expect("success metadata must carry a byte range");
    let start = byte_range.start;
    let end = byte_range.end;

    let replacement = if metadata.reindented {
        let matched_block = &content[start..end];
        let reindented = reindent_replacement(matched_block, new_text);
        let needs_trailing_newline = content[..end].ends_with('\n') && !reindented.ends_with('\n');
        let mut r = reindented;
        if needs_trailing_newline {
            r.push('\n');
        }
        r
    } else {
        new_text.to_string()
    };

    let mut result = String::with_capacity(content.len() + replacement.len());
    result.push_str(&content[..start]);
    result.push_str(&replacement);
    result.push_str(&content[end..]);
    result
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
#[allow(dead_code)] // retained for test backward compatibility
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
pub(super) fn match_note_for(strategy: MatchStrategy) -> Option<String> {
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
        MatchStrategy::UnicodeNormalized => {
            Some("(matched with Unicode normalization)".to_string())
        }
        MatchStrategy::BlockAnchor => Some("(matched with block anchor)".to_string()),
        MatchStrategy::ContextAware => Some("(matched with context-aware scoring)".to_string()),
    }
}

/// Ambiguity qualifier used in the error message. Matches the pre-refactor
/// user-visible phrasing.
#[allow(dead_code)] // retained for test backward compatibility
pub(super) fn ambiguity_phrase(strategy: MatchStrategy) -> &'static str {
    match strategy {
        MatchStrategy::Exact => "in file",
        MatchStrategy::LineTrimmed => "after trimming trailing whitespace",
        MatchStrategy::WhitespaceNormalized => "after whitespace normalization",
        MatchStrategy::IndentationFlexible => "after stripping indentation",
        MatchStrategy::EscapeNormalized => "after escape normalization",
        MatchStrategy::TrimmedBoundary => "after trimming boundary lines",
        MatchStrategy::UnicodeNormalized => "after Unicode normalization",
        MatchStrategy::BlockAnchor => "with block anchor matching",
        MatchStrategy::ContextAware => "with context-aware matching",
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[path = "../fuzzy_tests.rs"]
mod fuzzy_tests;

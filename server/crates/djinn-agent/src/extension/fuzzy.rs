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
        MatchStrategy::BlockAnchor | MatchStrategy::ContextAware => None,
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
    let mut last_byte = end.max(start).saturating_sub(1);
    // Adjust to the nearest preceding char boundary. When the matched range
    // ends immediately after a multi-byte character, `end - 1` may land
    // inside that character. Walking backward at most 3 bytes is safe for
    // line counting because `\n` is always a single-byte ASCII character and
    // no UTF-8 continuation byte equals 0x0A.
    while last_byte > start && !content.is_char_boundary(last_byte) {
        last_byte -= 1;
    }
    let prefix = &content[..last_byte];
    let line_end = prefix.matches('\n').count() + 1;
    LineRange {
        start: line_start,
        end: line_end,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Shared safety guards & helpers
// ════════════════════════════════════════════════════════════════════════════

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

/// Check whether a byte position falls on a line boundary in `content`.
///
/// A position is a line boundary when it is:
/// - at the very start of `content` (byte 0),
/// - immediately after a `\n` byte, or
/// - immediately before a `\n` byte (the match includes it as its last byte),
/// - at the very end of `content`.
fn is_line_boundary(content: &str, pos: usize) -> bool {
    if pos == 0 || pos >= content.len() {
        return true;
    }
    let bytes = content.as_bytes();
    // Just after a line ending.
    if bytes[pos - 1] == b'\n' {
        return true;
    }
    // Just before a line ending (match includes the \n as its last byte).
    if bytes[pos] == b'\n' {
        return true;
    }
    false
}

/// Reject a candidate whose byte range splits a CRLF pair.
///
/// When the original content uses `\r\n` line endings and a normalization
/// strategy maps positions back to the original without accounting for the
/// extra `\r` bytes, the resulting range boundaries may fall inside a `\r\n`
/// pair. Replacing such a range would silently convert CRLF to LF or
/// corrupt the file's line-ending style.
fn guard_crlf_preservation(content: &str, start: usize, end: usize) -> Result<(), &'static str> {
    let bytes = content.as_bytes();
    // Check if start falls between \r and \n of a CRLF pair.
    if start > 0 && start < bytes.len() && bytes[start - 1] == b'\r' && bytes[start] == b'\n' {
        return Err("candidate splits a CRLF line ending at match start");
    }
    // Check if end falls between \r and \n of a CRLF pair.
    if end > 0 && end < bytes.len() && bytes[end - 1] == b'\r' && bytes[end] == b'\n' {
        return Err("candidate splits a CRLF line ending at match end");
    }
    Ok(())
}

/// Reject a candidate whose byte range would replace only part of a line
/// for multi-line matches.
///
/// For a multi-line match (one containing `\n`), the start position must be
/// at a line boundary (start of file or just after `\n`) and the end position
/// must be at a line boundary (end of file or just after `\n`). Single-line
/// matches are exempt — partial-line matches on a single line are safe
/// (e.g. trailing-whitespace trimming).
fn guard_line_boundary(
    content: &str,
    start: usize,
    end: usize,
    matched_text: &str,
) -> Result<(), &'static str> {
    if !matched_text.contains('\n') {
        return Ok(());
    }
    if !is_line_boundary(content, start) {
        return Err("multi-line candidate start is not at a line boundary");
    }
    if !is_line_boundary(content, end) {
        return Err("multi-line candidate end is not at a line boundary");
    }
    Ok(())
}

/// Reject a candidate whose byte range is not valid UTF-8.
///
/// Normalization strategies map byte positions from a normalized string back
/// to the original content. When the original contains multi-byte UTF-8
/// characters, a naive mapping may land inside a multi-byte sequence,
/// producing a byte range that would corrupt the replacement splice.
fn guard_utf8_boundary(content: &str, start: usize, end: usize) -> Result<(), &'static str> {
    if !content.is_char_boundary(start) {
        return Err("candidate start splits a multi-byte UTF-8 character");
    }
    if !content.is_char_boundary(end) {
        return Err("candidate end splits a multi-byte UTF-8 character");
    }
    Ok(())
}

/// Compute a nearest-miss score (0.0..=1.0) for a no-match outcome.
///
/// The score is the length of the longest normalized substring of `old_text`
/// that also appears in `content`, divided by the length of normalized
/// `old_text`. Uses whitespace normalization so the score is comparable
/// across strategies.
fn nearest_miss_score(content: &str, old_text: &str) -> f64 {
    let (norm_content, _) = normalize_whitespace_with_map(content);
    let (norm_old, _) = normalize_whitespace_with_map(old_text);

    let old_len = norm_old.len();
    if old_len == 0 {
        return 0.0;
    }

    let old_bytes = norm_old.as_bytes();
    let content_bytes = norm_content.as_bytes();

    // Try substrings from longest to shortest.
    for len in (1..=old_len).rev() {
        for start in 0..=old_len - len {
            let candidate = &old_bytes[start..start + len];
            if content_bytes.windows(len).any(|window| window == candidate) {
                return len as f64 / old_len as f64;
            }
        }
    }
    0.0
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

/// Return `Some(reason)` if `condition` is true, else `None`.
fn guard_reason_if(condition: bool, reason: &'static str) -> Option<&'static str> {
    if condition { Some(reason) } else { None }
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

// ── Unicode normalization helpers (NFKC + confusables) ────────────────────

/// NFKD + confusable normalization with byte-level position map.
/// Conceptual design only — no third-party source is vendored or copied.
fn normalize_with_confusables(s: &str) -> (String, Vec<usize>) {
    // Characters that NFKD does NOT decompose but are visually confusable
    // with common ASCII punctuation.
    let confusable: fn(char) -> Option<char> = |c| match c {
        // Smart / curly ASCII quotes → straight equivalents
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => Some('\''), // ' ' ' '
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => Some('"'),  // " " " "
        // Typographic dashes → ASCII minus
        '\u{2013}' => Some('-'), // en dash
        '\u{2014}' => Some('-'), // em dash
        '\u{2012}' => Some('-'), // figure dash
        '\u{2212}' => Some('-'), // minus sign
        // Ellipsis
        '\u{2026}' => Some('.'),
        // Non-breaking space variants → regular space
        '\u{00A0}' | '\u{2007}' | '\u{202F}' | '\u{2060}' => Some(' '),
        _ => None,
    };

    let mut map: Vec<usize> = Vec::with_capacity(s.len());
    let mut norm = String::with_capacity(s.len());
    let mut orig_pos: usize = 0;

    for orig_ch in s.chars() {
        // Step 1: apply explicit confusable substitution (char → char).
        let mapped = confusable(orig_ch).unwrap_or(orig_ch);
        // Step 2: apply NFKD compatibility decomposition.
        // `decompose_compatible` yields 0..N chars that are the NFKD
        // decomposition of `mapped` (handles ligatures, fullwidth letters,
        // superscripts/subscripts, precomposed → decomposed, etc.).
        use unicode_normalization::char::decompose_compatible;
        decompose_compatible(mapped, |decomp_ch| {
            norm.push(decomp_ch);
            // Push one map entry per *byte* of the decomposed char so
            // byte-indexed lookups work for multi-byte normalised chars.
            for _ in 0..decomp_ch.len_utf8() {
                map.push(orig_pos);
            }
        });
        orig_pos += orig_ch.len_utf8();
    }
    // Sentinel: map[len] == orig.len() for boundary indexing.
    map.push(orig_pos);
    (norm, map)
}

/// Returns `true` when `b` is the first byte of a Unicode code-point whose
/// canonical combining class is non-zero (common combining marks).
///
/// Covers U+0300..U+036F (Combining Diacritical Marks) and U+FE20..U+FE2F
/// (Combining Half Marks). Ranges checked via the UTF-8 byte pattern of the
/// leading byte.
#[inline]
fn is_combining_start_byte(b: u8) -> bool {
    // 0xCC/0xCD lead code-points U+0300..U+037F (Combining Diacritical Marks).
    // 0xEF leads U+FE00..U+FE2F (Variation Selectors & Combining Half Marks).
    matches!(b, 0xCC | 0xCD)
}

/// Walk backward from byte position `pos` (which must be a `char` boundary)
/// in `content` so that the returned position is the first byte of the
/// *grapheme cluster* containing `pos`. If `pos` is already a grapheme
/// boundary, returns `pos` unchanged.
fn adjusted_grapheme_start(content: &str, pos: usize) -> usize {
    let mut p = pos;
    loop {
        if p == 0 || !content.is_char_boundary(p) {
            return p;
        }
        let b = content.as_bytes()[p];
        if !is_combining_start_byte(b) {
            return p;
        }
        // Current position is a combining mark — walk backward past it.
        let ch_start = p;
        // Find start of the previous char.
        let mut prev = ch_start;
        while prev > 0 {
            prev -= 1;
            if content.is_char_boundary(prev) {
                break;
            }
        }
        p = prev;
    }
}

/// Walk forward from byte position `pos` (which must be a `char` boundary)
/// in `content` so that the returned position is the first byte *after* the
/// grapheme cluster containing `pos`. If `pos` is already a grapheme
/// boundary, returns `pos` unchanged.
fn adjusted_grapheme_end(content: &str, pos: usize) -> usize {
    let mut p = pos;
    loop {
        if p >= content.len() || !content.is_char_boundary(p) {
            return p;
        }
        let b = content.as_bytes()[p];
        if !is_combining_start_byte(b) {
            return p;
        }
        // Current position is a combining mark — walk forward past it.
        p += 1;
        while p < content.len() && !content.is_char_boundary(p) {
            p += 1;
        }
    }
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
        MatchStrategy::UnicodeNormalized => {
            Some("(matched with Unicode normalization)".to_string())
        }
        MatchStrategy::BlockAnchor | MatchStrategy::ContextAware => None,
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
        MatchStrategy::UnicodeNormalized => "after Unicode normalization",
        MatchStrategy::BlockAnchor | MatchStrategy::ContextAware => "with future strategy",
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
#[path = "fuzzy_tests.rs"]
mod fuzzy_tests;

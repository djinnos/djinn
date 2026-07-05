//! Pure helper functions for the fuzzy matcher: normalization, safety guards,
//! line-range computation, grapheme helpers, and reindentation.
//!
//! These functions are stateless building blocks consumed by the strategy
//! implementations in the parent module.

use super::LineRange;

// ════════════════════════════════════════════════════════════════════════════
// Line/byte range helpers
// ════════════════════════════════════════════════════════════════════════════

/// Compute the 1-based inclusive line range for a half-open byte range
/// `[start, end)` in `content`. Line numbers count `\n` separators. If `end`
/// lands immediately after a newline (the byte at `end-1` is `\n`), that
/// trailing newline does not open a new matched line — the last matched line
/// is the one the newline terminates.
pub(super) fn line_range_for(content: &str, start: usize, end: usize) -> LineRange {
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
// Safety guards
// ════════════════════════════════════════════════════════════════════════════

/// Check whether a byte position falls on a line boundary in `content`.
///
/// A position is a line boundary when it is:
/// - at the very start of `content` (byte 0),
/// - immediately after a `\n` byte, or
/// - immediately before a `\n` byte (the match includes it as its last byte),
/// - at the very end of `content`.
pub(super) fn is_line_boundary(content: &str, pos: usize) -> bool {
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
pub(super) fn guard_crlf_preservation(
    content: &str,
    start: usize,
    end: usize,
) -> Result<(), &'static str> {
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
pub(super) fn guard_line_boundary(
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
pub(super) fn guard_utf8_boundary(
    content: &str,
    start: usize,
    end: usize,
) -> Result<(), &'static str> {
    if !content.is_char_boundary(start) {
        return Err("candidate start splits a multi-byte UTF-8 character");
    }
    if !content.is_char_boundary(end) {
        return Err("candidate end splits a multi-byte UTF-8 character");
    }
    Ok(())
}

/// Return `Some(reason)` if `condition` is true, else `None`.
pub(super) fn guard_reason_if(condition: bool, reason: &'static str) -> Option<&'static str> {
    if condition { Some(reason) } else { None }
}

// ════════════════════════════════════════════════════════════════════════════
// Normalization helpers
// ════════════════════════════════════════════════════════════════════════════

/// Normalize whitespace: collapse runs of spaces/tabs to a single space.
/// Returns (normalized_string, map from normalized byte index to original byte
/// index).
pub(super) fn normalize_whitespace_with_map(s: &str) -> (String, Vec<usize>) {
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

/// Normalize string-literal escaping: treat backslash-escaped quotes and
/// backslash-escaped backslashes as equivalent to their literal counterparts
/// for matching purposes only. The original content's byte range is preserved
/// for the replacement, so no actual escaping is changed in the output.
///
/// The escape map maps each byte in the normalized string back to the original
/// byte index.
pub(super) fn normalize_escapes_with_map(s: &str) -> (String, Vec<usize>) {
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

/// NFKD + confusable normalization with byte-level position map.
/// Conceptual design only — no third-party source is vendored or copied.
pub(super) fn normalize_with_confusables(s: &str) -> (String, Vec<usize>) {
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

/// Collapse all runs of whitespace (spaces and tabs) in `s` to a single space,
/// without any normalization map. Used for lightweight context comparison in
/// the context-aware strategy.
pub(super) fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

// ════════════════════════════════════════════════════════════════════════════
// Unicode/grapheme helpers
// ════════════════════════════════════════════════════════════════════════════

/// Returns `true` when `b` is the first byte of a Unicode code-point whose
/// canonical combining class is non-zero (common combining marks).
///
/// Covers U+0300..U+036F (Combining Diacritical Marks) and U+FE20..U+FE2F
/// (Combining Half Marks). Ranges checked via the UTF-8 byte pattern of the
/// leading byte.
#[inline]
pub(super) fn is_combining_start_byte(b: u8) -> bool {
    // 0xCC/0xCD lead code-points U+0300..U+037F (Combining Diacritical Marks).
    // 0xEF leads U+FE00..U+FE2F (Variation Selectors & Combining Half Marks).
    matches!(b, 0xCC | 0xCD)
}

/// Walk backward from byte position `pos` (which must be a `char` boundary)
/// in `content` so that the returned position is the first byte of the
/// *grapheme cluster* containing `pos`. If `pos` is already a grapheme
/// boundary, returns `pos` unchanged.
pub(super) fn adjusted_grapheme_start(content: &str, pos: usize) -> usize {
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
pub(super) fn adjusted_grapheme_end(content: &str, pos: usize) -> usize {
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

// ════════════════════════════════════════════════════════════════════════════
// Candidate analysis
// ════════════════════════════════════════════════════════════════════════════

/// Returns true if `candidate` contains an unescaped single or double quote.
/// Such a candidate would cross or corrupt a string-literal boundary, so the
/// escape-normalized strategy rejects it.
pub(super) fn candidate_crosses_quote_boundary(candidate: &str) -> bool {
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
pub(super) fn position_inside_escape_sequence(content: &str, pos: usize) -> bool {
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

// ════════════════════════════════════════════════════════════════════════════
// String helpers
// ════════════════════════════════════════════════════════════════════════════

/// Remove leading/trailing lines that are empty or contain only whitespace.
pub(super) fn trim_boundary_lines(s: &str) -> String {
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

/// Return the first non-empty (after trimming) line of `s`.
pub(super) fn first_non_empty_line(s: &str) -> &str {
    for line in s.split('\n') {
        if !line.trim().is_empty() {
            return line;
        }
    }
    ""
}

/// Return the last non-empty (after trimming) line of `s`.
pub(super) fn last_non_empty_line(s: &str) -> &str {
    for line in s.split('\n').rev() {
        if !line.trim().is_empty() {
            return line;
        }
    }
    ""
}

// ════════════════════════════════════════════════════════════════════════════
// Nearest-miss scoring
// ════════════════════════════════════════════════════════════════════════════

/// Compute a nearest-miss score (0.0..=1.0) for a no-match outcome.
///
/// The score is the length of the longest normalized substring of `old_text`
/// that also appears in `content`, divided by the length of normalized
/// `old_text`. Uses whitespace normalization so the score is comparable
/// across strategies.
pub(super) fn nearest_miss_score(content: &str, old_text: &str) -> f64 {
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
// Reindentation
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

pub(super) fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Map byte positions from a trimmed version back to the original content.
pub(super) fn map_trimmed_to_original(
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

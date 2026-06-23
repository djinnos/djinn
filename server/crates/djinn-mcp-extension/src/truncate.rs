//! Smart truncation utilities that preserve both the head and tail of output.
//!
//! When shell commands, tool results, or verification feedback exceed size limits,
//! naive head-only truncation loses errors and conclusions (which appear at the end).
//! These functions use a 60/40 head+tail split inspired by context-mode, preserving
//! both the initial context and the final results/errors.

/// Find the largest byte index <= `idx` that is a valid UTF-8 char boundary.
pub(crate) fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smart-truncate text to `max_bytes` using a 60% head + 40% tail split.
///
/// Preserves both the beginning (context, setup) and end (errors, results) of output.
/// Line-aware: splits happen at line boundaries when possible.
/// Returns the original string unchanged if it fits within the budget.
pub(crate) fn smart_truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let total_bytes = text.len();
    let head_budget = (max_bytes * 6) / 10;
    let tail_budget = max_bytes - head_budget;

    // Find a line boundary near the head budget.
    let head_end = floor_char_boundary(text, head_budget);
    // Walk back to the last newline in the head.
    let head_cut = text[..head_end]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(head_end);

    // Find a line boundary near the tail.
    let tail_start_raw = total_bytes - tail_budget;
    let tail_start = floor_char_boundary(text, tail_start_raw);
    // Walk forward to the next newline.
    let tail_cut = text[tail_start..]
        .find('\n')
        .map(|p| tail_start + p + 1)
        .unwrap_or(tail_start);

    if tail_cut <= head_cut {
        // Degenerate: just hard-cut.
        let hard_end = floor_char_boundary(text, max_bytes.saturating_sub(20));
        return format!("{}... (truncated)", &text[..hard_end]);
    }

    format!(
        "{}\n... ({} bytes truncated) ...\n{}",
        &text[..head_cut],
        total_bytes - (head_cut + (total_bytes - tail_cut)),
        &text[tail_cut..]
    )
}

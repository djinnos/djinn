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

    // Reserve some bytes for the separator line
    let separator_reserve = 80; // plenty for "... [N bytes omitted] ..."
    let usable = max_bytes.saturating_sub(separator_reserve);
    if usable == 0 {
        return format!("[truncated — {total_bytes} bytes total]");
    }

    let head_budget = (usable * 60) / 100;
    let tail_budget = usable - head_budget;

    // Collect head lines within budget
    let mut head_end = 0;
    for line in text.split_inclusive('\n') {
        if head_end + line.len() > head_budget && head_end > 0 {
            break;
        }
        head_end += line.len();
    }
    // Snap to char boundary
    head_end = floor_char_boundary(text, head_end);

    // Collect tail lines within budget (scan backwards).
    // We walk backwards by finding newlines from the end.
    let mut tail_start = text.len();
    let mut tail_used = 0usize;
    let bytes = text.as_bytes();
    let mut pos = text.len();
    loop {
        // Find the previous newline (or start of string)
        let line_start = if pos == 0 {
            0
        } else {
            match bytes[..pos].iter().rposition(|&b| b == b'\n') {
                Some(nl) => nl + 1,
                None => 0,
            }
        };
        let line_len = pos - line_start;
        if tail_used + line_len > tail_budget && tail_used > 0 {
            break;
        }
        tail_used += line_len;
        tail_start = line_start;
        if line_start == 0 {
            break;
        }
        // Move pos to before the newline
        pos = line_start - 1;
    }
    // Ensure we don't start mid-char
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    // Ensure no overlap
    if head_end >= tail_start {
        // Overlap means the split covers everything — just do a hard head truncation
        let end = floor_char_boundary(text, max_bytes.saturating_sub(separator_reserve));
        return format!(
            "{}\n\n[truncated — {total_bytes} bytes total]",
            &text[..end]
        );
    }

    let omitted = tail_start - head_end;
    format!(
        "{}\n\n... [{omitted} bytes omitted — {total_bytes} bytes total] ...\n\n{}",
        text[..head_end].trim_end(),
        text[tail_start..].trim_start()
    )
}

/// Smart-truncate with a line count limit as well as byte limit.
/// Used for shell output where both dimensions matter.
pub(crate) fn smart_truncate_lines(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let line_count = text.split('\n').count();
    if text.len() <= max_bytes && line_count <= max_lines {
        return text.to_string();
    }

    // If line count is the binding constraint, pre-trim by lines first
    if line_count > max_lines {
        let lines: Vec<&str> = text.split('\n').collect();
        let head_lines = (max_lines * 60) / 100;
        let tail_lines = max_lines - head_lines;

        let head = lines[..head_lines].join("\n");
        let tail_start = lines.len() - tail_lines;
        let tail = lines[tail_start..].join("\n");

        let omitted_lines = tail_start - head_lines;
        let reassembled = format!(
            "{head}\n\n... [{omitted_lines} lines omitted — {line_count} lines total] ...\n\n{tail}"
        );

        // Now also apply byte limit if needed
        if reassembled.len() > max_bytes {
            return smart_truncate(&reassembled, max_bytes);
        }
        return reassembled;
    }

    // Only byte limit applies
    smart_truncate(text, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_input_returned_unchanged() {
        let s = "hello world";
        assert_eq!(smart_truncate(s, 1000), s);
    }

    #[test]
    fn preserves_head_and_tail() {
        let mut lines = Vec::new();
        lines.push("=== TEST START ===");
        for i in 0..100 {
            lines.push(leaked(format!("test line {i} ... ok")));
        }
        lines.push("FAILURES:");
        lines.push("test_foo: assertion failed");
        lines.push("test_bar: panicked at 'not yet implemented'");
        let text = lines.join("\n");

        let truncated = smart_truncate(&text, 500);

        // Head preserved
        assert!(truncated.contains("=== TEST START ==="));
        // Tail preserved — this is the critical part
        assert!(truncated.contains("FAILURES:"));
        assert!(truncated.contains("assertion failed"));
        assert!(truncated.contains("panicked at"));
        // Omission marker present
        assert!(truncated.contains("bytes omitted"));
    }

    #[test]
    fn line_count_limit_preserves_both_ends() {
        let mut lines: Vec<String> = Vec::new();
        lines.push("HEADER".to_string());
        for i in 0..200 {
            lines.push(format!("middle line {i}"));
        }
        lines.push("FOOTER: the important error".to_string());
        let text = lines.join("\n");

        let truncated = smart_truncate_lines(&text, 100_000, 50);

        assert!(truncated.contains("HEADER"));
        assert!(truncated.contains("FOOTER: the important error"));
        assert!(truncated.contains("lines omitted"));
    }

    #[test]
    fn floor_char_boundary_multibyte() {
        let s = "a\u{2500}b"; // ─ is 3 bytes
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 2), 1); // inside ─
        assert_eq!(floor_char_boundary(s, 3), 1); // still inside ─
        assert_eq!(floor_char_boundary(s, 4), 4); // start of b
    }

    #[test]
    fn floor_char_boundary_beyond_len() {
        assert_eq!(floor_char_boundary("hi", 100), 2);
    }

    #[test]
    fn handles_empty_input() {
        assert_eq!(smart_truncate("", 100), "");
        assert_eq!(smart_truncate_lines("", 100, 50), "");
    }

    #[test]
    fn both_limits_applied() {
        // Many short lines + large byte count
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let truncated = smart_truncate_lines(&text, 500, 50);

        // Should mention omitted lines
        assert!(truncated.contains("omitted"));
        // And fit within byte budget
        assert!(truncated.len() <= 600); // some slack for separator
    }

    fn leaked(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }
}

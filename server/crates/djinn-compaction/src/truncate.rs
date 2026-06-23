//! Minimal truncation helper for capping compaction summaries.
//!
//! This is a private, reduced version of the `smart_truncate` utility in
//! `djinn-agent::truncate`.  It preserves both the head and tail of text when
//! truncating, using a 60/40 split.

/// Find the largest byte index ≤ `idx` that is a valid UTF-8 char boundary.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
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
/// Preserves both the beginning (context, setup) and end (errors, results) of
/// output.  Line-aware: splits happen at line boundaries when possible.
/// Returns the original string unchanged if it fits within the budget.
pub fn smart_truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let total_bytes = text.len();

    // Reserve some bytes for the separator line
    let separator_reserve = 80;
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
    head_end = floor_char_boundary(text, head_end);

    // Collect tail lines within budget (scan backwards).
    let mut tail_start = text.len();
    let mut tail_used = 0usize;
    let bytes = text.as_bytes();
    let mut pos = text.len();
    loop {
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
        pos = line_start - 1;
    }
    // Ensure we don't start mid-char
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    // Ensure no overlap
    if head_end >= tail_start {
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
    fn handles_empty_input() {
        assert_eq!(smart_truncate("", 100), "");
    }

    fn leaked(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }
}

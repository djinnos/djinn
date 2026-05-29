//! Session-scoped stash for full tool outputs that exceed the truncation limit.
//!
//! Before `smart_truncate` discards the middle of a large tool result, the full
//! text is stashed here so the agent can paginate (`output_view`) or search
//! (`output_grep`) it later without re-running the command.
//!
//! Bounded: max 10 entries, max 5 MB total. FIFO eviction when either limit is
//! hit. Each reply-loop instance owns its own stash — no cross-session sharing.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Maximum number of stashed entries.
const MAX_ENTRIES: usize = 10;
/// Maximum total bytes across all entries.
const MAX_TOTAL_BYTES: usize = 5 * 1024 * 1024; // 5 MB

/// Maximum characters per tool result handed to the model. Results larger than
/// this are stashed in full and replaced inline with a `smart_truncate`d view
/// plus an `output_view`/`output_grep` navigation hint.
///
/// ~30k chars ≈ 7.5k tokens — enough for diagnosis, safe with multiple calls,
/// and well under the provider's per-string limit. Shared by every surface
/// that feeds tool results back into a conversation (the worker reply loop and
/// the chat loop) so the two can never drift — the chat path lacking this clamp
/// is what let a ~12 MB tool result reach the provider and 400 the request.
pub const MAX_TOOL_RESULT_CHARS: usize = 30_000;

struct StashedOutput {
    tool_use_id: String,
    tool_name: String,
    full_text: String,
}

pub struct OutputStash {
    entries: VecDeque<StashedOutput>,
    total_bytes: usize,
}

impl OutputStash {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
        }
    }

    /// Stash full tool output. Evicts oldest entries if count or byte limits are exceeded.
    pub fn insert(&mut self, tool_use_id: String, tool_name: String, full_text: String) {
        let new_bytes = full_text.len();

        // Evict until we have room for the new entry (both count and bytes).
        while self.entries.len() >= MAX_ENTRIES
            || (self.total_bytes + new_bytes > MAX_TOTAL_BYTES && !self.entries.is_empty())
        {
            if let Some(evicted) = self.entries.pop_front() {
                self.total_bytes -= evicted.full_text.len();
            }
        }

        self.total_bytes += new_bytes;
        self.entries.push_back(StashedOutput {
            tool_use_id,
            tool_name,
            full_text,
        });
    }

    /// Paginated line view of a stashed output.
    pub fn view(
        &self,
        tool_use_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<String, String> {
        let entry = self.find(tool_use_id)?;
        let lines: Vec<&str> = entry.full_text.lines().collect();
        let total_lines = lines.len();

        if offset >= total_lines {
            return Ok(format!(
                "[offset {offset} is past end of output ({total_lines} lines)]"
            ));
        }

        let end = (offset + limit).min(total_lines);
        let mut result = String::new();

        // Line-number width for alignment.
        let width = end.to_string().len();
        for (i, line) in lines[offset..end].iter().enumerate() {
            let line_num = offset + i + 1; // 1-based
            result.push_str(&format!("{line_num:>width$}  {line}\n"));
        }

        // Navigation hint.
        if end < total_lines {
            result.push_str(&format!(
                "\n[Showing lines {}-{} of {total_lines}. Use output_view(tool_use_id=\"{}\", offset={end}) to see more.]",
                offset + 1,
                end,
                tool_use_id,
            ));
        } else {
            result.push_str(&format!("\n[End of output ({total_lines} lines)]"));
        }

        Ok(result)
    }

    /// Regex search within a stashed output, returning matching lines with context.
    pub fn grep(
        &self,
        tool_use_id: &str,
        pattern: &str,
        context_lines: usize,
    ) -> Result<String, String> {
        let entry = self.find(tool_use_id)?;
        let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;

        let lines: Vec<&str> = entry.full_text.lines().collect();
        let total_lines = lines.len();

        // Collect matching line indices.
        let matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| re.is_match(line))
            .map(|(i, _)| i)
            .collect();

        if matches.is_empty() {
            return Ok(format!(
                "[No matches for pattern \"{pattern}\" in output from {} ({total_lines} lines)]",
                entry.tool_name
            ));
        }

        // Build context ranges (merge overlapping).
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &m in &matches {
            let start = m.saturating_sub(context_lines);
            let end = (m + context_lines + 1).min(total_lines);
            if let Some(last) = ranges.last_mut()
                && start <= last.1
            {
                last.1 = end;
                continue;
            }
            ranges.push((start, end));
        }

        // Cap output at 30KB to avoid recursive truncation.
        const MAX_GREP_BYTES: usize = 30_000;
        let mut result = String::new();
        let width = total_lines.to_string().len();
        let mut capped = false;

        for (ri, &(start, end)) in ranges.iter().enumerate() {
            if ri > 0 {
                result.push_str("  ...\n");
            }
            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                let marker = if matches.contains(&i) { ">" } else { " " };
                let formatted = format!("{}{:>width$}  {}\n", marker, i + 1, line);
                if result.len() + formatted.len() > MAX_GREP_BYTES {
                    capped = true;
                    break;
                }
                result.push_str(&formatted);
            }
            if capped {
                break;
            }
        }

        let match_count = matches.len();
        if capped {
            result.push_str(&format!(
                "\n[Output capped at 30KB. {match_count} total matches for \"{pattern}\". \
                 Use output_view to paginate the full output.]"
            ));
        } else {
            result.push_str(&format!(
                "\n[{match_count} match{} for \"{pattern}\" in {total_lines} lines]",
                if match_count == 1 { "" } else { "es" }
            ));
        }

        Ok(result)
    }

    /// Clear all stashed outputs (called after compaction).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    fn find(&self, tool_use_id: &str) -> Result<&StashedOutput, String> {
        self.entries
            .iter()
            .find(|e| e.tool_use_id == tool_use_id)
            .ok_or_else(|| {
                format!(
                    "No stashed output for tool_use_id \"{tool_use_id}\". \
                     Stashed outputs are cleared after context compaction and \
                     only exist for results that were truncated."
                )
            })
    }
}

impl Default for OutputStash {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the browsable text to stash for a tool result.
///
/// For `shell` the model wants to page through raw stdout/stderr — not the
/// `{"ok":true,"stdout":"…"}` JSON envelope — so we splice those fields into a
/// log-like view. For every other tool we return `None` and the caller stashes
/// the pretty-printed JSON as-is.
pub fn extract_stash_content(tool_name: &str, value: &serde_json::Value) -> Option<String> {
    if tool_name != "shell" {
        return None;
    }
    let obj = value.as_object()?;
    let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let exit_code = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);

    let mut out = String::with_capacity(stdout.len() + stderr.len() + 64);
    if !stdout.is_empty() {
        out.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(stderr);
    }
    if exit_code != 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[exit code: {exit_code}]"));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Render a successful tool result to the text handed back to the model.
///
/// Serializes `value` to text (raw string, or pretty JSON), and when that
/// exceeds [`MAX_TOOL_RESULT_CHARS`] stashes the full output under
/// `tool_use_id` and returns a `smart_truncate`d view with an
/// `output_view`/`output_grep` navigation hint appended.
///
/// This is the single chokepoint both the worker reply loop and the chat loop
/// route successful tool results through, so neither can ever ship an
/// unbounded result into a conversation again.
pub fn render_tool_result(
    stash: &Mutex<OutputStash>,
    tool_use_id: &str,
    tool_name: &str,
    value: &serde_json::Value,
) -> String {
    let mut text = if value.is_string() {
        value.as_str().unwrap_or("").to_string()
    } else {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    };
    if text.len() > MAX_TOOL_RESULT_CHARS {
        let stash_text = extract_stash_content(tool_name, value).unwrap_or_else(|| text.clone());
        stash.lock().unwrap().insert(
            tool_use_id.to_string(),
            tool_name.to_string(),
            stash_text,
        );
        let full_bytes = text.len();
        text = crate::truncate::smart_truncate(&text, MAX_TOOL_RESULT_CHARS);
        text.push_str(&format!(
            "\n\n[Full output stashed ({full_bytes} bytes). Use output_view(tool_use_id=\"{tool_use_id}\") to paginate or output_grep(tool_use_id=\"{tool_use_id}\", pattern=\"...\") to search.]"
        ));
    }
    text
}

/// `true` for the two stash-navigation tools handled in-process against the
/// [`OutputStash`] rather than dispatched to a real handler.
pub fn is_stash_tool(name: &str) -> bool {
    name == "output_view" || name == "output_grep"
}

/// Service an `output_view` / `output_grep` call against the stash.
///
/// Returns `Ok(text)` for a successful view/grep (including the "no match" and
/// "offset past end" informational cases) or `Err(message)` for an unknown
/// `tool_use_id`, an invalid regex, or a name that isn't a stash tool.
pub fn handle_stash_tool(
    stash: &Mutex<OutputStash>,
    name: &str,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<String, String> {
    let guard = stash.lock().unwrap();
    let tid = args
        .and_then(|m| m.get("tool_use_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match name {
        "output_view" => {
            let offset = args
                .and_then(|m| m.get("offset"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let limit = args
                .and_then(|m| m.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            guard.view(tid, offset, limit)
        }
        "output_grep" => {
            let pattern = args
                .and_then(|m| m.get("pattern"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ctx_lines = args
                .and_then(|m| m.get("context_lines"))
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as usize;
            guard.grep(tid, pattern, ctx_lines)
        }
        other => Err(format!("not a stash tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_view_round_trip() {
        let mut stash = OutputStash::new();
        stash.insert(
            "t1".into(),
            "shell".into(),
            "line one\nline two\nline three\n".into(),
        );
        let result = stash.view("t1", 0, 200).unwrap();
        assert!(result.contains("line one"));
        assert!(result.contains("line three"));
        assert!(result.contains("End of output"));
    }

    #[test]
    fn pagination() {
        let mut stash = OutputStash::new();
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        stash.insert("t1".into(), "shell".into(), text);

        let page1 = stash.view("t1", 0, 10).unwrap();
        assert!(page1.contains("line 0"));
        assert!(page1.contains("line 9"));
        assert!(!page1.contains("line 10"));
        assert!(page1.contains("offset=10"));

        let page2 = stash.view("t1", 10, 10).unwrap();
        assert!(page2.contains("line 10"));
        assert!(page2.contains("line 19"));
    }

    #[test]
    fn view_offset_past_end() {
        let mut stash = OutputStash::new();
        stash.insert("t1".into(), "shell".into(), "one\ntwo\n".into());
        let result = stash.view("t1", 999, 10).unwrap();
        assert!(result.contains("past end"));
    }

    #[test]
    fn grep_with_context() {
        let mut stash = OutputStash::new();
        let text = "aaa\nbbb\nccc\nERROR: bad\nddd\neee\nfff\n";
        stash.insert("t1".into(), "shell".into(), text.into());

        let result = stash.grep("t1", "ERROR", 1).unwrap();
        assert!(result.contains(">"));
        assert!(result.contains("ERROR: bad"));
        assert!(result.contains("ccc")); // context before
        assert!(result.contains("ddd")); // context after
        assert!(result.contains("1 match"));
    }

    #[test]
    fn grep_no_matches() {
        let mut stash = OutputStash::new();
        stash.insert("t1".into(), "shell".into(), "hello\nworld\n".into());
        let result = stash.grep("t1", "NONEXISTENT", 2).unwrap();
        assert!(result.contains("No matches"));
    }

    #[test]
    fn grep_invalid_regex() {
        let mut stash = OutputStash::new();
        stash.insert("t1".into(), "shell".into(), "hello\n".into());
        let result = stash.grep("t1", "[invalid", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid regex"));
    }

    #[test]
    fn eviction_by_count() {
        let mut stash = OutputStash::new();
        for i in 0..12 {
            stash.insert(format!("t{i}"), "shell".into(), format!("output {i}"));
        }
        // Oldest should be evicted; only last 10 remain.
        assert!(stash.find("t0").is_err());
        assert!(stash.find("t1").is_err());
        assert!(stash.find("t2").is_ok());
        assert!(stash.find("t11").is_ok());
        assert_eq!(stash.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn eviction_by_bytes() {
        let mut stash = OutputStash::new();
        // Each entry is ~1MB. After 5, inserting a 6th should evict.
        let big = "x".repeat(1_024 * 1_024);
        for i in 0..6 {
            stash.insert(format!("t{i}"), "shell".into(), big.clone());
        }
        assert!(stash.total_bytes <= MAX_TOTAL_BYTES);
        // At least the first one should be evicted.
        assert!(stash.find("t0").is_err());
        assert!(stash.find("t5").is_ok());
    }

    #[test]
    fn clear_empties_everything() {
        let mut stash = OutputStash::new();
        stash.insert("t1".into(), "shell".into(), "data".into());
        stash.clear();
        assert!(stash.find("t1").is_err());
        assert_eq!(stash.total_bytes, 0);
        assert!(stash.entries.is_empty());
    }

    #[test]
    fn unknown_id_error() {
        let stash = OutputStash::new();
        assert!(stash.view("nonexistent", 0, 10).is_err());
        assert!(stash.grep("nonexistent", "foo", 0).is_err());
    }

    #[test]
    fn grep_output_capping() {
        let mut stash = OutputStash::new();
        // Create output where every line matches — should cap at 30KB.
        let text: String = (0..10_000).map(|i| format!("MATCH line {i}\n")).collect();
        stash.insert("t1".into(), "shell".into(), text);

        let result = stash.grep("t1", "MATCH", 0).unwrap();
        assert!(result.len() <= 31_000); // small slack for footer
        assert!(result.contains("capped at 30KB"));
    }

    #[test]
    fn render_small_result_is_passthrough() {
        let stash = Mutex::new(OutputStash::new());
        let value = serde_json::json!({"ok": true, "rows": 3});
        let text = render_tool_result(&stash, "t1", "task_list", &value);
        // Pretty JSON, untruncated, nothing stashed.
        assert!(text.contains("\"rows\""));
        assert!(!text.contains("Full output stashed"));
        assert!(stash.lock().unwrap().view("t1", 0, 10).is_err());
    }

    #[test]
    fn render_oversized_result_truncates_and_stashes() {
        let stash = Mutex::new(OutputStash::new());
        // A string value well over the clamp.
        let big = "x".repeat(MAX_TOOL_RESULT_CHARS * 2);
        let value = serde_json::Value::String(big.clone());
        let text = render_tool_result(&stash, "call-1", "shell", &value);

        // The inline text is clamped and carries the navigation hint…
        assert!(text.len() < big.len());
        assert!(text.contains("Full output stashed"));
        assert!(text.contains("output_view(tool_use_id=\"call-1\")"));
        // …and the full output is retrievable from the stash.
        let viewed = handle_stash_tool(
            &stash,
            "output_view",
            Some(&serde_json::json!({"tool_use_id": "call-1"}).as_object().unwrap().clone()),
        )
        .unwrap();
        assert!(viewed.contains("xxx"));
    }

    #[test]
    fn render_oversized_shell_stashes_raw_stdout() {
        let stash = Mutex::new(OutputStash::new());
        let stdout = "line\n".repeat(MAX_TOOL_RESULT_CHARS); // far over the clamp
        let value = serde_json::json!({
            "ok": true, "exit_code": 0, "stdout": stdout, "stderr": ""
        });
        render_tool_result(&stash, "sh-1", "shell", &value);
        // The stash holds raw stdout (no JSON envelope), via extract_stash_content.
        let grepped =
            handle_stash_tool(&stash, "output_grep", Some(&serde_json::json!({
                "tool_use_id": "sh-1", "pattern": "line"
            }).as_object().unwrap().clone()))
            .unwrap();
        assert!(grepped.contains("line"));
        assert!(!grepped.contains("\"stdout\""));
    }

    #[test]
    fn handle_stash_tool_rejects_unknown_name() {
        let stash = Mutex::new(OutputStash::new());
        assert!(handle_stash_tool(&stash, "shell", None).is_err());
    }
}

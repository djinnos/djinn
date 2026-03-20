/// Parsed output from an agent session.
///
/// After removing markers and nudging (see ADR-022 revision), this struct only
/// tracks runtime errors and reviewer feedback extracted from agent text.
/// Worker completion is determined by session end (agent stops calling tools).
/// Reviewer verdict is determined by acceptance criteria state on the task.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedAgentOutput {
    captures_feedback: bool,
    pub runtime_error: Option<String>,
    pub reviewer_feedback: Option<String>,
    /// Payload from the finalize tool call (e.g. `submit_work`, `submit_review`).
    /// Set when the reply loop exits via finalize-tool detection (ADR-036).
    pub finalize_payload: Option<serde_json::Value>,
    /// Name of the finalize tool that was actually called (e.g. `"submit_work"`,
    /// `"request_lead"`). Set alongside `finalize_payload`.
    pub finalize_tool_name: Option<String>,
}

impl Default for ParsedAgentOutput {
    fn default() -> Self {
        Self::new(false)
    }
}

impl ParsedAgentOutput {
    pub(crate) fn new(captures_feedback: bool) -> Self {
        Self {
            captures_feedback,
            runtime_error: None,
            reviewer_feedback: None,
            finalize_payload: None,
            finalize_tool_name: None,
        }
    }

    pub(crate) fn ingest_text(&mut self, text: &str) {
        let normalized = text.replace("\\r\\n", "\n").replace("\\n", "\n");
        for raw_line in normalized.lines() {
            let line = sanitize_line(raw_line);
            if line.is_empty() {
                continue;
            }

            // Extract reviewer feedback if present (still useful for logging).
            if self.captures_feedback
                && let Some(payload) = marker_payload(&line, "FEEDBACK")
            {
                let feedback = payload.trim();
                if !feedback.is_empty() {
                    self.reviewer_feedback = Some(feedback.to_string());
                }
            }

            if self.runtime_error.is_none()
                && let Some(error) = extract_runtime_error(&line)
            {
                self.runtime_error = Some(error.to_string());
            }
        }
    }
}

fn marker_payload<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let upper = line.to_ascii_uppercase();
    let needle = format!("{marker}:");
    let index = upper.find(&needle)?;
    let start = index + needle.len();
    Some(line[start..].trim())
}

fn sanitize_line(line: &str) -> String {
    line.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == ',')
        .trim()
        .to_string()
}

fn extract_runtime_error(line: &str) -> Option<&str> {
    let marker = "Execution error:";
    let idx = line.find(marker)?;
    let value = line[idx + marker.len()..].trim();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_runtime_error_handles_prefixed_text() {
        assert_eq!(
            extract_runtime_error("Error: Execution error: No such file or directory (os error 2)"),
            Some("No such file or directory (os error 2)")
        );
    }

    #[test]
    fn extract_runtime_error_ignores_empty_payload() {
        assert_eq!(extract_runtime_error("Execution error:   "), None);
    }

    #[test]
    fn extract_runtime_error_ignores_unrelated_lines() {
        assert_eq!(extract_runtime_error("Error: command failed"), None);
    }

    #[test]
    fn marker_payload_matches_case_insensitively() {
        assert_eq!(
            marker_payload("feedback: missing test coverage", "FEEDBACK"),
            Some("missing test coverage")
        );
    }

    #[test]
    fn marker_payload_rejects_partial_marker_names() {
        assert_eq!(
            marker_payload("FEEDBACK_LOOP: hidden marker", "FEEDBACK"),
            None
        );
        assert_eq!(
            marker_payload("PREFIX FEEDBACK_LOOP: hidden marker", "FEEDBACK"),
            None
        );
    }

    #[test]
    fn marker_payload_allows_empty_payload() {
        assert_eq!(marker_payload("FEEDBACK:", "FEEDBACK"), Some(""));
    }

    #[test]
    fn sanitize_line_trims_wrappers_without_removing_interior_text() {
        assert_eq!(
            sanitize_line("\"quoted, marker text\","),
            "quoted, marker text"
        );
        assert_eq!(
            sanitize_line("`quoted, marker text`"),
            "quoted, marker text"
        );
    }

    #[test]
    fn sanitize_line_preserves_interior_markers() {
        assert_eq!(
            sanitize_line("`Execution error: still visible, with comma`"),
            "Execution error: still visible, with comma"
        );
    }

    #[test]
    fn extracts_runtime_execution_errors() {
        let mut out = ParsedAgentOutput::new(false);
        out.ingest_text("Error: Execution error: No such file or directory (os error 2)");
        assert_eq!(
            out.runtime_error.as_deref(),
            Some("No such file or directory (os error 2)")
        );
    }

    #[test]
    fn extracts_reviewer_feedback() {
        let mut out = ParsedAgentOutput::new(true);
        out.ingest_text("FEEDBACK: missing test for malformed payload");
        assert_eq!(
            out.reviewer_feedback.as_deref(),
            Some("missing test for malformed payload")
        );
    }

    #[test]
    fn ignores_feedback_for_non_reviewer() {
        let mut out = ParsedAgentOutput::new(false);
        out.ingest_text("FEEDBACK: something");
        assert_eq!(out.reviewer_feedback, None);
    }

    #[test]
    fn ingest_text_normalizes_literal_newlines_before_marker_extraction() {
        let mut out = ParsedAgentOutput::new(true);
        out.ingest_text(
            "prefix\\nFEEDBACK: missing coverage\\r\\nError: Execution error: disk full",
        );

        assert_eq!(out.reviewer_feedback.as_deref(), Some("missing coverage"));
        assert_eq!(out.runtime_error.as_deref(), Some("disk full"));
    }
}

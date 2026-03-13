use super::AgentType;

/// Parsed output from an agent session.
///
/// After removing markers and nudging (see ADR-022 revision), this struct only
/// tracks runtime errors and reviewer feedback extracted from agent text.
/// Worker completion is determined by session end (agent stops calling tools).
/// Reviewer verdict is determined by acceptance criteria state on the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedAgentOutput {
    agent_type: AgentType,
    pub runtime_error: Option<String>,
    pub reviewer_feedback: Option<String>,
}

impl Default for ParsedAgentOutput {
    fn default() -> Self {
        Self::new(AgentType::Worker)
    }
}

impl ParsedAgentOutput {
    pub(crate) fn new(agent_type: AgentType) -> Self {
        Self {
            agent_type,
            runtime_error: None,
            reviewer_feedback: None,
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
            if matches!(self.agent_type, AgentType::TaskReviewer)
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
    fn extracts_runtime_execution_errors() {
        let mut out = ParsedAgentOutput::new(AgentType::Worker);
        out.ingest_text("Error: Execution error: No such file or directory (os error 2)");
        assert_eq!(
            out.runtime_error.as_deref(),
            Some("No such file or directory (os error 2)")
        );
    }

    #[test]
    fn extracts_reviewer_feedback() {
        let mut out = ParsedAgentOutput::new(AgentType::TaskReviewer);
        out.ingest_text("FEEDBACK: missing test for malformed payload");
        assert_eq!(
            out.reviewer_feedback.as_deref(),
            Some("missing test for malformed payload")
        );
    }

    #[test]
    fn ignores_feedback_for_non_reviewer() {
        let mut out = ParsedAgentOutput::new(AgentType::Worker);
        out.ingest_text("FEEDBACK: something");
        assert_eq!(out.reviewer_feedback, None);
    }
}

use goose::agents::AgentEvent;

use super::AgentType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSignal {
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerVerdict {
    Verified,
    Reopen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpicReviewVerdict {
    Clean,
    IssuesFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAgentOutput {
    agent_type: AgentType,
    pub worker_signal: Option<WorkerSignal>,
    pub worker_reason: Option<String>,
    pub runtime_error: Option<String>,
    pub reviewer_verdict: Option<ReviewerVerdict>,
    pub reviewer_feedback: Option<String>,
    pub epic_verdict: Option<EpicReviewVerdict>,
}

impl Default for ParsedAgentOutput {
    fn default() -> Self {
        Self::new(AgentType::Worker)
    }
}

impl ParsedAgentOutput {
    pub fn new(agent_type: AgentType) -> Self {
        Self {
            agent_type,
            worker_signal: None,
            worker_reason: None,
            runtime_error: None,
            reviewer_verdict: None,
            reviewer_feedback: None,
            epic_verdict: None,
        }
    }

    pub fn ingest_event(&mut self, event: &AgentEvent) {
        self.ingest_text(&format!("{event:?}"));
    }

    pub fn ingest_text(&mut self, text: &str) {
        let normalized = text.replace("\\r\\n", "\n").replace("\\n", "\n");
        for raw_line in normalized.lines() {
            let line = sanitize_line(raw_line);
            if line.is_empty() {
                continue;
            }

            match self.agent_type {
                AgentType::Worker | AgentType::ConflictResolver => {
                    if let Some(payload) = marker_payload(&line, "WORKER_RESULT") {
                        self.parse_worker_signal(payload, &line);
                    } else {
                        self.parse_worker_signal(line.as_str(), &line);
                    }
                }
                AgentType::TaskReviewer => {
                    if let Some(payload) = marker_payload(&line, "REVIEW_RESULT") {
                        self.parse_reviewer_verdict(payload);
                    }
                    if let Some(payload) = marker_payload(&line, "FEEDBACK") {
                        let feedback = payload.trim();
                        if !feedback.is_empty() {
                            self.reviewer_feedback = Some(feedback.to_string());
                        }
                    }
                }
                AgentType::EpicReviewer => {
                    if let Some(payload) = marker_payload(&line, "EPIC_REVIEW_RESULT") {
                        self.parse_epic_verdict(payload);
                    }
                }
            }

            if self.runtime_error.is_none()
                && let Some(error) = extract_runtime_error(&line)
            {
                self.runtime_error = Some(error.to_string());
            }

        }
    }

    fn parse_worker_signal(&mut self, payload: &str, raw_line: &str) {
        let normalized = payload.trim();
        if normalized.is_empty() {
            return;
        }
        let upper = normalized.to_ascii_uppercase();

        if upper.starts_with("DONE") {
            self.worker_signal = Some(WorkerSignal::Done);
            self.worker_reason = None;
            return;
        }

        if raw_line.to_ascii_uppercase().contains("WORKER_RESULT") {
            tracing::warn!(line = %raw_line, "malformed WORKER_RESULT marker");
        }
    }

    fn parse_reviewer_verdict(&mut self, payload: &str) {
        let verdict = payload
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .to_ascii_uppercase();

        self.reviewer_verdict = match verdict.as_str() {
            "VERIFIED" => Some(ReviewerVerdict::Verified),
            "REOPEN" => Some(ReviewerVerdict::Reopen),
            _ => {
                tracing::warn!(value = %payload, "malformed REVIEW_RESULT marker");
                self.reviewer_verdict
            }
        };
    }

    fn parse_epic_verdict(&mut self, payload: &str) {
        let verdict = payload
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .to_ascii_uppercase();

        self.epic_verdict = match verdict.as_str() {
            "CLEAN" => Some(EpicReviewVerdict::Clean),
            "ISSUES_FOUND" => Some(EpicReviewVerdict::IssuesFound),
            _ => {
                tracing::warn!(value = %payload, "malformed EPIC_REVIEW_RESULT marker");
                self.epic_verdict
            }
        };
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
    fn parses_worker_done() {
        let mut out = ParsedAgentOutput::new(AgentType::Worker);
        out.ingest_text("WORKER_RESULT: DONE");
        assert_eq!(out.worker_signal, Some(WorkerSignal::Done));
    }

    #[test]
    fn parses_reviewer_verdict_and_feedback() {
        let mut out = ParsedAgentOutput::new(AgentType::TaskReviewer);
        out.ingest_text("REVIEW_RESULT: REOPEN\nFEEDBACK: missing test for malformed payload");

        assert_eq!(out.reviewer_verdict, Some(ReviewerVerdict::Reopen));
        assert_eq!(
            out.reviewer_feedback.as_deref(),
            Some("missing test for malformed payload")
        );
    }

    #[test]
    fn parses_epic_review_result() {
        let mut out = ParsedAgentOutput::new(AgentType::EpicReviewer);
        out.ingest_text("EPIC_REVIEW_RESULT: CLEAN");
        assert_eq!(out.epic_verdict, Some(EpicReviewVerdict::Clean));
    }

    #[test]
    fn ignores_malformed_markers_without_crashing() {
        let mut reviewer = ParsedAgentOutput::new(AgentType::TaskReviewer);
        reviewer.ingest_text("REVIEW_RESULT: MAYBE");
        assert_eq!(reviewer.reviewer_verdict, None);

        let mut epic = ParsedAgentOutput::new(AgentType::EpicReviewer);
        epic.ingest_text("EPIC_REVIEW_RESULT: ???");
        assert_eq!(epic.epic_verdict, None);
    }

    #[test]
    fn extracts_runtime_execution_errors() {
        let mut out = ParsedAgentOutput::new(AgentType::Worker);
        out.ingest_text("Error: Execution error: No such file or directory (os error 2)");
        assert_eq!(
            out.runtime_error.as_deref(),
            Some("No such file or directory (os error 2)")
        );
    }
}

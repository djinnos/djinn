use goose::agents::AgentEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSignal {
    Done,
    Progress,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerVerdict {
    Verified,
    Reopen,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseReviewVerdict {
    Clean,
    IssuesFound,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedAgentOutput {
    pub worker_signal: Option<WorkerSignal>,
    pub worker_reason: Option<String>,
    pub reviewer_verdict: Option<ReviewerVerdict>,
    pub reviewer_feedback: Option<String>,
    pub phase_verdict: Option<PhaseReviewVerdict>,
}

impl ParsedAgentOutput {
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

            if let Some(payload) = marker_payload(&line, "WORKER_RESULT") {
                self.parse_worker_signal(payload, &line);
            } else {
                self.parse_worker_signal(line.as_str(), &line);
            }

            if let Some(payload) = marker_payload(&line, "REVIEW_RESULT") {
                self.parse_reviewer_verdict(payload);
            }

            if let Some(payload) = marker_payload(&line, "FEEDBACK") {
                let feedback = payload.trim();
                if !feedback.is_empty() {
                    self.reviewer_feedback = Some(feedback.to_string());
                }
            }

            if let Some(payload) = marker_payload(&line, "ARCHITECT_BATCH_RESULT") {
                self.parse_phase_verdict(payload);
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

        if upper.starts_with("PROGRESS") {
            self.worker_signal = Some(WorkerSignal::Progress);
            self.worker_reason = split_reason(normalized);
            return;
        }

        if upper.starts_with("BLOCKED") {
            self.worker_signal = Some(WorkerSignal::Blocked);
            self.worker_reason = split_reason(normalized)
                .or_else(|| Some("worker reported BLOCKED without reason".to_string()));
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
            "CANCEL" => Some(ReviewerVerdict::Cancel),
            _ => {
                tracing::warn!(value = %payload, "malformed REVIEW_RESULT marker");
                self.reviewer_verdict
            }
        };
    }

    fn parse_phase_verdict(&mut self, payload: &str) {
        let verdict = payload
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .to_ascii_uppercase();

        self.phase_verdict = match verdict.as_str() {
            "CLEAN" => Some(PhaseReviewVerdict::Clean),
            "ISSUES_FOUND" => Some(PhaseReviewVerdict::IssuesFound),
            _ => {
                tracing::warn!(value = %payload, "malformed ARCHITECT_BATCH_RESULT marker");
                self.phase_verdict
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

fn split_reason(payload: &str) -> Option<String> {
    let mut parts = payload.splitn(2, ':');
    let _ = parts.next();
    let reason = parts.next()?.trim();
    if reason.is_empty() {
        None
    } else {
        Some(reason.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worker_done_and_blocked() {
        let mut out = ParsedAgentOutput::default();
        out.ingest_text("WORKER_RESULT: DONE");
        assert_eq!(out.worker_signal, Some(WorkerSignal::Done));

        out.ingest_text("WORKER_RESULT: BLOCKED: waiting on API token");
        assert_eq!(out.worker_signal, Some(WorkerSignal::Blocked));
        assert_eq!(out.worker_reason.as_deref(), Some("waiting on API token"));
    }

    #[test]
    fn parses_reviewer_verdict_and_feedback() {
        let mut out = ParsedAgentOutput::default();
        out.ingest_text("REVIEW_RESULT: REOPEN\nFEEDBACK: missing test for malformed payload");

        assert_eq!(out.reviewer_verdict, Some(ReviewerVerdict::Reopen));
        assert_eq!(
            out.reviewer_feedback.as_deref(),
            Some("missing test for malformed payload")
        );
    }

    #[test]
    fn parses_architect_batch_result() {
        let mut out = ParsedAgentOutput::default();
        out.ingest_text("ARCHITECT_BATCH_RESULT: CLEAN");
        assert_eq!(out.phase_verdict, Some(PhaseReviewVerdict::Clean));
    }

    #[test]
    fn ignores_malformed_markers_without_crashing() {
        let mut out = ParsedAgentOutput::default();
        out.ingest_text("REVIEW_RESULT: MAYBE\nARCHITECT_BATCH_RESULT: ???");

        assert_eq!(out.reviewer_verdict, None);
        assert_eq!(out.phase_verdict, None);
    }
}

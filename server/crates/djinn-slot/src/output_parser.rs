//! Parsed agent output types.
//!
//! Extracted from `djinn-agent::output_parser` so slot code can reference
//! these types without depending on `djinn-agent`.

use djinn_core::auto_submit_decision::{
    AutoSubmitDecision, ReviewAutoSubmitDecisionEvent, VerifyFreshnessEvaluatedEvent,
};
use djinn_core::models::VerifyRunRecord;

/// Validated worker completion awaiting authoritative verification.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionIntent {
    pub finalize_payload: serde_json::Value,
    pub tool_use_id: String,
}

/// Side-effect-free auto-submit settlement data prepared for lifecycle teardown.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoSubmitSettlement {
    pub task_run_id: String,
    pub decision: AutoSubmitDecision,
    pub freshness_event: VerifyFreshnessEvaluatedEvent,
    pub review_event: ReviewAutoSubmitDecisionEvent,
    pub verify_run: Option<VerifyRunRecord>,
    pub commit_title: Option<String>,
    pub summary: Option<String>,
    pub files_changed: Vec<String>,
    pub remaining_concerns: Vec<String>,
}

/// Parsed output from an agent session.
///
/// After removing markers and nudging (see ADR-022 revision), this struct only
/// tracks runtime errors and reviewer feedback extracted from agent text.
/// Worker completion is determined by session end (agent stops calling tools).
/// Reviewer verdict is determined by acceptance criteria state on the task.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAgentOutput {
    captures_feedback: bool,
    pub runtime_error: Option<String>,
    pub reviewer_feedback: Option<String>,
    /// Payload from the finalize tool call (e.g. `submit_work`, `submit_review`).
    /// Set when the reply loop exits via finalize-tool detection (ADR-036).
    pub finalize_payload: Option<serde_json::Value>,
    /// Name of the finalize tool that was actually called (e.g. `"submit_work"`,
    /// `"request_planner"`). Set alongside `finalize_payload`.
    pub finalize_tool_name: Option<String>,
    /// Present while a worker submission awaits final verification.
    pub completion_intent: Option<CompletionIntent>,
    /// Text-only handoff captured after a budget-triggered wind-down directive.
    /// This is intentionally separate from normal assistant text so settlement
    /// can park the run and persist an extractor-compatible `work_submitted`
    /// activity without treating every text-only stop as a budget park.
    pub budget_wind_down_summary: Option<String>,
    /// Structured details describing why the budget wind-down was triggered.
    /// Paired with `budget_wind_down_summary` so the handoff activity can record
    /// `remaining_concerns: "budget-parked: <details>"` instead of a generic
    /// placeholder.
    pub budget_wind_down_details: Option<String>,
    /// Auto-submit decision payload consumed by lifecycle teardown when the
    /// model did not call the role's finalize tool.
    pub auto_submit: Option<AutoSubmitSettlement>,
    /// Set to `true` when the reply loop's no-progress integrity gate detected
    /// a second consecutive identical rejected-fingerprint `submit_work`. The
    /// finalize payload is NOT accepted (no `finalize_payload`); lifecycle
    /// teardown settles this as a typed `no_progress_submission` and routes
    /// the task into planner intervention.
    pub no_progress_submission: bool,
}

impl Default for ParsedAgentOutput {
    fn default() -> Self {
        Self::new(false)
    }
}

impl ParsedAgentOutput {
    pub fn new(captures_feedback: bool) -> Self {
        Self {
            captures_feedback,
            runtime_error: None,
            reviewer_feedback: None,
            finalize_payload: None,
            finalize_tool_name: None,
            completion_intent: None,
            budget_wind_down_summary: None,
            budget_wind_down_details: None,
            auto_submit: None,
            no_progress_submission: false,
        }
    }
    /// Create an empty output (no errors, no feedback).
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn ingest_text(&mut self, text: &str) {
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
    line.trim().trim_start_matches(['>', ' ']).to_string()
}

fn extract_runtime_error(line: &str) -> Option<String> {
    // Look for common runtime error patterns.
    let lower = line.to_ascii_lowercase();
    if lower.contains("error:")
        || lower.contains("panicked at")
        || lower.contains("thread '")
        || lower.contains("fatal:")
    {
        Some(line.to_string())
    } else {
        None
    }
}

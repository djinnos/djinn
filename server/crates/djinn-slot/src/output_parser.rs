//! Parsed agent output types.
//!
//! Extracted from `djinn-agent::output_parser` so slot code can reference
//! these types without depending on `djinn-agent`.

/// Parsed output from an agent session.
///
/// After removing markers and nudging (see ADR-022 revision), this struct only
/// tracks runtime errors and reviewer feedback extracted from agent text.
/// Worker completion is determined by session end (agent stops calling tools).
/// Reviewer verdict is determined by acceptance criteria state on the task.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedAgentOutput {
    pub runtime_error: Option<String>,
    pub reviewer_feedback: Option<String>,
    /// Payload from the finalize tool call (e.g. `submit_work`, `submit_review`).
    pub finalize_payload: Option<serde_json::Value>,
    /// Name of the finalize tool that was actually called.
    pub finalize_tool_name: Option<String>,
    /// Text-only handoff captured after a budget-triggered wind-down directive.
    pub handoff_text: Option<String>,
}

impl ParsedAgentOutput {
    /// Create an empty output (no errors, no feedback).
    pub fn empty() -> Self {
        Self::default()
    }
}

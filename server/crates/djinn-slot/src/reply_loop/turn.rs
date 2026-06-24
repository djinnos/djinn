//! Reply loop turn orchestration (stub).
use crate::host::SlotContext;
use crate::output_parser::ParsedAgentOutput;
use crate::roles_support::AgentRole;
use std::sync::Arc;

/// Context for the reply loop.
pub(crate) struct ReplyLoopContext {
    pub task_id: String,
    pub session_id: String,
    pub role: Arc<dyn AgentRole>,
    pub ctx: SlotContext,
}

/// Run the reply loop for a session.
/// This is a stub — the real implementation is in djinn-agent.
pub(crate) async fn run_reply_loop(
    _loop_ctx: ReplyLoopContext,
    _kill: tokio_util::sync::CancellationToken,
    _pause: tokio_util::sync::CancellationToken,
) -> Result<ParsedAgentOutput, String> {
    Err("reply_loop not yet implemented in djinn-slot; host should provide".to_string())
}

//! MCP resolution: delegates to host callbacks.
use crate::host::{ResolvedMcpTools, SlotContext};

pub(crate) async fn resolve_mcp_for_session(
    worktree_path: &str,
    role_name: &str,
    ctx: &SlotContext,
) -> Result<ResolvedMcpTools, String> {
    ctx.callbacks
        .resolve_mcp_tools(worktree_path, role_name, ctx)
        .await
}

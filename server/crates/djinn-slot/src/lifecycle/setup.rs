//! Setup-command execution: delegates to host callbacks.
use crate::host::SlotContext;

/// Run setup commands for a task session. Delegates to host.
pub(crate) async fn run_setup_commands(
    _worktree_path: &str,
    _ctx: &SlotContext,
) -> Result<(), String> {
    // Host provides command execution through callbacks.
    Ok(())
}

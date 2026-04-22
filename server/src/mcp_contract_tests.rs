//! Residual MCP contract tests kept in the server crate.
//!
//! The bulk of the contract suite moved to
//! `server/crates/djinn-control-plane/tests/` where tests dispatch tools via
//! `McpTestHarness::call_tool(..)` without needing Axum, `AppState`, or live
//! actors.  What stays here needs one of:
//!
//! * The real slot-pool / coordinator actors (`board_reconcile`, the two
//!   `session_*_returns_error_without_pool` tests).
//! * The real `RuntimeOps::apply_settings` path (the three `settings_set_*`
//!   tests — the harness' stub errors instead of running provider-connection
//!   and mount-config validation).
//! * HTTP-level header propagation (the four `x-djinn-worktree-root` memory
//!   tests — `DjinnMcpServer::dispatch_tool_with_worktree` is not yet
//!   exposed through the bare `call_tool(name, args)` harness surface).
//! * A long-running `#[ignore]`d Dolt cascade-DELETE regression
//!   (`project_remove_success_and_missing`).

mod board_tools;
mod memory_tools;
mod project_tools;
mod session_tools;
mod settings_tools;

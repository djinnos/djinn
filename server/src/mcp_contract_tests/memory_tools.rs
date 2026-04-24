//! Memory-tool worktree contract tests removed by the db-only knowledge-base
//! cut-over.
//!
//! These tests exercised the `x-djinn-worktree-root` header that used to
//! route memory_write/edit/delete/move calls to a worktree-scoped mirror of
//! `.djinn/<note>.md` files on disk. With db-only KB storage there is no
//! file mirror, the `with_worktree_root` constructor on `NoteRepository` is
//! gone, and `dispatch_tool_with_worktree` is now a thin compatibility
//! forward to `dispatch_tool` that ignores the worktree header. The header
//! itself is still accepted so MCP clients don't break, just no-op.

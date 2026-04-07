//! Shared constants for the ADR-050 §3 server-managed `_index` worktree.
//!
//! The actual lifecycle (fetch / reset / IndexerLock / repo_graph_cache) is
//! owned by `djinn-server`'s `index_tree` module.  This module exists so the
//! djinn-agent crate (which assembles architect/chat sessions) can compute
//! the canonical index-tree path without depending on djinn-server.

use std::path::{Path, PathBuf};

/// Reserved file-name prefix marking server infrastructure entries under
/// `.djinn/worktrees/`.  Task-worktree enumeration paths must skip any entry
/// whose name starts with this character.
pub const RESERVED_WORKTREE_PREFIX: char = '_';

/// Subdirectory name of the canonical-main indexing checkout.
pub const INDEX_TREE_DIR_NAME: &str = "_index";

/// Returns the absolute path of the canonical-main index tree for a project
/// rooted at `project_root`.
pub fn index_tree_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".djinn")
        .join("worktrees")
        .join(INDEX_TREE_DIR_NAME)
}

/// Returns `true` when `entry_name` should be treated as reserved server
/// infrastructure.
#[inline]
pub fn is_reserved_worktree_entry(entry_name: &str) -> bool {
    entry_name.starts_with(RESERVED_WORKTREE_PREFIX)
}

//! Djinn workspace layer.
//!
//! Replaces the pre-migration git-worktree model. Three primitives:
//!
//! - [`MirrorManager`] owns per-project bare mirrors on disk
//!   (`<root>/{project_id}.git`) and serves fetches + ephemeral clones.
//! - [`Workspace`] is a tempdir-backed, hardlink-shared local clone of a
//!   mirror, scoped to one task-run.
//! - [`WorkspaceStore`] owns a single persistent, read-only working-tree
//!   clone per project (`<root>/{project_id}/`) for the chat subsystem.
//!   Refreshed whenever the mirror fetcher advances the project's refs.

pub mod mirror;
pub mod workspace;
pub mod workspace_store;

pub use mirror::{MirrorError, MirrorManager, mirror_path_for, mirrors_root};
pub use workspace::{EphemeralWorkspaceError, GitIdentity, Workspace};
pub use workspace_store::{
    WorkspaceError, WorkspaceStore, workspace_path_for, workspaces_root,
};

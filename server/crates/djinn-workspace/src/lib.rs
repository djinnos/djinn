//! Djinn workspace layer.
//!
//! Replaces the pre-migration git-worktree model. Two primitives:
//!
//! - [`MirrorManager`] owns per-project bare mirrors on disk
//!   (`<root>/{project_id}.git`) and serves fetches + ephemeral clones.
//! - [`Workspace`] is a tempdir-backed, hardlink-shared local clone of a
//!   mirror, scoped to one task-run.
//!
//! Task-runs obtain a [`Workspace`] from [`MirrorManager::clone_ephemeral`]
//! at start and drop it at end. No state persists in the workspace across
//! task-runs; all durable state lives in the mirror or in the task branch
//! pushed back to the origin remote.

pub mod mirror;
pub mod workspace;

pub use mirror::{MirrorError, MirrorManager, mirror_path_for, mirrors_root};
pub use workspace::{GitIdentity, Workspace, WorkspaceError};

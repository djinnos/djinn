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

pub mod commit_safety;
pub mod git_helpers;
pub mod merge_safety;
pub mod mirror;
pub mod read_source;
pub mod workspace;
pub mod workspace_store;

pub use merge_safety::{
    CHECKPOINT_REF_PREFIX, MergeSafetyDecision, PROTECTED_REFS, RefRole, classify_ref,
    evaluate_merge_head, is_checkpoint_ref, is_protected_ref,
};
pub use mirror::{
    GcGuardError, MirrorError, MirrorManager, gc_mirror_under, gc_project_clone_under,
    mirror_path_for, mirrors_root,
};
pub use read_source::{
    LegacyKind, LegacyReadSource, MigrationFailurePoint, ReadSourceMigrationError,
    ReadSourceMigrationRequest, ReadSourceMigrationResult, ReadSourceMigrator, ReadSourcePathState,
};
pub use workspace::{
    CommitOutcome, EphemeralWorkspaceError, GitIdentity, MergeOutcome, MergeParentOutcome,
    Workspace, normalize_mtimes_at,
};
pub use workspace_store::{WorkspaceError, WorkspaceStore, workspace_path_for, workspaces_root};

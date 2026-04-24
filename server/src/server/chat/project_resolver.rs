//! Per-tool-call project resolution for the user-scoped chat handler.
//!
//! Chat sessions are no longer pinned to a single project.  Every
//! project-scoped chat tool (`shell`, `read`, `code_graph`,
//! `pr_review_context`) carries an explicit `project` argument; this
//! resolver turns that argument into a `(project_id, clone_path)` tuple
//! and returns the caller the persistent per-project working-tree
//! clone owned by [`WorkspaceStore`].
//!
//! ## Authz
//!
//! Projects are globally accessible to any authenticated user in this
//! deployment (one-org-per-deployment; see `project_multiuser_roadmap`).
//! There is no per-user access check — dropped in commit 7 of the
//! chat-user-global refactor along with the `warn!` stub that used to
//! flag the gap.  If per-user access later becomes necessary, gate it
//! at this boundary.
//!
//! ## UUID-shape validation
//!
//! The resolved `project_id` flows into filesystem paths under
//! `{DJINN_HOME}/workspaces/<project>/`.  The `WorkspaceStore` enforces
//! the UUID shape at its own boundary; we additionally bail early if
//! the id coming out of the DB isn't UUID-shaped so the caller gets a
//! clean `InvalidId` instead of a generic workspace error.

use std::path::PathBuf;
use std::sync::Arc;

use djinn_db::{Database, ProjectRepository};
use djinn_workspace::{WorkspaceError, WorkspaceStore};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::events::EventBus;

/// Fallback branch when `projects.default_branch` is unset.  Git
/// accepts `HEAD` as a ref for `--branch` on local clones.
const DEFAULT_BRANCH_FALLBACK: &str = "HEAD";

/// Resolves a `project` tool-call argument (slug or UUID) to the
/// persistent working-tree clone on disk.
pub(crate) struct ProjectResolver {
    db: Database,
    event_bus: EventBus,
    workspace_store: Arc<WorkspaceStore>,
    /// Simple `project_ref -> project_id` cache.  A project ref is
    /// immutable after creation (slug is `owner/repo`, id is a UUID),
    /// so once resolved we can memoize.  Not LRU — the chat session
    /// lifecycle is short-lived enough that unbounded growth is a
    /// non-issue.
    lookup_cache: Mutex<std::collections::HashMap<String, String>>,
}

/// A successfully-resolved project + its persistent clone path.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedProject {
    pub id: String,
    pub clone_path: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum ProjectResolverError {
    #[error("project not found: {0}")]
    NotFound(String),

    #[error("project id must be UUID-shaped")]
    InvalidId,

    #[error("workspace acquisition failed: {0}")]
    Workspace(#[from] WorkspaceError),

    #[error("database error: {0}")]
    Database(#[from] djinn_db::Error),
}

impl ProjectResolver {
    pub(crate) fn new(
        db: Database,
        event_bus: EventBus,
        workspace_store: Arc<WorkspaceStore>,
    ) -> Self {
        Self {
            db,
            event_bus,
            workspace_store,
            lookup_cache: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Resolve `project_ref` (slug or UUID) into a `ResolvedProject`.
    pub(crate) async fn resolve(
        &self,
        project_ref: &str,
    ) -> Result<ResolvedProject, ProjectResolverError> {
        let project_ref = project_ref.trim();
        if project_ref.is_empty() {
            return Err(ProjectResolverError::NotFound(String::new()));
        }

        let project_id: String = {
            let guard = self.lookup_cache.lock().await;
            if let Some(id) = guard.get(project_ref) {
                id.clone()
            } else {
                drop(guard);
                let project_repo =
                    ProjectRepository::new(self.db.clone(), self.event_bus.clone());
                let Some(id) = project_repo.resolve(project_ref).await? else {
                    return Err(ProjectResolverError::NotFound(project_ref.to_owned()));
                };
                self.lookup_cache
                    .lock()
                    .await
                    .insert(project_ref.to_owned(), id.clone());
                id
            }
        };

        if !is_uuid(&project_id) {
            return Err(ProjectResolverError::InvalidId);
        }

        // Branch source: `projects.default_branch` → `projects.target_branch` →
        // `HEAD`.  `get_default_branch` is the column populated at clone
        // time from the GitHub API; `target_branch` is the legacy
        // column still set by older rows.  Either suffices for
        // `git clone --branch`.
        let project_repo = ProjectRepository::new(self.db.clone(), self.event_bus.clone());
        let default_branch = match project_repo.get_default_branch(&project_id).await? {
            Some(b) => b,
            None => match project_repo.get(&project_id).await? {
                Some(p) if !p.target_branch.trim().is_empty() => p.target_branch.clone(),
                Some(_) => DEFAULT_BRANCH_FALLBACK.to_owned(),
                None => return Err(ProjectResolverError::NotFound(project_ref.to_owned())),
            },
        };

        let clone_path = self
            .workspace_store
            .ensure_workspace(&project_id, &default_branch)
            .await?;

        Ok(ResolvedProject {
            id: project_id,
            clone_path,
        })
    }
}

/// UUID-shape check (matches
/// [`djinn_workspace::WorkspaceStore`]'s internal gate).
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    for (i, c) in s.chars().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != '-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

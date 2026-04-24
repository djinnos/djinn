//! Per-tool-call project resolution for the user-scoped chat handler.
//!
//! Chat sessions are no longer pinned to a single project.  Every
//! project-scoped chat tool (`shell`, `read`, `code_graph`, `pr_review_context`)
//! carries an explicit `project` argument; this resolver turns that argument
//! into a `(project_id, clone_path)` tuple, authz-checks the caller, and
//! acquires an ephemeral clone via [`ChatCloneCache`].
//!
//! # UUID-shape validation
//!
//! Both the resolved `project_id` and the tool-supplied `chat_session_id`
//! flow into filesystem paths under `/var/tmp/djinn-chat/<session>/<project>/`.
//! A path-traversal check is **inside** [`ChatCloneCache::acquire`]; we
//! additionally bail early if the resolved `project_id` isn't UUID-shaped so
//! we never hand a bogus id to the cache.
//!
//! # Authz
//!
//! There is no per-user / per-project access table today — `projects` has
//! no `owner_user_id` column.  Until the multiuser authz model lands, we
//! allow any authenticated user to resolve any project but emit a `warn!`
//! per call so the gap is visible.  See `TODO(multiuser-authz)` below.

use std::path::PathBuf;
use std::sync::Arc;

use djinn_core::models::Project;
use djinn_db::{Database, ProjectRepository};
use djinn_workspace::{ChatCloneCache, ChatCloneError};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::events::EventBus;

/// Fallback branch when `projects.target_branch` is empty.  Git accepts
/// `HEAD` as a ref for `--branch` on local clones.
const DEFAULT_BRANCH_FALLBACK: &str = "HEAD";

/// Resolves a `project` tool-call argument (slug or UUID) to the clone on
/// disk, gated by an authz check against the caller's `user_id`.
pub(crate) struct ProjectResolver {
    db: Database,
    event_bus: EventBus,
    clone_cache: Arc<ChatCloneCache>,
    /// Simple `project_ref -> project_id` cache.  A project ref is immutable
    /// after creation (slug is `owner/repo`, id is a UUID), so once resolved
    /// we can memoize.  Not LRU — the chat session lifecycle is short-lived
    /// enough that unbounded growth is a non-issue; the cache is also dropped
    /// with the session.
    lookup_cache: Mutex<std::collections::HashMap<String, String>>,
}

/// A successfully-resolved project + its ephemeral clone path.
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

    #[error("access denied")]
    #[allow(dead_code)]
    AccessDenied,

    #[error("chat clone acquisition failed: {0}")]
    CloneFailed(#[from] ChatCloneError),

    #[error("database error: {0}")]
    Database(#[from] djinn_db::Error),
}

impl ProjectResolver {
    pub(crate) fn new(
        db: Database,
        event_bus: EventBus,
        clone_cache: Arc<ChatCloneCache>,
    ) -> Self {
        Self {
            db,
            event_bus,
            clone_cache,
            lookup_cache: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Resolve `project_ref` (slug or UUID), enforce authz, and acquire an
    /// ephemeral clone.
    pub(crate) async fn resolve(
        &self,
        project_ref: &str,
        user_id: &str,
        chat_session_id: &str,
    ) -> Result<ResolvedProject, ProjectResolverError> {
        let project_ref = project_ref.trim();
        if project_ref.is_empty() {
            return Err(ProjectResolverError::NotFound(String::new()));
        }

        // Cache lookup.
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

        // Authz. There is no `project_access` table today and no
        // `owner_user_id` column on `projects`, so we cannot enforce a
        // real rule yet. Log the gap loudly per call.
        //
        // TODO(multiuser-authz): replace the warn! with a real
        // `user_id ∈ project_members(project_id)` check once the
        // multiuser authz model lands (see project_multiuser_roadmap).
        let project_repo = ProjectRepository::new(self.db.clone(), self.event_bus.clone());
        let project: Project = match project_repo.get(&project_id).await? {
            Some(p) => p,
            None => {
                return Err(ProjectResolverError::NotFound(project_ref.to_owned()));
            }
        };
        tracing::warn!(
            user_id = user_id,
            project_id = project_id,
            "TODO(multiuser-authz): authz not enforced yet; \
             any authenticated user may resolve any project"
        );

        // Pick the clone branch from the project row; fall back to HEAD.
        let branch = if project.target_branch.trim().is_empty() {
            DEFAULT_BRANCH_FALLBACK.to_owned()
        } else {
            project.target_branch.clone()
        };

        let clone = self
            .clone_cache
            .acquire(chat_session_id, &project_id, &branch)
            .await?;

        Ok(ResolvedProject {
            id: project_id,
            clone_path: clone.path.clone(),
        })
    }
}

/// UUID-shape check (same character class as
/// [`djinn_workspace::ChatCloneCache`] uses internally).
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

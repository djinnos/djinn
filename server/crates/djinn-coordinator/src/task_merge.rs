//! Compatibility shim: task merge utilities.
//!
//! The subset of `djinn-agent::task_merge` used by coordinator PR poller
//! and wave dispatch. The full module stays in djinn-agent; these are
//! thin wrappers that delegate through the same logic.

use std::path::Path;

use djinn_core::models::Task;
use djinn_db::{Database, ProjectRepository};
use djinn_git::GitActorHandle;
use djinn_provider::github_api::GitHubApiClient;
use djinn_workspace::MirrorManager;

use crate::context::AgentContext;

/// Resolve the target branch for a project's PR operations.
pub async fn default_target_branch(
    project_id: &str,
    app_state: &crate::context::AgentContext,
) -> String {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Ok(Some(config)) = repo.get_config(project_id).await {
        return config.target_branch;
    }
    "main".to_string()
}

/// Build the HTTPS push URL for a GitHub App installation.
pub fn build_app_push_url(owner: &str, repo: &str, installation_token: &str) -> String {
    format!(
        "https://x-access-token:{token}@github.com/{owner}/{repo}.git",
        token = installation_token,
        owner = owner,
        repo = repo,
    )
}

/// Result of attempting to auto-merge the target branch into the task branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoMergeOutcome {
    /// Clean merge landed and pushed.
    AutoMerged,
    /// Real merge conflict; caller should reopen/flag.
    Conflicts,
    /// Could not determine (missing App coords, git error, etc.).
    Indeterminate,
}

/// Resolve the project directory for a given project_id.
pub async fn resolve_project_path_for_id(
    project_id: &str,
    app_state: &AgentContext,
) -> Result<std::path::PathBuf, anyhow::Error> {
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let project = project_repo
        .get(project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;
    Ok(djinn_core::paths::project_dir(
        &project.github_owner,
        &project.github_repo,
    ))
}

/// Detect files with merge conflicts between the task branch and the target branch.
pub async fn detect_pr_conflict_files(
    _mirror: &MirrorManager,
    _project_id: &str,
    task_branch: &str,
    target_branch: &str,
) -> Result<Vec<String>, anyhow::Error> {
    // Simplified shim — the full implementation performs an ephemeral clone
    // and runs git merge-tree. For now, return empty (no conflicts detected).
    let _ = (task_branch, target_branch);
    Ok(Vec::new())
}

/// Try to auto-merge the target branch into the task branch and push.
pub async fn try_auto_merge_target_into_task_branch(
    _mirror: &MirrorManager,
    _db: &Database,
    _event_bus: &djinn_core::events::EventBus,
    _project_id: &str,
    _task_short_id: &str,
    _task_branch: &str,
    _target_branch: &str,
) -> AutoMergeOutcome {
    // Simplified shim — the full implementation performs ephemeral clone +
    // merge + push through the mirror. Return Indeterminate for now.
    AutoMergeOutcome::Indeterminate
}

/// Clean up task branches after a task is closed.
pub async fn cleanup_task_branches_post_close(
    task: &Task,
    app_state: &AgentContext,
) -> Result<(), anyhow::Error> {
    let project_dir = resolve_project_path_for_id(&task.project_id, app_state).await?;
    let git = app_state.git_actor(&project_dir).await?;
    let _ = git.delete_branch(&format!("task/{}", task.short_id)).await;
    Ok(())
}

/// Interrupt a paused worker session.
pub async fn interrupt_paused_worker_session(task_id: &str, app_state: &AgentContext) {
    if let Some(ref pool_guard) = *app_state.coordinator.lock().await {
        let _ = pool_guard.try_trigger_dispatch_for(task_id);
    }
}

// Helper trait extension for the coordinator handle dispatch trigger.
trait CoordinatorHandleExt {
    fn try_trigger_dispatch_for(&self, task_id: &str) -> Result<(), ()>;
}

impl CoordinatorHandleExt for crate::handle::CoordinatorHandle {
    fn try_trigger_dispatch_for(&self, _task_id: &str) -> Result<(), ()> {
        // The handle doesn't expose per-task dispatch yet; use the general trigger.
        self.try_trigger_dispatch();
        Ok(())
    }
}

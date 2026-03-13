use std::path::PathBuf;

use crate::commands::CommandSpec;
use crate::db::ProjectRepository;
use crate::db::TaskRepository;
use crate::models::TransitionAction;
use crate::server::AppState;

use super::*;

/// After this many consecutive verification failures, escalate to PM.
const VERIFICATION_ESCALATION_THRESHOLD: i64 = 3;

/// Spawn a background verification pipeline for a completed worker task.
///
/// The task should already be in `verifying` status.  This function:
/// 1. Creates a fresh worktree from the task branch
/// 2. Runs setup commands
/// 3. Runs verification commands
/// 4. On pass: transitions to `needs_task_review` (VerificationPass)
/// 5. On fail: logs the failure as an activity comment, transitions to `open` (VerificationFail)
/// 6. Cleans up the worktree
/// 7. Triggers redispatch for the project
pub fn spawn_verification(
    task_id: String,
    project_path: String,
    app_state: AppState,
) {
    app_state.register_verification(&task_id);
    tokio::spawn(async move {
        if let Err(e) = run_verification_pipeline(&task_id, &project_path, &app_state).await {
            tracing::error!(
                task_id = %task_id,
                error = %e,
                "Verification pipeline crashed; releasing task"
            );
            let repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
            let _ = repo
                .transition(
                    &task_id,
                    TransitionAction::ReleaseVerification,
                    "agent-supervisor",
                    "system",
                    Some(&format!("verification pipeline error: {e}")),
                    None,
                )
                .await;
        }

        app_state.deregister_verification(&task_id);

        // Trigger redispatch so newly-open tasks (on failure) or newly-ready
        // review tasks (on pass) get picked up promptly.
        if let Ok(task) = load_task(&task_id, &app_state).await
            && let Some(coordinator) = app_state.coordinator().await
        {
            let _ = coordinator
                .trigger_dispatch_for_project(&task.project_id)
                .await;
        }
    });
}

async fn run_verification_pipeline(
    task_id: &str,
    project_path: &str,
    app_state: &AppState,
) -> anyhow::Result<()> {
    let task = load_task(task_id, app_state).await?;
    let project_dir = PathBuf::from(project_path);
    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());

    // Fast path: if no setup or verification commands are configured, skip
    // worktree creation entirely and go straight to needs_task_review.
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Ok(Some(project)) = project_repo.get(&task.project_id).await {
        let setup: Vec<CommandSpec> =
            serde_json::from_str(&project.setup_commands).unwrap_or_default();
        let verify: Vec<CommandSpec> =
            serde_json::from_str(&project.verification_commands).unwrap_or_default();
        if setup.is_empty() && verify.is_empty() {
            tracing::info!(task_id = %task_id, "Verification: no commands configured; skipping");
            let _ = task_repo
                .transition(
                    task_id,
                    TransitionAction::VerificationPass,
                    "agent-supervisor",
                    "system",
                    None,
                    None,
                )
                .await;
            return Ok(());
        }
    }

    // Create a fresh worktree from the task branch.
    let worktree_path = prepare_worktree(&project_dir, &task, app_state).await?;

    // Run setup commands (e.g. npm install, cargo fetch).
    if let Some(feedback) = run_setup_commands_checked(task_id, &worktree_path, app_state).await {
        tracing::info!(task_id = %task_id, "Verification: setup commands failed");
        handle_verification_failure(task_id, &feedback, &task_repo, app_state).await;
        cleanup_worktree(task_id, &worktree_path, app_state).await;
        return Ok(());
    }

    // Run verification commands (e.g. cargo check, cargo test).
    if let Some(feedback) = run_verification_commands(task_id, &worktree_path, app_state).await {
        tracing::info!(task_id = %task_id, "Verification: verification commands failed");
        handle_verification_failure(task_id, &feedback, &task_repo, app_state).await;
        cleanup_worktree(task_id, &worktree_path, app_state).await;
        return Ok(());
    }

    // All passed — transition to needs_task_review.
    tracing::info!(task_id = %task_id, "Verification: all commands passed");
    let _ = task_repo
        .transition(
            task_id,
            TransitionAction::VerificationPass,
            "agent-supervisor",
            "system",
            None,
            None,
        )
        .await;
    cleanup_worktree(task_id, &worktree_path, app_state).await;
    Ok(())
}

/// Run verification commands synchronously (blocking the caller) and return
/// the failure feedback string if any command fails.  Used by `pm_approve` to
/// gate merges — the task status is NOT modified here.
///
/// Returns `Ok(())` when all commands pass (or none are configured), and
/// `Err(feedback)` with a human-readable failure description otherwise.
pub(crate) async fn run_verification_gate(
    task_id: &str,
    project_path: &str,
    app_state: &AppState,
) -> Result<(), String> {
    let task = load_task(task_id, app_state)
        .await
        .map_err(|e| format!("failed to load task: {e}"))?;
    let project_dir = PathBuf::from(project_path);

    // Check if there are any commands to run.
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Ok(Some(project)) = project_repo.get(&task.project_id).await {
        let setup: Vec<CommandSpec> =
            serde_json::from_str(&project.setup_commands).unwrap_or_default();
        let verify: Vec<CommandSpec> =
            serde_json::from_str(&project.verification_commands).unwrap_or_default();
        if setup.is_empty() && verify.is_empty() {
            return Ok(());
        }
    }

    let worktree_path = prepare_worktree(&project_dir, &task, app_state)
        .await
        .map_err(|e| format!("failed to create verification worktree: {e}"))?;

    // Run setup commands.
    if let Some(feedback) =
        run_setup_commands_checked(task_id, &worktree_path, app_state).await
    {
        cleanup_worktree(task_id, &worktree_path, app_state).await;
        return Err(feedback);
    }

    // Run verification commands.
    if let Some(feedback) =
        run_verification_commands(task_id, &worktree_path, app_state).await
    {
        cleanup_worktree(task_id, &worktree_path, app_state).await;
        return Err(feedback);
    }

    cleanup_worktree(task_id, &worktree_path, app_state).await;
    Ok(())
}

/// Log verification failure and transition appropriately.
///
/// If the consecutive failure count will reach the escalation threshold, go
/// directly from `verifying` → `needs_pm_intervention` (single Escalate
/// transition) to avoid a race where the intermediate `open` status triggers
/// a worker dispatch before the PM escalation happens.
async fn handle_verification_failure(
    task_id: &str,
    feedback: &str,
    task_repo: &TaskRepository,
    _app_state: &AppState,
) {
    let payload = serde_json::json!({ "body": feedback }).to_string();
    let _ = task_repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "verification",
            "comment",
            &payload,
        )
        .await;

    // Check if this failure will hit the escalation threshold BEFORE
    // transitioning, so we can go directly to PM without an intermediate
    // `open` state that would trigger a spurious worker dispatch.
    let current_count = task_repo
        .get(task_id)
        .await
        .ok()
        .flatten()
        .map(|t| t.verification_failure_count)
        .unwrap_or(0);

    // VerificationFail increments the count, so the post-transition count
    // will be current_count + 1.
    if current_count + 1 >= VERIFICATION_ESCALATION_THRESHOLD {
        tracing::warn!(
            task_id = %task_id,
            verification_failure_count = current_count + 1,
            "Verification: escalating directly to PM after {} consecutive failures",
            current_count + 1,
        );
        let reason = format!(
            "verification failed {} consecutive times; last failure:\n{}",
            current_count + 1,
            feedback
        );
        // Single transition: verifying → needs_pm_intervention.
        let _ = task_repo
            .transition(
                task_id,
                TransitionAction::Escalate,
                "agent-supervisor",
                "system",
                Some(&reason),
                None,
            )
            .await;
    } else {
        // Normal path: transition to open for re-dispatch to worker.
        let _ = task_repo
            .transition(
                task_id,
                TransitionAction::VerificationFail,
                "agent-supervisor",
                "system",
                Some(feedback),
                None,
            )
            .await;
    }
}

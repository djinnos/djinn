use std::path::Path;

use crate::commands::{CommandSpec, run_commands};
use crate::db::repositories::project::ProjectRepository;
use crate::server::AppState;

use super::*;

// ─── Command helpers ──────────────────────────────────────────────────────────

pub(crate) async fn run_setup_commands_checked(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) -> Option<String> {
    let task = load_task(task_id, app_state).await.ok()?;
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    let project = project_repo.get(&task.project_id).await.ok()??;
    let specs: Vec<CommandSpec> =
        serde_json::from_str(&project.setup_commands).unwrap_or_default();
    if specs.is_empty() {
        return None;
    }
    tracing::info!(
        task_id = %task_id,
        command_count = specs.len(),
        "Lifecycle: running setup commands"
    );
    match run_commands(&specs, worktree_path).await {
        Ok(results) => {
            let failed = results.iter().find(|r| r.exit_code != 0)?;
            tracing::info!(
                task_id = %task_id,
                command = %failed.name,
                exit_code = failed.exit_code,
                "Lifecycle: setup command failed"
            );
            let trim_output = |s: &str| -> String {
                let lines: Vec<&str> = s.trim().lines().collect();
                if lines.len() > 50 {
                    format!(
                        "... ({} lines truncated) ...\n{}",
                        lines.len() - 50,
                        lines[lines.len() - 50..].join("\n")
                    )
                } else {
                    lines.join("\n")
                }
            };
            Some(format!(
                "Setup command '{}' failed with exit code {}.\n\nYour changes likely broke a setup step (e.g. lockfile out of sync with package.json). Use your shell tools to fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                failed.name,
                failed.exit_code,
                trim_output(&failed.stdout),
                trim_output(&failed.stderr),
            ))
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: setup command system error");
            Some(format!(
                "Setup commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
            ))
        }
    }
}

pub(crate) async fn run_verification_commands(
    task_id: &str,
    worktree_path: &Path,
    app_state: &AppState,
) -> Option<String> {
    let task = load_task(task_id, app_state).await.ok()?;
    let project_repo = ProjectRepository::new(app_state.db().clone(), app_state.events().clone());
    let project = project_repo.get(&task.project_id).await.ok()??;
    let specs: Vec<CommandSpec> =
        serde_json::from_str(&project.verification_commands).unwrap_or_default();
    if specs.is_empty() {
        return None;
    }
    tracing::info!(
        task_id = %task_id,
        command_count = specs.len(),
        "Lifecycle: running verification commands"
    );
    match run_commands(&specs, worktree_path).await {
        Ok(results) => {
            let failed = results.iter().find(|r| r.exit_code != 0)?;
            tracing::info!(
                task_id = %task_id,
                command = %failed.name,
                exit_code = failed.exit_code,
                "Lifecycle: verification command failed"
            );
            let trim_output = |s: &str| -> String {
                let lines: Vec<&str> = s.trim().lines().collect();
                if lines.len() > 50 {
                    format!(
                        "... ({} lines truncated) ...\n{}",
                        lines.len() - 50,
                        lines[lines.len() - 50..].join("\n")
                    )
                } else {
                    lines.join("\n")
                }
            };
            Some(format!(
                "Verification command '{}' failed with exit code {}.\n\nUse your shell and editor tools to inspect and fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                failed.name,
                failed.exit_code,
                trim_output(&failed.stdout),
                trim_output(&failed.stderr),
            ))
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: verification command system error");
            Some(format!(
                "Verification commands could not run: {e}\n\nFix the issue and signal WORKER_RESULT: DONE when complete."
            ))
        }
    }
}

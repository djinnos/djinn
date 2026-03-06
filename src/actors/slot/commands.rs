use std::path::Path;

use crate::commands::{CommandResult, CommandSpec, run_commands};
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::task::TaskRepository;
use crate::server::AppState;

fn truncate_output(s: &str) -> String {
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
}

pub(crate) async fn log_commands_run_event(
    task_id: &str,
    phase: &str,
    specs: &[CommandSpec],
    results: &[CommandResult],
    app_state: &AppState,
) {
    let success = results.last().map(|r| r.exit_code == 0).unwrap_or(true);
    let commands = results
        .iter()
        .zip(specs.iter())
        .map(|(r, spec)| {
            let failed = r.exit_code != 0;
            serde_json::json!({
                "name": r.name,
                "command": spec.command,
                "exit_code": r.exit_code,
                "duration_ms": r.duration_ms,
                "stdout": if failed { serde_json::Value::String(truncate_output(&r.stdout)) } else { serde_json::Value::Null },
                "stderr": if failed { serde_json::Value::String(truncate_output(&r.stderr)) } else { serde_json::Value::Null },
            })
        })
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "phase": phase,
        "success": success,
        "commands": commands,
    });

    let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
    if let Err(e) = task_repo
        .log_activity(Some(task_id), "system", "system", "commands_run", &payload.to_string())
        .await
    {
        tracing::warn!(task_id = %task_id, phase = %phase, error = %e, "Lifecycle: failed to log commands_run activity");
    }
}


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
    let specs: Vec<CommandSpec> = serde_json::from_str(&project.setup_commands).unwrap_or_default();
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
            log_commands_run_event(task_id, "setup", &specs, &results, app_state).await;
            let failed = results.iter().find(|r| r.exit_code != 0)?;
            tracing::info!(
                task_id = %task_id,
                command = %failed.name,
                exit_code = failed.exit_code,
                "Lifecycle: setup command failed"
            );

            Some(format!(
                "Setup command '{}' failed with exit code {}.\n\nYour changes likely broke a setup step (e.g. lockfile out of sync with package.json). Use your shell tools to fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                failed.name,
                failed.exit_code,
                truncate_output(&failed.stdout),
                truncate_output(&failed.stderr),
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
            log_commands_run_event(task_id, "verification", &specs, &results, app_state).await;
            let failed = results.iter().find(|r| r.exit_code != 0)?;

            Some(format!(
                "Verification command '{}' failed with exit code {}.\n\nUse your shell and editor tools to inspect and fix the issue, then signal WORKER_RESULT: DONE.\n\nstdout:\n{}\nstderr:\n{}",
                failed.name,
                failed.exit_code,
                truncate_output(&failed.stdout),
                truncate_output(&failed.stderr),
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

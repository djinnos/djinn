use thiserror::Error;
use tokio::sync::oneshot;

use crate::context::AgentContext;
use djinn_core::commands::{CommandResult, CommandSpec};
use djinn_db::TaskRepository;

#[derive(Debug)]
pub(crate) enum SlotCommand {
    /// Run a task lifecycle in this slot.
    RunTask {
        task_id: String,
        project_path: String,
        respond_to: oneshot::Sender<Result<(), SlotError>>,
    },
    /// Run a project lifecycle in this slot.
    RunProject {
        project_id: String,
        project_path: String,
        agent_type: String,
        model_id: String,
        respond_to: oneshot::Sender<Result<(), SlotError>>,
    },
    /// Kill the currently running task.
    Kill,
    /// Pause the currently running task (commit WIP, preserve worktree).
    Pause,
    /// Finish current task then shut down (for capacity reduction).
    Drain,
}

#[derive(Debug, Error, Clone)]
pub enum SlotError {
    #[error("slot is busy")]
    SlotBusy,
    #[error("session failed: {0}")]
    SessionFailed(String),
    #[error("setup failed: {0}")]
    SetupFailed(String),
    #[error("worktree failed: {0}")]
    WorktreeFailed(String),
    #[error("agent error: {0}")]
    AgentError(String),
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("cancelled")]
    Cancelled,
}

fn truncate_output(s: &str) -> String {
    // 10KB / 100 lines — enough for diagnosis of setup/verification command failures.
    crate::truncate::smart_truncate_lines(s, 10_000, 100)
}

pub(crate) async fn log_commands_run_event(
    task_id: &str,
    phase: &str,
    specs: &[CommandSpec],
    results: &[CommandResult],
    app_state: &AgentContext,
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

    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = task_repo
        .log_activity(
            Some(task_id),
            "system",
            "system",
            "commands_run",
            &payload.to_string(),
        )
        .await
    {
        tracing::warn!(task_id = %task_id, phase = %phase, error = %e, "Lifecycle: failed to log commands_run activity");
    }
}

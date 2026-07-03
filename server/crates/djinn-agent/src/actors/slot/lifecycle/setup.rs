//! Setup-command execution for the task lifecycle.
//!
//! Pure extraction from `run_task_lifecycle` (task #14). The caller handles
//! error-to-transition and worktree teardown.

use std::path::Path;

use crate::actors::slot::helpers::format_command_details;
use crate::commands::run_commands;
use crate::context::AgentContext;
use crate::environment::hook_commands_to_specs;

/// Resolved prompt-context fragments from setup commands.
pub(crate) struct SetupContext {
    pub prompt_setup_commands: Option<String>,
}

/// Failure from [`resolve_setup_context`].
pub(crate) struct SetupError {
    pub reason: String,
}

/// Run project setup commands and format them for the prompt.
/// Mirrors the former inline block in `run_task_lifecycle`.
pub(crate) async fn resolve_setup_context(
    pre_verification_hooks: Vec<djinn_stack::environment::HookCommand>,
    worktree_path: &Path,
    task_id: &str,
    task_short_id: &str,
    app_state: &AgentContext,
) -> Result<SetupContext, SetupError> {
    let emit_step = |step: &str, detail: serde_json::Value| {
        app_state
            .event_bus
            .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
                task_id, step, &detail,
            ));
    };

    let setup_specs = hook_commands_to_specs(&pre_verification_hooks);
    let prompt_setup_commands = format_command_details(&setup_specs);
    if !setup_specs.is_empty() {
        let setup_start =
            djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
        tracing::info!(
            task_id = %task_short_id,
            command_count = setup_specs.len(),
            "Lifecycle: running setup commands"
        );
        let mut setup_results = Vec::new();
        let mut setup_error: Option<anyhow::Error> = None;
        for spec in &setup_specs {
            emit_step(
                "setup_command_started",
                serde_json::json!({"name": spec.name, "command": spec.command}),
            );
            match run_commands(std::slice::from_ref(spec), worktree_path).await {
                Ok(mut results) => {
                    if let Some(result) = results.pop() {
                        let status = if result.exit_code == 0 { "ok" } else { "error" };
                        emit_step(
                            "setup_command_finished",
                            serde_json::json!({"name": result.name, "status": status, "exit_code": result.exit_code}),
                        );
                        setup_results.push(result);
                        if status == "error" {
                            break;
                        }
                    }
                }
                Err(e) => {
                    emit_step(
                        "setup_command_finished",
                        serde_json::json!({"name": spec.name, "status": "error", "error": e.to_string()}),
                    );
                    setup_error = Some(e);
                    break;
                }
            }
        }

        match setup_error {
            Some(e) => {
                let reason = format!("Setup commands error: {e}");
                tracing::warn!(task_id = %task_short_id, error = %e, "Lifecycle: setup command error");
                return Err(SetupError { reason });
            }
            None => {
                crate::actors::slot::commands::log_commands_run_event(
                    task_id,
                    "setup",
                    &setup_specs,
                    &setup_results,
                    app_state,
                )
                .await;
                let failed = setup_results.iter().find(|r| r.exit_code != 0);
                if let Some(failure) = failed {
                    let reason = format!(
                        "Setup command '{}' failed (exit {})\nstdout: {}\nstderr: {}",
                        failure.name,
                        failure.exit_code,
                        failure.stdout.trim(),
                        failure.stderr.trim(),
                    );
                    tracing::warn!(
                        task_id = %task_short_id,
                        command = %failure.name,
                        "Lifecycle: setup command failed; releasing task"
                    );
                    return Err(SetupError { reason });
                }
                tracing::info!(
                    task_id = %task_short_id,
                    duration_ms = setup_start.elapsed().as_millis(),
                    "Lifecycle: setup commands completed"
                );
            }
        }
    }
    Ok(SetupContext {
        prompt_setup_commands,
    })
}

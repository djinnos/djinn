//! Setup-command execution + verification-command/rules resolution for the
//! task lifecycle.
//!
//! This is a pure code-motion extraction from `run_task_lifecycle` (task #14
//! preparatory work). The caller is responsible for reacting to
//! [`SetupError`] — e.g. task-status transition + worktree teardown — so that
//! the extracted function has no knowledge of the surrounding task-run
//! context.

use std::path::Path;

use crate::actors::slot::helpers::format_command_details;
use crate::commands::run_commands;
use crate::context::AgentContext;
use crate::verification::environment::hook_commands_to_specs;

/// Resolved prompt-context fragments produced after running project setup
/// commands and resolving the verification configuration.
pub(crate) struct SetupAndVerificationContext {
    pub prompt_setup_commands: Option<String>,
    pub prompt_verification_commands: Option<String>,
    pub prompt_verification_rules: Option<String>,
}

/// Failure from [`resolve_setup_and_verification_context`].
///
/// Carries the human-readable reason string that the caller will thread into
/// the task-status transition (preserving the original error-to-transition
/// semantics of `run_task_lifecycle`).
pub(crate) struct SetupError {
    pub reason: String,
}

/// Run project setup commands (if any), format them for the prompt, and
/// resolve verification commands + rules.
///
/// Setup commands come from `environment_config.lifecycle.pre_verification`
/// and rules come from `environment_config.verification.rules`; callers
/// fetch both upstream and pass them in.
///
/// This mirrors the byte-for-byte behaviour of the former inline block in
/// `run_task_lifecycle`:
///   - emits `setup_command_started` / `setup_command_finished` task-lifecycle
///     step events for each spec,
///   - logs a `commands_run` activity entry on success,
///   - returns a [`SetupError`] with the same reason strings the old block
///     used for task-status transitions on setup failure.
///
/// The caller is responsible for all task-status transitions and worktree
/// teardown on error — this function does not touch either.
pub(crate) async fn resolve_setup_and_verification_context(
    pre_verification_hooks: Vec<djinn_stack::environment::HookCommand>,
    verification_rules: Vec<djinn_stack::environment::VerificationRule>,
    role_verification_command: Option<&str>,
    worktree_path: &Path,
    task_id: &str,
    task_short_id: &str,
    app_state: &AgentContext,
) -> Result<SetupAndVerificationContext, SetupError> {
    let emit_step = |step: &str, detail: serde_json::Value| {
        app_state
            .event_bus
            .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
                task_id, step, &detail,
            ));
    };

    let setup_specs = hook_commands_to_specs(&pre_verification_hooks);
    let prompt_setup_commands = format_command_details(&setup_specs);
    // Role-level verification_command overrides the project's environment
    // config when set.
    let prompt_verification_commands = if let Some(cmd) = role_verification_command {
        if !cmd.trim().is_empty() {
            tracing::debug!(
                task_id = %task_short_id,
                command = %cmd,
                "Lifecycle: using role-level verification_command override"
            );
            Some(cmd.to_string())
        } else {
            None
        }
    } else {
        None
    };
    if !setup_specs.is_empty() {
        let setup_start = std::time::Instant::now();
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
    // Format verification_rules as a markdown list for the prompt.
    // Each rule becomes: "- `<pattern>`: `cmd1`, `cmd2`"
    let prompt_verification_rules = if verification_rules.is_empty() {
        None
    } else {
        let formatted = verification_rules
            .iter()
            .map(|r| {
                let cmds = r
                    .commands
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("- `{}`: {}", r.match_pattern, cmds)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(formatted)
    };
    Ok(SetupAndVerificationContext {
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
    })
}

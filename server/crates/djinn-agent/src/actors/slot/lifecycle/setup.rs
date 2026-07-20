//! Setup-command execution for the task lifecycle.

use std::path::Path;

use crate::actors::slot::helpers::format_command_details;
use crate::commands::run_commands;
use crate::context::AgentContext;
use crate::environment::hook_commands_to_specs;

/// Resolved prompt-context fragments produced after running project setup commands.
pub(crate) struct SetupContext {
    pub prompt_setup_commands: Option<String>,
}

/// Failure from [`resolve_setup_context`].
pub(crate) struct SetupError {
    pub reason: String,
}

/// Run project setup commands (if any) and format them for the prompt.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{
        agent_context_from_db, create_test_db, create_test_epic, create_test_project,
        create_test_task, test_tempdir,
    };
    use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
    use djinn_db::repositories::verify_run::VerifyRunRepository;
    use tokio_util::sync::CancellationToken;

    /// Setup-time `pre_verification` hooks run in the worktree, but the setup
    /// lifecycle must stay setup-only: it opens no final-verification attempt
    /// and records no reusable verify-run pass. The post-authoring writer in
    /// `djinn-slot` is the only completion boundary that may record one (the
    /// companion regression in
    /// `djinn-slot/src/final_verification/recording_tests.rs` proves the
    /// persisted fingerprint is computed after authoring and reflects the
    /// post-setup edit).
    #[tokio::test]
    async fn pre_verification_setup_never_opens_a_final_verification_attempt() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let run_id = uuid::Uuid::now_v7().to_string();
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: "dispatch",
                status: Some("running"),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create task run");
        let worktree = test_tempdir("djinn-setup-pre-verification-");
        let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
        let hooks = vec![djinn_stack::environment::HookCommand::Shell(
            "printf 'setup ran' > setup_marker.txt".to_owned(),
        )];

        let context = match resolve_setup_context(
            hooks,
            worktree.path(),
            &task.id,
            &task.short_id,
            &app_state,
        )
        .await
        {
            Ok(context) => context,
            Err(error) => panic!("setup hooks must succeed: {}", error.reason),
        };

        // The setup-time `pre_verification` hook really executed inside the
        // worktree and was surfaced for the prompt.
        assert_eq!(
            std::fs::read_to_string(worktree.path().join("setup_marker.txt"))
                .expect("setup hook ran in the worktree"),
            "setup ran"
        );
        let prompt_commands = context
            .prompt_setup_commands
            .as_deref()
            .expect("setup commands are formatted for the prompt");
        assert!(prompt_commands.contains("setup-1"), "{prompt_commands}");

        // … and yet no final-verification attempt was opened and no reusable
        // pass was recorded for the task run.
        let rows = VerifyRunRepository::new(db)
            .list_for_task_run(&run_id)
            .await
            .expect("list verify runs");
        assert!(
            rows.is_empty(),
            "setup must never record a verify-run pass ({} rows)",
            rows.len()
        );
    }
}

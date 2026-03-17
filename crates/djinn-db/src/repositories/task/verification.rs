// These helpers are defined as infrastructure for callers that provide a
// VerificationRunner implementation.  They are not yet wired into the main
// execution path, so suppress dead_code until they are consumed.
#![allow(dead_code)]

use std::path::Path;

use djinn_core::commands::{CommandResult, CommandSpec, VerificationRunner};

use super::TaskRepository;

pub(crate) async fn run_setup_commands_checked(
    cwd: &Path,
    setup_commands: &[String],
    task_id: Option<&str>,
    task_repo: &TaskRepository,
    runner: &dyn VerificationRunner,
) -> Result<(), String> {
    if setup_commands.is_empty() {
        return Ok(());
    }

    let setup_specs: Vec<CommandSpec> = setup_commands
        .iter()
        .enumerate()
        .map(|(idx, c)| CommandSpec {
            name: format!("setup-{}", idx + 1),
            command: c.clone(),
            timeout_secs: Some(1800),
        })
        .collect();

    let setup_results = runner.run_commands(&setup_specs, cwd).await?;
    if let Some(task_id) = task_id {
        log_commands_run_event(task_id, "setup", &setup_results, task_repo).await;
    }

    let failed: Vec<&CommandResult> = setup_results.iter().filter(|r| r.exit_code != 0).collect();
    if failed.is_empty() {
        return Ok(());
    }

    let summary = failed
        .into_iter()
        .map(|r| {
            format!(
                "- command: {}\n  exit: {}\n  stdout:\n{}\n  stderr:\n{}",
                r.name, r.exit_code, r.stdout, r.stderr
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let body = format!("Setup commands failed before verification:\n{}", summary);

    if let Some(task_id) = task_id {
        let payload = serde_json::json!({ "body": body }).to_string();
        let _ = task_repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "comment",
                &payload,
            )
            .await;
    }

    Err(body)
}

pub(crate) async fn run_verification_commands(
    task_id: &str,
    actor_id: Option<&str>,
    actor_role: Option<&str>,
    verify_specs: &[CommandSpec],
    task_repo: &TaskRepository,
    runner: &dyn VerificationRunner,
    working_dir: &Path,
) -> Result<(), String> {
    if verify_specs.is_empty() {
        return Ok(());
    }

    let verify_results = runner.run_commands(verify_specs, working_dir).await?;
    log_commands_run_event(task_id, "verification", &verify_results, task_repo).await;

    let failed: Vec<&CommandResult> = verify_results.iter().filter(|r| r.exit_code != 0).collect();
    if failed.is_empty() {
        return Ok(());
    }

    let summary = failed
        .into_iter()
        .map(|r| {
            format!(
                "- command: {}\n  exit: {}\n  stdout:\n{}\n  stderr:\n{}",
                r.name, r.exit_code, r.stdout, r.stderr
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let body = format!(
        "Automated verification failed before merge. Please inspect and fix:\n{}",
        summary
    );

    let payload = serde_json::json!({ "body": body }).to_string();
    let _ = task_repo
        .log_activity(
            Some(task_id),
            actor_id.unwrap_or("agent-supervisor"),
            actor_role.unwrap_or("verification"),
            "comment",
            &payload,
        )
        .await;

    Err(body)
}

pub(crate) async fn log_commands_run_event(
    task_id: &str,
    phase: &str,
    results: &[CommandResult],
    task_repo: &TaskRepository,
) {
    for r in results {
        let payload = serde_json::json!({
            "phase": phase,
            "command": r.name,
            "exit_code": r.exit_code,
            "stdout": r.stdout,
            "stderr": r.stderr,
            "duration_ms": r.duration_ms,
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(task_id),
                "agent-supervisor",
                "verification",
                "commands_run",
                &payload,
            )
            .await;
    }
}

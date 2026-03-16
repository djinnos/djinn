use crate::commands::{CommandResult, CommandSpec, run_commands};
use crate::db::TaskRepository;
use crate::server::AppState;

pub(crate) async fn run_setup_commands_checked(
    cwd: &std::path::Path,
    setup_commands: &[String],
    task_id: Option<&str>,
    app_state: &AppState,
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

    let setup_results = run_commands(&setup_specs, cwd)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(task_id) = task_id {
        log_commands_run_event(task_id, "setup", &setup_results, app_state).await;
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
        let repo = TaskRepository::new(app_state.db().clone(), app_state.event_bus());
        let payload = serde_json::json!({ "body": body }).to_string();
        let _ = repo
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
    app_state: &AppState,
    working_dir: &std::path::Path,
) -> Result<(), String> {
    if verify_specs.is_empty() {
        return Ok(());
    }

    let verify_results = run_commands(verify_specs, working_dir)
        .await
        .map_err(|e| e.to_string())?;
    log_commands_run_event(task_id, "verification", &verify_results, app_state).await;

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

    let repo = TaskRepository::new(app_state.db().clone(), app_state.event_bus());
    let payload = serde_json::json!({ "body": body }).to_string();
    let _ = repo
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
    app_state: &AppState,
) {
    let repo = TaskRepository::new(app_state.db().clone(), app_state.event_bus());
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
        let _ = repo
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

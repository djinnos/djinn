use std::path::PathBuf;

use crate::context::AgentContext;
use crate::verification::StepEvent;
use crate::verification::service::verify_commit;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::TransitionAction;
use djinn_db::TaskRepository;
use djinn_db::VerificationCacheRepository;

use super::*;

/// After this many consecutive verification failures, escalate to PM.
const VERIFICATION_ESCALATION_THRESHOLD: i64 = 3;

/// Minimum pipeline timeout even when no commands are configured.
const MIN_PIPELINE_TIMEOUT_SECS: u64 = 120;
/// Extra headroom on top of the sum of per-command timeouts to account for
/// worktree creation, cache lookup, and cleanup.
const PIPELINE_TIMEOUT_OVERHEAD_SECS: u64 = 120;

/// Compute the overall pipeline timeout from the per-command `timeout_secs`
/// in `.djinn/settings.json`.  Falls back to a safe default when the
/// settings file is missing or unreadable.
fn compute_pipeline_timeout(project_path: &str) -> std::time::Duration {
    let path = std::path::Path::new(project_path);
    let sum = match crate::verification::settings::load_commands(path) {
        Ok((setup, verification)) => {
            let total: u64 = setup
                .iter()
                .chain(verification.iter())
                .map(|c| c.timeout_secs.unwrap_or(300))
                .sum();
            total
        }
        Err(_) => 0,
    };
    let secs = (sum + PIPELINE_TIMEOUT_OVERHEAD_SECS).max(MIN_PIPELINE_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

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
pub(crate) fn spawn_verification(task_id: String, project_path: String, app_state: AgentContext) {
    let pipeline_timeout = compute_pipeline_timeout(&project_path);
    app_state.register_verification(&task_id);
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            pipeline_timeout,
            run_verification_pipeline(&task_id, &project_path, &app_state),
        )
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(
                    task_id = %task_id,
                    error = %e,
                    "Verification pipeline crashed; releasing task"
                );
                let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
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
            Err(_elapsed) => {
                tracing::error!(
                    task_id = %task_id,
                    timeout_secs = pipeline_timeout.as_secs(),
                    "Verification pipeline timed out; releasing task"
                );
                let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                let _ = repo
                    .transition(
                        &task_id,
                        TransitionAction::ReleaseVerification,
                        "agent-supervisor",
                        "system",
                        Some(&format!(
                            "verification pipeline timed out after {}s",
                            pipeline_timeout.as_secs()
                        )),
                        None,
                    )
                    .await;
            }
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
    app_state: &AgentContext,
) -> anyhow::Result<()> {
    let task = load_task(task_id, app_state).await?;
    let project_dir = PathBuf::from(project_path);
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Create a fresh worktree from the task branch.
    let worktree_path = prepare_worktree(&project_dir, &task, app_state).await?;
    let commit_sha = resolve_head_commit(&worktree_path)?;

    let result =
        verify_commit(&task.project_id, &commit_sha, &worktree_path, &app_state.db).await?;
    emit_verification_steps(&task.project_id, Some(task_id), &result, app_state).await;

    if !result.passed {
        let feedback = format_verification_failure_feedback(&result);
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
    app_state: &AgentContext,
) -> Result<(), String> {
    let task = load_task(task_id, app_state)
        .await
        .map_err(|e| format!("failed to load task: {e}"))?;
    let project_dir = PathBuf::from(project_path);

    let branch = format!("task/{}", task.short_id);
    let commit_sha = resolve_head_commit_for_branch(&project_dir, &branch)
        .map_err(|e| format!("failed to resolve branch HEAD: {e}"))?;

    let cache_repo = VerificationCacheRepository::new(app_state.db.clone());
    if cache_repo
        .get(&task.project_id, &commit_sha)
        .await
        .map_err(|e| format!("failed to query verification cache: {e}"))?
        .is_some()
    {
        app_state
            .event_bus
            .send(DjinnEventEnvelope::verification_step(
                &task.project_id,
                Some(task_id),
                "verification",
                &StepEvent::CacheHit {
                    commit_sha: commit_sha.clone(),
                    cached_at: String::new(),
                    original_duration_ms: 0,
                },
            ));
        return Ok(());
    }

    let worktree_path = prepare_worktree(&project_dir, &task, app_state)
        .await
        .map_err(|e| format!("failed to create verification worktree: {e}"))?;

    let result = verify_commit(&task.project_id, &commit_sha, &worktree_path, &app_state.db)
        .await
        .map_err(|e| format!("verification execution failed: {e}"))?;
    emit_verification_steps(&task.project_id, Some(task_id), &result, app_state).await;

    cleanup_worktree(task_id, &worktree_path, app_state).await;
    if result.passed {
        Ok(())
    } else {
        Err(format_verification_failure_feedback(&result))
    }
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
    _app_state: &AgentContext,
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

fn resolve_head_commit(worktree_path: &std::path::Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(worktree_path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_head_commit_for_branch(
    project_dir: &std::path::Path,
    branch_name: &str,
) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg(branch_name)
        .current_dir(project_dir)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse {} failed: {}",
            branch_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn emit_verification_steps(
    project_id: &str,
    task_id: Option<&str>,
    result: &crate::verification::service::VerificationResult,
    app_state: &AgentContext,
) {
    for (idx, r) in result.setup_results.iter().enumerate() {
        app_state
            .event_bus
            .send(DjinnEventEnvelope::verification_step(
                project_id,
                task_id,
                "setup",
                &StepEvent::Finished {
                    index: (idx + 1) as u32,
                    name: r.name.clone(),
                    exit_code: r.exit_code,
                    duration_ms: r.duration_ms,
                    stdout: r.stdout.clone(),
                    stderr: r.stderr.clone(),
                },
            ));
    }
    for (idx, r) in result.verification_results.iter().enumerate() {
        app_state
            .event_bus
            .send(DjinnEventEnvelope::verification_step(
                project_id,
                task_id,
                "verification",
                &StepEvent::Finished {
                    index: (idx + 1) as u32,
                    name: r.name.clone(),
                    exit_code: r.exit_code,
                    duration_ms: r.duration_ms,
                    stdout: r.stdout.clone(),
                    stderr: r.stderr.clone(),
                },
            ));
    }
}

/// Max chars per stdout/stderr field in verification feedback.
/// Keeps the activity log entry and downstream prompts reasonable.
const MAX_OUTPUT_CHARS: usize = 3000;

fn format_verification_failure_feedback(
    result: &crate::verification::service::VerificationResult,
) -> String {
    let failed = result
        .setup_results
        .iter()
        .chain(result.verification_results.iter())
        .find(|r| r.exit_code != 0);
    if let Some(cmd) = failed {
        let stdout = crate::truncate::smart_truncate(&cmd.stdout, MAX_OUTPUT_CHARS);
        let stderr = crate::truncate::smart_truncate(&cmd.stderr, MAX_OUTPUT_CHARS);
        format!(
            "Verification command '{}' (`{}`) failed with exit code {}.\n\nstdout:\n{stdout}\nstderr:\n{stderr}",
            cmd.name, cmd.command, cmd.exit_code,
        )
    } else {
        "Verification failed".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{
        agent_context_from_db, create_test_db, create_test_epic, create_test_project,
        create_test_task, test_events,
    };
    use crate::verification::service::VerificationResult;
    use djinn_core::commands::CommandResult;
    use djinn_core::models::TransitionAction;
    use djinn_db::TaskRepository;
    use tokio_util::sync::CancellationToken;

    fn make_result(stdout: &str, stderr: &str) -> VerificationResult {
        VerificationResult {
            passed: false,
            cached: false,
            setup_results: vec![],
            verification_results: vec![CommandResult {
                name: "clippy".into(),
                command: "cargo clippy --workspace -- -D warnings".into(),
                exit_code: 101,
                stdout: stdout.into(),
                stderr: stderr.into(),
                duration_ms: 5000,
            }],
            total_duration_ms: 5000,
        }
    }

    #[test]
    fn feedback_truncates_large_stderr() {
        let huge_stderr = "e".repeat(10_000);
        let result = make_result("", &huge_stderr);
        let feedback = format_verification_failure_feedback(&result);

        assert!(
            feedback.len() < 7_000,
            "feedback should be under 7k chars, got {}",
            feedback.len()
        );
        assert!(feedback.contains("bytes omitted") || feedback.contains("truncated"));
        assert!(feedback.contains("clippy"));
        assert!(feedback.contains("cargo clippy --workspace -- -D warnings"));
        assert!(feedback.contains("exit code 101"));
    }

    #[test]
    fn feedback_not_truncated_when_small() {
        let result = make_result("ok", "error[E0599]: something");
        let feedback = format_verification_failure_feedback(&result);

        assert!(!feedback.contains("omitted"));
        assert!(feedback.contains("error[E0599]: something"));
    }

    #[test]
    fn feedback_truncates_large_stdout() {
        let huge_stdout = "o".repeat(10_000);
        let result = make_result(&huge_stdout, "small error");
        let feedback = format_verification_failure_feedback(&result);

        assert!(feedback.contains("bytes omitted") || feedback.contains("truncated"));
        assert!(feedback.len() < 7_000);
    }

    async fn setup_verifying_task_with_count(count: i64) -> (TaskRepository, String, AgentContext) {
        let db = create_test_db();
        let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let task_repo = TaskRepository::new(db.clone(), test_events());

        task_repo
            .set_status(&task.id, "open")
            .await
            .expect("set status open");
        task_repo
            .transition(
                &task.id,
                TransitionAction::Start,
                "test",
                "system",
                None,
                None,
            )
            .await
            .expect("transition to in_progress");
        task_repo
            .transition(
                &task.id,
                TransitionAction::SubmitVerification,
                "test",
                "system",
                None,
                None,
            )
            .await
            .expect("transition to verifying");

        if count > 0 {
            sqlx::query("UPDATE tasks SET verification_failure_count = ?1 WHERE id = ?2")
                .bind(count)
                .bind(&task.id)
                .execute(db.pool())
                .await
                .expect("set verification_failure_count");
        }

        (task_repo, task.id, app_state)
    }

    #[tokio::test]
    async fn handle_verification_failure_first_failure_goes_open() {
        let (task_repo, task_id, app_state) = setup_verifying_task_with_count(0).await;
        let feedback = "first failure feedback";
        handle_verification_failure(&task_id, feedback, &task_repo, &app_state).await;

        let task = task_repo
            .get(&task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, "open");

        let activity = task_repo
            .list_activity(&task_id)
            .await
            .expect("list activity");
        let verification_comment = activity
            .iter()
            .find(|e| e.actor_role == "verification" && e.event_type == "comment")
            .expect("verification comment present");
        let payload: serde_json::Value =
            serde_json::from_str(&verification_comment.payload).expect("json payload");
        assert_eq!(payload["body"], feedback);
    }

    #[tokio::test]
    async fn handle_verification_failure_second_failure_still_goes_open() {
        let (task_repo, task_id, app_state) = setup_verifying_task_with_count(1).await;
        handle_verification_failure(&task_id, "second failure", &task_repo, &app_state).await;
        let task = task_repo
            .get(&task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, "open");
    }

    #[tokio::test]
    async fn handle_verification_failure_threshold_escalates_directly() {
        let (task_repo, task_id, app_state) = setup_verifying_task_with_count(2).await;
        handle_verification_failure(&task_id, "third failure", &task_repo, &app_state).await;
        let task = task_repo
            .get(&task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, "needs_pm_intervention");

        let activity = task_repo
            .list_activity(&task_id)
            .await
            .expect("list activity");
        let statuses: Vec<serde_json::Value> = activity
            .iter()
            .filter(|e| e.event_type == "status_changed")
            .map(|e| serde_json::from_str(&e.payload).expect("status payload json"))
            .collect();
        assert!(!statuses.iter().any(|p| p["to_status"] == "open"));
        assert!(
            statuses
                .iter()
                .any(|p| p["to_status"] == "needs_pm_intervention")
        );
    }

    #[tokio::test]
    async fn handle_verification_failure_past_threshold_escalates() {
        let (task_repo, task_id, app_state) = setup_verifying_task_with_count(5).await;
        handle_verification_failure(&task_id, "many failures", &task_repo, &app_state).await;
        let task = task_repo
            .get(&task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, "needs_pm_intervention");
    }
}

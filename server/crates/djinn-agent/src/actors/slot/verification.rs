use crate::context::AgentContext;
use crate::verification::StepEvent;
use crate::verification::scoped::resolve_scoped_commands;
use crate::verification::service::verify_commit;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::TransitionAction;
use djinn_db::TaskRepository;
use djinn_db::{VerificationResultRepository, VerificationStepInsert};

use super::*;

/// After this many consecutive verification failures, escalate to lead.
const VERIFICATION_ESCALATION_THRESHOLD: i64 = 3;

/// Minimum pipeline timeout floor — chosen to accommodate workspace-wide
/// `cargo test` + `cargo clippy` runs on medium-sized Rust projects.  Projects
/// with heavier verification pipelines should set `verification_timeout_secs`
/// in `.djinn/settings.json` explicitly.
const MIN_PIPELINE_TIMEOUT_SECS: u64 = 900;
/// Extra headroom on top of the sum of per-command timeouts to account for
/// worktree creation, cache lookup, and cleanup.
const PIPELINE_TIMEOUT_OVERHEAD_SECS: u64 = 120;

/// Compute the overall pipeline timeout from `.djinn/settings.json`.
///
/// Precedence:
/// 1. If `verification_timeout_secs` is set, use it (clamped below by the
///    `MIN_PIPELINE_TIMEOUT_SECS` floor).
/// 2. Otherwise fall back to `sum(setup.timeout_secs) + overhead`, also
///    floored to `MIN_PIPELINE_TIMEOUT_SECS`.
///
/// Note: `verification_rules.commands` are plain strings without per-command
/// timeouts, so they contribute nothing to the computed sum.  That is why
/// the `MIN_PIPELINE_TIMEOUT_SECS` floor matters in practice.
fn compute_pipeline_timeout(project_path: &str) -> std::time::Duration {
    let path = std::path::Path::new(project_path);
    let settings = crate::verification::settings::load_settings(path).ok();

    if let Some(explicit) = settings.as_ref().and_then(|s| s.verification_timeout_secs) {
        return std::time::Duration::from_secs(explicit.max(MIN_PIPELINE_TIMEOUT_SECS));
    }

    let sum: u64 = settings
        .as_ref()
        .map(|s| s.setup.iter().map(|c| c.timeout_secs.unwrap_or(300)).sum())
        .unwrap_or(0);
    let secs = (sum + PIPELINE_TIMEOUT_OVERHEAD_SECS).max(MIN_PIPELINE_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

struct VerificationRegistrationGuard {
    app_state: AgentContext,
    task_id: String,
}

impl Drop for VerificationRegistrationGuard {
    fn drop(&mut self) {
        self.app_state.deregister_verification(&self.task_id);
    }
}

fn spawn_verification_with_timeout<F>(
    task_id: String,
    app_state: AgentContext,
    pipeline_timeout: std::time::Duration,
    pipeline: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let _guard = VerificationRegistrationGuard {
            app_state: app_state.clone(),
            task_id: task_id.clone(),
        };
        let result = tokio::time::timeout(pipeline_timeout, pipeline).await;

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

        if let Ok(task) = load_task(&task_id, &app_state).await
            && let Some(coordinator) = app_state.coordinator().await
        {
            let _ = coordinator
                .trigger_dispatch_for_project(&task.project_id)
                .await;
        }
    })
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
    let task_id_for_pipeline = task_id.clone();
    let project_path_for_pipeline = project_path.clone();
    let app_state_for_pipeline = app_state.clone();
    let pipeline = async move {
        run_verification_pipeline(
            &task_id_for_pipeline,
            &project_path_for_pipeline,
            &app_state_for_pipeline,
        )
        .await
    };

    std::mem::drop(spawn_verification_with_timeout(
        task_id,
        app_state,
        pipeline_timeout,
        pipeline,
    ));
}

/// Resolve the role-level `verification_command` override for the given task.
///
/// Returns `None` when the task has no `agent_type`, the role cannot be found,
/// or the role's `verification_command` is `None` / empty.
async fn role_verification_command_for_task(
    task: &djinn_core::models::Task,
    app_state: &AgentContext,
) -> Option<String> {
    let specialist_name = task.agent_type.as_deref().filter(|s| !s.is_empty())?;
    let role_repo =
        djinn_db::AgentRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let role = role_repo
        .get_by_name_for_project(&task.project_id, specialist_name)
        .await
        .ok()
        .flatten()?;
    role.verification_command
        .filter(|cmd| !cmd.trim().is_empty())
}

async fn run_verification_pipeline(
    task_id: &str,
    _project_path: &str,
    app_state: &AgentContext,
) -> anyhow::Result<()> {
    let task = load_task(task_id, app_state).await?;
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Task #8: verification now runs against a mirror-native ephemeral
    // workspace instead of a user-visible `.djinn/worktrees/<short_id>` task
    // worktree.  We clone-ephemeral on the target branch, then fetch + check
    // out the task branch so verification sees the same tree the worker just
    // pushed to.  The workspace tempdir is dropped at the end of the pipeline.
    let mirror = app_state.mirror.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "verification requires a MirrorManager on AgentContext; none configured"
        )
    })?;
    let target_branch = default_target_branch(&task.project_id, app_state).await;
    let task_branch = format!("task/{}", task.short_id);

    let workspace = mirror
        .clone_ephemeral(&task.project_id, &target_branch)
        .await
        .map_err(|e| anyhow::anyhow!("verification clone_ephemeral: {e}"))?;
    let workspace_path = workspace.path_buf();

    // Fetch the task branch from the mirror so we can check it out.
    djinn_git::run_git_command(
        workspace_path.clone(),
        vec![
            "fetch".into(),
            "origin".into(),
            format!("{task_branch}:refs/remotes/origin/{task_branch}"),
        ],
    )
    .await
    .map_err(|e| anyhow::anyhow!("verification fetch task branch: {e}"))?;
    djinn_git::run_git_command(
        workspace_path.clone(),
        vec![
            "checkout".into(),
            "-B".into(),
            task_branch.clone(),
            format!("origin/{task_branch}"),
        ],
    )
    .await
    .map_err(|e| anyhow::anyhow!("verification checkout task branch: {e}"))?;

    let commit_sha = resolve_head_commit(&workspace_path)?;

    // Resolve scoped verification commands (AC-1 through AC-7).
    let role_cmd_override = role_verification_command_for_task(&task, app_state).await;
    let scoped_commands =
        resolve_scoped_commands(&workspace_path, &target_branch, role_cmd_override.as_deref());

    let result = verify_commit(
        &task.project_id,
        &commit_sha,
        &workspace_path,
        &app_state.db,
        &scoped_commands,
    )
    .await?;
    emit_verification_steps(&task.project_id, Some(task_id), &result, app_state).await;

    if !result.passed {
        let feedback = format_verification_failure_feedback(&result);
        tracing::info!(task_id = %task_id, "Verification: verification commands failed");
        handle_verification_failure(task_id, &feedback, &task_repo, app_state).await;
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
    Ok(())
}

/// Log verification failure and transition appropriately.
///
/// If the consecutive failure count will reach the escalation threshold, go
/// directly from `verifying` → `needs_lead_intervention` (single Escalate
/// transition) to avoid a race where the intermediate `open` status triggers
/// a worker dispatch before the lead escalation happens.
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
    // transitioning, so we can go directly to lead without an intermediate
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
            "Verification: escalating directly to lead after {} consecutive failures",
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

async fn emit_verification_steps(
    project_id: &str,
    task_id: Option<&str>,
    result: &crate::verification::service::VerificationResult,
    app_state: &AgentContext,
) {
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut db_rows: Vec<VerificationStepInsert> = Vec::new();
    let mut step_index: i32 = 1;

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
        db_rows.push(VerificationStepInsert {
            project_id: project_id.to_string(),
            task_id: task_id.map(|s| s.to_string()),
            run_id: run_id.clone(),
            phase: "setup".to_string(),
            step_index,
            name: r.name.clone(),
            command: r.command.clone(),
            exit_code: r.exit_code,
            stdout: r.stdout.clone(),
            stderr: r.stderr.clone(),
            duration_ms: r.duration_ms as i64,
        });
        step_index += 1;
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
        db_rows.push(VerificationStepInsert {
            project_id: project_id.to_string(),
            task_id: task_id.map(|s| s.to_string()),
            run_id: run_id.clone(),
            phase: "verification".to_string(),
            step_index,
            name: r.name.clone(),
            command: r.command.clone(),
            exit_code: r.exit_code,
            stdout: r.stdout.clone(),
            stderr: r.stderr.clone(),
            duration_ms: r.duration_ms as i64,
        });
        step_index += 1;
    }

    // Persist to DB so the frontend can load results on page open.
    if let Some(tid) = task_id {
        let repo = VerificationResultRepository::new(app_state.db.clone());
        if let Err(e) = repo.replace_for_task(tid, &db_rows).await {
            tracing::warn!(task_id = %tid, error = %e, "Failed to persist verification results");
        }
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
    use crate::verification::settings::load_setup_commands;
    use djinn_core::commands::CommandResult;
    use djinn_core::models::TransitionAction;
    use djinn_db::TaskRepository;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-verification-")
    }

    fn write_settings(dir: &std::path::Path, body: &str) {
        let djinn_dir = dir.join(".djinn");
        std::fs::create_dir_all(&djinn_dir).expect("create .djinn directory");
        std::fs::write(djinn_dir.join("settings.json"), body).expect("write settings.json");
    }

    async fn tick_spawned_verification() {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
    }

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

    fn setup_verifying_task_with_count_blocking(
        count: i64,
    ) -> (TaskRepository, String, AgentContext) {
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build runtime");
                rt.block_on(async move {
                    let db = create_test_db();
                    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
                    let project = create_test_project(&db).await;
                    let epic = create_test_epic(&db, &project.id).await;
                    let task = create_test_task(&db, &project.id, &epic.id).await;
                    let task_repo = TaskRepository::new(db.clone(), test_events());

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
                        task_repo
                            .set_verification_failure_count(&task.id, count)
                            .await
                            .expect("set verification_failure_count");
                    }

                    (task_repo, task.id, app_state)
                })
            })
            .join()
            .expect("thread panicked")
        })
    }

    async fn setup_verifying_task_with_count(count: i64) -> (TaskRepository, String, AgentContext) {
        let db = create_test_db();
        let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let task_repo = TaskRepository::new(db.clone(), test_events());

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
            task_repo
                .set_verification_failure_count(&task.id, count)
                .await
                .expect("set verification_failure_count");
        }

        (task_repo, task.id, app_state)
    }

    #[tokio::test(start_paused = true)]
    async fn compute_pipeline_timeout_uses_configured_timeouts_with_overhead() {
        let dir = tempdir_in_tmp();
        write_settings(
            dir.path(),
            r#"{
                "setup": [{"name": "fmt", "command": "cargo fmt --check", "timeout_secs": 7}]
            }"#,
        );

        let timeout = compute_pipeline_timeout(dir.path().to_str().expect("utf8 path"));
        let setup = load_setup_commands(dir.path()).expect("load settings commands");
        let configured_timeout_secs: u64 =
            setup.iter().map(|c| c.timeout_secs.unwrap_or(300)).sum();
        let expected_timeout_secs = (configured_timeout_secs + PIPELINE_TIMEOUT_OVERHEAD_SECS)
            .max(MIN_PIPELINE_TIMEOUT_SECS);

        assert_eq!(timeout, Duration::from_secs(expected_timeout_secs));
    }

    #[tokio::test(start_paused = true)]
    async fn compute_pipeline_timeout_uses_minimum_when_settings_missing() {
        let dir = tempdir_in_tmp();

        let timeout = compute_pipeline_timeout(dir.path().to_str().expect("utf8 path"));

        assert_eq!(timeout, Duration::from_secs(MIN_PIPELINE_TIMEOUT_SECS));
    }

    #[tokio::test(start_paused = true)]
    async fn compute_pipeline_timeout_explicit_override_takes_precedence() {
        let dir = tempdir_in_tmp();
        write_settings(
            dir.path(),
            r#"{
                "setup": [{"name": "fmt", "command": "cargo fmt --check", "timeout_secs": 7}],
                "verification_timeout_secs": 1800
            }"#,
        );

        let timeout = compute_pipeline_timeout(dir.path().to_str().expect("utf8 path"));
        assert_eq!(timeout, Duration::from_secs(1800));
    }

    #[tokio::test(start_paused = true)]
    async fn compute_pipeline_timeout_explicit_override_clamped_to_floor() {
        let dir = tempdir_in_tmp();
        write_settings(
            dir.path(),
            r#"{
                "setup": [],
                "verification_timeout_secs": 10
            }"#,
        );

        let timeout = compute_pipeline_timeout(dir.path().to_str().expect("utf8 path"));
        assert_eq!(timeout, Duration::from_secs(MIN_PIPELINE_TIMEOUT_SECS));
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_verification_times_out_deterministically_and_releases_task() {
        let (_task_repo, task_id, app_state) = setup_verifying_task_with_count_blocking(0);
        let timeout = Duration::from_secs(5);

        app_state.register_verification(&task_id);
        let background = spawn_verification_with_timeout(
            task_id.clone(),
            app_state.clone(),
            timeout,
            std::future::pending::<anyhow::Result<()>>(),
        );
        tick_spawned_verification().await;

        assert!(app_state.has_verification(&task_id));

        tokio::time::advance(timeout - Duration::from_secs(1)).await;
        tick_spawned_verification().await;
        assert!(
            app_state.has_verification(&task_id),
            "should still be verifying before timeout"
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        tick_spawned_verification().await;
        background.await.expect("background task completed");

        assert!(
            !app_state.has_verification(&task_id),
            "verification should be released after timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn aborting_verification_task_releases_tracker_before_timeout() {
        let (_task_repo, task_id, app_state) = setup_verifying_task_with_count_blocking(0);
        let timeout = Duration::from_secs(60);

        app_state.register_verification(&task_id);
        let background = spawn_verification_with_timeout(
            task_id.clone(),
            app_state.clone(),
            timeout,
            std::future::pending::<anyhow::Result<()>>(),
        );

        tick_spawned_verification().await;
        assert!(app_state.has_verification(&task_id));

        background.abort();
        let _ = background.await;

        assert!(!app_state.has_verification(&task_id));

        tokio::time::advance(timeout - Duration::from_secs(1)).await;
        tick_spawned_verification().await;
        assert!(!app_state.has_verification(&task_id));
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
        assert_eq!(task.status, "needs_lead_intervention");

        let activity = task_repo
            .list_activity(&task_id)
            .await
            .expect("list activity");
        let statuses: Vec<serde_json::Value> = activity
            .iter()
            .filter(|e| e.event_type == "status_changed")
            .map(|e| serde_json::from_str(&e.payload).expect("status payload json"))
            .collect();
        // After setup, we should NOT see an intermediate open status
        // when escalating directly to Lead; the transition should be verifying->needs_lead_intervention
        assert!(!statuses.iter().any(|p| p["to_status"] == "open"));
        assert!(
            statuses
                .iter()
                .any(|p| p["to_status"] == "needs_lead_intervention")
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
        assert_eq!(task.status, "needs_lead_intervention");
    }
}

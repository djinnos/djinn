use crate::context::AgentContext;
use crate::verification::StepEvent;
use crate::verification::scoped::resolve_scoped_commands;
use crate::verification::service::verify_commit;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::TransitionAction;
use djinn_db::TaskRepository;
use djinn_db::retry::DEFAULT_MAX_TX_RETRIES;
use djinn_db::retry::retry_on_serialization_failure;
use djinn_db::{VerificationResultRepository, VerificationStepInsert};
use std::sync::Arc;

use super::*;

/// After this many consecutive verification failures, escalate to lead.
const VERIFICATION_ESCALATION_THRESHOLD: i64 = 3;

/// Minimum pipeline timeout floor — chosen to accommodate workspace-wide
/// `cargo test` + `cargo clippy` runs on medium-sized Rust projects.
///
/// Post-P8 cut-over the verification config lives in
/// `projects.environment_config.verification`, which does not (yet) model a
/// pipeline-level timeout. Every project falls back to this floor; when/if
/// the schema grows an explicit field this function can start consulting it.
// Sized to fit a COLD `cargo clippy --workspace --all-targets --all-features`
// (+ per-crate `cargo test --no-run`) in the verification Job pod, which on a
// fresh sccache can take far longer than the old 900s floor — a cold run that
// blew the floor was killed mid-compile (exit -1) and false-failed the task.
// Matches the verification/warm Job's own active-deadline (3600s) so the server
// polls for the full life of the pod rather than giving up early. Warm runs
// finish in minutes, well under this ceiling.
const MIN_PIPELINE_TIMEOUT_SECS: u64 = 3600;

/// Return the fixed pipeline timeout floor.
///
/// Retained as a named function so callers that previously threaded a
/// project-specific `verification_timeout_secs` keep their signature while
/// the environment-config schema catches up. Projects with heavier pipelines
/// that regularly bump into the floor should file follow-up work to add a
/// dedicated field; until then, bumping the constant is the knob.
fn compute_pipeline_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(MIN_PIPELINE_TIMEOUT_SECS)
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
                // `transition` internally wraps its body with
                // `retry_on_serialization_failure`/`DEFAULT_MAX_TX_RETRIES` so we
                // do not re-wrap here. On non-retryable failure we must surface
                // the loss loudly — the task is otherwise stuck in `verifying`
                // and the pipeline already failed.
                if let Err(transition_err) = repo
                    .transition(
                        &task_id,
                        TransitionAction::ReleaseVerification,
                        "agent-supervisor",
                        "system",
                        Some(&format!("verification pipeline error: {e}")),
                        None,
                    )
                    .await
                {
                    tracing::error!(
                        task_id = %task_id,
                        error = %transition_err,
                        "Failed to release task after verification pipeline error; task may stay in `verifying`"
                    );
                }
            }
            Err(_elapsed) => {
                tracing::error!(
                    task_id = %task_id,
                    timeout_secs = pipeline_timeout.as_secs(),
                    "Verification pipeline timed out; releasing task"
                );
                let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                // See note above: `transition` already retries internally.
                if let Err(transition_err) = repo
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
                    .await
                {
                    tracing::error!(
                        task_id = %task_id,
                        error = %transition_err,
                        "Failed to release task after verification pipeline timeout; task may stay in `verifying`"
                    );
                }
            }
        }

        if let Ok(task) = load_task(&task_id, &app_state).await
            && let Some(coordinator) = app_state.coordinator().await
        {
            // Redispatch is state-bearing: if the coordinator ack fails (actor
            // dead, channel closed) the project sits idle until something else
            // nudges it. Log loudly so an outage is visible.
            if let Err(coord_err) = coordinator
                .trigger_dispatch_for_project(&task.project_id)
                .await
            {
                tracing::error!(
                    task_id = %task_id,
                    project_id = %task.project_id,
                    error = %coord_err,
                    "Failed to trigger project redispatch after verification completion; project may stay idle"
                );
            }
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
    let pipeline_timeout = compute_pipeline_timeout();
    // Detach a tiny admission task first so durable administrative dispatch
    // pauses are checked before registering or spawning the host verification
    // pipeline. The task is tied to a concrete task/project/user, so scoped
    // pauses are honored here in addition to the proposal-required global gate.
    std::mem::drop(tokio::spawn(async move {
        let task_for_gate = match load_task(&task_id, &app_state).await {
            Ok(task) => task,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "Verification pipeline deferred — failed to load task before dispatch-pause gate"
                );
                return;
            }
        };

        let pause_state = match crate::dispatch_pause::load_dispatch_pause_state(
            app_state.db.clone(),
            app_state.event_bus.clone(),
        )
        .await
        {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_for_gate.short_id,
                    task_uuid = %task_for_gate.id,
                    project_id = %task_for_gate.project_id,
                    error = %e,
                    "Verification pipeline deferred — failed to load dispatch-pause state"
                );
                return;
            }
        };

        if let Some((pause_scope, pause_target_id, pause)) =
            crate::dispatch_pause::matching_task_dispatch_pause(&pause_state, &task_for_gate)
        {
            tracing::info!(
                task_id = %task_for_gate.short_id,
                task_uuid = %task_for_gate.id,
                project_id = %task_for_gate.project_id,
                created_by_user_id = ?task_for_gate.created_by_user_id,
                pause_scope,
                pause_target_id,
                paused_by = %pause.paused_by,
                paused_at = %pause.paused_at,
                reason = %pause.reason,
                "Verification pipeline deferred by administrative dispatch pause"
            );
            return;
        }

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

        // Detach the verification `JoinHandle` — the pipeline owns its own
        // lifetime via `VerificationRegistrationGuard` and we have no further
        // work to do here. This is intentional fire-and-forget of a tokio task
        // handle, not a state-bearing drop.
        std::mem::drop(spawn_verification_with_timeout(
            task_id,
            app_state,
            pipeline_timeout,
            pipeline,
        ));
    }));
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

    // Verification commands need the project's toolchain + the shared `/cache`
    // PVC, neither of which exists on the toolchain-less djinn-server host —
    // running them inline there exits 127 ("cargo: not found") and false-fails
    // every task. On the Kubernetes runtime we dispatch a one-shot Job in the
    // project's image that runs the SAME pipeline (`verify_commit`) against the
    // task branch's tree, with `/cache` shared, and poll its `verification_runs`
    // row. On non-Kubernetes runtimes (dev/test) we keep the inline host path so
    // unit tests and local dev still work.
    let result = match crate::runtime_bridge::runtime_kind() {
        crate::runtime_bridge::RuntimeKind::Kubernetes => {
            run_verification_in_pod(&task, app_state).await?
        }
        crate::runtime_bridge::RuntimeKind::Test => {
            run_verification_on_host(&task, app_state).await?
        }
    };
    emit_verification_steps(&task.project_id, Some(task_id), &result, app_state).await;

    if !result.passed {
        let feedback = format_verification_failure_feedback(&result);
        tracing::info!(task_id = %task_id, "Verification: verification commands failed");
        handle_verification_failure(task_id, &feedback, &task_repo, app_state).await;
        return Ok(());
    }

    // All passed — transition to needs_task_review.
    tracing::info!(task_id = %task_id, "Verification: all commands passed");
    // `transition` already retries internally on 40001/40P01. A non-retryable
    // failure here would leave the task stuck in `verifying` even though the
    // pipeline succeeded — surface it loudly.
    if let Err(e) = task_repo
        .transition(
            task_id,
            TransitionAction::VerificationPass,
            "agent-supervisor",
            "system",
            None,
            None,
        )
        .await
    {
        tracing::error!(
            task_id = %task_id,
            error = %e,
            "Failed to transition task to `needs_task_review` after verification pass; task may stay in `verifying`"
        );
    }
    Ok(())
}

/// Inline (host) verification — the dev/test path.
///
/// Clones the target branch into a mirror-native ephemeral workspace, fetches +
/// checks out the task branch so verification sees the same tree the worker
/// pushed, normalizes tracked-file mtimes (cargo-cache reuse), resolves the
/// scoped commands, and runs `verify_commit` directly on the host. Only used on
/// the non-Kubernetes runtime — the production path runs the same work in a pod
/// (see [`run_verification_in_pod`]).
async fn run_verification_on_host(
    task: &djinn_core::models::Task,
    app_state: &AgentContext,
) -> anyhow::Result<crate::verification::service::VerificationResult> {
    let mirror = app_state.mirror.as_ref().ok_or_else(|| {
        anyhow::anyhow!("verification requires a MirrorManager on AgentContext; none configured")
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

    // Reset tracked-file mtimes to their last-touched commit time so the
    // verification build can reuse the verification/warm-owned cargo target base
    // for byte-identical workspace crates instead of recompiling everything off
    // fresh checkout mtimes. Task-run pods use private target dirs and must not
    // write that base directly. Best-effort; never fails verification.
    workspace.normalize_mtimes().await;

    let commit_sha = resolve_head_commit(&workspace_path)?;

    // Resolve scoped verification commands (AC-1 through AC-7).
    let role_cmd_override = role_verification_command_for_task(task, app_state).await;
    let scoped_commands = resolve_scoped_commands(
        &app_state.db,
        Some(&task.project_id),
        &workspace_path,
        &target_branch,
        role_cmd_override.as_deref(),
    )
    .await;

    let result = verify_commit(
        &task.project_id,
        &commit_sha,
        &workspace_path,
        &app_state.db,
        &scoped_commands,
    )
    .await?;
    Ok(result)
}

/// How often [`run_verification_in_pod`] polls the `verification_runs` row for a
/// terminal status. The pipeline-level timeout (`compute_pipeline_timeout`) is
/// the real backstop — this just bounds how often we hit the DB while the Job
/// compiles + runs the project's verification commands.
const VERIFICATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Production (Kubernetes) verification — dispatch a one-shot Job in the
/// project's image and poll its `verification_runs` row.
///
/// The Job clones the target branch, fetches + checks out the task branch
/// (building the same tree the worker pushed) with `/cache` shared, then runs
/// the SAME `verify_commit` pipeline the host path runs — the worker writes
/// per-command results + pass/fail back to the row, which we reconstruct into a
/// [`crate::verification::service::VerificationResult`] for the shared
/// emit/transition tail.
///
/// The surrounding [`spawn_verification_with_timeout`] enforces the wall-clock
/// pipeline timeout (and releases the task on expiry), so the poll loop here
/// runs unbounded inside that budget.
/// Per-project verification gate: at most ONE verification pod per project at a
/// time. Concurrent verifications share the project's `CARGO_TARGET_DIR`, where
/// cargo takes an exclusive build-dir lock — so a second pod just blocks behind
/// the first anyway (observed: a verify pod stuck ~22min, partly serialized),
/// while burning a second pod's CPU/memory. Serializing keeps the single warm
/// target base uncontended (fast, cache-friendly) and bounds resource use.
/// In-process gate — correct for the single-replica VPS; a multi-replica
/// deployment would need a DB/advisory lock instead.
static VERIFICATION_PROJECT_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn project_verification_semaphore(project_id: &str) -> Arc<tokio::sync::Semaphore> {
    VERIFICATION_PROJECT_LOCKS
        .lock()
        .expect("verification locks mutex poisoned")
        .entry(project_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
}

async fn run_verification_in_pod(
    task: &djinn_core::models::Task,
    app_state: &AgentContext,
) -> anyhow::Result<crate::verification::service::VerificationResult> {
    // Serialize verification per project (one verify pod at a time). The permit
    // is held until this function returns — including on `?` early-return or
    // when the outer pipeline timeout cancels the future (drops the guard) — so
    // the next queued verification for the project proceeds promptly.
    let _verify_permit = project_verification_semaphore(&task.project_id)
        .acquire_owned()
        .await
        .map_err(|e| anyhow::anyhow!("verification semaphore closed: {e}"))?;

    let runtime_ops = app_state.runtime_ops.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "verification requires a RuntimeOps bridge on AgentContext to dispatch the pod; none configured"
        )
    })?;

    let target_branch = default_target_branch(&task.project_id, app_state).await;
    let task_branch = format!("task/{}", task.short_id);

    let run_id = uuid::Uuid::now_v7().to_string();
    let run_repo = djinn_db::VerificationRunRepository::new(app_state.db.clone());
    run_repo
        .create(&run_id, &task.id, &task.project_id)
        .await
        .map_err(|e| anyhow::anyhow!("verification create run row: {e}"))?;

    // Dispatch the one-shot Job (project image → clone target → fetch+checkout
    // task branch → run verify_commit → write outcome to the row). On dispatch
    // failure mark the row errored so the poll loop sees a terminal state.
    if let Err(e) = runtime_ops
        .dispatch_verification(&run_id, &task.project_id, &task_branch, &target_branch)
        .await
    {
        let _ = run_repo
            .complete(
                &run_id,
                djinn_db::VerificationRunStatus::ERROR,
                "[]",
                "[]",
                Some(&e),
            )
            .await;
        anyhow::bail!("verification dispatch failed: {e}");
    }

    // Poll the row until terminal. The outer pipeline timeout caps total wait.
    loop {
        tokio::time::sleep(VERIFICATION_POLL_INTERVAL).await;
        let run = run_repo
            .get(&run_id)
            .await
            .map_err(|e| anyhow::anyhow!("verification poll run row: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!("verification run row {run_id} disappeared before completion")
            })?;
        match run.status.as_str() {
            djinn_db::VerificationRunStatus::PENDING
            | djinn_db::VerificationRunStatus::RUNNING => continue,
            djinn_db::VerificationRunStatus::ERROR => {
                anyhow::bail!(
                    "verification pod errored: {}",
                    run.error.as_deref().unwrap_or("unknown error")
                );
            }
            djinn_db::VerificationRunStatus::PASSED
            | djinn_db::VerificationRunStatus::FAILED => {
                let passed = run.status == djinn_db::VerificationRunStatus::PASSED;
                let setup_results: Vec<djinn_core::commands::CommandResult> =
                    serde_json::from_str(&run.setup_results).unwrap_or_default();
                let verification_results: Vec<djinn_core::commands::CommandResult> =
                    serde_json::from_str(&run.verification_results).unwrap_or_default();
                let total_duration_ms: u64 = setup_results
                    .iter()
                    .chain(verification_results.iter())
                    .map(|r| r.duration_ms)
                    .sum();
                return Ok(crate::verification::service::VerificationResult {
                    passed,
                    // The pod path always runs the commands (the pass-cache lives
                    // in `verify_commit` inside the pod); from the server's view
                    // this is a fresh, uncached result set.
                    cached: false,
                    setup_results,
                    verification_results,
                    total_duration_ms,
                });
            }
            other => {
                anyhow::bail!("verification run row has unexpected status `{other}`");
            }
        }
    }
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
    // `log_activity` does not internally retry serialization failures. The
    // operation is idempotent at the row level (each retry just inserts a
    // fresh activity row), so wrap the call in the existing
    // serialization/deadlock retry helper. A persistent failure means the
    // worker won't see the verification feedback on rework — warn loudly.
    let log_result = retry_on_serialization_failure(DEFAULT_MAX_TX_RETRIES, || {
        let payload = payload.clone();
        let task_id_owned = task_id.to_owned();
        async move {
            task_repo
                .log_activity(
                    Some(&task_id_owned),
                    "agent-supervisor",
                    "verification",
                    "comment",
                    &payload,
                )
                .await
        }
    })
    .await;
    if let Err(e) = log_result {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "Failed to log verification failure activity; worker rework will not see the feedback"
        );
    }

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
        // `transition` already retries internally on 40001/40P01; a persistent
        // failure here would leave the task in `verifying` — surface it.
        if let Err(e) = task_repo
            .transition(
                task_id,
                TransitionAction::Escalate,
                "agent-supervisor",
                "system",
                Some(&reason),
                None,
            )
            .await
        {
            tracing::error!(
                task_id = %task_id,
                error = %e,
                "Failed to escalate task to `needs_lead_intervention` after consecutive verification failures; task may stay in `verifying`"
            );
        }
    } else {
        // Normal path: transition to open for re-dispatch to worker.
        // See note above: `transition` already retries internally.
        if let Err(e) = task_repo
            .transition(
                task_id,
                TransitionAction::VerificationFail,
                "agent-supervisor",
                "system",
                Some(feedback),
                None,
            )
            .await
        {
            tracing::error!(
                task_id = %task_id,
                error = %e,
                "Failed to transition task to `open` after verification failure; task may stay in `verifying` and the worker will not be re-dispatched"
            );
        }
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
        // Fire-and-forget event bus emission: state-bearing results are
        // persisted below via `replace_for_task`; this notification is a
        // best-effort progress signal to UI subscribers and is intentionally
        // non-blocking.
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
        // Fire-and-forget event bus emission — see comment above.
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
        let repo = Arc::new(VerificationResultRepository::new(app_state.db.clone()));
        // `replace_for_task` does not internally retry on 40001/40P01. The
        // operation is safe to re-run (delete-then-insert on a fresh
        // transaction) so wrap it in the existing serialization/deadlock
        // retry helper before logging on persistent failure.
        // `VerificationStepInsert` is not `Clone`, so share ownership of the
        // row set via `Arc` for the retry closure. `repo` is also `Arc`-ed
        // because the `FnMut` closure is invoked multiple times.
        let db_rows = Arc::new(db_rows);
        let persist_result = retry_on_serialization_failure(DEFAULT_MAX_TX_RETRIES, || {
            let db_rows = Arc::clone(&db_rows);
            let tid = tid.to_owned();
            let repo = Arc::clone(&repo);
            async move { repo.replace_for_task(&tid, &db_rows).await }
        })
        .await;
        if let Err(e) = persist_result {
            tracing::warn!(
                task_id = %tid,
                error = %e,
                "Failed to persist verification results after retry; frontend will fall back to live re-fetch"
            );
        }
    }
}

/// Max chars per stdout/stderr field in verification feedback.
/// Keeps the activity log entry and downstream prompts reasonable.
const MAX_OUTPUT_CHARS: usize = 3000;

/// Overall cap on the error-line distillation for one stream (well under the
/// ~16KB budget the worker-rework prompt can afford for failure feedback).
const MAX_DISTILLED_CHARS: usize = 8000;

/// Distill a build/test stream down to its actionable error lines.
///
/// Worker rework only needs the *errors*, not the full (often megabyte) build
/// log: compiler diagnostics, panics, and test failures. We keep the lines that
/// carry those signals (plus a couple of trailing context lines after each, so
/// a multi-line `error[E…]` block stays readable) and drop the noise. When no
/// such lines are found — or distillation would itself overflow — we fall back
/// to the head+tail `smart_truncate` so the caller never loses everything.
fn distill_error_lines(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }

    // Signals that mark a line as worth keeping. Lowercased compare.
    const NEEDLES: &[&str] = &[
        "error",
        "error[",
        "warning:",
        "failed",
        "failure",
        "panicked",
        "assertion",
        "fatal",
        "cannot find",
        "expected",
        "undefined",
        "unresolved",
        "test result:",
    ];

    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        let lc = line.to_ascii_lowercase();
        if NEEDLES.iter().any(|n| lc.contains(n)) {
            keep[i] = true;
            // Keep up to two trailing lines so a multi-line diagnostic block
            // (the ` --> file:line` / ` | ` continuation rust emits) survives.
            let ctx_end = (i + 3).min(lines.len());
            for slot in &mut keep[(i + 1)..ctx_end] {
                *slot = true;
            }
        }
    }

    let distilled: String = lines
        .iter()
        .zip(keep.iter())
        .filter_map(|(line, &k)| k.then_some(*line))
        .collect::<Vec<_>>()
        .join("\n");

    if distilled.trim().is_empty() {
        // No recognizable error lines — fall back to head+tail truncation so the
        // worker still gets the tail (where results/errors usually land).
        return crate::truncate::smart_truncate(text, MAX_OUTPUT_CHARS);
    }
    // Cap the distillation itself (a pathological run could emit thousands of
    // warning lines); smart_truncate preserves the head and the conclusive tail.
    crate::truncate::smart_truncate(&distilled, MAX_DISTILLED_CHARS)
}

fn format_verification_failure_feedback(
    result: &crate::verification::service::VerificationResult,
) -> String {
    let failed = result
        .setup_results
        .iter()
        .chain(result.verification_results.iter())
        .find(|r| r.exit_code != 0);
    if let Some(cmd) = failed {
        // Distill to actionable error lines (capped) rather than dumping the raw
        // build output — the worker rework prompt only needs the diagnostics.
        let stdout = distill_error_lines(&cmd.stdout);
        let stderr = distill_error_lines(&cmd.stderr);
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
    use djinn_db::{DispatchPauseRepository, DispatchPauseTarget, TaskRepository};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    async fn tick_spawned_verification() {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
    }

    async fn tick_spawned_verification_yield_only() {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    fn dispatch_pause() -> djinn_core::models::DispatchPause {
        djinn_core::models::DispatchPause {
            paused_by: "admin".to_owned(),
            paused_at: "2026-06-12T00:00:00Z".to_owned(),
            reason: "maintenance".to_owned(),
            expires_at: None,
        }
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
    fn distill_keeps_error_lines_and_drops_noise() {
        // A long build log where the actionable errors are buried in the middle.
        let mut lines = vec!["   Compiling foo v0.1.0".to_string()];
        for i in 0..500 {
            lines.push(format!("   Compiling crate_{i} v0.1.0"));
        }
        lines.push("error[E0308]: mismatched types".to_string());
        lines.push("   --> src/lib.rs:42:5".to_string());
        lines.push("    |".to_string());
        for i in 0..500 {
            lines.push(format!("    warning noise filler line {i}"));
        }
        let text = lines.join("\n");

        let distilled = distill_error_lines(&text);
        // The buried compiler error and its context line survive.
        assert!(distilled.contains("error[E0308]: mismatched types"));
        assert!(distilled.contains("src/lib.rs:42:5"));
        // The vast majority of plain "Compiling" noise is dropped.
        assert!(
            !distilled.contains("Compiling crate_250"),
            "noise line should be dropped"
        );
        // And it stays well under the distillation cap (and the 16KB budget).
        assert!(distilled.len() <= MAX_DISTILLED_CHARS + 200);
    }

    #[test]
    fn distill_falls_back_to_truncation_when_no_error_lines() {
        // No recognizable error tokens → fall back to head+tail truncation so we
        // never lose everything.
        let text = "a".repeat(10_000);
        let distilled = distill_error_lines(&text);
        assert!(distilled.len() < 7_000);
        assert!(distilled.contains("truncated") || distilled.contains("omitted"));
    }

    #[test]
    fn distill_empty_is_empty() {
        assert_eq!(distill_error_lines(""), "");
        assert_eq!(distill_error_lines("   \n  \n"), "");
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

    #[test]
    fn compute_pipeline_timeout_returns_minimum_floor() {
        // Post-P8 cut-over the environment-config schema does not model a
        // pipeline-level timeout, so every project gets the floor.
        let timeout = compute_pipeline_timeout();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_verification_defers_without_registering_when_global_pause_is_active() {
        let (_task_repo, task_id, app_state) = setup_verifying_task_with_count(0).await;
        let pause_repo =
            DispatchPauseRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        pause_repo
            .pause(DispatchPauseTarget::global(), dispatch_pause())
            .await
            .expect("pause dispatch globally");

        spawn_verification(
            task_id.clone(),
            "/unused/project/path".to_owned(),
            app_state.clone(),
        );
        tick_spawned_verification_yield_only().await;
        tick_spawned_verification_yield_only().await;

        assert!(
            !app_state.has_verification(&task_id),
            "global dispatch pause must prevent registering/spawning host verification work"
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
        // JoinError is expected when we just aborted the task — the only
        // outcome we care about is the registration guard running. Drop the
        // join error intentionally.
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

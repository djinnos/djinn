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
                        TransitionAction::Release,
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
                        TransitionAction::Release,
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
/// 4. On pass: transitions to `needs_task_review`
/// 5. On fail: logs the failure as an activity comment, transitions to `open`
/// 6. Cleans up the worktree
/// 7. Triggers redispatch for the project
#[allow(dead_code)]
pub(crate) fn spawn_verification(task_id: String, project_path: String, app_state: AgentContext) {
    spawn_verification_with_in_pod_run(task_id, project_path, app_state, None);
}

/// Like [`spawn_verification`] but carries the optional `verification_runs.id`
/// of an IN-POD verification the worker already ran (and wrote terminal) before
/// its Cargo target dir was torn down. When present, the Kubernetes path
/// CONSUMES that row directly instead of dispatching a second verify Job — the
/// double-compile fix. `None` (or a non-terminal in-pod row) falls through to
/// the standalone verification path (the separate verify Job).
#[allow(dead_code)]
pub(crate) fn spawn_verification_with_in_pod_run(
    task_id: String,
    project_path: String,
    app_state: AgentContext,
    in_pod_run_id: Option<String>,
) {
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
        let in_pod_run_id_for_pipeline = in_pod_run_id.clone();
        let pipeline = async move {
            run_verification_pipeline(
                &task_id_for_pipeline,
                &project_path_for_pipeline,
                &app_state_for_pipeline,
                in_pod_run_id_for_pipeline,
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
    in_pod_run_id: Option<String>,
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
    //
    // FAST PATH: the worker already ran the verification IN-POD (reusing its
    // freshly compiled artifacts) and shipped the terminal `verification_runs`
    // row id on its report. Consume that row directly — no second pod, no
    // re-seed, no re-compile. Falls through to dispatching a Job (the unchanged
    // fallback) when the in-pod row is missing / non-terminal.
    let result = match crate::runtime_bridge::runtime_kind() {
        crate::runtime_bridge::RuntimeKind::Kubernetes => {
            match consume_in_pod_verification_run(&task, in_pod_run_id.as_deref(), app_state).await
            {
                Some(result) => result,
                None => run_verification_in_pod(&task, app_state).await?,
            }
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
            TransitionAction::SubmitTaskReview,
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

/// Initial backoff for the image-readiness requeue loop in
/// [`run_verification_in_pod`]. A short initial value (5s) lets a quick
/// rebuild (typical mirror-fetch + image-controller round-trip) land before
/// we burn a second DB read; the loop doubles up to a cap below.
const IMAGE_READINESS_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum backoff between image-readiness retries. Capped at 60s so a long
/// rebuild doesn't push the requeue cadence into multi-minute silent windows
/// — the next tick still re-checks the dispatch image status within a
/// minute.
const IMAGE_READINESS_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Total bound on how long [`run_verification_in_pod`] will keep retrying
/// dispatch while the catalog image is transiently `building` (or
/// not-yet-assigned). After this elapsed time the verification is marked
/// `error` with a clear message; the surrounding verification pipeline
/// timeout (`MIN_PIPELINE_TIMEOUT_SECS`) is the outer bound, so 10 minutes
/// here is well within the 1-hour pipeline budget. Sized to comfortably
/// outlast a typical image rebuild (a few minutes; see
/// `[[cases/plan-a-warm-cargo-base-reuse-validated-working-v0-6-11-0-6-12]]`)
/// while still failing closed if the image genuinely never lands.
const IMAGE_READINESS_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

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

/// Dispatch a verification Job, retrying on transient `ImageNotReady` until
/// the bounded budget elapses or the image becomes ready.
///
/// The pre-fix behavior marked the `verification_runs` row as `error` the
/// moment the catalog image was observed `building` — typically a few-second
/// transient window during a routine rebuild (see [[ga7l]]). A verification
/// should not terminally fail a task for a transient image condition; the
/// whole point of having a shared catalog image is that the image is
/// reusable, so a momentary mid-rebuild is not the verification's problem
/// to surface.
///
/// Strategy: when the runtime returns `ImageNotReady { transient: true, .. }`
/// we DO NOT mark the row as `error` — we leave it in `pending` and re-poll
/// `resolve_dispatch_image` with exponential backoff (capped). When the
/// image lands (catalog image is `ready` again) the dispatch is retried
/// transparently. After [`IMAGE_READINESS_TOTAL_BUDGET`] the retry loop
/// gives up and surfaces a clear terminal error: this preserves the
/// backstop the acceptance criteria require ("a real, persistent
/// missing-image still surfaces a clear error after the bounded wait").
///
/// Returns `Ok(())` on successful dispatch (the Job is created in the
/// cluster). Returns `Err` (a clear terminal error) for:
/// * `ImageNotReady { transient: false, .. }` (project has no catalog
///   image assigned at all — operator must fix configuration),
/// * `Backend(_)` (K8s API / dispatcher failure),
/// * `ImageNotReady { transient: true, .. }` after the bounded budget
///   elapses (image genuinely never landed in time).
///
/// The verification `_run_id` row is updated in place on the terminal
/// failure path so the poll loop can observe a clear status without
/// re-querying the row.
/// Outcome of [`dispatch_verification_inner`]. The caller is responsible
/// for persisting the row status if the outcome is [`DispatchOutcome::Error`].
#[derive(Debug)]
enum DispatchOutcome {
    /// The dispatch succeeded — the Job is created in the cluster.
    Dispatched,
    /// A terminal error. The caller should mark the verification row as
    /// `error` with this message, then propagate the error.
    Error(String),
}

/// Inner retry loop for verification dispatch — stubbed after the
/// verification pre-PR gate was removed. Always returns an error so the
/// surrounding pipeline surfaces a clear message.
async fn dispatch_verification_inner<F, Fut>(
    _runtime_ops: &dyn djinn_control_plane::bridge::RuntimeOps,
    _run_id: &str,
    _project_id: &str,
    _task_branch: &str,
    _target_branch: &str,
    mark_error: &mut F,
) -> DispatchOutcome
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let msg =
        "verification dispatch removed: verification pre-PR gate is being retired".to_string();
    mark_error(&msg).await;
    DispatchOutcome::Error(msg)
}

async fn dispatch_verification_with_retry(
    runtime_ops: &dyn djinn_control_plane::bridge::RuntimeOps,
    run_id: &str,
    project_id: &str,
    task_branch: &str,
    target_branch: &str,
    run_repo: &djinn_db::VerificationRunRepository,
) -> anyhow::Result<()> {
    let run_id_owned = run_id.to_string();
    let mut mark_error = |msg: &str| {
        let run_id_owned = run_id_owned.clone();
        let msg_owned = msg.to_string();
        async move {
            let _ = run_repo
                .complete(
                    &run_id_owned,
                    djinn_db::VerificationRunStatus::ERROR,
                    "[]",
                    "[]",
                    Some(&msg_owned),
                )
                .await;
        }
    };
    match dispatch_verification_inner(
        runtime_ops,
        run_id,
        project_id,
        task_branch,
        target_branch,
        &mut mark_error,
    )
    .await
    {
        DispatchOutcome::Dispatched => Ok(()),
        DispatchOutcome::Error(msg) => anyhow::bail!(msg),
    }
}

/// FAST PATH: consume a terminal `verification_runs` row the worker wrote
/// IN-POD (right after committing, reusing its already-compiled Cargo target
/// dir). Returns `Some(result)` only when:
///   - a `run_id` was carried on the worker's report,
///   - the row exists and belongs to THIS task (guards against a stale id from a
///     prior cycle re-running against the wrong task), and
///   - the row reached a terminal state (`passed` / `failed`).
///
/// An `error` row, a missing/non-terminal row, or a task-id mismatch returns
/// `None`, so the caller falls back to dispatching the separate verify Job
/// (the unchanged path) — never silently passing a task whose in-pod verify
/// errored.
async fn consume_in_pod_verification_run(
    task: &djinn_core::models::Task,
    in_pod_run_id: Option<&str>,
    app_state: &AgentContext,
) -> Option<crate::verification::service::VerificationResult> {
    let run_id = in_pod_run_id?;
    let run_repo = djinn_db::VerificationRunRepository::new(app_state.db.clone());
    let run = match run_repo.get(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            tracing::warn!(
                task_id = %task.short_id,
                run_id,
                "in-pod verification row missing; dispatching verify Job (fallback)"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                run_id,
                error = %e,
                "failed to read in-pod verification row; dispatching verify Job (fallback)"
            );
            return None;
        }
    };

    if run.task_id != task.id {
        tracing::warn!(
            task_id = %task.short_id,
            run_id,
            row_task_id = %run.task_id,
            "in-pod verification row belongs to a different task; dispatching verify Job (fallback)"
        );
        return None;
    }

    match run.status.as_str() {
        djinn_db::VerificationRunStatus::PASSED | djinn_db::VerificationRunStatus::FAILED => {
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
            tracing::info!(
                task_id = %task.short_id,
                run_id,
                passed,
                "consuming in-pod verification result (reused worker artifacts; no second pod)"
            );
            Some(crate::verification::service::VerificationResult {
                passed,
                // The pod ran the commands; from the server's view this is a
                // fresh result set, same as the Job-dispatch path.
                cached: false,
                setup_results,
                verification_results,
                total_duration_ms,
            })
        }
        other => {
            // `error`, `pending`, or `running` — don't trust it; re-run via Job.
            tracing::warn!(
                task_id = %task.short_id,
                run_id,
                status = other,
                "in-pod verification row not in a usable terminal state; dispatching verify Job (fallback)"
            );
            None
        }
    }
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
    //
    // `dispatch_verification_with_retry` handles the transient
    // `ImageNotReady` requeue itself — including keeping the row in
    // `pending` (NOT `error`) while it waits. Only persistent failures
    // (permanent missing image, K8s backend error, bounded budget
    // exhaustion) propagate out of this call and result in a terminal
    // ERROR row + bail.
    dispatch_verification_with_retry(
        runtime_ops.as_ref(),
        &run_id,
        &task.project_id,
        &task_branch,
        &target_branch,
        &run_repo,
    )
    .await?;

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
            djinn_db::VerificationRunStatus::PENDING | djinn_db::VerificationRunStatus::RUNNING => {
                continue;
            }
            djinn_db::VerificationRunStatus::ERROR => {
                anyhow::bail!(
                    "verification pod errored: {}",
                    run.error.as_deref().unwrap_or("unknown error")
                );
            }
            djinn_db::VerificationRunStatus::PASSED | djinn_db::VerificationRunStatus::FAILED => {
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

    // Verification failure counting removed — transition to open for re-dispatch.
    if let Err(e) = task_repo
        .transition(
            task_id,
            TransitionAction::Release,
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
            "Failed to transition task after verification failure"
        );
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
#[path = "verification_tests.rs"]
mod tests;

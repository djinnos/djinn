use super::*;
use crate::test_helpers::{
    agent_context_from_db, create_test_db, create_test_epic, create_test_project, create_test_task,
    test_events,
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

fn setup_verifying_task_with_count_blocking(count: i64) -> (TaskRepository, String, AgentContext) {
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

// ── regression: image-not-ready requeues rather than erroring ──────────
//
// Pre-fix: a transient "image not ready" (catalog image mid-rebuild)
// was returned from `runtime_ops.dispatch_verification` as a
// generic `Backend` string. The slot actor's dispatch path saw the
// error and terminally marked the `verification_runs` row as
// `error`, then bailed out — failing the task for a transient
// condition. Post-fix: a structured
// `RuntimeDispatchError::ImageNotReady { transient: true, .. }`
// triggers a bounded requeue inside
// `dispatch_verification_with_retry`, keeping the row in `pending`
// while the image is being built.
//
// This test exercises that requeue path with a mock runtime that
// returns `ImageNotReady { transient: true }` for the first N
// attempts, then `Ok(())` once the image is "ready".

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

/// A `RuntimeOps` mock that records every `dispatch_verification`
/// call and replays a pre-scripted sequence of results. Used to
/// simulate a catalog image that flips from `building` to `ready`
/// mid-retry.
struct RequeueingRuntimeOps {
    /// Call counter — atomically incremented on every
    /// `dispatch_verification`.
    call_count: Arc<AtomicU32>,
    /// Number of leading calls that should return
    /// `ImageNotReady { transient: true }`. Subsequent calls
    /// return `Ok(())`.
    transient_calls: u32,
}

impl RequeueingRuntimeOps {
    fn new(transient_calls: u32) -> Self {
        Self {
            call_count: Arc::new(AtomicU32::new(0)),
            transient_calls,
        }
    }
}

#[async_trait::async_trait]
impl djinn_control_plane::bridge::RuntimeOps for RequeueingRuntimeOps {
    async fn apply_settings(&self, _: &djinn_core::models::DjinnSettings) -> Result<(), String> {
        Ok(())
    }
    async fn embed_memory_query(
        &self,
        _: &str,
    ) -> Result<Option<djinn_control_plane::bridge::SemanticQueryEmbedding>, String> {
        Ok(None)
    }
    async fn reset_runtime_settings(&self) {}
    async fn persist_model_health_state(&self) {}
    async fn apply_environment_config(
        &self,
        _: &str,
        _: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_mirror_refresh(&self, _: &str) {}
    async fn apply_user_model_change(&self) {}
    async fn dispatch_verification_test(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), djinn_control_plane::bridge::RuntimeDispatchError> {
        Ok(())
    }
    async fn dispatch_verification(
        &self,
        _run_id: &str,
        _project_id: &str,
        _task_branch: &str,
        _target_branch: &str,
    ) -> Result<(), djinn_control_plane::bridge::RuntimeDispatchError> {
        let n = self.call_count.fetch_add(1, AtomicOrdering::SeqCst);
        if n < self.transient_calls {
            // Simulate a catalog image mid-rebuild. The structured
            // `transient: true` flag is the key contract: the
            // requeue loop keys on it, NOT on parsing strings.
            Err(
                djinn_control_plane::bridge::RuntimeDispatchError::ImageNotReady {
                    project_id: "p1".into(),
                    image_status: "building".into(),
                    tag: Some("reg/img:abc".into()),
                    transient: true,
                },
            )
        } else {
            Ok(())
        }
    }
    async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_graph_warm(&self, _: &str) {}
    async fn provision_backing_service(
        &self,
        _: djinn_control_plane::bridge::ProvisionServiceRequest,
    ) -> Result<djinn_control_plane::bridge::ProvisionedService, String> {
        Err("not used".into())
    }
    async fn release_backing_service(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn list_taskrun_jobs(
        &self,
    ) -> Result<Vec<djinn_control_plane::bridge::TaskrunJobRef>, String> {
        Ok(Vec::new())
    }
    async fn cleanup_task_branches(&self, _: &str) {}
}

#[tokio::test(start_paused = true)]
async fn dispatch_verification_requeues_on_transient_image_not_ready() {
    // The pre-fix bug terminated the verification_run row with
    // `error` the moment it observed `ImageNotReady { transient:
    // true }`. The fix must keep the row in `pending` while the
    // requeue loop runs, and the dispatch must eventually succeed
    // once the runtime reports the image is ready.
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let run_repo = djinn_db::VerificationRunRepository::new(db.clone());
    let run_id = "run-requeue-1";
    run_repo
        .create(run_id, "task-1", &project.id)
        .await
        .expect("create verification run row");
    // Three transient failures, then success.
    let runtime = RequeueingRuntimeOps::new(3);
    let call_count = runtime.call_count.clone();

    // Run the retry helper. The mock sleeps are not involved —
    // `start_paused` lets `tokio::time::sleep` complete instantly,
    // and the exponential backoff stays bounded by the test wall
    // clock.
    let result = dispatch_verification_with_retry(
        &runtime,
        run_id,
        &project.id,
        "task/abc",
        "main",
        &run_repo,
    )
    .await;

    assert!(
        result.is_ok(),
        "dispatch_verification_with_retry must succeed once the image becomes ready, got {result:?}"
    );
    // 3 transient + 1 success = 4 calls. Use a >= bound to be
    // robust against the mock being called for any setup
    // housekeeping.
    let n = call_count.load(AtomicOrdering::SeqCst);
    assert!(
        n >= 4,
        "runtime must be polled at least 4 times (3 transient + 1 success), got {n}"
    );
    // The row MUST still be `pending` — `dispatch_verification_with_retry`
    // never marks the row errored on a transient not-ready;
    // the worker writes the terminal status once it actually runs.
    let row = run_repo
        .get(run_id)
        .await
        .expect("get row")
        .expect("row exists");
    assert_eq!(
        row.status,
        djinn_db::VerificationRunStatus::PENDING,
        "row must stay in `pending` while dispatch requeues (got {})",
        row.status
    );
    assert!(
        row.error.is_none(),
        "row must have no error while dispatch requeues"
    );
}

#[tokio::test(start_paused = true)]
async fn dispatch_verification_marks_error_on_permanent_image_not_ready() {
    // The permanent path: the project has no catalog image
    // assigned at all. The fix must mark the row errored
    // immediately so the surrounding pipeline surfaces a clear
    // failure to the worker (no requeue, no infinite wait).
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let run_repo = djinn_db::VerificationRunRepository::new(db.clone());
    let run_id = "run-permanent-1";
    run_repo
        .create(run_id, "task-1", &project.id)
        .await
        .expect("create verification run row");

    /// Always returns `ImageNotReady { transient: false, .. }` —
    /// i.e. a permanent "no catalog image" condition.
    struct PermanentRuntimeOps;
    #[async_trait::async_trait]
    impl djinn_control_plane::bridge::RuntimeOps for PermanentRuntimeOps {
        async fn apply_settings(
            &self,
            _: &djinn_core::models::DjinnSettings,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn embed_memory_query(
            &self,
            _: &str,
        ) -> Result<Option<djinn_control_plane::bridge::SemanticQueryEmbedding>, String> {
            Ok(None)
        }
        async fn reset_runtime_settings(&self) {}
        async fn persist_model_health_state(&self) {}
        async fn apply_environment_config(
            &self,
            _: &str,
            _: &djinn_stack::environment::EnvironmentConfig,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_mirror_refresh(&self, _: &str) {}
        async fn apply_user_model_change(&self) {}
        async fn dispatch_verification_test(
            &self,
            _: &str,
            _: &str,
        ) -> Result<(), djinn_control_plane::bridge::RuntimeDispatchError> {
            Ok(())
        }
        async fn dispatch_verification(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(), djinn_control_plane::bridge::RuntimeDispatchError> {
            Err(
                djinn_control_plane::bridge::RuntimeDispatchError::ImageNotReady {
                    project_id: "p1".into(),
                    image_status: "none".into(),
                    tag: None,
                    transient: false,
                },
            )
        }
        async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_graph_warm(&self, _: &str) {}
        async fn provision_backing_service(
            &self,
            _: djinn_control_plane::bridge::ProvisionServiceRequest,
        ) -> Result<djinn_control_plane::bridge::ProvisionedService, String> {
            Err("not used".into())
        }
        async fn release_backing_service(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_taskrun_jobs(
            &self,
        ) -> Result<Vec<djinn_control_plane::bridge::TaskrunJobRef>, String> {
            Ok(Vec::new())
        }
        async fn cleanup_task_branches(&self, _: &str) {}
    }

    let result = dispatch_verification_with_retry(
        &PermanentRuntimeOps,
        run_id,
        &project.id,
        "task/abc",
        "main",
        &run_repo,
    )
    .await;
    assert!(result.is_err(), "permanent missing-image must bail");
    // The row MUST be marked errored so the surrounding
    // verification pipeline (and the poll loop in
    // `run_verification_in_pod`) can observe a clear terminal
    // status.
    let row = run_repo
        .get(run_id)
        .await
        .expect("get row")
        .expect("row exists");
    assert_eq!(
        row.status,
        djinn_db::VerificationRunStatus::ERROR,
        "row must be marked errored on a permanent missing-image"
    );
    let err = row.error.unwrap_or_default();
    assert!(
        err.contains("no catalog image assigned"),
        "error message must be actionable for an operator, got: {err}"
    );
}

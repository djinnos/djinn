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
                        TransitionAction::SubmitTaskReview,
                        "test",
                        "system",
                        None,
                        None,
                    )
                    .await
                    .expect("transition to needs_task_review");

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
            TransitionAction::SubmitTaskReview,
            "test",
            "system",
            None,
            None,
        )
        .await
        .expect("transition to needs_task_review");

    (task_repo, task.id, app_state)
}

#[test]
fn compute_pipeline_timeout_returns_minimum_floor() {
    // Post-P8 cut-over the environment-config schema does not model a
    // pipeline-level timeout, so every project gets the floor.
    let timeout = compute_pipeline_timeout();
    assert_eq!(timeout, Duration::from_secs(MIN_PIPELINE_TIMEOUT_SECS));
}

/// FAST PATH: a terminal in-pod `verification_runs` row that belongs to the
/// task is consumed directly (its per-command results reconstructed),
/// skipping the separate verify Job — the double-compile fix.
#[tokio::test]
async fn consume_in_pod_run_returns_terminal_result() {
    let db = create_test_db();
    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;

    let repo = djinn_db::VerificationRunRepository::new(db.clone());
    let run_id = "vr-inpod-pass";
    repo.create(run_id, &task.id, &project.id).await.unwrap();
    let verify = r#"[{"name":"clippy","command":"cargo clippy","exit_code":0,"stdout":"","stderr":"","duration_ms":42}]"#;
    repo.complete(
        run_id,
        djinn_db::VerificationRunStatus::PASSED,
        "[]",
        verify,
        None,
    )
    .await
    .unwrap();

    let result = consume_in_pod_verification_run(&task, Some(run_id), &app_state)
        .await
        .expect("terminal in-pod row must be consumed");
    assert!(result.passed);
    assert!(!result.cached);
    assert_eq!(result.verification_results.len(), 1);
    assert_eq!(result.total_duration_ms, 42);
}

/// Bug 1 recovery detection: the supervisor infra-death seam and the
/// coordinator's stuck-`verifying` sweep both decide "re-arm vs reset to open"
/// by looking up the worker's terminal in-pod verification row via
/// `latest_terminal_for_task`. This pins that lookup: a `passed`/`failed` row
/// is found (drives re-arm), while `error`/`pending`/`running` rows are not (so
/// recovery falls through to releasing the task). Without a usable row the
/// completed work would be discarded.
#[tokio::test]
async fn latest_terminal_inpod_run_drives_rearm_decision() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let repo = djinn_db::VerificationRunRepository::new(db.clone());

    // No row yet → no re-arm; recovery would release to open.
    assert!(
        repo.latest_terminal_for_task(&task.id)
            .await
            .unwrap()
            .is_none(),
        "no verification row → nothing to re-arm against"
    );

    // A `running` in-pod row (verify in flight, no terminal write) is NOT a
    // recoverable artifact.
    repo.create("vr-running", &task.id, &project.id)
        .await
        .unwrap();
    repo.mark_running("vr-running").await.unwrap();
    assert!(
        repo.latest_terminal_for_task(&task.id)
            .await
            .unwrap()
            .is_none(),
        "a non-terminal running row must not drive re-arm"
    );

    // A terminal `passed` row IS the recoverable artifact the seams re-arm on.
    repo.create("vr-passed", &task.id, &project.id)
        .await
        .unwrap();
    repo.complete(
        "vr-passed",
        djinn_db::VerificationRunStatus::PASSED,
        "[]",
        "[]",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        repo.latest_terminal_for_task(&task.id)
            .await
            .unwrap()
            .as_deref(),
        Some("vr-passed"),
        "a terminal passed row must be detected so the host pipeline is re-armed \
         instead of discarding the completed work"
    );
}

/// FALLBACK selection: no in-pod run id on the report → `None` so the caller
/// dispatches the separate verify Job (unchanged behavior).
#[tokio::test]
async fn consume_in_pod_run_none_when_no_run_id() {
    let db = create_test_db();
    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;

    assert!(
        consume_in_pod_verification_run(&task, None, &app_state)
            .await
            .is_none(),
        "absent in-pod run id must fall back to the Job"
    );
}

/// FALLBACK selection: an `error`/non-terminal row is NOT trusted — returns
/// `None` so the caller re-runs via the Job rather than silently passing.
#[tokio::test]
async fn consume_in_pod_run_none_on_error_or_mismatch() {
    let db = create_test_db();
    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let other_task = create_test_task(&db, &project.id, &epic.id).await;

    let repo = djinn_db::VerificationRunRepository::new(db.clone());

    // (a) error row → not usable.
    repo.create("vr-err", &task.id, &project.id).await.unwrap();
    repo.complete(
        "vr-err",
        djinn_db::VerificationRunStatus::ERROR,
        "[]",
        "[]",
        Some("boom"),
    )
    .await
    .unwrap();
    assert!(
        consume_in_pod_verification_run(&task, Some("vr-err"), &app_state)
            .await
            .is_none(),
        "errored in-pod row must fall back to the Job"
    );

    // (b) missing row id → not usable.
    assert!(
        consume_in_pod_verification_run(&task, Some("does-not-exist"), &app_state)
            .await
            .is_none(),
        "missing in-pod row must fall back to the Job"
    );

    // (c) terminal row that belongs to a DIFFERENT task → not usable.
    repo.create("vr-other", &other_task.id, &project.id)
        .await
        .unwrap();
    repo.complete(
        "vr-other",
        djinn_db::VerificationRunStatus::PASSED,
        "[]",
        "[]",
        None,
    )
    .await
    .unwrap();
    assert!(
        consume_in_pod_verification_run(&task, Some("vr-other"), &app_state)
            .await
            .is_none(),
        "a row for another task must not satisfy this task's verification"
    );
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

use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

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
    //
    // This test calls `dispatch_verification_inner` directly with an
    // in-memory `mark_error` callback, avoiding the sqlx connection
    // pool which is incompatible with `start_paused = true` (the pool's
    // internal acquire timeout fires instantly when the clock is frozen,
    // causing spurious `PoolTimedOut` failures).

    // Three transient failures, then success.
    let runtime = RequeueingRuntimeOps::new(3);
    let call_count = runtime.call_count.clone();

    // `mark_error` captures any error message that the inner function
    // tries to persist. On the transient→success path this should
    // NEVER be called — the row stays in `pending`.
    let captured_error = Arc::new(Mutex::new(None::<String>));
    let captured_error_clone = captured_error.clone();
    let mut mark_error = move |msg: &str| {
        let captured_error_clone = captured_error_clone.clone();
        let msg_owned = msg.to_string();
        async move {
            *captured_error_clone.lock().unwrap() = Some(msg_owned);
        }
    };

    let outcome = dispatch_verification_inner(
        &runtime,
        "run-requeue-1",
        "p1",
        "task/abc",
        "main",
        &mut mark_error,
    )
    .await;

    assert!(
        matches!(outcome, DispatchOutcome::Dispatched),
        "dispatch must succeed once the image becomes ready, got {outcome:?}"
    );
    // 3 transient + 1 success = 4 calls. Use a >= bound to be
    // robust against the mock being called for any setup
    // housekeeping.
    let n = call_count.load(AtomicOrdering::SeqCst);
    assert!(
        n >= 4,
        "runtime must be polled at least 4 times (3 transient + 1 success), got {n}"
    );
    // The mark_error callback MUST NOT have been called — the
    // transient→success path never marks the row errored; the
    // worker writes the terminal status once it actually runs.
    assert!(
        captured_error.lock().unwrap().is_none(),
        "mark_error must not be called on a transient→success path"
    );
}

#[tokio::test(start_paused = true)]
async fn dispatch_verification_marks_error_on_permanent_image_not_ready() {
    // The permanent path: the project has no catalog image
    // assigned at all. The fix must mark the row errored
    // immediately so the surrounding pipeline surfaces a clear
    // failure to the worker (no requeue, no infinite wait).
    //
    // Uses `dispatch_verification_inner` directly to avoid the sqlx
    // connection pool, which is incompatible with `start_paused = true`.

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

    let captured_error = Arc::new(Mutex::new(None::<String>));
    let captured_error_clone = captured_error.clone();
    let mut mark_error = move |msg: &str| {
        let captured_error_clone = captured_error_clone.clone();
        let msg_owned = msg.to_string();
        async move {
            *captured_error_clone.lock().unwrap() = Some(msg_owned);
        }
    };

    let outcome = dispatch_verification_inner(
        &PermanentRuntimeOps,
        "run-permanent-1",
        "p1",
        "task/abc",
        "main",
        &mut mark_error,
    )
    .await;
    assert!(
        matches!(outcome, DispatchOutcome::Error(_)),
        "permanent missing-image must produce an error outcome"
    );
    // The mark_error callback MUST have been called so the surrounding
    // verification pipeline (and the poll loop in
    // `run_verification_in_pod`) can observe a clear terminal status.
    let err = captured_error.lock().unwrap().clone().unwrap_or_default();
    assert!(
        err.contains("no catalog image assigned"),
        "error message must be actionable for an operator, got: {err}"
    );
}

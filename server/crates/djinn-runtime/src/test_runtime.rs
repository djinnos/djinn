//! Real in-process [`SessionRuntime`] used by integration tests.
//!
//! Phase 2 PR 4 — replaces the PR 1 synchronous stub with a runtime that
//! actually spawns a runner task on the current Tokio runtime, bridges its
//! terminal [`TaskRunReport`] onto a [`BiStream`], honours cooperative
//! cancellation, and cleans up on `teardown`.
//!
//! ## Layering note — why there is no direct `TaskRunSupervisor` reference
//!
//! `djinn-runtime` sits *below* `djinn-supervisor` in the dependency tree
//! (`djinn-supervisor` depends on `djinn-runtime` for the wire-capable spec
//! types).  Linking the supervisor back in — even as a dev-dep — introduces a
//! cycle that `cargo` rejects.
//!
//! The workaround is to parameterise [`TestRuntime`] over a [`TaskRunner`]
//! trait: the crate that *does* own `TaskRunSupervisor` (today that's
//! `djinn-agent::actors::slot::supervisor_runner`; tomorrow PR 7's dispatch
//! cutover) supplies a `TaskRunner` impl that closes over
//! `Arc<dyn SupervisorServices>` and calls `TaskRunSupervisor::run` inside.
//! The runtime crate stays oblivious to the supervisor body but still drives
//! the four-verb lifecycle (`prepare` / `attach_stdio` / `cancel` /
//! `teardown`) end-to-end.
//!
//! The tests in this module exercise that contract with a trivial fake
//! runner (no supervisor, no mirror) — enough to prove the runtime's
//! own plumbing, not the supervisor's.
//!
//! Gated behind `cfg(any(test, feature = "test-runtime"))` so production
//! binaries never link it.  The `test-runtime` feature already exists in
//! `Cargo.toml` (PR 1), so downstream integration tests can flip it without
//! any further crate surgery.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::handle::RunHandle;
use crate::session_runtime::{RuntimeError, SessionRuntime};
use crate::spec::{TaskRunOutcome, TaskRunReport, TaskRunSpec};
use crate::stream::{BiStream, StreamEvent};

/// Cooperative cancellation flag handed to a [`TaskRunner`].
///
/// Implemented as a `watch::Receiver<bool>` — the runner can `await` a flip
/// via `changed()` for push-style cancellation, or poll `borrow()` between
/// await points for pull-style.
#[derive(Clone, Debug)]
pub struct RunnerCancel {
    rx: watch::Receiver<bool>,
}

impl RunnerCancel {
    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Await the next transition on the cancellation flag.  Resolves as soon
    /// as the flag flips from `false` → `true`; also resolves if the sender
    /// is dropped (treat as cancelled).
    pub async fn cancelled(&mut self) {
        if *self.rx.borrow() {
            return;
        }
        let _ = self.rx.changed().await;
    }
}

/// Abstract "drive a task-run to a terminal report" hook.
///
/// Implemented by the crate that owns `TaskRunSupervisor` (see module-level
/// layering note) and by tests that only need a deterministic result.  The
/// runner is free to honour [`RunnerCancel`] cooperatively; [`TestRuntime`]
/// will also abort the spawned task on `cancel` to guarantee a bounded
/// teardown even if the runner ignores the flag.
#[async_trait]
pub trait TaskRunner: Send + Sync + 'static {
    async fn run(
        &self,
        spec: TaskRunSpec,
        cancel: RunnerCancel,
    ) -> Result<TaskRunReport, RuntimeError>;
}

/// Trivial adapter so closures satisfy [`TaskRunner`] without a newtype.
pub struct FnRunner<F> {
    f: F,
}

impl<F, Fut> FnRunner<F>
where
    F: Fn(TaskRunSpec, RunnerCancel) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<TaskRunReport, RuntimeError>> + Send + 'static,
{
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait]
impl<F, Fut> TaskRunner for FnRunner<F>
where
    F: Fn(TaskRunSpec, RunnerCancel) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<TaskRunReport, RuntimeError>> + Send + 'static,
{
    async fn run(
        &self,
        spec: TaskRunSpec,
        cancel: RunnerCancel,
    ) -> Result<TaskRunReport, RuntimeError> {
        (self.f)(spec, cancel).await
    }
}

/// Default grace period teardown waits for the spawned runner before
/// reporting a timeout error.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-run bookkeeping kept behind a `Mutex` so the object-safe
/// `&self`-only [`SessionRuntime`] methods can mutate it.
struct RunState {
    join: Option<JoinHandle<Result<TaskRunReport, RuntimeError>>>,
    cancel_tx: watch::Sender<bool>,
    /// The upstream half `attach_stdio` hands back.  The runtime holds it
    /// until `attach_stdio` moves it into a `BiStream`; after that the field
    /// is empty and subsequent calls return an `Attach` error.
    events_rx: Option<mpsc::Receiver<StreamEvent>>,
    /// The downstream half [`BiStream`] exposes — attached tests can
    /// inspect requests the consumer sent (none today; reserved for later
    /// PRs that wire a real worker).
    requests_tx: mpsc::Sender<crate::stream::StreamFrame>,
    /// Keeps the request channel alive so consumers can send without tripping
    /// a `SendError` on the closed receiver.  Dropped in `teardown`.
    requests_rx: Option<mpsc::Receiver<crate::stream::StreamFrame>>,
    /// Cached report after `teardown`/runner completion so a second
    /// `teardown` still sees the outcome.  `None` until the runner finishes.
    final_report: Option<TaskRunReport>,
    /// Set once the runner has been fully joined — subsequent `cancel`
    /// / `teardown` calls become idempotent no-ops.
    completed: bool,
}

/// In-process [`SessionRuntime`] — spawns a [`TaskRunner`] per `prepare`,
/// bridges the terminal report onto a [`BiStream`], supports cooperative
/// cancellation + bounded teardown.
pub struct TestRuntime {
    runner: Arc<dyn TaskRunner>,
    runs: Mutex<HashMap<String, RunState>>,
    /// Most-recently-prepared spec — preserved from the PR 1 stub so the
    /// existing call site (`run_supervisor_dispatch` once PR 7 flips) can
    /// assert on model routing without reaching into the runs map.
    last_spec: Mutex<Option<TaskRunSpec>>,
}

impl TestRuntime {
    /// Construct a runtime that drives every `prepare` through `runner`.
    pub fn new<R: TaskRunner>(runner: R) -> Self {
        Self {
            runner: Arc::new(runner),
            runs: Mutex::new(HashMap::new()),
            last_spec: Mutex::new(None),
        }
    }

    /// Construct from an already-wrapped `Arc<dyn TaskRunner>` — convenient
    /// when the same runner is shared across tests.
    pub fn from_arc(runner: Arc<dyn TaskRunner>) -> Self {
        Self {
            runner,
            runs: Mutex::new(HashMap::new()),
            last_spec: Mutex::new(None),
        }
    }

    /// Construct from a closure — the common case in tests where the runner
    /// is just `async move { Ok(TaskRunReport { … }) }`.
    pub fn from_fn<F, Fut>(f: F) -> Self
    where
        F: Fn(TaskRunSpec, RunnerCancel) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskRunReport, RuntimeError>> + Send + 'static,
    {
        Self::new(FnRunner::new(f))
    }

    /// Clone of the most recent [`TaskRunSpec`] seen by `prepare`, or `None`
    /// if `prepare` has not been called yet.  Preserved from the PR 1 stub.
    pub fn last_spec(&self) -> Option<TaskRunSpec> {
        self.last_spec.lock().expect("mutex poisoned").clone()
    }
}

impl Default for TestRuntime {
    /// A default runtime synthesises a trivial `TaskRunOutcome::Closed` report
    /// — matches the PR 1 stub's behaviour so existing call sites that did
    /// `TestRuntime::default()` keep compiling.
    fn default() -> Self {
        Self::from_fn(|spec, _cancel| async move {
            Ok(TaskRunReport {
                task_run_id: format!("test-{}", spec.task_id),
                outcome: TaskRunOutcome::Closed {
                    reason: "TestRuntime default runner: no runner wired".to_string(),
                },
                stages_completed: Vec::new(),
            })
        })
    }
}

#[async_trait]
impl SessionRuntime for TestRuntime {
    async fn prepare(&self, spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError> {
        let task_run_id = format!("test-{}", spec.task_id);

        *self.last_spec.lock().expect("mutex poisoned") = Some(spec.clone());

        // Duplicate prepare for the same spec is a programmer error; bail
        // before we spawn a second copy.
        if self
            .runs
            .lock()
            .expect("mutex poisoned")
            .contains_key(&task_run_id)
        {
            return Err(RuntimeError::Prepare(format!(
                "run {task_run_id} already prepared; call teardown before re-preparing",
            )));
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (events_tx, events_rx) = mpsc::channel::<StreamEvent>(16);
        let (requests_tx, requests_rx) = mpsc::channel::<crate::stream::StreamFrame>(16);

        let runner = Arc::clone(&self.runner);
        let spec_for_task = spec.clone();

        // The spawned task runs the user-supplied `TaskRunner`, then
        // forwards the terminal report as a `StreamEvent::Report` on
        // `events_tx` so `attach_stdio` consumers observe a terminal frame
        // without needing their own bridging logic.
        let join = tokio::spawn(async move {
            let runner_cancel = RunnerCancel { rx: cancel_rx };
            let result = runner.run(spec_for_task, runner_cancel).await;
            if let Ok(ref report) = result {
                // Best-effort bridge — consumers that dropped `BiStream`
                // simply won't see the frame.
                let _ = events_tx.send(StreamEvent::Report(report.clone())).await;
            }
            result
        });

        let state = RunState {
            join: Some(join),
            cancel_tx,
            events_rx: Some(events_rx),
            requests_tx,
            requests_rx: Some(requests_rx),
            final_report: None,
            completed: false,
        };
        self.runs
            .lock()
            .expect("mutex poisoned")
            .insert(task_run_id.clone(), state);

        Ok(RunHandle {
            task_run_id,
            container_id: None,
            pod_ref: None,
            ipc_socket: PathBuf::from("/tmp/djinn-test-runtime.sock"),
            started_at: SystemTime::now(),
        })
    }

    async fn attach_stdio(&self, handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        let mut runs = self.runs.lock().expect("mutex poisoned");
        let state = runs.get_mut(&handle.task_run_id).ok_or_else(|| {
            RuntimeError::Attach(format!("unknown task_run_id {}", handle.task_run_id))
        })?;

        let events_rx = state.events_rx.take().ok_or_else(|| {
            RuntimeError::Attach(format!(
                "attach_stdio already consumed for {}",
                handle.task_run_id
            ))
        })?;
        let requests_tx = state.requests_tx.clone();

        Ok(BiStream {
            events_rx,
            requests_tx,
        })
    }

    async fn cancel(&self, handle: &RunHandle) -> Result<(), RuntimeError> {
        let runs = self.runs.lock().expect("mutex poisoned");
        let state = runs.get(&handle.task_run_id).ok_or_else(|| {
            RuntimeError::Cancel(format!("unknown task_run_id {}", handle.task_run_id))
        })?;
        // Best-effort cooperative signal — the runner may finish before
        // observing it, which is fine.
        let _ = state.cancel_tx.send(true);
        // Hard backstop: abort the join handle so teardown is always bounded
        // even if the runner ignored `RunnerCancel`.
        if let Some(join) = state.join.as_ref() {
            join.abort();
        }
        Ok(())
    }

    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        // Pull the state out of the map first so we don't hold the mutex
        // across any awaits — `Mutex` is `std::sync`, not tokio.
        let mut state = self
            .runs
            .lock()
            .expect("mutex poisoned")
            .remove(&handle.task_run_id)
            .ok_or_else(|| {
                RuntimeError::Teardown(format!("unknown task_run_id {}", handle.task_run_id))
            })?;

        // If teardown has run before and cached the report, return it.
        if state.completed {
            if let Some(report) = state.final_report.take() {
                return Ok(report);
            }
            return Err(RuntimeError::Teardown(format!(
                "run {} already torn down",
                handle.task_run_id
            )));
        }

        let join = state.join.take().ok_or_else(|| {
            RuntimeError::Teardown(format!(
                "run {} has no join handle (double teardown?)",
                handle.task_run_id
            ))
        })?;

        // Bounded await.  If the runner is still computing after
        // `TEARDOWN_TIMEOUT` we abort and synthesise an `Interrupted`
        // outcome rather than wedging the caller.
        let outcome = match timeout(TEARDOWN_TIMEOUT, join).await {
            Ok(Ok(Ok(report))) => report,
            Ok(Ok(Err(e))) => {
                return Err(RuntimeError::Teardown(format!(
                    "runner returned error: {e}"
                )));
            }
            Ok(Err(join_err)) if join_err.is_cancelled() => {
                // Aborted by `cancel` — synthesise an `Interrupted`
                // report so the caller still gets a typed terminal
                // value instead of an error.
                TaskRunReport {
                    task_run_id: handle.task_run_id.clone(),
                    outcome: TaskRunOutcome::Interrupted,
                    stages_completed: Vec::new(),
                }
            }
            Ok(Err(join_err)) => {
                return Err(RuntimeError::Teardown(format!(
                    "runner task panicked: {join_err}"
                )));
            }
            Err(_elapsed) => {
                return Err(RuntimeError::Teardown(format!(
                    "runner did not finish within {TEARDOWN_TIMEOUT:?}"
                )));
            }
        };

        // Drop the request-channel receiver so any outstanding `requests_tx`
        // handles observe a closed channel cleanly.
        drop(state.requests_rx.take());
        state.completed = true;
        state.final_report = Some(outcome.clone());

        // State is already removed from the map — nothing to reinsert.
        let _ = state;
        let _ = handle;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use djinn_core::models::TaskRunTrigger;
    use tokio::time::sleep;

    use super::*;
    use crate::spec::{RoleKind, SupervisorFlow};

    fn dummy_spec(task_id: &str) -> TaskRunSpec {
        TaskRunSpec {
            task_id: task_id.into(),
            project_id: "p1".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: format!("djinn/{task_id}"),
            flow: SupervisorFlow::Planning,
            model_id_per_role: HashMap::new(),
        }
    }

    /// Compile-time proof that `TestRuntime` can live behind `dyn
    /// SessionRuntime`.  Matches the analogous `_obj_safe` check in
    /// `djinn-supervisor`.
    #[test]
    fn test_runtime_object_safety() {
        fn _obj(_: &dyn SessionRuntime) {}
        let rt = TestRuntime::default();
        _obj(&rt);
    }

    /// Full happy path — a runner that returns `TaskRunOutcome::Closed`
    /// drives prepare → attach → teardown cleanly, and the final report
    /// rides `events_rx` as a terminal `StreamEvent::Report`.
    #[tokio::test]
    async fn test_runtime_happy_path() {
        let rt = TestRuntime::from_fn(|spec, _cancel| async move {
            Ok(TaskRunReport {
                task_run_id: format!("test-{}", spec.task_id),
                outcome: TaskRunOutcome::Closed {
                    reason: "happy path: fake runner done".into(),
                },
                stages_completed: vec![RoleKind::Planner],
            })
        });

        let spec = dummy_spec("happy");
        let handle = rt.prepare(&spec).await.expect("prepare");
        assert_eq!(handle.task_run_id, "test-happy");
        assert!(handle.container_id.is_none());
        assert_eq!(rt.last_spec().unwrap().task_id, "happy");

        let mut stream = rt.attach_stdio(&handle).await.expect("attach");

        // Runner completes almost immediately; the bridge task forwards the
        // terminal frame.  A short timeout guards against regressions that
        // wedge the pump.
        let frame = timeout(Duration::from_secs(2), stream.events_rx.recv())
            .await
            .expect("stream timed out")
            .expect("stream closed before Report");

        match frame {
            StreamEvent::Report(report) => {
                assert_eq!(report.task_run_id, "test-happy");
                assert!(matches!(report.outcome, TaskRunOutcome::Closed { .. }));
                assert_eq!(report.stages_completed, vec![RoleKind::Planner]);
            }
            other => panic!("expected StreamEvent::Report, got {other:?}"),
        }

        let report = rt.teardown(handle).await.expect("teardown");
        assert_eq!(report.task_run_id, "test-happy");
        assert!(matches!(report.outcome, TaskRunOutcome::Closed { .. }));
    }

    /// `cancel` interrupts a slow runner and `teardown` resolves within a
    /// tight bound — the backstop `JoinHandle::abort` ensures we never
    /// depend on the runner observing `RunnerCancel`.
    #[tokio::test]
    async fn test_runtime_cancels_midrun() {
        // Deliberately-slow runner that sleeps far past any reasonable
        // teardown timeout.  It does *not* consult `RunnerCancel` so we
        // also prove the hard abort backstop works.
        let started = Arc::new(AtomicBool::new(false));
        let started_clone = Arc::clone(&started);
        let rt = TestRuntime::from_fn(move |spec, _cancel| {
            let started = Arc::clone(&started_clone);
            async move {
                started.store(true, Ordering::SeqCst);
                sleep(Duration::from_secs(60)).await;
                Ok(TaskRunReport {
                    task_run_id: format!("test-{}", spec.task_id),
                    outcome: TaskRunOutcome::Closed {
                        reason: "should not reach here".into(),
                    },
                    stages_completed: Vec::new(),
                })
            }
        });

        let spec = dummy_spec("slow");
        let handle = rt.prepare(&spec).await.expect("prepare");

        // Yield so the spawned runner has a chance to observe the
        // cancellation path rather than being aborted pre-start.
        for _ in 0..20 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }

        let t0 = std::time::Instant::now();
        rt.cancel(&handle).await.expect("cancel");
        let report = timeout(Duration::from_secs(2), rt.teardown(handle))
            .await
            .expect("teardown exceeded bound")
            .expect("teardown returned error");
        let elapsed = t0.elapsed();

        assert!(
            matches!(report.outcome, TaskRunOutcome::Interrupted),
            "expected Interrupted outcome, got {:?}",
            report.outcome
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel → teardown took too long: {elapsed:?}"
        );
    }

    /// Sanity: `prepare` + `teardown` round-trip with the default runner
    /// still works (preserves the PR 1 stub behaviour for call sites that
    /// relied on `TestRuntime::default()`).
    #[tokio::test]
    async fn default_runner_preserves_pr1_behaviour() {
        let rt = TestRuntime::default();
        let spec = dummy_spec("t1");
        let handle = rt.prepare(&spec).await.expect("prepare");
        assert_eq!(handle.task_run_id, "test-t1");
        let _ = rt.attach_stdio(&handle).await.expect("attach");
        let report = rt.teardown(handle).await.expect("teardown");
        assert_eq!(report.task_run_id, "test-t1");
        assert!(
            matches!(report.outcome, TaskRunOutcome::Closed { .. }),
            "expected Closed outcome, got {:?}",
            report.outcome
        );
        assert_eq!(rt.last_spec().unwrap().task_id, "t1");
    }
}

use super::*;
use async_trait::async_trait;
use djinn_cgroup_launcher::CpuStat;
use djinn_core::clock::TestClock;
use djinn_supervisor::services::{
    LeaseGrant, LeaseStatus, SerializableCreateSessionParams, SerializableCreateTaskRunParams,
    SerializableDjinnEvent,
};
use djinn_supervisor::{
    BranchPublicationResult, RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec,
};
use std::collections::VecDeque;
use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct ScriptedLauncher {
    pid: Arc<Mutex<Option<u32>>>,
    lifts: Arc<Mutex<Vec<LeaseFencingToken>>>,
    kills: Arc<AtomicUsize>,
    empties: Arc<AtomicUsize>,
}

impl CgroupLauncherClient for ScriptedLauncher {
    fn launch(
        &self,
        mut command: Command,
        _: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        let child = command.spawn()?;
        *self.pid.lock().unwrap() = Some(child.id());
        Ok(Box::new(ScriptedHandle {
            child,
            lifts: self.lifts.clone(),
            kills: self.kills.clone(),
            empties: self.empties.clone(),
        }))
    }
}
struct ScriptedHandle {
    child: std::process::Child,
    lifts: Arc<Mutex<Vec<LeaseFencingToken>>>,
    kills: Arc<AtomicUsize>,
    empties: Arc<AtomicUsize>,
}
impl ProcessHandle for ScriptedHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        Ok(CpuStat {
            usage_usec: 10,
            ..CpuStat::default()
        })
    }
    fn fenced_lift(&mut self, token: &LeaseFencingToken) -> io::Result<()> {
        self.lifts.lock().unwrap().push(token.clone());
        Ok(())
    }
    fn kill(&mut self) -> io::Result<()> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        let _ = self.child.kill();
        Ok(())
    }
    fn wait_empty(&mut self) -> io::Result<()> {
        self.empties.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ScriptedServices {
    cancel: CancellationToken,
    queue: Mutex<VecDeque<LeaseResult>>,
    grant: Mutex<VecDeque<LeaseResult>>,
    status: Mutex<VecDeque<LeaseResult>>,
    abandon: Mutex<VecDeque<LeaseResult>>,
    release: Mutex<VecDeque<LeaseResult>>,
    queue_calls: AtomicUsize,
    grant_calls: AtomicUsize,
    status_calls: AtomicUsize,
    abandon_calls: AtomicUsize,
    release_calls: AtomicUsize,
    pause_queue: AtomicBool,
    pause_grant: AtomicBool,
    queue_entered: Notify,
    grant_entered: Notify,
}

impl ScriptedServices {
    fn new(queue: Vec<LeaseResult>, grant: Vec<LeaseResult>, status: Vec<LeaseResult>) -> Self {
        Self {
            cancel: CancellationToken::new(),
            queue: Mutex::new(queue.into()),
            grant: Mutex::new(grant.into()),
            status: Mutex::new(status.into()),
            abandon: Mutex::new(VecDeque::new()),
            release: Mutex::new(VecDeque::new()),
            queue_calls: AtomicUsize::new(0),
            grant_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            abandon_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
            pause_queue: AtomicBool::new(false),
            pause_grant: AtomicBool::new(false),
            queue_entered: Notify::new(),
            grant_entered: Notify::new(),
        }
    }
    fn pop(script: &Mutex<VecDeque<LeaseResult>>) -> LeaseResult {
        script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(LeaseResult::LeaseUnavailable)
    }
}

#[async_trait]
impl SupervisorServices for ScriptedServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
    async fn queue_lease(&self, _: LeaseQueueRequest) -> LeaseResult {
        self.queue_calls.fetch_add(1, Ordering::SeqCst);
        self.queue_entered.notify_waiters();
        if self.pause_queue.load(Ordering::SeqCst) {
            std::future::pending().await
        } else {
            Self::pop(&self.queue)
        }
    }
    async fn grant_lease(&self, _: LeaseGrantRequest) -> LeaseResult {
        self.grant_calls.fetch_add(1, Ordering::SeqCst);
        self.grant_entered.notify_waiters();
        if self.pause_grant.load(Ordering::SeqCst) {
            std::future::pending().await
        } else {
            Self::pop(&self.grant)
        }
    }
    async fn lease_status(&self, _: LeaseStatusRequest) -> LeaseResult {
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        Self::pop(&self.status)
    }
    async fn abandon_lease(&self, _: LeaseAbandonRequest) -> LeaseResult {
        self.abandon_calls.fetch_add(1, Ordering::SeqCst);
        Self::pop(&self.abandon)
    }
    async fn release_lease(&self, _: LeaseReleaseRequest) -> LeaseResult {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
        Self::pop(&self.release)
    }
    async fn load_task(&self, _: String) -> Result<djinn_core::models::Task, String> {
        unimplemented!()
    }
    async fn execute_stage(
        &self,
        _: &djinn_core::models::Task,
        _: &djinn_workspace::Workspace,
        _: RoleKind,
        _: &str,
        _: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        unimplemented!()
    }
    async fn open_pr(&self, _: &TaskRunSpec, _: &djinn_core::models::Task) -> TaskRunOutcome {
        unimplemented!()
    }
    async fn create_task_run(&self, _: SerializableCreateTaskRunParams) -> Result<(), String> {
        unimplemented!()
    }
    async fn update_task_run_status(
        &self,
        _: String,
        _: djinn_core::models::TaskRunStatus,
    ) -> Result<(), String> {
        unimplemented!()
    }
    async fn get_model_context_window(&self, _: String) -> Result<i64, String> {
        unimplemented!()
    }
    async fn get_provider_base_url(&self, _: String) -> Result<String, String> {
        unimplemented!()
    }
    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        unimplemented!()
    }
    async fn create_session(
        &self,
        _: SerializableCreateSessionParams,
    ) -> Result<djinn_core::models::SessionRecord, String> {
        unimplemented!()
    }
    async fn publish_session_message(
        &self,
        _: String,
        _: String,
        _: String,
        _: serde_json::Value,
    ) -> Result<(), String> {
        unimplemented!()
    }
    async fn get_environment_config(
        &self,
        _: String,
    ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
        unimplemented!()
    }
    async fn invoke_llm(
        &self,
        _: String,
        _: djinn_provider::message::Conversation,
        _: Vec<serde_json::Value>,
        _: Option<djinn_provider::provider::ToolChoice>,
    ) -> Result<djinn_provider::provider::LlmResponse, String> {
        unimplemented!()
    }
    async fn update_session_status(
        &self,
        _: String,
        _: djinn_core::models::SessionStatus,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
        _: Option<String>,
    ) -> Result<(), String> {
        unimplemented!()
    }
    async fn emit_djinn_event(&self, _: SerializableDjinnEvent) -> Result<(), String> {
        unimplemented!()
    }
    async fn tool_github_search(
        &self,
        _: Option<String>,
        _: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!()
    }
    async fn tool_github_fetch_file(
        &self,
        _: Option<String>,
        _: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!()
    }
    async fn tool_ci_job_log(
        &self,
        _: Option<String>,
        _: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!()
    }
    async fn touch_activity(&self, _: String) -> Result<(), String> {
        Ok(())
    }
    async fn transition_task(&self, _: String, _: String, _: Option<String>) -> Result<(), String> {
        unimplemented!()
    }
    async fn record_arbiter_decision(&self, _: String, _: String, _: String) -> Result<(), String> {
        unimplemented!()
    }
    async fn start_monitored_reopen(
        &self,
        _: String,
        _: String,
        _: String,
        _: Vec<String>,
    ) -> Result<(), String> {
        unimplemented!()
    }
    async fn complete_monitored_reopen(&self, _: String) -> Result<(), String> {
        unimplemented!()
    }
    async fn record_arbiter_session_termination(&self, _: String, _: bool) -> Result<bool, String> {
        unimplemented!()
    }
    async fn publish_branch_to_github(
        &self,
        _: &TaskRunSpec,
        _: &djinn_core::models::Task,
    ) -> BranchPublicationResult {
        unimplemented!()
    }
}

fn deadlines() -> LeaseDeadlines {
    LeaseDeadlines {
        queue_deadline_ms: 100,
        launch_deadline_ms: 200,
    }
}
fn status(state: LeaseState, token: Option<u64>) -> LeaseResult {
    LeaseResult::Status(LeaseStatus {
        state,
        fencing_token: token.map(LeaseFencingToken),
        deadlines: deadlines(),
        pod_uid: None,
        candidate_cleanup: false,
    })
}
fn granted(token: u64) -> LeaseResult {
    LeaseResult::Granted(LeaseGrant {
        fencing_token: LeaseFencingToken(token),
        deadlines: deadlines(),
    })
}
fn command() -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg("sleep 30");
    c
}
fn config() -> LeaseInvocationConfig {
    LeaseInvocationConfig {
        task_id: "task".into(),
        task_run_id: "run".into(),
        cpu_usage_threshold_usec: 1,
        queue_deadline_ms: 100,
        launch_deadline_ms: 200,
        timeout: Duration::from_secs(60),
    }
}
fn clock() -> Arc<TestClock> {
    Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()))
}
async fn wait_for(counter: &AtomicUsize, n: usize) {
    for _ in 0..10_000 {
        if counter.load(Ordering::SeqCst) >= n {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("counter did not reach {n}");
}

#[tokio::test]
async fn repeated_active_statuses_queue_and_lift_exactly_once() {
    let services = Arc::new(ScriptedServices::new(
        vec![granted(7)],
        vec![status(LeaseState::Active, Some(7))],
        vec![status(LeaseState::Active, Some(7)); 20],
    ));
    services
        .release
        .lock()
        .unwrap()
        .push_back(LeaseResult::Released {
            candidate_cleanup: false,
        });
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 3).await;
    cancel.cancel();
    let output = run.await.unwrap().unwrap();
    assert_eq!(output.process.termination, ProcessTermination::Cancelled);
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 1);
    assert_eq!(services.grant_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*launcher.lifts.lock().unwrap(), vec![LeaseFencingToken(7)]);
    assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.empties.load(Ordering::SeqCst), 1);
    assert!(services.release_calls.load(Ordering::SeqCst) <= 1);
}

#[tokio::test]
async fn terminal_intent_while_grant_paused_permanently_prevents_lift() {
    let services = Arc::new(ScriptedServices::new(
        vec![granted(9)],
        vec![],
        vec![status(LeaseState::Granted, Some(9))],
    ));
    services.pause_grant.store(true, Ordering::SeqCst);
    services
        .release
        .lock()
        .unwrap()
        .push_back(LeaseResult::Released {
            candidate_cleanup: false,
        });
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.grant_calls, 1).await;
    cancel.cancel();
    let output = run.await.unwrap().unwrap();
    assert_eq!(output.process.termination, ProcessTermination::Cancelled);
    assert!(launcher.lifts.lock().unwrap().is_empty());
    assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.empties.load(Ordering::SeqCst), 1);
    assert!(services.release_calls.load(Ordering::SeqCst) <= 1);
    assert_eq!(services.abandon_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transient_queue_grant_status_and_release_are_reconciled() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::LeaseUnavailable],
        vec![
            LeaseResult::LeaseUnavailable,
            status(LeaseState::Active, Some(11)),
        ],
        vec![
            LeaseResult::LeaseUnavailable,
            status(LeaseState::Granted, Some(11)),
            status(LeaseState::Active, Some(11)),
            status(LeaseState::Active, Some(11)),
            status(LeaseState::Active, Some(11)),
        ],
    ));
    services.release.lock().unwrap().extend([
        LeaseResult::LeaseUnavailable,
        LeaseResult::Released {
            candidate_cleanup: false,
        },
    ]);
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.status_calls, 4).await;
    cancel.cancel();
    run.await.unwrap().unwrap();
    assert_eq!(*launcher.lifts.lock().unwrap(), vec![LeaseFencingToken(11)]);
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 1);
    assert_eq!(services.grant_calls.load(Ordering::SeqCst), 2);
    assert_eq!(services.release_calls.load(Ordering::SeqCst), 2);
    assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.empties.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transient_status_and_abandon_are_reconciled_without_capacity_double_return() {
    let services = Arc::new(ScriptedServices::new(
        vec![],
        vec![],
        vec![
            LeaseResult::LeaseUnavailable,
            status(LeaseState::Queued, None),
            status(LeaseState::Queued, None),
        ],
    ));
    services.pause_queue.store(true, Ordering::SeqCst);
    services.abandon.lock().unwrap().extend([
        LeaseResult::LeaseUnavailable,
        LeaseResult::Abandoned {
            candidate_cleanup: false,
        },
    ]);
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.queue_calls, 1).await;
    cancel.cancel();
    run.await.unwrap().unwrap();
    assert!(launcher.lifts.lock().unwrap().is_empty());
    assert_eq!(services.abandon_calls.load(Ordering::SeqCst), 2);
    assert_eq!(services.release_calls.load(Ordering::SeqCst), 0);
    assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.empties.load(Ordering::SeqCst), 1);
}

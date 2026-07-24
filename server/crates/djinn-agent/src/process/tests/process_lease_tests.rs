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

/// Remote-child-shaped launcher double: unlike `ScriptedLauncher`, it never
/// calls `Command::spawn`; output and lifecycle arrive through broker handles.
#[derive(Clone)]
struct BrokerBackedLauncher {
    state: Arc<Mutex<BrokerBackedState>>,
    cpu_usage_usec: u64,
}

struct BrokerBackedState {
    identities: Vec<TaskInvocationLeaseIdentity>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<std::process::ExitStatus>,
    kills: usize,
    empties: usize,
    cleanups: usize,
    samples: usize,
}

impl BrokerBackedLauncher {
    fn exited(stdout: &[u8], stderr: &[u8], code: i32) -> Self {
        use std::os::unix::process::ExitStatusExt;
        Self {
            state: Arc::new(Mutex::new(BrokerBackedState {
                identities: Vec::new(),
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
                status: Some(std::process::ExitStatus::from_raw(code << 8)),
                kills: 0,
                empties: 0,
                cleanups: 0,
                samples: 0,
            })),
            cpu_usage_usec: 0,
        }
    }

    fn running(cpu_usage_usec: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(BrokerBackedState {
                identities: Vec::new(),
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: None,
                kills: 0,
                empties: 0,
                cleanups: 0,
                samples: 0,
            })),
            cpu_usage_usec,
        }
    }
}

impl CgroupLauncherClient for BrokerBackedLauncher {
    fn launch(
        &self,
        _: Command,
        identity: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        self.state.lock().unwrap().identities.push(identity.clone());
        Ok(Box::new(BrokerBackedHandle {
            state: self.state.clone(),
            cpu_usage_usec: self.cpu_usage_usec,
        }))
    }
}

struct BrokerBackedHandle {
    state: Arc<Mutex<BrokerBackedState>>,
    cpu_usage_usec: u64,
}

impl ProcessHandle for BrokerBackedHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.state.lock().unwrap().stdout))
    }
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.state.lock().unwrap().stderr))
    }
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        Ok(self.state.lock().unwrap().status)
    }
    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.state
            .lock()
            .unwrap()
            .status
            .ok_or_else(|| io::Error::other("remote child is still running"))
    }
    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        self.state.lock().unwrap().samples += 1;
        Ok(CpuStat {
            usage_usec: self.cpu_usage_usec,
            ..CpuStat::default()
        })
    }
    fn fenced_lift(&mut self, _: &LeaseFencingToken) -> io::Result<()> {
        Ok(())
    }
    fn kill(&mut self) -> io::Result<()> {
        use std::os::unix::process::ExitStatusExt;

        let mut state = self.state.lock().unwrap();
        state.kills += 1;
        state
            .status
            .get_or_insert_with(|| std::process::ExitStatus::from_raw(9));
        Ok(())
    }
    fn wait_empty(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().empties += 1;
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().cleanups += 1;
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
    release_fences: Mutex<Vec<LeaseFencingToken>>,
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
            release_fences: Mutex::new(Vec::new()),
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
    async fn bind_lease_pod(
        &self,
        request: djinn_supervisor::services::LeaseBindRequest,
    ) -> LeaseResult {
        LeaseResult::Bound(LeaseStatus {
            state: LeaseState::Bound,
            fencing_token: Some(request.fencing_token),
            deadlines: deadlines(),
            pod_uid: Some(request.pod_uid),
            candidate_cleanup: false,
        })
    }
    async fn release_lease(&self, request: LeaseReleaseRequest) -> LeaseResult {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
        self.release_fences
            .lock()
            .unwrap()
            .push(request.fencing_token);
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
        pod_uid: "pod".into(),
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

#[tokio::test]
async fn broker_backed_shell_preserves_streams_exit_and_immutable_identity() {
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    let launcher = Arc::new(BrokerBackedLauncher::exited(
        b"broker stdout\n",
        b"broker stderr\n",
        17,
    ));
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let output = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect("broker-backed remote child completes");

    assert_eq!(output.process.output.stdout, b"broker stdout\n");
    assert_eq!(output.process.output.stderr, b"broker stderr\n");
    assert_eq!(output.process.output.status.code(), Some(17));
    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 0);
    let state = launcher.state.lock().unwrap();
    assert_eq!(state.identities.len(), 1);
    assert_eq!(state.identities[0].task_id, "task");
    assert_eq!(state.identities[0].task_run_id, "run");
    assert!(!state.identities[0].invocation_id.is_empty());
    assert_eq!((state.kills, state.empties, state.cleanups), (1, 1, 1));
}

#[tokio::test]
async fn broker_backed_shell_cancellation_kills_waits_empty_and_cleans_up() {
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    let launcher = Arc::new(BrokerBackedLauncher::running(0));
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let output = runner
        .output(command(), config(), cancel)
        .await
        .expect("cancelled remote child is cleaned up");

    assert_eq!(output.process.termination, ProcessTermination::Cancelled);
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 0);
    let state = launcher.state.lock().unwrap();
    assert_eq!((state.kills, state.empties, state.cleanups), (1, 1, 1));
}

#[tokio::test]
async fn below_threshold_broker_backed_shell_never_contacts_supervisor() {
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    let launcher = Arc::new(BrokerBackedLauncher::running(0));
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        launcher.clone(),
        clock(),
    ));
    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    for _ in 0..10_000 {
        if launcher.state.lock().unwrap().samples > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        launcher.state.lock().unwrap().samples > 0,
        "broker-backed child was never sampled"
    );
    cancel.cancel();
    run.await
        .expect("runner task joins")
        .expect("light remote child is cleaned up");

    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 0);
    assert_eq!(services.grant_calls.load(Ordering::SeqCst), 0);
    assert_eq!(services.status_calls.load(Ordering::SeqCst), 0);
}

// ══════════════════════════════════════════════════════════════════════════
// AC 1 + AC 4 fixture harness.
//
// A remote-child launcher double whose queue-relevant behaviour is driven only
// by an injected `cpu.stat` reading and a wait-poll countdown — never by the
// command string. It records lifts/kills/empties/cleanups and how many times it
// was sampled so tests can prove the queue decision is parser-independent and
// the escalation race orderings resolve deterministically.
// ══════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
struct FixtureLauncher {
    state: Arc<Mutex<FixtureState>>,
}

struct FixtureState {
    cpu_usage_usec: u64,
    /// Number of `try_wait` polls after which the child reports natural exit.
    /// `None` means it runs until killed.
    exit_after_polls: Option<usize>,
    exit_code: i32,
    wait_polls: usize,
    killed: bool,
    samples: usize,
    lifts: Vec<LeaseFencingToken>,
    kills: usize,
    empties: usize,
    cleanups: usize,
}

impl FixtureLauncher {
    fn new(cpu_usage_usec: u64, exit_after_polls: Option<usize>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FixtureState {
                cpu_usage_usec,
                exit_after_polls,
                exit_code: 0,
                wait_polls: 0,
                killed: false,
                samples: 0,
                lifts: Vec::new(),
                kills: 0,
                empties: 0,
                cleanups: 0,
            })),
        }
    }
    fn set_cpu(&self, usage_usec: u64) {
        self.state.lock().unwrap().cpu_usage_usec = usage_usec;
    }
    fn samples(&self) -> usize {
        self.state.lock().unwrap().samples
    }
    fn lifts(&self) -> Vec<LeaseFencingToken> {
        self.state.lock().unwrap().lifts.clone()
    }
}

impl CgroupLauncherClient for FixtureLauncher {
    fn launch(
        &self,
        _: Command,
        _: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        Ok(Box::new(FixtureHandle {
            state: self.state.clone(),
        }))
    }
}

struct FixtureHandle {
    state: Arc<Mutex<FixtureState>>,
}

impl ProcessHandle for FixtureHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        let mut state = self.state.lock().unwrap();
        if state.killed {
            return Ok(Some(std::process::ExitStatus::from_raw(9)));
        }
        state.wait_polls += 1;
        if let Some(limit) = state.exit_after_polls
            && state.wait_polls >= limit
        {
            return Ok(Some(std::process::ExitStatus::from_raw(
                state.exit_code << 8,
            )));
        }
        Ok(None)
    }
    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::yield_now();
        }
    }
    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        let mut state = self.state.lock().unwrap();
        state.samples += 1;
        Ok(CpuStat {
            usage_usec: state.cpu_usage_usec,
            ..CpuStat::default()
        })
    }
    fn fenced_lift(&mut self, token: &LeaseFencingToken) -> io::Result<()> {
        self.state.lock().unwrap().lifts.push(token.clone());
        Ok(())
    }
    fn kill(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.kills += 1;
        state.killed = true;
        Ok(())
    }
    fn wait_empty(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().empties += 1;
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().cleanups += 1;
        Ok(())
    }
}

/// Fixtures whose consumption never crosses the threshold: light `grep`,
/// `git status`, and a short script.
const LIGHT_FIXTURES: &[&str] = &["grep -rn TODO src", "git status", "sh -c 'echo hello'"];

/// Fixtures whose consumption crosses the threshold and must queue exactly once
/// regardless of whether the command classifier can name them: direct Cargo,
/// a nested repository script, a malformed/parser-over-budget command, Make,
/// npm, Bazel, and Go.
const HEAVY_FIXTURES: &[&str] = &[
    "cargo build --workspace",
    "sh -c 'bash scripts/repository-build.sh'",
    "cargo ((( --parser-cannot-classify-this",
    "make -j16 all",
    "npm run build",
    "bazel build //...",
    "go build ./...",
];

fn cmd_from(fixture: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(fixture);
    command
}

fn config_with_threshold(threshold_usec: u64) -> LeaseInvocationConfig {
    LeaseInvocationConfig {
        cpu_usage_threshold_usec: threshold_usec,
        ..config()
    }
}

async fn poll_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition was never observed");
}

// ─── AC 1: light fixtures finish unleased with zero lease calls ───────────

#[tokio::test]
async fn ac1_light_fixtures_finish_unleased_with_zero_lease_calls() {
    for fixture in LIGHT_FIXTURES {
        let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
        // Consumption stays at zero; the child exits on its own after a few polls.
        let launcher = Arc::new(FixtureLauncher::new(0, Some(4)));
        let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
        let output = runner
            .output(
                cmd_from(fixture),
                config_with_threshold(1_000),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|_| panic!("light fixture {fixture:?} completes"));

        assert_eq!(
            output.process.termination,
            ProcessTermination::Exited,
            "light fixture {fixture:?} finishes on its own"
        );
        assert!(
            launcher.samples() > 0,
            "light fixture {fixture:?} is measured at the unleased quota"
        );
        assert!(
            launcher.lifts().is_empty(),
            "light fixture {fixture:?} never lifts its quota"
        );
        assert_eq!(
            services.queue_calls.load(Ordering::SeqCst),
            0,
            "light fixture {fixture:?} must issue zero lease calls"
        );
        assert_eq!(services.grant_calls.load(Ordering::SeqCst), 0);
        assert_eq!(services.status_calls.load(Ordering::SeqCst), 0);
    }
}

// ─── AC 1: heavy fixtures queue exactly once after crossing the threshold ──

#[tokio::test]
async fn ac1_heavy_fixtures_queue_exactly_once_after_cpu_threshold() {
    for fixture in HEAVY_FIXTURES {
        let services = Arc::new(ScriptedServices::new(
            vec![status(LeaseState::Queued, None)],
            vec![],
            vec![status(LeaseState::Queued, None); 64],
        ));
        services
            .abandon
            .lock()
            .unwrap()
            .push_back(LeaseResult::Abandoned {
                candidate_cleanup: false,
            });
        // Starts below threshold and never exits on its own.
        let launcher = Arc::new(FixtureLauncher::new(0, None));
        let runner = Arc::new(LeaseInvocationRunner::new(
            services.clone(),
            launcher.clone(),
            clock(),
        ));
        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let run_services = services.clone();
        let run_launcher = launcher.clone();
        let run = tokio::spawn(async move {
            runner
                .output(cmd_from(fixture), config_with_threshold(1_000), run_cancel)
                .await
        });

        // Below the threshold: sampled repeatedly, never queued.
        let below = run_launcher.clone();
        poll_until(move || below.samples() >= 3).await;
        assert_eq!(
            run_services.queue_calls.load(Ordering::SeqCst),
            0,
            "heavy fixture {fixture:?} must not queue below the threshold"
        );

        // Consumption crosses the configured threshold.
        launcher.set_cpu(5_000);
        let queued = services.clone();
        poll_until(move || queued.queue_calls.load(Ordering::SeqCst) >= 1).await;

        cancel.cancel();
        run.await
            .expect("runner task joins")
            .unwrap_or_else(|_| panic!("heavy fixture {fixture:?} is cleaned up"));

        assert_eq!(
            services.queue_calls.load(Ordering::SeqCst),
            1,
            "heavy fixture {fixture:?} must queue exactly once"
        );
    }
}

/// The heavy expectation comes only from injected `cpu.stat` growth: a
/// well-formed Cargo invocation and a malformed/parser-over-budget command that
/// no classifier can name produce byte-identical queue behaviour.
#[tokio::test]
async fn ac1_queue_decision_is_identical_for_wellformed_and_malformed_commands() {
    async fn queue_calls_for(fixture: &'static str) -> usize {
        let services = Arc::new(ScriptedServices::new(
            vec![status(LeaseState::Queued, None)],
            vec![],
            vec![status(LeaseState::Queued, None); 64],
        ));
        services
            .abandon
            .lock()
            .unwrap()
            .push_back(LeaseResult::Abandoned {
                candidate_cleanup: false,
            });
        let launcher = Arc::new(FixtureLauncher::new(5_000, None));
        let runner = Arc::new(LeaseInvocationRunner::new(
            services.clone(),
            launcher.clone(),
            clock(),
        ));
        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let run_services = services.clone();
        let run = tokio::spawn(async move {
            runner
                .output(cmd_from(fixture), config_with_threshold(1_000), run_cancel)
                .await
        });
        poll_until(move || run_services.queue_calls.load(Ordering::SeqCst) >= 1).await;
        cancel.cancel();
        run.await.expect("join").expect("cleaned up");
        services.queue_calls.load(Ordering::SeqCst)
    }

    let wellformed = queue_calls_for("cargo build --workspace").await;
    let malformed = queue_calls_for("cargo ((( --parser-cannot-classify-this").await;
    assert_eq!(wellformed, 1);
    assert_eq!(
        wellformed, malformed,
        "queue behaviour must be identical regardless of command classification"
    );
}

// ─── AC 4: paused-response race orderings around grant ────────────────────

/// Natural exit on the grant side of the race: the grant response is paused, the
/// child exits first, and terminal intent permanently prevents any lift.
#[tokio::test]
async fn ac4_natural_exit_while_grant_paused_prevents_lift() {
    let services = Arc::new(ScriptedServices::new(
        vec![granted(7)],
        vec![],
        vec![status(LeaseState::Granted, Some(7)); 8],
    ));
    services.pause_grant.store(true, Ordering::SeqCst);
    // The child exits while the grant future is still left pending.
    let launcher = Arc::new(FixtureLauncher::new(5_000, Some(6)));
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let output = runner
        .output(
            cmd_from("cargo build"),
            config_with_threshold(1_000),
            CancellationToken::new(),
        )
        .await
        .expect("child exits before the paused grant resolves");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 1);
    assert!(
        services.grant_calls.load(Ordering::SeqCst) >= 1,
        "the grant stage is reached before the child's natural exit wins"
    );
    assert!(
        launcher.lifts().is_empty(),
        "a natural exit before grant resolution can never lift the quota"
    );
}

/// Fallback timeout on the queue side: the queue response is paused and the
/// injected fake clock crosses the deadline, terminating without a lift.
#[tokio::test]
async fn ac4_fallback_timeout_on_paused_response_terminates_without_lift() {
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    services.pause_queue.store(true, Ordering::SeqCst);
    let launcher = Arc::new(FixtureLauncher::new(5_000, None));
    let test_clock = clock();
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), test_clock.clone());
    let run_services = services.clone();
    let run = tokio::spawn(async move {
        runner
            .output(
                cmd_from("cargo build"),
                config_with_threshold(1_000),
                CancellationToken::new(),
            )
            .await
    });
    poll_until(move || run_services.queue_calls.load(Ordering::SeqCst) >= 1).await;
    // Cross the fallback deadline while the response is still paused.
    test_clock.advance_mono(Duration::from_secs(120));
    let output = run.await.expect("join").expect("times out cleanly");

    assert_eq!(output.process.termination, ProcessTermination::TimedOut);
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 1);
    assert!(launcher.lifts().is_empty(), "a timed-out queue never lifts");
}

/// Cancellation before a grant is issued: the queue response is paused, cancel
/// fires, and the whole cgroup lifecycle still runs exactly once with no lift.
#[tokio::test]
async fn ac4_cancellation_before_grant_prevents_lift() {
    let services = Arc::new(ScriptedServices::new(vec![], vec![], vec![]));
    services.pause_queue.store(true, Ordering::SeqCst);
    let launcher = Arc::new(FixtureLauncher::new(5_000, None));
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        launcher.clone(),
        clock(),
    ));
    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let run_services = services.clone();
    let run = tokio::spawn(async move {
        runner
            .output(
                cmd_from("cargo build"),
                config_with_threshold(1_000),
                run_cancel,
            )
            .await
    });
    poll_until(move || run_services.queue_calls.load(Ordering::SeqCst) >= 1).await;
    cancel.cancel();
    let output = run.await.expect("join").expect("cancelled cleanly");

    assert_eq!(output.process.termination, ProcessTermination::Cancelled);
    assert!(launcher.lifts().is_empty());
    let state = launcher.state.lock().unwrap();
    assert_eq!((state.kills, state.empties, state.cleanups), (1, 1, 1));
}

/// Unresolved terminal state with a recorded fence reconciles with a release
/// that carries the exact fencing token, at most once, and never abandons.
#[tokio::test]
async fn ac4_unresolved_state_releases_with_exact_fence_at_most_once() {
    let services = Arc::new(ScriptedServices::new(
        vec![granted(11)],
        vec![status(LeaseState::Active, Some(11))],
        vec![status(LeaseState::Active, Some(11)); 32],
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
    run.await.unwrap().unwrap();

    assert_eq!(*launcher.lifts.lock().unwrap(), vec![LeaseFencingToken(11)]);
    assert_eq!(
        *services.release_fences.lock().unwrap(),
        vec![LeaseFencingToken(11)],
        "release must carry the exact fencing token"
    );
    assert!(
        services.release_calls.load(Ordering::SeqCst) <= 1,
        "capacity is released at most once"
    );
    assert_eq!(services.abandon_calls.load(Ordering::SeqCst), 0);
}

/// Unresolved terminal state before any fence was recorded reconciles with a
/// single abandon and never releases capacity.
#[tokio::test]
async fn ac4_unresolved_state_without_fence_abandons_at_most_once() {
    let services = Arc::new(ScriptedServices::new(
        vec![],
        vec![],
        vec![status(LeaseState::Queued, None); 8],
    ));
    services.pause_queue.store(true, Ordering::SeqCst);
    services
        .abandon
        .lock()
        .unwrap()
        .push_back(LeaseResult::Abandoned {
            candidate_cleanup: false,
        });
    let launcher = Arc::new(ScriptedLauncher::default());
    let cancel = CancellationToken::new();
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    wait_for(&services.queue_calls, 1).await;
    cancel.cancel();
    run.await.unwrap().unwrap();

    assert!(launcher.lifts.lock().unwrap().is_empty());
    assert_eq!(
        services.abandon_calls.load(Ordering::SeqCst),
        1,
        "an unresolved lease without a fence abandons exactly once"
    );
    assert_eq!(services.release_calls.load(Ordering::SeqCst), 0);
    assert!(services.release_fences.lock().unwrap().is_empty());
}

//! Contention-degrade tests for [`LeaseInvocationRunner`].
//!
//! A command that cannot get a build lease must queue and be slow, never die.
//! These tests drive the real runner state machine — the same
//! `LeaseInvocationRunner::output` composition the workspace shell handler
//! calls — and pin the boundary between a *lost queue* (degrade to unleased
//! execution, successful command result) and a *broken lease authority*
//! (still an error). Split out of `process_lease_tests.rs` to stay within the
//! file-size guard; shares its harness (`ScriptedServices`, `clock`, `status`,
//! …) via `use super::*`.

use super::*;

/// Launcher double that keeps the cgroup boundary observable for a degraded
/// invocation. Unlike the harness launchers it never spawns a local process:
/// the child's lifetime is scripted, so a test can prove the runner let it run
/// to its own exit instead of killing it.
///
/// `fenced_lift` is the only call that can raise the leaf above the broker's
/// unleased quota, so "the command is slow, not unconstrained" is asserted as
/// "`lifts` stayed empty". `killed_while_running` records any kill that landed
/// before the child reached a terminal status, which is exactly the defect the
/// degrade removes.
#[derive(Clone)]
struct DegradeLauncher {
    state: Arc<Mutex<DegradeState>>,
}

struct DegradeState {
    /// The child exits on its own once it has been sampled this many times.
    /// `None` keeps it running until it is killed.
    exit_after_samples: Option<usize>,
    samples: usize,
    stdout: Vec<u8>,
    status: Option<std::process::ExitStatus>,
    lifts: Vec<LeaseFencingToken>,
    kills: usize,
    killed_while_running: usize,
    empties: usize,
    cleanups: usize,
}

impl DegradeLauncher {
    fn new(exit_after_samples: Option<usize>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DegradeState {
                exit_after_samples,
                samples: 0,
                stdout: b"degraded command output\n".to_vec(),
                status: None,
                lifts: Vec::new(),
                kills: 0,
                killed_while_running: 0,
                empties: 0,
                cleanups: 0,
            })),
        }
    }
    /// A child that finishes on its own a few polls in.
    fn completing() -> Self {
        Self::new(Some(3))
    }
    /// A child that never finishes by itself.
    fn running() -> Self {
        Self::new(None)
    }
    fn lifts(&self) -> Vec<LeaseFencingToken> {
        self.state.lock().unwrap().lifts.clone()
    }
    fn killed_while_running(&self) -> usize {
        self.state.lock().unwrap().killed_while_running
    }
    fn samples(&self) -> usize {
        self.state.lock().unwrap().samples
    }
}

impl CgroupLauncherClient for DegradeLauncher {
    fn launch(
        &self,
        _: Command,
        _: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        Ok(Box::new(DegradeHandle {
            state: self.state.clone(),
        }))
    }
}

struct DegradeHandle {
    state: Arc<Mutex<DegradeState>>,
}

impl ProcessHandle for DegradeHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.state.lock().unwrap().stdout))
    }
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        Ok(self.state.lock().unwrap().status)
    }
    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.state
            .lock()
            .unwrap()
            .status
            .ok_or_else(|| io::Error::other("scripted child is still running"))
    }
    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        use std::os::unix::process::ExitStatusExt;

        let mut state = self.state.lock().unwrap();
        state.samples += 1;
        if state.exit_after_samples.is_some_and(|n| state.samples >= n) {
            // Exit code 0 << 8: a clean, self-terminated child. A killed child
            // would carry the injected signal status instead.
            state
                .status
                .get_or_insert_with(|| std::process::ExitStatus::from_raw(0));
        }
        // Always above the runner's escalation threshold: this is a build-shaped
        // command, so it always reaches the lease authority.
        Ok(CpuStat {
            usage_usec: 1_000_000,
            ..CpuStat::default()
        })
    }
    fn fenced_lift(&mut self, token: &LeaseFencingToken) -> io::Result<()> {
        self.state.lock().unwrap().lifts.push(token.clone());
        Ok(())
    }
    fn kill(&mut self) -> io::Result<()> {
        use std::os::unix::process::ExitStatusExt;

        let mut state = self.state.lock().unwrap();
        state.kills += 1;
        if state.status.is_none() {
            state.killed_while_running += 1;
            state.status = Some(std::process::ExitStatus::from_raw(9));
        }
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

/// The durable terminal record the coordinator leaves behind for a queue whose
/// deadline expired: `status` reports a terminalized row as cancelled.
fn cancelled_terminal_status() -> LeaseResult {
    status(LeaseState::Cancelled, None)
}

/// The defect: a queue-wait timeout used to kill the child and return
/// `Err(LeaseWaitTimeout)`, which surfaced at the workspace shell handler as
/// "failed to run shell command: lease invocation failed: LeaseWaitTimeout" and
/// burned the agent's session for work that was merely queued. It must instead
/// leave the child running at the unleased quota and return its real result.
#[tokio::test]
async fn lease_wait_timeout_degrades_to_unleased_not_error() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::LeaseWaitTimeout {
            timeout_credit: None,
        }],
        vec![],
        vec![cancelled_terminal_status(); 3],
    ));
    let launcher = Arc::new(DegradeLauncher::completing());
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let output = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect("a queue-wait timeout must not fail the command");

    // The child ran to its own exit and its output survived.
    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(output.process.output.stdout, b"degraded command output\n");
    assert_eq!(
        launcher.killed_while_running(),
        0,
        "a timed-out lease wait must never kill a running child"
    );
    // Slow, not unconstrained: nothing lifted the leaf off the broker's
    // unleased quota, and the runner never tried to escalate again.
    assert!(
        launcher.lifts().is_empty(),
        "a degraded invocation must keep the unleased quota"
    );
    assert_eq!(services.queue_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        services.grant_calls.load(Ordering::SeqCst),
        0,
        "the degrade is one-way: no grant is attempted after the queue is lost"
    );
    assert!(launcher.samples() >= 3, "the child kept being driven");
}

/// The single bounded timeout credit still buys one more wait; only the spent
/// credit degrades. The runner must not treat the credited retry as a degrade,
/// or the one retry the coordinator paid for would be skipped.
#[tokio::test]
async fn credited_timeout_retries_once_then_degrades() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::LeaseWaitTimeout {
            timeout_credit: Some(djinn_supervisor::services::TimeoutCredit {
                units: 1,
                retry_after_ms: 0,
            }),
        }],
        vec![],
        vec![
            // The credited retry: still nothing, then the terminal record.
            LeaseResult::LeaseWaitTimeout {
                timeout_credit: None,
            },
            cancelled_terminal_status(),
            cancelled_terminal_status(),
            cancelled_terminal_status(),
        ],
    ));
    let launcher = Arc::new(DegradeLauncher::completing());
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let output = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect("a spent timeout credit degrades instead of failing");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(launcher.killed_while_running(), 0);
    assert!(launcher.lifts().is_empty());
    assert!(services.status_calls.load(Ordering::SeqCst) >= 1);
}

/// The response shape production actually produces after a queue deadline
/// expires: the coordinator terminalizes the row and `status` reports it as a
/// cancelled terminal state. That is the same lost queue, so it degrades too —
/// and the runner must stop polling. The scripted status queue holds exactly
/// one terminal answer; every later poll would fall through to
/// `LeaseUnavailable`, and three of those still fail the invocation. A
/// successful result therefore proves the polling stopped at the degrade.
#[tokio::test]
async fn terminal_record_before_any_grant_degrades_and_stops_polling() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::Queued(LeaseStatus {
            state: LeaseState::Queued,
            fencing_token: None,
            deadlines: deadlines(),
            pod_uid: None,
            candidate_cleanup: false,
        })],
        vec![],
        vec![cancelled_terminal_status()],
    ));
    let launcher = Arc::new(DegradeLauncher::completing());
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let output = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect("a terminal lease record degrades the invocation");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(launcher.killed_while_running(), 0);
    assert!(launcher.lifts().is_empty());
    assert_eq!(services.grant_calls.load(Ordering::SeqCst), 0);
}

/// Degrading must not disarm cancellation: a degraded child is still killed
/// when the tool call is cancelled, and the run still reports `Cancelled`.
#[tokio::test]
async fn cancellation_after_degrade_still_terminates_the_child() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::LeaseWaitTimeout {
            timeout_credit: None,
        }],
        vec![],
        vec![cancelled_terminal_status(); 3],
    ));
    let launcher = Arc::new(DegradeLauncher::running());
    let cancel = CancellationToken::new();
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        launcher.clone(),
        clock(),
    ));
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runner.output(command(), config(), run_cancel).await });
    // The degraded child keeps being driven; cancel it mid-flight.
    for _ in 0..10_000 {
        if launcher.samples() >= 3 && services.queue_calls.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    let output = run
        .await
        .expect("runner task joins")
        .expect("cancellation is a clean terminal result");

    assert_eq!(output.process.termination, ProcessTermination::Cancelled);
    assert_eq!(
        launcher.killed_while_running(),
        1,
        "cancellation must still kill the degraded child"
    );
    assert!(launcher.lifts().is_empty());
}

/// The degrade is scoped to a lost queue. An identity conflict means the lease
/// authority cannot be used coherently for this invocation at all, and must
/// still fail rather than silently running unleased.
#[tokio::test]
async fn lease_identity_conflict_still_fails() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::LeaseIdentityConflict {
            identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                task_id: "task".into(),
                task_run_id: "run".into(),
                invocation_id: "conflicting".into(),
            }),
        }],
        vec![],
        vec![],
    ));
    let launcher = Arc::new(DegradeLauncher::running());
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let error = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect_err("an identity conflict is not contention");

    assert!(matches!(error, LeaseInvocationError::LeaseIdentityConflict));
    assert!(launcher.lifts().is_empty());
}

/// Repeated unavailability is a broken authority, not a queue: it must still
/// fail after the bounded re-read, so a coordinator outage cannot be mistaken
/// for contention.
#[tokio::test]
async fn repeated_unavailability_still_fails() {
    let services = Arc::new(ScriptedServices::new(
        vec![LeaseResult::LeaseUnavailable],
        vec![],
        vec![LeaseResult::LeaseUnavailable; 8],
    ));
    let launcher = Arc::new(DegradeLauncher::running());
    let runner = LeaseInvocationRunner::new(services.clone(), launcher.clone(), clock());
    let error = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect_err("an unusable lease authority is not contention");

    assert!(matches!(error, LeaseInvocationError::LeaseUnavailable));
    assert!(launcher.lifts().is_empty());
}

/// Production composition seam: the real `DirectServices` lease authority over
/// a real durable repository, driven by the real runner — no hand-built lease
/// response anywhere.
///
/// `queue_deadline_ms` is passed exactly as `ShellLaunchContext::invocation`
/// passes it (30_000), which the coordinator reads as an absolute epoch
/// timestamp and therefore treats as already expired. That is the production
/// path that made every escalating shell command hit a queue timeout: it must
/// now produce a successful command result at the unleased quota.
#[tokio::test]
async fn real_lease_authority_queue_timeout_degrades_to_a_successful_command() {
    let db = crate::test_helpers::create_test_db();
    let services = Arc::new(crate::direct_services::DirectServices::new(
        crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
        CancellationToken::new(),
    ));
    let launcher = Arc::new(DegradeLauncher::completing());
    let runner = LeaseInvocationRunner::new(services, launcher.clone(), clock());
    // The exact deadlines `ShellLaunchContext::invocation` renders in a task
    // pod.
    let production_deadlines = LeaseInvocationConfig {
        queue_deadline_ms: 30_000,
        launch_deadline_ms: 60_000,
        ..config()
    };
    let output = runner
        .output(command(), production_deadlines, CancellationToken::new())
        .await
        .expect("a real queue timeout must not fail the command");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(output.process.output.stdout, b"degraded command output\n");
    assert_eq!(
        launcher.killed_while_running(),
        0,
        "the real timeout path must never kill a running child"
    );
    assert!(
        launcher.lifts().is_empty(),
        "a degraded invocation never lifts the unleased quota"
    );

    // Prove the successful result came through the timeout path rather than a
    // queue that simply never resolved: the durable row this invocation created
    // is terminal for the expired deadline.
    let row = djinn_db::BuildLeaseRepository::new(db)
        .get(&djinn_db::BuildLeaseKey {
            consumer_kind: djinn_db::BuildLeaseConsumerKind::TaskInvocation,
            consumer_id: output.identity.invocation_id.clone(),
        })
        .await
        .expect("read the durable lease row")
        .expect("the invocation queued a durable lease row");
    assert_eq!(row.state, djinn_db::BuildLeaseState::Terminal);
    assert_eq!(row.terminal_reason.as_deref(), Some("deadline_expired"));
}

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
    lifts: usize,
    /// When set, `fenced_lift` fails the way the privileged broker failed in
    /// production. It still counts the attempt, so a test can tell "the runner
    /// tried and the broker refused" from "the runner never tried".
    lift_error: Option<&'static str>,
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
                lifts: 0,
                lift_error: None,
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
    /// A child whose lift the privileged broker REFUSES — goxi blocker 14. The
    /// message is the one production actually produced.
    fn lift_refused() -> Self {
        let launcher = Self::new(Some(3));
        launcher.state.lock().unwrap().lift_error =
            Some("lease invocation failed: Launcher(ControlRejected(Fence))");
        launcher
    }
    fn lifts(&self) -> usize {
        self.state.lock().unwrap().lifts
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
        _: djinn_cgroup_launcher::LeaseAuthority,
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
    fn fenced_lift(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.lifts += 1;
        match state.lift_error {
            Some(message) => Err(io::Error::other(message)),
            None => Ok(()),
        }
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
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
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
        launcher.lifts() == 0,
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
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
    let output = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect("a spent timeout credit degrades instead of failing");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(launcher.killed_while_running(), 0);
    assert!(launcher.lifts() == 0);
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
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
    let output = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect("a terminal lease record degrades the invocation");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(launcher.killed_while_running(), 0);
    assert!(launcher.lifts() == 0);
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
    assert!(launcher.lifts() == 0);
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
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
    let error = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect_err("an identity conflict is not contention");

    assert!(matches!(error, LeaseInvocationError::LeaseIdentityConflict));
    assert!(launcher.lifts() == 0);
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
    let runner = LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock(),
    );
    let error = runner
        .output(command(), config(), CancellationToken::new())
        .await
        .expect_err("an unusable lease authority is not contention");

    assert!(matches!(error, LeaseInvocationError::LeaseUnavailable));
    assert!(launcher.lifts() == 0);
}

// ---------------------------------------------------------------------------
// Production composition seam: the real `DirectServices` lease authority over a
// real durable repository, driven by the real runner — no hand-built lease
// response anywhere.
// ---------------------------------------------------------------------------

/// Current wall clock as Unix epoch milliseconds.
#[allow(clippy::disallowed_methods)] // test-only reference clock for absolute deadlines
fn wall_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as i64
}

/// A runner clock frozen at an exact, caller-known epoch millisecond. The
/// invocation deadline contract is absolute epoch milliseconds, so the runner's
/// clock and the coordinator's clock must agree on "now" for a rendered deadline
/// to be in the future — and pinning `now` to a value the test already holds
/// makes the rendered deadline exactly predictable rather than a tolerance band.
#[allow(clippy::disallowed_methods)] // test-only monotonic seam; the wall side is explicit
fn clock_pinned_at(now_ms: i64) -> Arc<TestClock> {
    Arc::new(TestClock::new(
        std::time::UNIX_EPOCH + Duration::from_millis(now_ms as u64),
        Instant::now(),
    ))
}

/// Drive the durable admission epoch to a committed forward overlap with v1
/// enforcing — the only state in which a bound invocation may lift `cpu.max`.
/// Every write goes through the real repository, so the arming sequence is the
/// operator sequence.
pub(super) async fn arm_invocation_lift(db: &djinn_db::Database) {
    use djinn_db::{AdmissionHandoffAuthority, AdmissionHandoffPhase, V0Mode, V1Mode};

    let handoff = djinn_db::AdmissionHandoffRepository::new(db.clone());
    let row = handoff
        .seed_baseline()
        .await
        .expect("seed the baseline epoch");
    let row = handoff
        .set_modes_and_cap(row.epoch, V0Mode::Enforce, V1Mode::Enforce, None)
        .await
        .expect("arm v1 enforcement");
    handoff
        .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
        .await
        .expect("emergency acknowledges the armed epoch");
    let row = handoff
        .advance(row.epoch, AdmissionHandoffPhase::ForwardOverlap, &[])
        .await
        .expect("enter the forward overlap");
    handoff
        .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
        .await
        .expect("emergency acknowledges the overlap");
    handoff
        .acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
        .await
        .expect("invocation acknowledges the overlap");
}

/// Compose the production lease authority over a fresh database with an armed
/// cap. `lease_clock` is the coordinator's own deadline clock: advancing it past
/// a rendered deadline is how a test expires one without waiting.
async fn real_lease_services(
    db: &djinn_db::Database,
    lease_clock: Arc<dyn djinn_coordinator::build_lease::LeaseClock>,
) -> Arc<crate::direct_services::DirectServices> {
    let build_lease = Arc::new(
        djinn_coordinator::build_lease::BuildLeaseService::with_seams(
            Arc::new(djinn_db::BuildLeaseRepository::new(db.clone())),
            // The cap `DJINN_MAX_BUILD_TASKRUNS` arms in production; without it
            // `grant_next` short-circuits on `occupied >= cap` and nothing is
            // ever granted (see `build_lease_cap_arming_tests`).
            4,
            lease_clock,
            Arc::new(djinn_coordinator::build_lease::NoopLeaseTransactionPause),
            Arc::new(djinn_coordinator::build_lease::MetricsLeaseTelemetry),
        )
        .with_handoff_epoch(Arc::new(djinn_db::AdmissionHandoffRepository::new(
            db.clone(),
        ))),
    );
    Arc::new(crate::direct_services::DirectServices::with_build_lease(
        crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
        CancellationToken::new(),
        build_lease,
    ))
}

fn durable_row(
    db: &djinn_db::Database,
    invocation_id: &str,
) -> impl std::future::Future<Output = djinn_db::BuildLeaseRow> + use<> {
    let repository = djinn_db::BuildLeaseRepository::new(db.clone());
    let key = djinn_db::BuildLeaseKey {
        consumer_kind: djinn_db::BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: invocation_id.to_string(),
    };
    async move {
        repository
            .get(&key)
            .await
            .expect("read the durable lease row")
            .expect("the invocation queued a durable lease row")
    }
}

/// The defect this test exists for: `ShellLaunchContext::invocation` rendered
/// `queue_deadline_ms: 30_000` and `launch_deadline_ms: 60_000` — plainly
/// intended as 30s and 60s — but [`LeaseDeadlines`] carries ABSOLUTE Unix epoch
/// milliseconds. `30_000` is therefore `1970-01-01T00:00:30Z`, so the
/// coordinator terminalized every task-invocation lease as `deadline_expired`
/// the instant it was queued. Combined with the (correct) degrade to unleased
/// execution, every escalating shell command would have run at the launcher's
/// 250m quota forever instead of the leased 4-CPU quota — silently, ~16x slower.
///
/// The deadlines here are not restated: they come from the production producer
/// itself, so a regression in `ShellLaunchContext::invocation` fails this test.
#[tokio::test]
async fn rendered_invocation_deadlines_are_granted_and_reach_the_fenced_lift() {
    let db = crate::test_helpers::create_test_db();
    arm_invocation_lift(&db).await;
    let services = real_lease_services(
        &db,
        Arc::new(djinn_coordinator::build_lease::SystemLeaseClock),
    )
    .await;
    let launcher = Arc::new(DegradeLauncher::completing());
    // The runner's "now". The coordinator keeps its own real clock, so a
    // correctly rendered deadline must land ahead of it for the lease to be
    // granted at all.
    let now_ms = wall_clock_ms();
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        // The real durable admission authority over the armed row above. Not a
        // decision double: this is the read the production launcher performs.
        Arc::new(
            djinn_supervisor::services::DurableInvocationLiftAuthority::new(
                db.clone(),
                "degrade-test",
            ),
        ),
        launcher.clone(),
        clock_pinned_at(now_ms),
    ));
    // The exact config a task pod renders, straight from the production
    // producer — no deadline literal appears in this test.
    let context = crate::context::ShellLaunchContext::for_test(
        Arc::clone(&runner),
        "task".into(),
        "run".into(),
        "pod".into(),
    );
    let output = runner
        .output(
            command(),
            context.invocation(Duration::from_secs(60)),
            CancellationToken::new(),
        )
        .await
        .expect("the rendered deadlines must produce a usable lease");

    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(launcher.killed_while_running(), 0);
    // The whole point: the lease was actually granted, bound, and lifted off the
    // unleased 250m quota. Under the defect this list was always empty.
    assert_eq!(
        launcher.lifts(),
        1,
        "a lease queued with the rendered deadlines must reach the fenced lift"
    );

    let row = durable_row(&db, &output.identity.invocation_id).await;
    assert_ne!(
        row.terminal_reason.as_deref(),
        Some("deadline_expired"),
        "the rendered queue deadline must not be in the past"
    );

    // The stored deadlines are absolute instants exactly one queue deadline /
    // 60s past the runner's `now` — not 1970, and not the raw timeouts. The
    // queue value is read from the shared constant rather than restated: it is
    // deliberately the SAME deadline the coordinator stamps on the dispatch row
    // that blocks this invocation in the FIFO.
    assert_eq!(
        durable_deadline_ms(
            row.queue_deadline
                .as_deref()
                .expect("the queued row retains its deadline"),
        ),
        now_ms + djinn_supervisor::services::BUILD_LEASE_QUEUE_DEADLINE_MS,
        "the durable queue deadline must be now + BUILD_LEASE_QUEUE_DEADLINE"
    );
    assert_eq!(
        durable_deadline_ms(
            row.launch_deadline
                .as_deref()
                .expect("the granted row retains its launch deadline"),
        ),
        now_ms + 60_000,
        "the durable launch deadline must be now + 60s"
    );
}

/// Parse a durable deadline column back to epoch milliseconds.
///
/// `BuildLeaseRepository`'s shared column list renders every `timestamptz` as
/// RFC3339 in UTC with millisecond precision, which is the same representation
/// callers bind on the way in. That is deliberately the only timestamp format
/// this repository speaks, so this helper is the well-known parser and not a
/// second, format-specific one.
fn durable_deadline_ms(value: &str) -> i64 {
    (time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|error| panic!("durable deadline `{value}` is not parseable: {error}"))
        .unix_timestamp_nanos()
        / 1_000_000) as i64
}

/// The degrade path from the lost-queue fix must stay reachable and correct: a
/// deadline the coordinator's clock has genuinely passed still expires, and the
/// command still succeeds at the unleased quota rather than dying.
///
/// Expiry is constructed explicitly by advancing the coordinator's own deadline
/// clock an hour past the rendered (absolute, correct) deadline. It deliberately
/// no longer relies on a relative value being misread as an epoch timestamp —
/// that was the defect, and once fixed this test would otherwise have asserted
/// nothing.
#[tokio::test]
async fn real_lease_authority_queue_timeout_degrades_to_a_successful_command() {
    let db = crate::test_helpers::create_test_db();
    arm_invocation_lift(&db).await;
    let now_ms = wall_clock_ms();
    // One hour past every deadline this invocation can render.
    let expired_clock = Arc::new(djinn_coordinator::build_lease::ManualLeaseClock::new(
        now_ms + 3_600_000,
    ));
    let services = real_lease_services(&db, expired_clock).await;
    let launcher = Arc::new(DegradeLauncher::completing());
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        Arc::new(
            djinn_supervisor::services::DurableInvocationLiftAuthority::new(
                db.clone(),
                "degrade-test",
            ),
        ),
        launcher.clone(),
        clock_pinned_at(now_ms),
    ));
    let context = crate::context::ShellLaunchContext::for_test(
        Arc::clone(&runner),
        "task".into(),
        "run".into(),
        "pod".into(),
    );
    let output = runner
        .output(
            command(),
            context.invocation(Duration::from_secs(60)),
            CancellationToken::new(),
        )
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
        launcher.lifts() == 0,
        "a degraded invocation never lifts the unleased quota"
    );

    // Prove the successful result came through the timeout path rather than a
    // queue that simply never resolved: the durable row this invocation created
    // is terminal for the expired deadline.
    let row = durable_row(&db, &output.identity.invocation_id).await;
    assert_eq!(row.state, djinn_db::BuildLeaseState::Terminal);
    assert_eq!(row.terminal_reason.as_deref(), Some("deadline_expired"));
}

/// Unit boundary for the conversion itself: a configured timeout becomes an
/// absolute instant, and `Duration::ZERO` stays `0` — the contract's "no
/// deadline", which the graph-warm recovery and worker paths already pass.
#[test]
fn timeouts_render_as_absolute_epoch_deadlines() {
    let now_ms = 1_800_000_000_000_i64;
    assert_eq!(
        deadline_epoch_ms(now_ms, Duration::from_secs(30)),
        now_ms + 30_000
    );
    assert_eq!(
        deadline_epoch_ms(now_ms, Duration::ZERO),
        0,
        "zero means no deadline, never an expired one"
    );
    assert_eq!(
        deadline_epoch_ms(i64::MAX, Duration::from_secs(30)),
        i64::MAX,
        "the conversion saturates instead of wrapping into the past"
    );
    assert_eq!(epoch_ms(std::time::UNIX_EPOCH), 0);
    assert_eq!(
        epoch_ms(std::time::UNIX_EPOCH + Duration::from_millis(1_234)),
        1_234
    );
}

/// A REFUSED LIFT MUST DEGRADE, NOT FAIL THE COMMAND (goxi blocker 14).
///
/// The lift used to propagate with `?`, so any refusal from the privileged
/// broker became `LeaseInvocationError::Launcher` and failed the whole shell
/// tool call. Production measured 5 such failures against 10 launches in one
/// pod and the agent's `shell` tool errored repeatedly:
///
/// ```text
/// ReplyLoop: tool call returned error tool=shell
///   error=failed to run shell command: lease invocation failed: …
/// ```
///
/// That converts a defect in a *throttling optimisation* into total loss of the
/// agent's ability to run commands, when the fallback it denied itself — keep
/// running at the unleased quota — is strictly better than dying. It also
/// contradicted the precedent every test above pins: a lost lease QUEUE degrades
/// to continued unleased execution because contention must make a command slow,
/// never dead. A rejected lift is the same class of event.
///
/// This drives the REAL runner over the REAL durable lease services against an
/// armed epoch, so the invocation genuinely reaches the `Lift` arm — the lift is
/// attempted and refused, not skipped.
#[tokio::test]
async fn a_refused_lift_degrades_the_command_instead_of_failing_it() {
    let db = crate::test_helpers::create_test_db();
    arm_invocation_lift(&db).await;
    let services = real_lease_services(
        &db,
        Arc::new(djinn_coordinator::build_lease::SystemLeaseClock),
    )
    .await;
    let launcher = Arc::new(DegradeLauncher::lift_refused());
    let now_ms = wall_clock_ms();
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        Arc::new(
            djinn_supervisor::services::DurableInvocationLiftAuthority::new(
                db.clone(),
                "degrade-test",
            ),
        ),
        launcher.clone(),
        clock_pinned_at(now_ms),
    ));
    let context = crate::context::ShellLaunchContext::for_test(
        Arc::clone(&runner),
        "task".into(),
        "run".into(),
        "pod".into(),
    );

    let output = runner
        .output(
            command(),
            context.invocation(Duration::from_secs(60)),
            CancellationToken::new(),
        )
        .await
        .expect(
            "a refused cgroup lift must NOT fail the command. This is the assertion that would \
             have caught goxi blocker 14 turning a lease-subsystem defect into a dead `shell` \
             tool",
        );

    // The command completed normally, output intact, child never killed.
    assert_eq!(output.process.termination, ProcessTermination::Exited);
    assert_eq!(output.process.output.status.code(), Some(0));
    assert_eq!(output.process.output.stdout, b"degraded command output\n");
    assert_eq!(
        launcher.killed_while_running(),
        0,
        "a refused lift must never kill a running child"
    );

    // The lift was genuinely ATTEMPTED and refused — not skipped. Without this
    // the test would also pass on a runner that never reached the `Lift` arm at
    // all, which is a different (and equally shipped) defect. It is also the
    // one-way assertion: the runner must not retry a refused one-way lift.
    assert_eq!(
        launcher.lifts(),
        1,
        "the invocation must have reached the lift exactly once and had it refused"
    );

    // And the durable lease is still reconciled to terminal, so a refused lift
    // never leaks a counted build slot.
    let row = durable_row(&db, &output.identity.invocation_id).await;
    assert_eq!(
        row.state,
        djinn_db::BuildLeaseState::Terminal,
        "a degraded invocation must still reconcile its durable lease to terminal"
    );
}

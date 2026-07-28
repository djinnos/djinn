//! The queue deadline an escalating command actually sends, and what the FIFO
//! does with it.
//!
//! These tests drive the real [`LeaseInvocationRunner`] with the real
//! [`ShellContext::invocation`] configuration — not a hand-built
//! `LeaseInvocationConfig` — because the production defect was in the constant,
//! not in the state machine. Every other test in this suite passes a 100ms
//! scripted timeout and can never see it.
//!
//! # The production failure
//!
//! `grant_next` reads only the FIFO head. A denied dispatch attempt leaves a
//! weight-1 `queued` row carrying a THIRTY MINUTE queue deadline, and denials
//! fired every ~30s for hours (279 in 3h), so there was almost always such a
//! row ahead of every zero-weight invocation. The invocation's own deadline was
//! THIRTY SECONDS, so it always expired first, and because the degrade is
//! one-way the command then ran its entire life at the launcher's 250m unleased
//! quota. Measured: two live task-run pods issued 7 and 5 lease invocations and
//! ZERO escalated, while a sampled leaf still read `cpu.max = 25000 100000`
//! after burning 52.8 CPU-seconds — 211x the 0.25 CPU-s escalation threshold.

use super::*;

/// Launcher double whose leaf is always past the escalation threshold and whose
/// child finishes a few polls in. `lifts` is the side effect under test: it is
/// incremented only by `fenced_lift`, the one call that raises `cpu.max` off
/// the unleased quota.
#[derive(Clone)]
struct QueueLauncher {
    state: Arc<Mutex<QueueLauncherState>>,
}

#[derive(Default)]
struct QueueLauncherState {
    samples: usize,
    status: Option<std::process::ExitStatus>,
    lifts: usize,
}

impl QueueLauncher {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(QueueLauncherState::default())),
        }
    }
    fn lifts(&self) -> usize {
        self.state.lock().unwrap().lifts
    }
}

impl CgroupLauncherClient for QueueLauncher {
    fn launch(
        &self,
        _: Command,
        _: &TaskInvocationLeaseIdentity,
        _: djinn_cgroup_launcher::LeaseAuthority,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        Ok(Box::new(QueueHandle {
            state: self.state.clone(),
        }))
    }
}

struct QueueHandle {
    state: Arc<Mutex<QueueLauncherState>>,
}

impl ProcessHandle for QueueHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
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
        if state.samples >= 4 {
            state
                .status
                .get_or_insert_with(|| std::process::ExitStatus::from_raw(0));
        }
        // Build-shaped: always past the 0.25 CPU-s escalation threshold, so the
        // invocation always reaches the lease authority.
        Ok(CpuStat {
            usage_usec: 52_800_000,
            ..CpuStat::default()
        })
    }
    fn fenced_lift(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().lifts += 1;
        Ok(())
    }
    fn kill(&mut self) -> io::Result<()> {
        use std::os::unix::process::ExitStatusExt;

        let mut state = self.state.lock().unwrap();
        state
            .status
            .get_or_insert_with(|| std::process::ExitStatus::from_raw(9));
        Ok(())
    }
    fn wait_empty(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A `ShellContext` around the runner under test, so the invocation config
/// comes from PRODUCTION code (`ShellContext::invocation`) rather than from the
/// suite's 100ms `config()` helper.
fn shell_context(runner: Arc<LeaseInvocationRunner>) -> crate::context::ShellLaunchContext {
    crate::context::ShellLaunchContext::for_test(runner, "task".into(), "run".into(), "pod".into())
}

/// Wait 60 seconds behind the FIFO head — longer than the old 30s queue
/// timeout, far shorter than the 30-minute dispatch deadline that blocks it —
/// and the escalation must still land.
///
/// The assertion is the SIDE EFFECT: `fenced_lift` was called, i.e. the cgroup
/// quota was actually raised off the unleased 250m. Reverting the constant to
/// `Duration::from_secs(30)` makes this fail with `lifts == 0`.
#[tokio::test]
async fn invocation_waiting_past_thirty_seconds_still_escalates() {
    let services = Arc::new(ScriptedServices::new(
        // Unused: the FIFO answers `queue_lease` from the deadline instead.
        vec![],
        // The grant acknowledgement, then the still-live durable state that
        // authorizes the bind.
        vec![status(LeaseState::Launching, Some(1))],
        vec![status(LeaseState::Active, Some(1))],
    ));
    let clock = clock();
    services.honour_queue_deadline(clock.clone(), Duration::from_secs(60));
    let launcher = Arc::new(QueueLauncher::new());
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock.clone(),
    ));
    let context = shell_context(runner.clone());

    let output = runner
        .output(
            command(),
            context.invocation(Duration::from_secs(3_600)),
            CancellationToken::new(),
        )
        .await
        .expect("a queued invocation must not fail the command");

    assert_eq!(
        launcher.lifts(),
        1,
        "an invocation that waited 60s behind the FIFO head must still be \
         escalated: its queue deadline has to outlast the dispatch row blocking \
         it, not expire 60x sooner"
    );
    assert_eq!(output.process.termination, ProcessTermination::Exited);
}

/// The complement, so the test above is not passing because the deadline is
/// ignored: a wait past the configured deadline still degrades.
#[tokio::test]
async fn invocation_waiting_past_the_queue_deadline_degrades() {
    let services = Arc::new(ScriptedServices::new(
        vec![],
        vec![],
        vec![status(LeaseState::Cancelled, None); 4],
    ));
    let clock = clock();
    // One second past `BUILD_LEASE_QUEUE_DEADLINE`.
    services.honour_queue_deadline(
        clock.clone(),
        djinn_supervisor::services::BUILD_LEASE_QUEUE_DEADLINE + Duration::from_secs(1),
    );
    let launcher = Arc::new(QueueLauncher::new());
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock.clone(),
    ));
    let context = shell_context(runner.clone());

    let output = runner
        .output(
            command(),
            context.invocation(Duration::from_secs(3_600)),
            CancellationToken::new(),
        )
        .await
        .expect("a lost queue degrades, it never fails the command");

    assert_eq!(
        launcher.lifts(),
        0,
        "an invocation whose queue deadline genuinely expired must not lift"
    );
    assert_eq!(output.process.termination, ProcessTermination::Exited);
}

// ── Degrade diagnostic ────────────────────────────────────────────────────
//
// The degrade path had NO `tracing::` call at all. Its only observable was a
// `lease invocation launched` line with no matching `cgroup quota lifted` —
// an absence, which nothing greps for and no alarm fires on. Four armed
// rollouts were diagnosed by reading `cpu.max` out of cgroupfs by hand.

use std::collections::HashMap;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

#[derive(Clone, Debug)]
struct CapturedEvent {
    level: tracing::Level,
    fields: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct EventCaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[derive(Default)]
struct FieldVisitor {
    fields: HashMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S> Layer<S> for EventCaptureLayer
where
    S: tracing::Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            level: *event.metadata().level(),
            fields: visitor.fields,
        });
    }
}

/// A silent 16x slowdown must become a greppable event naming what happened,
/// to whom, how much CPU it had already burned, and what quota it is stuck at.
#[tokio::test]
async fn a_degraded_invocation_reports_why_and_at_what_quota() {
    let services = Arc::new(ScriptedServices::new(
        vec![],
        vec![],
        vec![status(LeaseState::Cancelled, None); 4],
    ));
    let clock = clock();
    services.honour_queue_deadline(
        clock.clone(),
        djinn_supervisor::services::BUILD_LEASE_QUEUE_DEADLINE + Duration::from_secs(1),
    );
    let launcher = Arc::new(QueueLauncher::new());
    let runner = Arc::new(LeaseInvocationRunner::new(
        services.clone(),
        services.clone(),
        launcher.clone(),
        clock.clone(),
    ));
    let context = shell_context(runner.clone());

    let capture = EventCaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    let output = runner
        .output(
            command(),
            context.invocation(Duration::from_secs(3_600)),
            CancellationToken::new(),
        )
        .await
        .expect("a lost queue degrades, it never fails the command");
    drop(guard);

    assert_eq!(launcher.lifts(), 0, "this invocation must have degraded");
    let events = capture.events.lock().unwrap().clone();
    let degrades: Vec<_> = events
        .iter()
        .filter(|event| {
            event
                .fields
                .get("message")
                .is_some_and(|message| message.contains("DEGRADED"))
        })
        .collect();
    assert_eq!(
        degrades.len(),
        1,
        "the one-way degrade reports exactly once, and it reports at all \
         (observed events: {events:?})"
    );
    let degrade = degrades[0];
    assert_eq!(degrade.level, tracing::Level::WARN);
    assert_eq!(
        degrade.fields.get("terminal_reason").map(String::as_str),
        Some("deadline_expired"),
        "the reason has to name WHY the lease can never be granted"
    );
    assert_eq!(
        degrade.fields.get("invocation_id").map(String::as_str),
        Some(output.identity.invocation_id.as_str()),
        "the line must identify the invocation that is now throttled"
    );
    assert_eq!(
        degrade
            .fields
            .get("observed_usage_usec")
            .map(String::as_str),
        Some("52800000"),
        "the CPU already burned is what makes a degrade actionable"
    );
    assert_eq!(
        degrade.fields.get("degraded_quota").map(String::as_str),
        Some("launcher_unleased"),
        "the line must name the quota the command is stuck at"
    );
    // A status read reports a terminalized row as a bare `Cancelled` and
    // carries no terminal reason over the wire, so the deadline and the clock
    // are the only evidence in the pod that the queue position expired rather
    // than somebody cancelling it.
    assert_eq!(
        degrade
            .fields
            .get("queue_deadline_passed")
            .map(String::as_str),
        Some("true")
    );
    let deadline: i64 = degrade
        .fields
        .get("queue_deadline_ms")
        .expect("the deadline it sent")
        .parse()
        .expect("epoch milliseconds");
    let now: i64 = degrade
        .fields
        .get("now_ms")
        .expect("the clock it is judged against")
        .parse()
        .expect("epoch milliseconds");
    assert!(
        deadline > 0 && now >= deadline,
        "the diagnostic must carry the deadline it actually sent ({deadline}) and the \
         clock that outlived it ({now})"
    );
}

/// The invariant the whole defect reduces to: the deadline an invocation sends
/// and the deadline the coordinator stamps on the dispatch row that can block
/// it are ONE value. They drifted by a factor of 60 (30s vs 30min) and every
/// invocation in the fleet expired behind a dispatch row.
///
/// Also pins the env override, including that it is what lets an operator make
/// them differ deliberately.
#[test]
fn queue_timeout_defaults_to_the_shared_dispatch_deadline() {
    use crate::context::ShellLaunchContext;
    use djinn_supervisor::services::{BUILD_LEASE_QUEUE_DEADLINE, BUILD_LEASE_QUEUE_DEADLINE_MS};

    // Both assertions live in one test on purpose: they read and write the same
    // process-global environment variable, so splitting them would race under a
    // thread-per-test runner.
    // SAFETY: single-threaded test body; no other test reads this variable.
    unsafe { std::env::remove_var(ShellLaunchContext::QUEUE_TIMEOUT_ENV) };
    assert_eq!(
        ShellLaunchContext::queue_timeout(),
        BUILD_LEASE_QUEUE_DEADLINE
    );
    assert_eq!(
        i64::try_from(BUILD_LEASE_QUEUE_DEADLINE.as_millis()).unwrap(),
        BUILD_LEASE_QUEUE_DEADLINE_MS,
        "the dispatch row's deadline and the invocation's are one value"
    );

    // SAFETY: as above.
    unsafe { std::env::set_var(ShellLaunchContext::QUEUE_TIMEOUT_ENV, "90") };
    assert_eq!(ShellLaunchContext::queue_timeout(), Duration::from_secs(90));
    // SAFETY: as above.
    unsafe { std::env::remove_var(ShellLaunchContext::QUEUE_TIMEOUT_ENV) };
    assert_eq!(
        ShellLaunchContext::queue_timeout(),
        BUILD_LEASE_QUEUE_DEADLINE
    );
}

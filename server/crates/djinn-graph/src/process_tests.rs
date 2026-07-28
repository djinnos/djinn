//! Unit tests for [`super`], the async process-spawn wrappers.
//!
//! Split out of `process.rs` so the production module stays inside the
//! `Server Guards` file-size budget (MAX_LINES=1500 / MAX_BYTES=51200).
//! Included with `#[path]` so it stays a child of the module under test,
//! alongside the Linux supervisor fixtures in
//! `process_linux_supervisor_tests.rs`, which are included the same way and
//! share this module's [`super::child_lifecycle_test_lock`].

use super::*;
#[cfg(not(target_os = "linux"))]
use djinn_core::clock::{Clock, SystemClock};

/// Drains the process-global child reaper on scope exit — including on
/// panic — while [`child_lifecycle_test_lock`] is still held. Declared
/// AFTER the lock guard in each test so it drops (and drains) BEFORE the
/// lock is released.
struct ReaperDrainGuard;

impl Drop for ReaperDrainGuard {
    fn drop(&mut self) {
        drain_child_reaper_for_tests();
    }
}

fn scopeguard_drain() -> ReaperDrainGuard {
    ReaperDrainGuard
}

/// A killed child's captured output is the ONLY account of what it was
/// doing, and it used to be discarded: callers got the bare four-word
/// string `"process timed out"` for an hour-long rust-analyzer run. The
/// error must carry the tail.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_error_carries_the_partial_output_the_child_produced() {
    let _serial = child_lifecycle_test_lock().lock().await;
    let _drain = scopeguard_drain();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("echo indexing-crate-427 >&2; echo scanned 1200 files; sleep 30");

    let err = output_with_timeout(cmd, Duration::from_millis(500))
        .await
        .expect_err("hung child should surface as a timeout error");

    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    let message = err.to_string();
    assert!(
        message.contains("indexing-crate-427"),
        "the stderr the child produced before the kill must survive onto \
         the error; got: {message}"
    );
    assert!(
        message.contains("scanned 1200 files"),
        "the stdout tail must survive too; got: {message}"
    );
    assert!(
        message.contains("500ms"),
        "the deadline that fired must be named; got: {message}"
    );
}

/// A silent child must say so explicitly rather than leaving the operator
/// wondering whether the capture was dropped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_error_says_so_when_the_child_produced_nothing() {
    let _serial = child_lifecycle_test_lock().lock().await;
    let _drain = scopeguard_drain();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("sleep 30");

    let err = output_with_timeout(cmd, Duration::from_millis(300))
        .await
        .expect_err("hung child should surface as a timeout error");

    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(err.to_string().contains("produced no output"), "got: {err}");
}

/// The tail is bounded, and cutting a multi-byte character must not panic.
#[test]
fn output_tail_is_bounded_and_utf8_safe() {
    let mut noisy = "x".repeat(TIMED_OUT_OUTPUT_TAIL_BYTES * 3).into_bytes();
    noisy.extend_from_slice("tail-marker".as_bytes());
    let tail = output_tail(&noisy);
    assert!(tail.len() <= TIMED_OUT_OUTPUT_TAIL_BYTES);
    assert!(tail.ends_with("tail-marker"));

    // A slice that lands mid-character is repaired, never panics.
    let multibyte = "é".repeat(TIMED_OUT_OUTPUT_TAIL_BYTES).into_bytes();
    let _ = output_tail(&multibyte);
}

/// A hung child (`sleep 30`) must be terminated within the grace window and
/// the call must return promptly rather than hanging. Linux has a stronger
/// fixture in `process_linux_supervisor_tests` which records its PID and
/// asserts the six-second lifecycle contract.
#[cfg(not(target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_kills_hung_child_promptly() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("sleep 30");

    let start = SystemClock::new().now_instant();
    let err = output_with_timeout(cmd, Duration::from_millis(200))
        .await
        .expect_err("hung child should surface as a timeout error");
    let elapsed = start.elapsed();

    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    // 200ms timeout + bounded TERM/KILL cleanup; comfortably under 8s.
    assert!(
        elapsed < Duration::from_secs(8),
        "call should return promptly after kill, took {elapsed:?}"
    );
}

/// Keep the macOS/other-Unix local waiter deadline-bound. This is separate
/// from the Linux supervisor test because a regression to `cmd.output()`
/// would otherwise only be caught when this module is exercised off Linux.
#[cfg(not(target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_linux_unix_timeout_argument_is_enforced() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("sleep 30");

    let start = SystemClock::new().now_instant();
    let err = output_with_timeout(cmd, Duration::from_millis(100))
        .await
        .expect_err("sleep must be terminated at the supplied deadline");

    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "non-Linux Unix must not fall back to unbounded Command::output()"
    );
}

/// The child runs in its own process group, and after the timeout kill the
/// whole group is gone (no leaked grandchildren).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_reaps_whole_process_group() {
    let _serial = child_lifecycle_test_lock().lock().await;
    let _drain = scopeguard_drain();
    // The shell prints its pgid, then forks a grandchild that sleeps. If the
    // group kill works, signalling -pgid reaches the grandchild too.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("ps -o pgid= -p $$ | tr -d ' '; sleep 30 & sleep 30");

    let out = output_with_kill(cmd, Duration::from_millis(200))
        .await
        .expect("process should be reaped after timeout kill");

    // Killed by signal => not a clean success.
    assert!(!out.status.success());
    assert!(killed_by_signal(&out.status).is_some());

    let pgid: i32 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .expect("child should have printed its pgid");

    // The supervisor should have already ensured the direct child's process
    // group was cleaned up.  On systems where PID 1 reaps orphans promptly,
    // kill(-pgid, 0) returns ESRCH.  In test containers where PID 1 is not a
    // reaping init, orphaned grandchildren may linger as zombies; verify that
    // no *live* (non-zombie) member of the group remains.
    let mut gone = false;
    for _ in 0..20 {
        let rc = unsafe { libc::kill(-pgid, 0) };
        if rc != 0 {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !gone {
        // The group didn't fully disappear — check that any remaining
        // processes are zombies (state Z), not live.
        let live = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ps -e -o pgid=,stat= --no-headers | awk '$1=={pgid} && $2!~/Z/' | wc -l"
            ))
            .output();
        if let Ok(out) = live {
            let count: usize = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            assert_eq!(
                count, 0,
                "live processes remain in process group {pgid} after timeout kill"
            );
        }
    }
}

/// A child killed by an EXTERNAL signal (modelling the kernel OOM killer)
/// *before* the deadline must NOT be reported as a timeout — otherwise the
/// `timed_out` status becomes untrustworthy. It surfaces as a distinct
/// non-timeout failure instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_signal_before_deadline_is_not_a_timeout() {
    let _serial = child_lifecycle_test_lock().lock().await;
    let _drain = scopeguard_drain();
    // The shell SIGKILLs itself immediately, well within the generous
    // 30s deadline, so our timeout-kill path never fires.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("kill -9 $$");

    let err = output_with_timeout(cmd, Duration::from_secs(30))
        .await
        .expect_err("a signal-killed child must surface as an error");

    assert_ne!(
        err.kind(),
        io::ErrorKind::TimedOut,
        "an external SIGKILL before the deadline must not be reported as a timeout"
    );
    assert!(
        err.to_string().contains("signal"),
        "expected a distinct signal-death message, got: {err}"
    );
}

/// The in-process warmer (no `DJINN_PROJECT_ROOT`) keeps nice 10 so the
/// indexer fan-out can't starve the interactive server it shares a cgroup
/// with.
#[test]
fn nice_level_defaults_to_10_in_process() {
    assert_eq!(resolve_nice_level(None, false), 10);
}

/// In a dedicated warm Pod the indexer owns the whole cgroup — nothing to
/// yield to — so it must run at normal priority (0), not niced. This is the
/// core of the starvation fix: niceing a sole-tenant cgroup only costs the
/// throughput graph freshness depends on.
#[test]
fn nice_level_is_zero_in_pod_workspace() {
    assert_eq!(resolve_nice_level(None, true), 0);
}

/// An explicit `DJINN_INDEXER_NICE` override wins in either context.
#[test]
fn explicit_nice_override_wins() {
    assert_eq!(resolve_nice_level(Some("5"), true), 5);
    assert_eq!(resolve_nice_level(Some("15"), false), 15);
    assert_eq!(resolve_nice_level(Some(" 3 "), true), 3);
}

/// A malformed override falls back to the context default rather than
/// crashing the spawn path.
#[test]
fn malformed_nice_override_falls_back() {
    assert_eq!(resolve_nice_level(Some("not-a-number"), true), 0);
    assert_eq!(resolve_nice_level(Some(""), false), 10);
}

/// A fast command still returns its real output and a success status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_command_returns_output() {
    let _serial = child_lifecycle_test_lock().lock().await;
    let _drain = scopeguard_drain();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("printf hello");

    let out = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect("fast command succeeds");
    assert!(out.status.success());
    assert_eq!(&out.stdout, b"hello");
}

use super::*;
use crate::child_reaper::worker_child_reaper;
use djinn_core::clock::{Clock, SystemClock};
use std::path::Path;
use std::sync::Once;

fn enable_subreaper_for_fixture_orphans() {
    static ENABLE: Once = Once::new();
    ENABLE.call_once(|| {
        // The fixture intentionally lets a same-PGID child outlive its direct parent.
        // Adopt and reap that orphan so kill(-pgid, 0) deterministically reaches ESRCH.
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
        assert_eq!(rc, 0, "enable Linux subreaper for lifecycle fixture");
    });
}

fn recorded_identities(output: &[u8]) -> (i32, i32, i32) {
    let line = String::from_utf8_lossy(output);
    let mut direct = None;
    let mut pgid = None;
    let mut descendant = None;
    for field in line.split_whitespace() {
        if let Some(pid) = field.strip_prefix("direct=") {
            direct = Some(pid.parse().expect("direct PID is numeric"));
        }
        if let Some(pid) = field.strip_prefix("pgid=") {
            pgid = Some(pid.parse().expect("PGID is numeric"));
        }
        if let Some(pid) = field.strip_prefix("descendant=") {
            descendant = Some(pid.parse().expect("descendant PID is numeric"));
        }
    }
    (
        direct.expect("fixture recorded direct PID"),
        pgid.expect("fixture recorded PGID"),
        descendant.expect("fixture recorded descendant PID"),
    )
}

fn assert_pgid_is_gone(pgid: i32) {
    // SAFETY: signal zero only probes the exact recorded process group.
    let rc = unsafe { libc::kill(-pgid, 0) };
    assert_eq!(rc, -1, "recorded process group {pgid} is still present");
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "recorded process group {pgid} did not disappear with ESRCH"
    );
}

fn wait_for_fixture_file(path: &Path, timeout: Duration) -> String {
    let clock = SystemClock::new();
    let deadline = clock.now_instant() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && !contents.trim().is_empty()
        {
            return contents;
        }
        assert!(
            clock.now_instant() < deadline,
            "fixture did not publish {} within {timeout:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_linux_supervisors_idle(timeout: Duration) -> bool {
    let clock = SystemClock::new();
    let deadline = clock.now_instant() + timeout;
    loop {
        if active_linux_supervisor_count_for_test() == 0 {
            return true;
        }
        if clock.now_instant() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct EscapedPid(i32);

impl Drop for EscapedPid {
    fn drop(&mut self) {
        // The escaped holder is deliberately outside the command PGID, so
        // it is not lifecycle-owned by this command supervisor. Do not let
        // the regression fixture leave that independent process behind.
        // SAFETY: this targets only the exact fixture PID recorded below.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

/// Normal direct exit must preserve natural grace for a same-PGID child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_exit_with_descendant_grace() {
    enable_subreaper_for_fixture_orphans();
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let outcome = temp.path().join("descendant-outcome");
    let outcome_path = outcome.to_str().expect("UTF-8 temp path");
    // Deliberately do not `wait`: the direct shell exits immediately while
    // its same-PGID descendant keeps the inherited pipes open. A TERM trap
    // makes a premature escalation observable in a fixture-owned file.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!(
        "(trap 'printf term > {outcome_path}; exit 99' TERM; sleep 0.35; \
             printf natural > {outcome_path}) & descendant=$!; \
             printf 'direct=%s pgid=%s descendant=%s\\n' \"$$\" \"$$\" \"$descendant\"; exit 0"
    ));
    let start = SystemClock::new().now_instant();
    let out = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect("normal exit should succeed");
    let elapsed = start.elapsed();
    assert!(out.status.success(), "expected exit 0, got {}", out.status);
    let (direct, pgid, descendant) = recorded_identities(&out.stdout);
    assert_eq!(
        pgid, direct,
        "isolated direct child must own its recorded PGID"
    );
    assert_ne!(direct, descendant, "fixture must record a descendant PID");
    assert_eq!(
        wait_for_fixture_file(&outcome, Duration::from_secs(1)).trim(),
        "natural",
        "the same-PGID descendant must exit naturally, not receive TERM"
    );
    assert_pgid_is_gone(pgid);
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
        "supervisor must wait for natural descendant disappearance, took {elapsed:?}"
    );
}

/// A TERM-ignoring same-PGID descendant forces TERM-to-KILL escalation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn term_ignoring_descendant_escalates_to_kill() {
    enable_subreaper_for_fixture_orphans();
    // The direct shell exits non-zero immediately and does not wait for its
    // descendant. `exec sleep` inherits SIG_IGN for TERM, so only SIGKILL
    // can remove the recorded same-PGID descendant.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(concat!(
        "(trap '' TERM; exec sleep 300) & ",
        "descendant=$!; ",
        "printf 'direct=%s pgid=%s descendant=%s\\n' \"$$\" \"$$\" \"$descendant\"; ",
        "exit 23"
    ));
    let start = SystemClock::new().now_instant();
    let out = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect("command should complete after KILL escalation");
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(23));
    let (direct, pgid, descendant) = recorded_identities(&out.stdout);
    assert_eq!(
        pgid, direct,
        "isolated direct child must own its recorded PGID"
    );
    assert_ne!(direct, descendant, "fixture must record its descendant PID");
    assert_pgid_is_gone(pgid);
    assert!(
        elapsed >= Duration::from_millis(3500) && elapsed < Duration::from_secs(6),
        "TERM-ignoring descendant must require bounded KILL escalation, took {elapsed:?}"
    );
}

/// Cancellation must finish even when an escaped setsid holder retains pipes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_caller_triggers_cleanup() {
    enable_subreaper_for_fixture_orphans();
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let escaped_pid_file = temp.path().join("escaped-pid");
    let identities = temp.path().join("identities");
    let escaped_path = escaped_pid_file.to_str().expect("UTF-8 temp path");
    let identities_path = identities.to_str().expect("UTF-8 temp path");
    // `setsid` puts the pipe holder outside the original PGID. It publishes
    // its PID only after the session transition, then execs sleep while
    // retaining stdout/stderr. The direct shell waits only for this setup
    // marker, never for the escaped process's lifetime.
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!(
            "setsid sh -c 'printf \"%s\\n\" \"$$\" > {escaped_path}; exec sleep 300' & \
             while [ ! -s {escaped_path} ]; do :; done; escaped=$(cat {escaped_path}); \
             printf 'direct=%s pgid=%s descendant=%s\\n' \"$$\" \"$$\" \"$escaped\" > {identities_path}; sleep 300"
        ));
    let start = SystemClock::new().now_instant();
    let handle = tokio::spawn(output_with_timeout(cmd, Duration::from_secs(300)));
    let fixture = wait_for_fixture_file(&identities, Duration::from_secs(2));
    let (direct, pgid, escaped) = recorded_identities(fixture.as_bytes());
    assert_eq!(
        pgid, direct,
        "isolated direct child must own its recorded PGID"
    );
    let escaped_pgid = unsafe { libc::getpgid(escaped) };
    assert_ne!(
        escaped_pgid, pgid,
        "setsid holder must escape the original PGID"
    );
    let _escaped_cleanup = EscapedPid(escaped);
    // Aborting drops the caller future. Its independent supervisor must
    // close its nonblocking drain ownership without waiting for escaped EOF.
    handle.abort();
    let _ = handle.await;
    // These are deliberately separate completion oracles: registry idle proves
    // the direct status route is gone, while supervisor idle acknowledges the
    // later return from `run_linux_supervisor` after synchronous drain closure.
    assert!(
        worker_child_reaper().wait_for_supervisors_idle(Duration::from_secs(4)),
        "independent supervisor did not empty its registry after caller cancellation"
    );
    assert!(
        wait_for_linux_supervisors_idle(Duration::from_secs(4)),
        "Linux supervisor retained drain ownership after its registry route emptied"
    );
    let elapsed = start.elapsed();
    assert_pgid_is_gone(pgid);
    assert!(
        elapsed < Duration::from_secs(4),
        "cleanup after caller drop should stay within TERM/KILL bound, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_zero_exit_preserved() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("exit 42");
    let out = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect("non-zero exit should not be an error here");
    assert_eq!(out.status.code(), Some(42));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_returns_original_io_error() {
    let cmd = Command::new("/nonexistent-binary-that-does-not-exist-12345");
    let err = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect_err("spawn should fail");
    assert_ne!(err.kind(), io::ErrorKind::TimedOut);
}

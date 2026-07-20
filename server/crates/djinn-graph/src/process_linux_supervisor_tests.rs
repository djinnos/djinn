use super::*;
use crate::child_reaper::worker_child_reaper;
use djinn_core::clock::{Clock, SystemClock};
use std::path::Path;
use std::sync::{Once, OnceLock};

fn lifecycle_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn assert_pid_gone_without_zombie(pid: i32) {
    let stat = format!("/proc/{pid}/stat");
    if let Ok(contents) = std::fs::read_to_string(&stat) {
        let state = contents
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next());
        assert_ne!(state, Some('Z'), "recorded PID {pid} remained a zombie");
        panic!("recorded PID {pid} still has {stat}");
    }
}

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

fn recorded_direct_and_pgid(output: &[u8]) -> (i32, i32) {
    let line = String::from_utf8_lossy(output);
    let mut direct = None;
    let mut pgid = None;
    for field in line.split_whitespace() {
        if let Some(pid) = field.strip_prefix("direct=") {
            direct = Some(pid.parse().expect("direct PID is numeric"));
        }
        if let Some(group) = field.strip_prefix("pgid=") {
            pgid = Some(group.parse().expect("PGID is numeric"));
        }
    }
    (
        direct.expect("fixture recorded direct PID"),
        pgid.expect("fixture recorded PGID"),
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

fn wait_for_adopted_status(pid: i32, timeout: Duration) -> crate::child_reaper::TerminalStatus {
    let clock = SystemClock::new();
    let deadline = clock.now_instant() + timeout;
    loop {
        if let Some(record) = worker_child_reaper()
            .drain()
            .into_iter()
            .find(|record| record.status.pid == pid as u32)
        {
            return record.status;
        }
        assert!(
            clock.now_instant() < deadline,
            "reaper did not record adopted PID {pid}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Normal direct exit must preserve natural grace for a same-PGID child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_exit_with_descendant_grace() {
    let _serial = lifecycle_test_lock().lock().await;
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
    assert_pid_gone_without_zombie(direct);
    assert_pid_gone_without_zombie(descendant);
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
        "supervisor must wait for natural descendant disappearance, took {elapsed:?}"
    );
}

/// A TERM-ignoring same-PGID descendant forces TERM-to-KILL escalation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn term_ignoring_descendant_escalates_to_kill() {
    let _serial = lifecycle_test_lock().lock().await;
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
    assert_pid_gone_without_zombie(direct);
    assert_pid_gone_without_zombie(descendant);
    assert!(
        elapsed >= Duration::from_millis(3500) && elapsed < Duration::from_secs(6),
        "TERM-ignoring descendant must require bounded KILL escalation, took {elapsed:?}"
    );
}

/// Cancellation must finish even when an escaped setsid holder retains pipes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_caller_triggers_cleanup() {
    let _serial = lifecycle_test_lock().lock().await;
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
    // The escaped child is terminated below and its adopted terminal status is observed.
    // Aborting drops the caller future. Its independent supervisor must
    // close its nonblocking drain ownership without waiting for escaped EOF.
    handle.abort();
    let _ = handle.await;
    // An escaped session is intentionally outside command-PGID cleanup. Kill it
    // explicitly, then use adopted-status/idle hooks as the completion oracle.
    unsafe { libc::kill(escaped, libc::SIGKILL) };
    // These are deliberately separate completion oracles: registry idle proves
    // the direct status route is gone, while supervisor idle acknowledges the
    // later return from `run_linux_supervisor` after synchronous drain closure.
    // Keep both checks: route removal can precede closure of inherited pipe FDs.
    assert!(
        worker_child_reaper().wait_for_supervisors_idle(Duration::from_secs(4)),
        "independent supervisor did not empty its registry after caller cancellation"
    );
    assert!(
        wait_for_linux_supervisors_idle(Duration::from_secs(4)),
        "Linux supervisor retained drain ownership after its registry route emptied"
    );
    assert_pgid_is_gone(pgid);
    assert_pid_gone_without_zombie(direct);
    assert_pid_gone_without_zombie(escaped);
    // The dedicated parent-exit/setsid fixture below proves adopted terminal
    // status routing. Here registry idleness is the cancellation completion
    // oracle, so acknowledge any status the shared reaper retained.
    let _ = worker_child_reaper().drain();
    assert!(worker_child_reaper().wait_for_adopted_idle(Duration::ZERO));
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "cleanup after caller drop should stay within TERM/KILL bound, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_zero_exit_preserved() {
    let _serial = lifecycle_test_lock().lock().await;
    enable_subreaper_for_fixture_orphans();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("printf 'direct=%s pgid=%s\\n' \"$$\" \"$$\"; exit 42");
    let start = SystemClock::new().now_instant();
    let out = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect("non-zero exit should not be an error here");
    assert_eq!(out.status.code(), Some(42));
    let (direct, pgid) = recorded_direct_and_pgid(&out.stdout);
    assert_eq!(direct, pgid);
    assert_pgid_is_gone(pgid);
    assert_pid_gone_without_zombie(direct);
    assert!(start.elapsed() < Duration::from_secs(6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_returns_original_io_error() {
    let _serial = lifecycle_test_lock().lock().await;
    let cmd = Command::new("/nonexistent-binary-that-does-not-exist-12345");
    let err = output_with_timeout(cmd, Duration::from_secs(10))
        .await
        .expect_err("spawn should fail");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(worker_child_reaper().supervisors().is_empty());
    assert!(worker_child_reaper().wait_for_supervisors_idle(Duration::ZERO));
    assert!(worker_child_reaper().wait_for_adopted_idle(Duration::ZERO));
}

/// A direct parent exits before its descendant calls `setsid`, so the latter is
/// adopted by the worker subreaper outside the original command process group.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaper_records_setsid_descendant_after_direct_parent_exit() {
    let _serial = lifecycle_test_lock().lock().await;
    enable_subreaper_for_fixture_orphans();
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let escaped_file = temp.path().join("escaped-pid");
    let escaped_path = escaped_file.to_str().expect("UTF-8 temp path");
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!(r#"(sleep 0.10; exec setsid sh -c 'printf "%s\n" "$$" > {escaped_path}; sleep 0.15; exit 37') & descendant=$!; printf 'direct=%s pgid=%s descendant=%s\n' "$$" "$$" "$descendant"; exit 0"#));
    let out = output_with_timeout(cmd, Duration::from_secs(2))
        .await
        .expect("direct parent exits successfully");
    let (direct, pgid, descendant) = recorded_identities(&out.stdout);
    let escaped: i32 = wait_for_fixture_file(&escaped_file, Duration::from_secs(2))
        .trim()
        .parse()
        .expect("setsid fixture PID");
    assert_eq!(
        descendant, escaped,
        "fixture must retain its PID across setsid"
    );
    assert_eq!(unsafe { libc::getpgid(escaped) }, escaped);
    assert_ne!(escaped, pgid, "setsid must escape the original PGID");
    assert_pgid_is_gone(pgid);
    let status = wait_for_adopted_status(escaped, Duration::from_secs(2));
    assert_eq!(
        status.code(),
        Some(37),
        "central reaper retained terminal status"
    );
    assert_pid_gone_without_zombie(direct);
    assert_pid_gone_without_zombie(escaped);
    assert!(worker_child_reaper().wait_for_adopted_idle(Duration::ZERO));
}

/// The timeout error discards command output, so the fixture publishes its
/// direct PID before invoking the public timeout API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_records_and_reaps_direct_pid_within_six_seconds() {
    let _serial = lifecycle_test_lock().lock().await;
    enable_subreaper_for_fixture_orphans();
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let identities = temp.path().join("identities");
    let path = identities.to_str().expect("UTF-8 temp path");
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(format!(
        "printf 'direct=%s pgid=%s\\n' \"$$\" \"$$\" > {path}; exec sleep 300"
    ));
    let start = SystemClock::new().now_instant();
    let err = output_with_timeout(cmd, Duration::from_millis(150))
        .await
        .expect_err("fixture must time out");
    let fixture = wait_for_fixture_file(&identities, Duration::from_secs(1));
    let (direct, pgid) = recorded_direct_and_pgid(fixture.as_bytes());
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert_pgid_is_gone(pgid);
    assert_pid_gone_without_zombie(direct);
    assert!(start.elapsed() < Duration::from_secs(6));
}

/// Exercise the supervisor's fault injection through `output_with_timeout`,
/// instead of manufacturing a diagnostic value in a separate unit test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn induced_cleanup_failure_preserves_supervisor_diagnostic_and_source() {
    let _serial = lifecycle_test_lock().lock().await;
    enable_subreaper_for_fixture_orphans();
    FORCE_CLEANUP_EXHAUSTION_FOR_TEST.store(true, Ordering::SeqCst);
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let identities = temp.path().join("identities");
    let path = identities.to_str().expect("UTF-8 temp path");
    let mut cmd = Command::new("sh");
    // The direct shell receives TERM, while this same-PGID descendant ignores
    // it. The injected failure snapshots a real survivor before SIGKILL.
    cmd.arg("-c").arg(format!(
        "(trap '' TERM; exec sleep 300) & descendant=$!; printf 'direct=%s pgid=%s descendant=%s\\n' \"$$\" \"$$\" \"$descendant\" > {path}; exec sleep 300"
    ));
    let err = output_with_timeout(cmd, Duration::from_millis(100))
        .await
        .expect_err("fault injection must exhaust actual supervisor cleanup");
    FORCE_CLEANUP_EXHAUSTION_FOR_TEST.store(false, Ordering::SeqCst);
    let fixture = wait_for_fixture_file(&identities, Duration::from_secs(1));
    let (direct, pgid, descendant) = recorded_identities(fixture.as_bytes());
    let diagnostic = err
        .get_ref()
        .and_then(|source| source.downcast_ref::<CleanupExhaustionError>())
        .expect("public command error must retain typed diagnostic");
    assert!(!diagnostic.command_id.is_empty());
    assert_eq!(diagnostic.pgid, pgid);
    assert!(diagnostic.direct_status.is_some());
    assert_eq!(diagnostic.phase, CleanupPhase::Kill);
    assert!(diagnostic.surviving_pids.contains(&(descendant as u32)));
    let source = diagnostic.source().expect("original timeout source");
    assert_eq!(
        source.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::TimedOut)
    );
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let status = wait_for_adopted_status(descendant, Duration::from_secs(2));
    assert_eq!(status.pid, descendant as u32);
    assert_pid_gone_without_zombie(direct);
    assert_pid_gone_without_zombie(descendant);
    assert!(worker_child_reaper().wait_for_adopted_idle(Duration::ZERO));
}

//! Deterministic cargo invocation outcome tests for `shell_exec::finish_shell`.

use super::*;
use crate::process::{ProcessOutput, ProcessRunError, ProcessTermination};
use djinn_core::clock::{Clock, TestClock};
use djinn_telemetry::cargo_invocation::{
    EXIT_CANCELLED, EXIT_FAIL, EXIT_OK, KIND_BUILD, KIND_CHECK, KIND_CLIPPY, KIND_OTHER, KIND_TEST,
};
use djinn_telemetry::role_class::{ALL_CLASSES, CLASS_BUILD_CAPABLE, CLASS_LIGHT};
use std::os::unix::process::ExitStatusExt;
use std::sync::{Arc, Mutex};

/// Collected recorder calls: `(kind, exit, duration, class)`. `class` is last
/// so the pre-existing positional assertions on kind/exit/duration keep their
/// meaning.
type Calls = Arc<
    Mutex<
        Vec<(
            &'static str,
            &'static str,
            std::time::Duration,
            &'static str,
        )>,
    >,
>;

fn fake_recorder(
    calls: &Calls,
) -> impl Fn(&'static str, &'static str, &'static str, std::time::Duration) {
    let calls = calls.clone();
    move |kind, exit, class, dur| {
        calls.lock().unwrap().push((kind, exit, dur, class));
    }
}

/// Construct a synthetic `ProcessOutput` from an exit code and termination.
fn make_output(code: i32, termination: ProcessTermination) -> ProcessOutput {
    ProcessOutput {
        output: std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: vec![],
            stderr: vec![],
        },
        termination,
    }
}

/// Run `finish_shell` with a `TestClock` advanced by `elapsed`, returning
/// the recorded calls.
#[allow(clippy::disallowed_methods)]
fn run_finish(
    classification: Option<&'static str>,
    result: &Result<ProcessOutput, ProcessRunError>,
    elapsed: std::time::Duration,
) -> Vec<(
    &'static str,
    &'static str,
    std::time::Duration,
    &'static str,
)> {
    run_finish_as(classification, CLASS_BUILD_CAPABLE, result, elapsed)
}

#[allow(clippy::disallowed_methods)]
fn run_finish_as(
    classification: Option<&'static str>,
    class: &'static str,
    result: &Result<ProcessOutput, ProcessRunError>,
    elapsed: std::time::Duration,
) -> Vec<(
    &'static str,
    &'static str,
    std::time::Duration,
    &'static str,
)> {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let recorder = fake_recorder(&calls);
    let clock = TestClock::new(std::time::SystemTime::UNIX_EPOCH, std::time::Instant::now());
    let started = clock.now_instant();
    clock.advance_mono(elapsed);
    finish_shell(classification, class, started, result, &clock, recorder);
    calls.lock().unwrap().clone()
}

#[test]
fn success_records_ok_once() {
    let result = Ok(make_output(0, ProcessTermination::Exited));
    let recorded = run_finish(
        Some(KIND_CHECK),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, KIND_CHECK);
    assert_eq!(recorded[0].1, EXIT_OK);
    assert_eq!(recorded[0].2, std::time::Duration::from_millis(2500));
}

#[test]
fn nonzero_exit_records_fail_once() {
    let result = Ok(make_output(1, ProcessTermination::Exited));
    let recorded = run_finish(
        Some(KIND_BUILD),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, KIND_BUILD);
    assert_eq!(recorded[0].1, EXIT_FAIL);
    assert_eq!(recorded[0].2, std::time::Duration::from_millis(2500));
}

#[test]
fn timeout_records_fail_once() {
    // A timeout kills the child; the exit status is non-success.
    let result = Ok(make_output(9, ProcessTermination::TimedOut));
    let recorded = run_finish(
        Some(KIND_TEST),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, KIND_TEST);
    assert_eq!(recorded[0].1, EXIT_FAIL);
    assert_eq!(recorded[0].2, std::time::Duration::from_millis(2500));
}

#[test]
fn cancellation_records_cancelled_once() {
    let result = Ok(make_output(9, ProcessTermination::Cancelled));
    let recorded = run_finish(
        Some(KIND_CLIPPY),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, KIND_CLIPPY);
    assert_eq!(recorded[0].1, EXIT_CANCELLED);
    assert_eq!(recorded[0].2, std::time::Duration::from_millis(2500));
}

#[test]
fn started_error_records_fail_once() {
    let result = Err(ProcessRunError::Started(std::io::Error::other(
        "wait failed",
    )));
    let recorded = run_finish(
        Some(KIND_OTHER),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, KIND_OTHER);
    assert_eq!(recorded[0].1, EXIT_FAIL);
    assert_eq!(recorded[0].2, std::time::Duration::from_millis(2500));
}

#[test]
fn spawn_error_records_zero() {
    let result = Err(ProcessRunError::Spawn(std::io::Error::other(
        "spawn failed",
    )));
    let recorded = run_finish(
        Some(KIND_CHECK),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert!(
        recorded.is_empty(),
        "spawn error should record nothing (child never started)"
    );
}

#[test]
fn non_cargo_success_records_zero() {
    let result = Ok(make_output(0, ProcessTermination::Exited));
    let recorded = run_finish(None, &result, std::time::Duration::from_millis(2500));
    assert!(
        recorded.is_empty(),
        "non-cargo command should record nothing"
    );
}

#[test]
fn non_cargo_nonzero_records_zero() {
    let result = Ok(make_output(1, ProcessTermination::Exited));
    let recorded = run_finish(None, &result, std::time::Duration::from_millis(2500));
    assert!(
        recorded.is_empty(),
        "non-cargo command should record nothing"
    );
}

#[test]
fn non_cargo_spawn_error_records_zero() {
    let result = Err(ProcessRunError::Spawn(std::io::Error::other(
        "spawn failed",
    )));
    let recorded = run_finish(None, &result, std::time::Duration::from_millis(2500));
    assert!(
        recorded.is_empty(),
        "non-cargo command should record nothing"
    );
}

#[test]
fn each_kind_records_correct_exit() {
    let kinds = [KIND_CHECK, KIND_CLIPPY, KIND_TEST, KIND_BUILD, KIND_OTHER];
    for &kind in &kinds {
        let result = Ok(make_output(0, ProcessTermination::Exited));
        let recorded = run_finish(Some(kind), &result, std::time::Duration::from_secs(1));
        assert_eq!(recorded.len(), 1, "kind {kind} should record exactly once");
        assert_eq!(recorded[0].0, kind);
        assert_eq!(recorded[0].1, EXIT_OK);
        assert_eq!(recorded[0].2, std::time::Duration::from_secs(1));
    }
}

#[test]
fn timeout_with_success_status_still_fail() {
    // Edge case: child exited successfully just as the deadline fired.
    // The timeout classification must still produce EXIT_FAIL.
    let result = Ok(make_output(0, ProcessTermination::TimedOut));
    let recorded = run_finish(
        Some(KIND_CHECK),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1, EXIT_FAIL);
}

#[test]
fn cancellation_with_success_status_still_cancelled() {
    let result = Ok(make_output(0, ProcessTermination::Cancelled));
    let recorded = run_finish(
        Some(KIND_CHECK),
        &result,
        std::time::Duration::from_millis(2500),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1, EXIT_CANCELLED);
}

#[test]
#[allow(clippy::disallowed_methods)]
fn duration_is_monotonic_not_wall_clock() {
    // Advancing wall-clock without advancing monotonic time must not
    // change the recorded duration.
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let recorder = fake_recorder(&calls);
    let clock = TestClock::new(std::time::SystemTime::UNIX_EPOCH, std::time::Instant::now());
    let started = clock.now_instant();
    // Move wall-clock forward by 10 minutes; leave monotonic unchanged.
    clock.advance_wall(std::time::Duration::from_secs(600));
    let result = Ok(make_output(0, ProcessTermination::Exited));
    finish_shell(
        Some(KIND_CHECK),
        CLASS_BUILD_CAPABLE,
        started,
        &result,
        &clock,
        recorder,
    );
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].2,
        std::time::Duration::ZERO,
        "duration must follow monotonic time, not wall-clock"
    );
}

/// The `class` label passes through verbatim and is the ONLY thing the class
/// changes: a light-role compile is recorded with the same kind, exit and
/// duration as a build-capable one. This is the observability half of "light
/// roles are not gated at dispatch, but they are not invisible either".
#[test]
fn class_label_passes_through_and_changes_nothing_else() {
    let result = Ok(make_output(0, ProcessTermination::Exited));
    let light = run_finish_as(
        Some(KIND_CLIPPY),
        CLASS_LIGHT,
        &result,
        std::time::Duration::from_millis(1500),
    );
    let build = run_finish_as(
        Some(KIND_CLIPPY),
        CLASS_BUILD_CAPABLE,
        &result,
        std::time::Duration::from_millis(1500),
    );
    assert_eq!(light.len(), 1);
    assert_eq!(build.len(), 1);
    assert_eq!(light[0].3, CLASS_LIGHT);
    assert_eq!(build[0].3, CLASS_BUILD_CAPABLE);
    assert_eq!(
        (light[0].0, light[0].1, light[0].2),
        (build[0].0, build[0].1, build[0].2),
        "class must label the observation, never alter it"
    );
}

/// A non-cargo command records nothing regardless of class: the class never
/// creates an observation that the kind classifier did not.
#[test]
fn class_does_not_create_observations() {
    let result = Ok(make_output(0, ProcessTermination::Exited));
    for class in ALL_CLASSES {
        assert!(
            run_finish_as(None, class, &result, std::time::Duration::from_secs(1)).is_empty(),
            "class {class} must not manufacture an observation"
        );
    }
}

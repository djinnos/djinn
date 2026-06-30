//! Low-level tests for `run_git_command`, `run_git_command_with_timeout`,
//! classifier helpers, and `retry_delay` from this crate's `lib.rs`.
//!
//! These tests exercise pure classifiers and command/error behavior without
//! network access, GitHub credentials, or global git config mutation.
//! All repos are local bare/path remotes inside unique `TempDir` roots.

#![allow(clippy::disallowed_methods)] // test-only: wall-clock in SystemClock jitter

use std::time::Duration;

use crate::test_support::init_repo_with_main_commit;
use crate::{
    GitError, is_non_fast_forward_error, is_retryable_git_command_error,
    is_transient_network_error, retry_delay, run_git_command, run_git_command_with_timeout,
};

// ── Helper ──────────────────────────────────────────────────────────────────

/// Build a `GitError::CommandFailed` with the given stderr message.
fn fake_command_failed(stderr: &str) -> GitError {
    GitError::CommandFailed {
        code: 1,
        command: "test-command".into(),
        cwd: "/tmp".into(),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

/// Build a `GitError::Timeout` with the given fields.
fn fake_timeout(timeout_secs: u64, command: &str, cwd: &str) -> GitError {
    GitError::Timeout {
        timeout_secs,
        command: command.into(),
        cwd: cwd.into(),
    }
}

// ── run_git_command: CommandFailed ──────────────────────────────────────────

/// Running `git fetch` against a non-existent remote returns
/// `GitError::CommandFailed` with non-zero code, the failing command text,
/// the repo cwd path, and non-empty stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_git_command_missing_remote_returns_command_failed() {
    let fixture = init_repo_with_main_commit();
    let repo_path = fixture.path().to_path_buf();

    let err = run_git_command(
        repo_path.clone(),
        vec!["fetch".into(), "missing-remote".into(), "main".into()],
    )
    .await
    .expect_err("fetch from missing remote should fail");

    match err {
        GitError::CommandFailed {
            code,
            ref command,
            ref cwd,
            ref stdout,
            ref stderr,
        } => {
            assert_ne!(code, 0, "exit code must be non-zero");
            assert!(
                command.contains("fetch") && command.contains("missing-remote"),
                "command text should contain the failing args, got: {command}"
            );
            assert_eq!(
                cwd,
                &repo_path.display().to_string(),
                "cwd should equal the temp repo path"
            );
            // At least one of stdout/stderr should be non-empty (typically stderr).
            assert!(
                !stderr.is_empty() || !stdout.is_empty(),
                "expected non-empty stderr or stdout"
            );
            assert!(
                !stderr.is_empty(),
                "stderr should contain the git error message, got empty stderr"
            );
        }
        other => panic!("expected CommandFailed, got: {other:?}"),
    }
}

// ── run_git_command_with_timeout: Timeout ───────────────────────────────────

/// Verifies that `run_git_command_with_timeout` returns `GitError::Timeout`
/// when the command exceeds the deadline.
///
/// **Deterministic substitute:** We use an extremely short timeout (1 ms)
/// against a real `git fetch` from a non-existent remote. The git process
/// spawns and begins its work, but the tokio `timeout` fires well before
/// git can complete its DNS resolution / error path. This reliably produces
/// a `GitError::Timeout` without flaky timing dependencies.
///
/// An alternative would be `git clone https://192.0.2.1/repo.git` (RFC 5737
/// TEST-NET, no route) with a 1-second timeout, but that makes CI
/// environment-dependent and slow. The 1ms approach exercises the same
/// `tokio::time::timeout` code path deterministically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_git_command_with_timeout_returns_timeout_variant() {
    let fixture = init_repo_with_main_commit();
    let repo_path = fixture.path().to_path_buf();
    let timeout = Duration::from_millis(1);
    let args = vec!["fetch".into(), "nonexistent-remote".into(), "main".into()];

    let err = run_git_command_with_timeout(repo_path.clone(), args.clone(), timeout)
        .await
        .expect_err("1ms timeout should fire before git completes");

    match err {
        GitError::Timeout {
            timeout_secs,
            ref command,
            ref cwd,
        } => {
            assert_eq!(
                timeout_secs,
                timeout.as_secs(),
                "timeout_secs should match the requested duration ({}s)",
                timeout.as_secs()
            );
            assert!(
                command.contains("fetch") && command.contains("nonexistent-remote"),
                "command text should contain the git args, got: {command}"
            );
            assert_eq!(
                cwd,
                &repo_path.display().to_string(),
                "cwd should equal the temp repo path"
            );
        }
        other => panic!("expected Timeout, got: {other:?}"),
    }
}

// ── Classifier: is_retryable_git_command_error ──────────────────────────────

/// Validates `is_retryable_git_command_error` for lock contention, transient
/// network errors, and non-retryable permanent errors.
#[test]
fn classifies_retryable_lock_and_network_errors() {
    // Lock/ref contention — retryable
    let lock_cases = [
        "unable to create lock: cannot lock ref",
        "fatal: failed to lock '/repo/.git/refs/heads/main'",
        "Another git process seems to be running",
        "resource temporarily unavailable",
    ];
    for stderr in &lock_cases {
        let err = fake_command_failed(stderr);
        assert!(
            is_retryable_git_command_error(&err),
            "should be retryable: {stderr}"
        );
    }

    // Network errors — retryable
    let network_cases = [
        "Connection reset by peer",
        "connection timed out",
        "timed out",
        "remote end hung up unexpectedly",
    ];
    for stderr in &network_cases {
        let err = fake_command_failed(stderr);
        assert!(
            is_retryable_git_command_error(&err),
            "should be retryable: {stderr}"
        );
    }

    // Permanent/auth-like errors — NOT retryable
    let permanent_cases = [
        "fatal: Authentication failed",
        "ERROR: Repository not found",
        "permission denied (publickey)",
    ];
    for stderr in &permanent_cases {
        let err = fake_command_failed(stderr);
        assert!(
            !is_retryable_git_command_error(&err),
            "should NOT be retryable: {stderr}"
        );
    }

    // Timeout variant — NOT retryable by is_retryable_git_command_error
    // (it doesn't match CommandFailed)
    let timeout_err = fake_timeout(30, "push origin main", "/tmp");
    assert!(
        !is_retryable_git_command_error(&timeout_err),
        "Timeout should not be retryable by is_retryable_git_command_error"
    );
}

// ── Classifier: is_transient_network_error ──────────────────────────────────

/// Validates `is_transient_network_error` for various transient network
/// conditions, permanent/auth errors, and the `Timeout` variant.
#[test]
fn classifies_transient_network_errors() {
    // Transient network errors
    let transient_cases = [
        "connection closed by remote host",
        "broken pipe",
        "could not read from remote repository",
        "unable to access 'https://github.com/org/repo.git/'",
        "Connection timed out",
        "Connection refused",
        "Could not resolve host: github.com",
        "SSL_connect: SSL_ERROR_SYSCALL",
        "server certificate verification failed: TLS error",
        "gnutls_handshake() failed",
        "Connection reset by peer",
        "the remote end hung up unexpectedly",
        "early EOF",
        "unexpected disconnect while reading sideband packet",
    ];
    for stderr in &transient_cases {
        let err = fake_command_failed(stderr);
        assert!(
            is_transient_network_error(&err),
            "should be transient network error: {stderr}"
        );
    }

    // Timeout variant is always transient
    let timeout_err = fake_timeout(30, "clone https://github.com/org/repo.git", "/tmp");
    assert!(
        is_transient_network_error(&timeout_err),
        "Timeout should be classified as transient network error"
    );

    // Permanent/auth-like errors — NOT transient
    let permanent_cases = [
        "fatal: Authentication failed for 'https://github.com/'",
        "ERROR: Repository not found",
        "permission denied (publickey)",
        "non-fast-forward",
        "fatal: cannot lock ref",
    ];
    for stderr in &permanent_cases {
        let err = fake_command_failed(stderr);
        assert!(
            !is_transient_network_error(&err),
            "should NOT be transient: {stderr}"
        );
    }
}

// ── Classifier: is_non_fast_forward_error ───────────────────────────────────

/// Validates `is_non_fast_forward_error` for non-fast-forward and
/// fetch-first stderr patterns, and that unrelated errors return false.
#[test]
fn classifies_non_fast_forward_errors() {
    let nff_cases = [
        "Updates were rejected because the tip of your current branch is behind",
        "fatal: 'origin/main' is not a commit and a branch 'main' cannot be created from it\nhint: Updates were rejected because the remote contains work that you do\nhint: not have locally. This is usually caused by another repository pushing\nhint: to the same ref. You may want to first integrate the remote changes\nhint: (e.g., 'git pull ...') before pushing again.",
        "rejected (non-fast-forward)",
        "rejected (fetch first)",
        "failed to push some refs to 'origin'\nhint: Updates were rejected because the remote contains work that you do\nhint: not have locally.",
    ];
    for stderr in &nff_cases {
        let err = fake_command_failed(stderr);
        assert!(
            is_non_fast_forward_error(&err),
            "should be non-fast-forward: {stderr}"
        );
    }

    // Non-NFF errors should return false
    let non_nff_cases = [
        "fatal: Authentication failed",
        "fatal: cannot lock ref",
        "Connection reset by peer",
        "permission denied",
    ];
    for stderr in &non_nff_cases {
        let err = fake_command_failed(stderr);
        assert!(
            !is_non_fast_forward_error(&err),
            "should NOT be non-fast-forward: {stderr}"
        );
    }
}

// ── retry_delay: bounded and increasing ─────────────────────────────────────

/// Verifies that `retry_delay` produces bounded, initially-increasing delays.
///
/// The function computes `base_ms + jitter_ms` where:
///   - `base_ms = 200 * 2^(min(attempt-1, 4))` — exponential backoff capped at exponent 4
///   - `jitter_ms = (unix_epoch_millis % 151)` — deterministic jitter in [0, 150]
///
/// We cannot predict the exact jitter (it depends on wall-clock time), so
/// assertions focus on:
///   1. Upper bound: `delay <= base_max + 150ms`
///   2. Lower bound: `delay >= base_min`
///   3. Initial increase: `delay(attempt=1) <= delay(attempt=2) <= delay(attempt=3)`
///      holds for the base component (jitter may cause occasional inversion
///      across attempts, so we check base-only monotonicity via iteration).
#[test]
fn retry_delay_is_bounded_and_increases_initially() {
    // Expected base values:
    // attempt 1: base = 200 * 2^0 = 200
    // attempt 2: base = 200 * 2^1 = 400
    // attempt 3: base = 200 * 2^2 = 800
    // attempt 4: base = 200 * 2^3 = 1600
    // attempt 5: base = 200 * 2^4 = 3200  (capped exponent)
    // attempt 6+: same as 5 (exponent stays at 4)
    let expected_bases = [200u64, 400, 800, 1600, 3200];
    let max_jitter = 150u64;

    let mut delays = Vec::new();
    for attempt in 1..=5 {
        let delay = retry_delay(attempt);
        delays.push(delay);

        let idx = (attempt - 1) as usize;
        let expected_base = expected_bases[idx];

        // Lower bound: must be at least the base (jitter >= 0)
        assert!(
            delay.as_millis() as u64 >= expected_base,
            "retry_delay({attempt}) = {}ms should be >= {expected_base}ms",
            delay.as_millis()
        );

        // Upper bound: base + max jitter
        let max_expected = expected_base + max_jitter;
        assert!(
            delay.as_millis() as u64 <= max_expected,
            "retry_delay({attempt}) = {}ms should be <= {max_expected}ms",
            delay.as_millis()
        );
    }

    // Verify that the base component is strictly increasing for attempts 1..=5.
    for i in 0..delays.len() - 1 {
        let current_base = expected_bases[i];
        let next_base = expected_bases[i + 1];
        assert!(
            current_base < next_base,
            "base for attempt {} ({current_base}ms) should be < base for attempt {} ({next_base}ms)",
            i + 1,
            i + 2,
        );
    }

    // Verify capped behavior: attempt 6+ shares the same base (3200ms) as attempt 5.
    let capped_base = expected_bases[4]; // 3200
    for attempt in [6u32, 7, 10, 100] {
        let delay = retry_delay(attempt);
        assert!(
            delay.as_millis() as u64 >= capped_base,
            "retry_delay({attempt}) should be >= capped base {capped_base}ms"
        );
        assert!(
            delay.as_millis() as u64 <= capped_base + max_jitter,
            "retry_delay({attempt}) should be <= {capped_base} + {max_jitter}ms"
        );
    }

    // Edge case: attempt=0 → exp = 0u32.saturating_sub(1).min(4) = 0,
    // base = 200 * 2^0 = 200, same as attempt=1.
    let delay_0 = retry_delay(0);
    assert!(
        delay_0.as_millis() as u64 >= 200,
        "retry_delay(0) should be >= 200ms (exp=0, base=200)"
    );
    assert!(
        delay_0.as_millis() as u64 <= 200 + max_jitter,
        "retry_delay(0) should be <= {}ms",
        200 + max_jitter
    );
}

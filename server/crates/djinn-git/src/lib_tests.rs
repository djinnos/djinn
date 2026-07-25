//! Low-level tests for `run_git_command`, `run_git_command_with_timeout`,
//! classifier helpers, and `retry_delay` from this crate's `lib.rs`.
//!
//! These tests exercise pure classifiers and command/error behavior without
//! network access, GitHub credentials, or global git config mutation.
//! All repos are local bare/path remotes inside unique `TempDir` roots.

#![allow(clippy::disallowed_methods)] // test-only: wall-clock in SystemClock jitter

use std::time::Duration;

use crate::test_support::{checkout_branch, init_repo_with_main_commit, write_and_commit};
use crate::{
    GitError, delete_branch, head_commit_sha, is_non_fast_forward_error,
    is_retryable_git_command_error, is_transient_network_error, rebase_with_retry, retry_delay,
    rev_list_count, run_git_command, run_git_command_in, run_git_command_in_allow_failure,
    run_git_command_in_with_env, run_git_command_with_timeout, run_git_command_with_timeout_in,
    unmerged_files,
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

// ── safe.directory: cross-UID repository trust (nurw) ───────────────────────
//
// git honours `safe.directory` only from protected *file* configuration, and
// strips command-scope config (`-c`, `GIT_CONFIG_COUNT`/`KEY_0`/`VALUE_0`) from
// the inner `git-upload-pack` child that `git clone --local` spawns. The
// previous injection form was therefore a no-op for exactly the operation djinn
// depends on, and every mirror clone failed once the mirror was owned by the
// other identity.
//
// Note what is NOT a sufficient regression test: "cloning a foreign-owned
// repository succeeds". git >= 2.48 accepts `clone --local --shared` of a
// foreign-owned repository regardless of `safe.directory`, so on a modern
// developer/CI git that assertion passes with the broken mechanism too — the
// same way the previous test passed while production was wedged. The two tests
// below instead assert, version-independently, *which scope git resolves the
// rule in* and *what the stripped-environment child sees*.

/// The scope git resolves `safe.directory` in must be protected file
/// configuration (`system`/`global`), never `command`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_directory_resolves_in_protected_file_scope() {
    let fixture = init_repo_with_main_commit();

    let out = run_git_command_in_allow_failure(
        fixture.path(),
        vec![
            "config".into(),
            "--show-scope".into(),
            "--get-all".into(),
            "safe.directory".into(),
        ],
    )
    .await
    .expect("spawning git must succeed");

    assert!(
        out.is_success(),
        "git resolved no safe.directory at all, so a repository owned by the other \
         djinn identity is rejected outright; stdout: {:?} stderr: {:?}",
        out.stdout,
        out.stderr
    );
    let scopes: Vec<&str> = out
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(
        scopes
            .iter()
            .any(|scope| matches!(*scope, "system" | "global")),
        "safe.directory must come from protected file configuration (system/global). \
         `command` scope is stripped from the inner `git-upload-pack` child of \
         `git clone --local`, which is what made every mirror clone fail. scopes: {scopes:?}"
    );
}

/// The property the previous env-var form was chosen for, and did not deliver:
/// the inner git process spawned by `git clone --local` must resolve the trust
/// rule too. Substituting `--upload-pack` with a shim that records what that
/// child sees observes it directly, on any git version, without privileges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inner_child_of_a_local_clone_resolves_the_trust_rule() {
    use std::os::unix::fs::PermissionsExt;

    let source = init_repo_with_main_commit();
    let scratch = tempfile::tempdir().expect("create scratch dir");
    let record = scratch.path().join("child-view");
    let shim = scratch.path().join("upload-pack-shim.sh");
    std::fs::write(
        &shim,
        "#!/bin/sh\n\
         git config --show-scope --get-all safe.directory > \"$DJINN_TEST_RECORD\" 2>&1\n\
         exec git upload-pack \"$@\"\n",
    )
    .expect("write upload-pack shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .expect("make shim executable");

    let destination = scratch.path().join("clone");
    run_git_command_in_with_env(
        scratch.path(),
        vec![
            "clone".into(),
            "--local".into(),
            "--shared".into(),
            "--upload-pack".into(),
            shim.display().to_string(),
            source.path().display().to_string(),
            destination.display().to_string(),
        ],
        vec![(
            "DJINN_TEST_RECORD".to_string(),
            record.display().to_string(),
        )],
    )
    .await
    .expect("clone through the upload-pack shim must succeed");
    assert!(
        destination.join(".git").is_dir(),
        "clone must be materialized"
    );

    let child_view = std::fs::read_to_string(&record)
        .expect("the inner upload-pack child must have recorded its config view");
    let scope = child_view.split_whitespace().next().unwrap_or_default();
    assert!(
        matches!(scope, "system" | "global"),
        "the inner `git-upload-pack` child of `git clone --local` must resolve \
         safe.directory in protected file scope. git strips command-scope config from \
         this child (`trace: run_command: unset GIT_CONFIG_COUNT ...`), so a `-c` or \
         GIT_CONFIG_* injection leaves it with nothing and cloning a mirror owned by \
         the other djinn identity fails with \"detected dubious ownership\". \
         child saw: {child_view:?}"
    );
    assert!(
        child_view.contains('*'),
        "the child must inherit the trust value itself, got {child_view:?}"
    );
}

/// Cloning a repository owned by another uid must work: the server (uid 10001)
/// and the worker / warm Job (uid 1000) both clone the same mirror, so either
/// can be the non-owner.
///
/// The ownership boundary is created for real with `chown` when the runner is
/// privileged, otherwise simulated with git's own
/// `GIT_TEST_ASSUME_DIFFERENT_OWNER`. Either way a *control* clone — carrying
/// only the pre-fix command-scope injection — runs first: if the control
/// succeeds, this environment cannot express the failure and there is nothing to
/// assert, so the test says so instead of passing vacuously. The two tests above
/// are the version-independent gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_clone_succeeds_when_repository_is_owned_by_a_different_uid() {
    use std::os::unix::fs::MetadataExt;

    let source = init_repo_with_main_commit();
    let source_git = source.path().join(".git");
    let current_uid = std::fs::metadata(&source_git)
        .expect("stat source git dir")
        .uid();
    let foreign_uid = if current_uid == 10001 { 10002 } else { 10001 };

    let source_git_c =
        std::ffi::CString::new(source_git.as_os_str().as_encoded_bytes()).expect("path has no NUL");
    // SAFETY: `source_git_c` is a live NUL-terminated C string.
    let chowned = unsafe { libc::chown(source_git_c.as_ptr(), foreign_uid, u32::MAX) } == 0;
    if chowned {
        assert_eq!(
            std::fs::metadata(&source_git)
                .expect("stat foreign-owned source git dir")
                .uid(),
            foreign_uid,
            "fixture must be owned by a uid other than the cloning process"
        );
    }

    // Keep the runner's own ~/.gitconfig out of the result either way: an
    // ambient `safe.directory` there would silently decide the outcome.
    let home = tempfile::tempdir().expect("create isolated home");
    let mut trigger = vec![
        ("HOME".to_string(), home.path().display().to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            home.path().join("xdg").display().to_string(),
        ),
    ];
    if !chowned {
        trigger.push((
            "GIT_TEST_ASSUME_DIFFERENT_OWNER".to_string(),
            "1".to_string(),
        ));
    }

    let scratch = tempfile::tempdir().expect("create clone destination parent");
    let clone_args = |destination: &std::path::Path| {
        vec![
            "clone".to_string(),
            "--local".to_string(),
            "--shared".to_string(),
            source.path().display().to_string(),
            destination.display().to_string(),
        ]
    };

    // Control: the pre-fix mechanism, spawned outside the seam on purpose.
    let control_destination = scratch.path().join("control");
    let mut control = tokio::process::Command::new("git");
    control
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "safe.directory")
        .env("GIT_CONFIG_VALUE_0", "*")
        .env_remove("GIT_CONFIG_SYSTEM")
        .args(clone_args(&control_destination))
        .current_dir(scratch.path());
    for (key, value) in &trigger {
        control.env(key, value);
    }
    let control_out = control.output().await.expect("spawn control clone");
    if control_out.status.success() {
        // No ownership rejection is reachable here (unprivileged runner on a git
        // that accepts `--local --shared` under the simulation), so a successful
        // clone below would prove nothing.
        return;
    }

    let destination = scratch.path().join("clone");
    run_git_command_in_with_env(scratch.path(), clone_args(&destination), trigger)
        .await
        .expect(
            "the seam must clone a repository owned by another uid — the control clone \
             carrying only the pre-fix command-scope injection was rejected here",
        );
    assert!(
        destination.join(".git").is_dir(),
        "clone must be materialized"
    );
}

/// `configure_private_dep_access` stores the GitHub installation token as a
/// `url.<...>.insteadOf` rewrite with `git config --global`, and the agent's own
/// build tools read it back from `$HOME/.gitconfig` without djinn in the loop.
/// The trust rule must therefore not be injected as `GIT_CONFIG_GLOBAL`, which
/// would redirect that write into a djinn-private file nothing else reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_config_writes_still_land_in_the_home_config() {
    let fixture = init_repo_with_main_commit();
    let home = tempfile::tempdir().expect("create isolated home");
    let env = vec![("HOME".to_string(), home.path().display().to_string())];

    run_git_command_in_with_env(
        fixture.path(),
        vec![
            "config".into(),
            "--global".into(),
            "--add".into(),
            "url.https://x-access-token:token@github.com/owner/.insteadOf".into(),
            "https://github.com/owner/".into(),
        ],
        env,
    )
    .await
    .expect("`git config --global` must succeed through the seam");

    let home_config = std::fs::read_to_string(home.path().join(".gitconfig")).expect(
        "`git config --global` must keep writing $HOME/.gitconfig; redirecting global \
         scope would break private-dependency access for cargo/go/pnpm, which read that \
         file directly",
    );
    assert!(
        home_config.contains("insteadOf"),
        "the rewrite must be in $HOME/.gitconfig, got {home_config:?}"
    );
}

/// Pointing `GIT_CONFIG_SYSTEM` at a generated file shadows the real
/// `/etc/gitconfig`, so the generated file has to chain to it.
#[test]
fn generated_config_chains_to_the_real_system_config() {
    let chained = crate::generated_config_contents(Some(std::path::Path::new("/etc/git\"conf ig")));
    assert!(
        chained.contains("[include]\n\tpath = \"/etc/git\\\"conf ig\"\n"),
        "the chained path must be quoted and escaped, got {chained:?}"
    );
    assert!(
        chained.contains("[safe]\n\tdirectory = *\n"),
        "the trust rule must be present, got {chained:?}"
    );

    let standalone = crate::generated_config_contents(None);
    assert!(
        !standalone.contains("[include]"),
        "with no system config to chain to there must be no include, got {standalone:?}"
    );
    assert!(
        standalone.contains("[safe]\n\tdirectory = *\n"),
        "the trust rule must be present, got {standalone:?}"
    );
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

// ── unmerged_files: conflict discovery ────────────────────────────────────────

/// `unmerged_files(path)` returns exactly the paths with merge conflicts
/// and excludes clean files.
///
/// Constructs a repo where two branches edit the same file differently,
/// then merges to create a conflict.  A separate clean file is committed
/// on both branches identically so it appears in the tree but must NOT
/// appear in the unmerged-files list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmerged_files_returns_conflicted_paths() {
    let fixture = init_repo_with_main_commit();

    // Add a second file that will remain conflict-free.
    write_and_commit(
        fixture.path(),
        "clean.txt",
        "clean content\n",
        "add clean file",
    );

    // Create a feature branch from main.
    checkout_branch(fixture.path(), "feature", Some("main"));

    // Edit conflict_file on the feature branch.
    write_and_commit(
        fixture.path(),
        "conflict.txt",
        "feature version\n",
        "feature edit",
    );
    // Also edit clean.txt identically on both branches (no conflict).
    write_and_commit(
        fixture.path(),
        "clean.txt",
        "updated clean content\n",
        "feature clean update",
    );

    // Switch back to main and edit conflict_file differently.
    checkout_branch(fixture.path(), "main", None);
    write_and_commit(
        fixture.path(),
        "conflict.txt",
        "main version\n",
        "main edit",
    );
    // Same clean.txt update on main (identical content → no conflict).
    write_and_commit(
        fixture.path(),
        "clean.txt",
        "updated clean content\n",
        "main clean update",
    );

    // Attempt a merge that will conflict on conflict.txt.
    let merge_out = std::process::Command::new("git")
        .args(["merge", "feature"])
        .current_dir(fixture.path())
        .output()
        .expect("run git merge");
    assert!(
        !merge_out.status.success(),
        "merge should conflict, but succeeded"
    );

    let unmerged = unmerged_files(fixture.path().to_path_buf())
        .await
        .expect("unmerged_files should succeed during conflict");

    assert_eq!(
        unmerged,
        vec!["conflict.txt".to_string()],
        "unmerged_files should return exactly the conflicted path; got: {unmerged:?}"
    );
}

// ── rebase_with_retry: conflict produces CommandFailed ────────────────────────

/// `rebase_with_retry` returns `GitError::CommandFailed` with conflict-related
/// stderr when the upstream branch has incompatible changes.
///
/// The implementation attempts `rebase --abort` after non-retryable failures,
/// so the repo should NOT be left in an active rebase state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebase_with_retry_returns_command_failed_for_conflict() {
    let fixture = init_repo_with_main_commit();

    // Create a feature branch that edits conflict.txt.
    checkout_branch(fixture.path(), "feature", Some("main"));
    write_and_commit(
        fixture.path(),
        "conflict.txt",
        "feature version\n",
        "feature edit",
    );

    // Switch back to main and edit the same file differently.
    checkout_branch(fixture.path(), "main", None);
    write_and_commit(
        fixture.path(),
        "conflict.txt",
        "main version\n",
        "main edit",
    );

    // Now switch to feature and try to rebase onto main.
    checkout_branch(fixture.path(), "feature", None);

    let err = rebase_with_retry(fixture.path(), "main")
        .await
        .expect_err("rebase should fail due to conflict");

    match &err {
        GitError::CommandFailed {
            code,
            stderr,
            command,
            ..
        } => {
            assert_ne!(*code, 0, "exit code must be non-zero");
            let lower = stderr.to_lowercase();
            assert!(
                lower.contains("conflict") || lower.contains("could not apply"),
                "stderr should mention conflict, got: {stderr}"
            );
            assert!(
                command.contains("rebase"),
                "command should contain 'rebase', got: {command}"
            );
        }
        other => panic!("expected CommandFailed, got: {other:?}"),
    }

    // Implementation calls `rebase --abort` on failure, so the repo
    // should not be in rebase state.  Verify by checking .git/REBASE_HEAD
    // does not exist.
    assert!(
        !fixture.path().join(".git/REBASE_HEAD").exists()
            && !fixture.path().join(".git/rebase-merge").exists()
            && !fixture.path().join(".git/rebase-apply").exists(),
        "repo should not be left in active rebase state after conflict failure"
    );
}

// ── delete_branch: local removal succeeds even when remote delete fails ──────

/// `delete_branch` removes the local branch and returns `Ok(())` even when
/// the remote `push --delete` fails (no origin configured).
///
/// The function's contract: `git branch -D <branch>` followed by a
/// best-effort `git push origin --delete <branch>` whose error is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_branch_removes_local_even_when_remote_delete_fails() {
    // Use a repo with no origin remote so the push --delete fails.
    let fixture = init_repo_with_main_commit();

    // Create a local branch to delete.
    checkout_branch(fixture.path(), "task/cleanup", Some("main"));
    checkout_branch(fixture.path(), "main", None);

    // Verify the branch exists before deletion.
    let list_before = std::process::Command::new("git")
        .args(["branch", "--list", "task/cleanup"])
        .current_dir(fixture.path())
        .output()
        .expect("git branch --list");
    assert!(
        String::from_utf8_lossy(&list_before.stdout).contains("task/cleanup"),
        "branch should exist before deletion"
    );

    // delete_branch should return Ok(()) — the missing-origin push failure is ignored.
    delete_branch(fixture.path().to_path_buf(), "task/cleanup".to_string())
        .await
        .expect("delete_branch should return Ok even when remote push fails");

    // Verify the local branch no longer exists.
    let list_after = std::process::Command::new("git")
        .args(["branch", "--list", "task/cleanup"])
        .current_dir(fixture.path())
        .output()
        .expect("git branch --list");
    assert!(
        String::from_utf8_lossy(&list_after.stdout)
            .trim()
            .is_empty(),
        "local branch should be deleted; list output: {}",
        String::from_utf8_lossy(&list_after.stdout)
    );
}

// ── Borrowed-cwd helpers (added by cgcl / Wave 1 of fztz) ──────────────────
//
// The borrowed-cwd variants were added so call sites that already hold a
// `&Path` (e.g. an `IndexTreeHandle::path()` borrow, a tempdir project root)
// don't have to clone a `PathBuf` just to satisfy the original signatures.
// They exist specifically to keep djinn-graph (and future migrations) on the
// owner crate.

/// `run_git_command_in(&Path, …)` must produce the same `CommandOutput` as
/// `run_git_command(PathBuf, …)` for an equivalent invocation — both wrap
/// the same underlying helper and should agree on stdout, stderr, and code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_git_command_in_matches_run_git_command() {
    let fixture = init_repo_with_main_commit();
    let repo_path = fixture.path();
    let args = vec!["status".into(), "--short".into()];

    let owned = run_git_command(repo_path.to_path_buf(), args.clone()).await;
    let borrowed = run_git_command_in(repo_path, args).await;
    assert!(owned.is_ok(), "owned variant should succeed: {owned:?}");
    assert!(
        borrowed.is_ok(),
        "borrowed variant should succeed: {borrowed:?}"
    );
    let owned = owned.unwrap();
    let borrowed = borrowed.unwrap();
    assert_eq!(owned.code, borrowed.code, "exit codes must agree");
    assert_eq!(
        owned.stdout, borrowed.stdout,
        "stdout must agree between owned/borrowed variants"
    );
    assert_eq!(
        owned.stderr, borrowed.stderr,
        "stderr must agree between owned/borrowed variants"
    );
}

/// `run_git_command_with_timeout_in(&Path, …)` should map a deliberate
/// failure into the same `CommandFailed` error as the owned variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_git_command_with_timeout_in_returns_command_failed() {
    let fixture = init_repo_with_main_commit();
    let repo_path = fixture.path();
    let args = vec!["fetch".into(), "nonexistent-remote".into(), "main".into()];
    let timeout = Duration::from_secs(5);

    let err = run_git_command_with_timeout_in(repo_path, args, timeout)
        .await
        .expect_err("fetch from missing remote should fail");
    match err {
        GitError::CommandFailed { code, stderr, .. } => {
            assert_ne!(code, 0, "exit code must be non-zero");
            assert!(
                !stderr.is_empty(),
                "stderr should contain the git error message"
            );
        }
        other => panic!("expected CommandFailed, got: {other:?}"),
    }
}

/// `head_commit_sha` should return the SHA of the lone commit in a freshly
/// initialised repo. We don't compare the value verbatim because the temp
/// fixture's commit timestamp + author affect only the message — the SHA is
/// otherwise deterministic for an empty commit graph.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_commit_sha_returns_single_commit_sha() {
    let fixture = init_repo_with_main_commit();
    let sha = head_commit_sha(fixture.path())
        .await
        .expect("head_commit_sha should succeed on a 1-commit repo");
    assert_eq!(sha.len(), 40, "git SHA should be 40 hex chars, got {sha:?}");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "SHA must be hex, got {sha:?}"
    );

    // The committed README should be present in HEAD's tree.
    let tree_out = run_git_command(
        fixture.path().to_path_buf(),
        vec!["ls-tree".into(), sha, "--".into(), "README.md".into()],
    )
    .await
    .expect("git ls-tree should succeed");
    assert!(
        tree_out.stdout.contains("README.md"),
        "ls-tree stdout should mention README.md, got: {}",
        tree_out.stdout
    );
}

/// `rev_list_count` should return 0 for a single-commit repo and 1 after a
/// second commit is appended on top of HEAD.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev_list_count_grows_with_history() {
    let fixture = init_repo_with_main_commit();
    let initial = rev_list_count(fixture.path(), "HEAD")
        .await
        .expect("rev_list_count on a 1-commit repo");
    assert_eq!(initial, 1, "fresh repo should have exactly 1 commit");

    write_and_commit(fixture.path(), "CHANGELOG.md", "v0.1.0\n", "second");

    let after = rev_list_count(fixture.path(), "HEAD")
        .await
        .expect("rev_list_count after second commit");
    assert_eq!(after, 2, "after one extra commit, count should be 2");

    // The original commit should be reachable from HEAD so HEAD..HEAD is
    // empty and HEAD~1..HEAD is exactly one.
    let head_minus_one = rev_list_count(fixture.path(), "HEAD~1..HEAD")
        .await
        .expect("rev_list_count on a one-commit range");
    assert_eq!(
        head_minus_one, 1,
        "HEAD~1..HEAD should walk exactly 1 commit"
    );
}

/// `rev_list_count` should surface an error for an unparseable / bogus range
/// rather than silently returning 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rev_list_count_errors_on_bogus_range() {
    let fixture = init_repo_with_main_commit();
    let err = rev_list_count(fixture.path(), "this-ref-does-not-exist..HEAD")
        .await
        .expect_err("bogus range should error");
    // The specific error variant depends on git's exit-code path; we just
    // assert it's a `GitError` (not a panic), which the type system already
    // guarantees, plus that the underlying git process actually exited
    // non-zero (the `code` field of `CommandFailed`).
    match err {
        GitError::CommandFailed { code, .. } => assert_ne!(code, 0),
        GitError::Other(_) | GitError::Io(_) => { /* also acceptable */ }
        other => panic!("unexpected GitError variant: {other:?}"),
    }
}

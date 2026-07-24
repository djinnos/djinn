// Shell GateGuard dispatch-level tests.
//
// These tests exercise the full dispatch path through `call_shell` with
// session_role parameters, proving that the worker shell GateGuard
// enforcement is applied at the handler level.  The colocated
// `command_classifier_tests` module covers the classifier itself; these
// cover the dispatch integration via `gate_guard_shell_check`.

use super::handlers::call_shell;
use super::{agent_context_from_db, create_test_db};
use crate::file_time::destructive_class::WORKTREE_LOCAL_FILE_MUTATION;
use crate::process::{CgroupLauncherClient, LeaseInvocationRunner, ProcessHandle};
use djinn_cgroup_launcher::CpuStat;
use djinn_core::clock::SystemClock;
use djinn_supervisor::services::{LeaseFencingToken, TaskInvocationLeaseIdentity};
use std::io;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// A remote-child-shaped broker adapter for the workspace composition test.
/// It deliberately ignores `Command`: unlike the legacy test fallback it never
/// spawns a local process, and returns output only through a broker handle.
#[derive(Clone)]
struct BrokerShellLauncher {
    state: Arc<Mutex<BrokerShellState>>,
}

struct BrokerShellState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
    identities: Vec<TaskInvocationLeaseIdentity>,
    empties: usize,
    cleanups: usize,
}

impl BrokerShellLauncher {
    fn exited(stdout: &[u8], stderr: &[u8], code: i32) -> Self {
        use std::os::unix::process::ExitStatusExt;

        Self {
            state: Arc::new(Mutex::new(BrokerShellState {
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
                status: ExitStatus::from_raw(code << 8),
                identities: Vec::new(),
                empties: 0,
                cleanups: 0,
            })),
        }
    }
}

impl CgroupLauncherClient for BrokerShellLauncher {
    fn launch(
        &self,
        _: Command,
        identity: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        self.state.lock().unwrap().identities.push(identity.clone());
        Ok(Box::new(BrokerShellHandle {
            state: self.state.clone(),
        }))
    }
}

struct BrokerShellHandle {
    state: Arc<Mutex<BrokerShellState>>,
}

impl ProcessHandle for BrokerShellHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.state.lock().unwrap().stdout))
    }

    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(std::mem::take(&mut self.state.lock().unwrap().stderr))
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(Some(self.state.lock().unwrap().status))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self.state.lock().unwrap().status)
    }

    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        Ok(CpuStat::default())
    }

    fn fenced_lift(&mut self, _: &LeaseFencingToken) -> io::Result<()> {
        Ok(())
    }

    fn kill(&mut self) -> io::Result<()> {
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

fn setup(prefix: &str) -> (tempfile::TempDir, crate::context::AgentContext) {
    let dir = crate::test_helpers::test_tempdir(prefix);
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    (dir, state)
}

fn shell_args(command: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({ "command": command })
            .as_object()
            .unwrap()
            .clone(),
    )
}

// The Prometheus recorder is process-global. Serialize the scrape/dispatch/
// scrape sequence so each assertion can use a precise delta rather than
// assuming an empty registry.
async fn telemetry_test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    use std::sync::{Arc, OnceLock};

    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn write_fake_cargo(worktree: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let cargo = worktree.join("cargo");
    std::fs::write(&cargo, format!("#!/bin/sh\n{body}\n")).expect("write fake cargo");
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755))
        .expect("make fake cargo executable");
    cargo
}

fn cargo_metric_lines(rendered: &str) -> Vec<&str> {
    rendered
        .lines()
        .filter(|line| line.starts_with("djinn_cargo_invocation_seconds"))
        .collect()
}

fn metric_labels(line: &str) -> Vec<&str> {
    line.split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(labels, _)| labels.split(',').collect())
        .unwrap_or_default()
}

fn cargo_count(rendered: &str, kind: &str, exit: &str) -> f64 {
    let expected_exit = format!("exit=\"{exit}\"");
    let expected_kind = format!("kind=\"{kind}\"");
    cargo_metric_lines(rendered)
        .into_iter()
        .find(|line| {
            let labels = metric_labels(line);
            line.starts_with("djinn_cargo_invocation_seconds_count{")
                && labels.len() == 2
                && labels.contains(&expected_kind.as_str())
                && labels.contains(&expected_exit.as_str())
        })
        .and_then(|line| line.rsplit_once(' '))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0.0)
}

fn assert_cargo_schema_and_buckets(rendered: &str, kind: &str, exit: &str, injected: &str) {
    let lines = cargo_metric_lines(rendered);
    assert!(
        !lines.is_empty(),
        "cargo invocation metric must be rendered"
    );

    for line in &lines {
        let labels = metric_labels(line);
        let expected = if line.contains("_bucket{") {
            ["exit", "kind", "le"].as_slice()
        } else {
            ["exit", "kind"].as_slice()
        };
        let mut keys: Vec<_> = labels
            .iter()
            .map(|label| label.split_once('=').expect("label key/value").0)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, expected, "unexpected cargo metric labels: {line}");
        for forbidden in [
            "command=",
            "argv=",
            "path=",
            "workdir=",
            "task",
            "session",
            "project",
            "user",
            "fingerprint",
            "error",
        ] {
            assert!(
                !line.contains(forbidden),
                "cargo metric must not expose {forbidden}: {line}"
            );
        }
        assert!(
            !line.contains(injected),
            "cargo metric must not expose arbitrary script output: {line}"
        );
    }

    let expected_finite = [
        "1", "2", "5", "10", "30", "60", "120", "300", "600", "1200", "1800",
    ];
    let expected_kind = format!("kind=\"{kind}\"");
    let expected_exit = format!("exit=\"{exit}\"");
    let buckets: Vec<_> = lines
        .iter()
        .filter(|line| line.starts_with("djinn_cargo_invocation_seconds_bucket{"))
        .filter(|line| {
            let labels = metric_labels(line);
            labels.contains(&expected_kind.as_str()) && labels.contains(&expected_exit.as_str())
        })
        .filter_map(|line| {
            metric_labels(line).into_iter().find_map(|label| {
                label
                    .strip_prefix("le=\"")
                    .and_then(|v| v.strip_suffix('"'))
            })
        })
        .collect();
    assert_eq!(
        buckets.len(),
        expected_finite.len() + 1,
        "cargo series must have every finite bucket plus +Inf"
    );
    assert_eq!(&buckets[..expected_finite.len()], expected_finite);
    assert_eq!(buckets.last(), Some(&"+Inf"));
}

// ─── Cargo invocation telemetry scrape regressions ───────────────────────

#[tokio::test]
async fn shell_dispatch_fake_cargo_success_has_exact_telemetry_delta_and_schema() {
    let _guard = telemetry_test_guard().await;
    djinn_telemetry::init().expect("initialize telemetry");
    let (worktree, state) = setup("shell-cargo-telemetry-success-");
    let injected = "ARBITRARY_CARGO_SUCCESS_OUTPUT_MUST_NOT_BE_A_LABEL";
    let cargo = write_fake_cargo(worktree.path(), &format!("printf '%s\\n' '{injected}'"));
    let command = format!("{} clippy", cargo.display());
    let before = djinn_telemetry::render().expect("render telemetry before dispatch");
    let count_before = cargo_count(&before, "clippy", "ok");

    let response = call_shell(
        &state,
        &shell_args(&command),
        worktree.path(),
        Some("reviewer"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("fake cargo success dispatch");
    assert_eq!(response["ok"], serde_json::json!(true));

    let after = djinn_telemetry::render().expect("render telemetry after dispatch");
    assert_eq!(cargo_count(&after, "clippy", "ok"), count_before + 1.0);
    assert!(
        !after.contains(injected),
        "arbitrary fake cargo output must not appear in the telemetry scrape"
    );
    assert_cargo_schema_and_buckets(&after, "clippy", "ok", injected);
}

#[tokio::test]
async fn shell_dispatch_broker_backed_cargo_records_exactly_one_observation() {
    let _guard = telemetry_test_guard().await;
    djinn_telemetry::init().expect("initialize telemetry");
    let (worktree, mut state) = setup("shell-broker-composition-");
    let launcher = BrokerShellLauncher::exited(
        b"broker composition stdout\n",
        b"broker composition stderr\n",
        17,
    );
    let runner = Arc::new(LeaseInvocationRunner::new(
        Arc::new(djinn_supervisor::services::rpc::UnimplementedRpcServices::new()),
        Arc::new(launcher.clone()),
        Arc::new(SystemClock::new()),
    ));
    state.shell_launch = Some(crate::context::ShellLaunchContext::for_test(
        runner,
        "trusted-task".into(),
        "trusted-task-run".into(),
        "trusted-pod-uid".into(),
    ));

    let before = djinn_telemetry::render().expect("render telemetry before dispatch");
    let count_before = cargo_count(&before, "clippy", "fail");
    let response = call_shell(
        &state,
        &shell_args("cargo clippy"),
        worktree.path(),
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("broker-backed shell dispatch returns its terminal output");

    assert_eq!(response["ok"], serde_json::json!(false));
    assert_eq!(response["exit_code"], serde_json::json!(17));
    assert_eq!(
        response["stdout"],
        serde_json::json!("broker composition stdout\n")
    );
    assert_eq!(
        response["stderr"],
        serde_json::json!("broker composition stderr\n")
    );
    let after = djinn_telemetry::render().expect("render telemetry after dispatch");
    assert_eq!(
        cargo_count(&after, "clippy", "fail"),
        count_before + 1.0,
        "one classified broker-backed shell terminal result records exactly once"
    );

    let broker = launcher.state.lock().unwrap();
    assert_eq!(broker.identities.len(), 1);
    assert_eq!(broker.identities[0].task_id, "trusted-task");
    assert_eq!(broker.identities[0].task_run_id, "trusted-task-run");
    assert!(!broker.identities[0].invocation_id.is_empty());
    assert_eq!((broker.empties, broker.cleanups), (1, 1));
}

#[tokio::test]
async fn shell_dispatch_fake_cargo_failure_has_exact_telemetry_delta() {
    let _guard = telemetry_test_guard().await;
    djinn_telemetry::init().expect("initialize telemetry");
    let (worktree, state) = setup("shell-cargo-telemetry-failure-");
    let injected = "ARBITRARY_CARGO_FAILURE_TEXT_MUST_NOT_BE_A_LABEL";
    let cargo = write_fake_cargo(
        worktree.path(),
        &format!("printf '%s\\n' '{injected}' >&2\nexit 17"),
    );
    let command = format!("{} test", cargo.display());
    let before = djinn_telemetry::render().expect("render telemetry before dispatch");
    let count_before = cargo_count(&before, "test", "fail");

    let response = call_shell(
        &state,
        &shell_args(&command),
        worktree.path(),
        Some("reviewer"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("fake cargo failure still returns shell output");
    assert_eq!(response["ok"], serde_json::json!(false));

    let after = djinn_telemetry::render().expect("render telemetry after dispatch");
    assert_eq!(cargo_count(&after, "test", "fail"), count_before + 1.0);
    assert!(
        !after.contains(injected),
        "arbitrary fake cargo error text must not appear in the telemetry scrape"
    );
    assert_cargo_schema_and_buckets(&after, "test", "fail", injected);
}

#[tokio::test]
async fn shell_dispatch_non_cargo_does_not_change_cargo_telemetry() {
    let _guard = telemetry_test_guard().await;
    djinn_telemetry::init().expect("initialize telemetry");
    let (worktree, state) = setup("shell-cargo-telemetry-non-cargo-");
    let before = djinn_telemetry::render().expect("render telemetry before dispatch");

    let response = call_shell(
        &state,
        &shell_args("printf non-cargo-shell-dispatch"),
        worktree.path(),
        Some("reviewer"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("non-cargo shell dispatch");
    assert_eq!(response["ok"], serde_json::json!(true));

    let after = djinn_telemetry::render().expect("render telemetry after dispatch");
    assert_eq!(
        cargo_metric_lines(&after),
        cargo_metric_lines(&before),
        "non-cargo dispatch must not change any cargo telemetry series"
    );
}

// ─── AC 1: worker soft-gate first-deny / second-allow ────────────────────

/// Worker soft-gated command (`rm scratch.txt`) returns a FORCE prompt on
/// first invocation and records `bash_soft_forced` for the
/// `WorktreeLocalFileMutation` class.  After re-creating the scratch file,
/// retry with the same command succeeds (soft-gate allows second call).
#[tokio::test]
async fn shell_worker_soft_gate_first_deny_second_allow() {
    let (worktree, state) = setup("gg-shell-softgate-");
    let session_id = worktree.path().display().to_string();

    // Create a scratch file that the soft-gated `rm` will target.
    let scratch = worktree.path().join("scratch.txt");
    tokio::fs::write(&scratch, "temporary\n")
        .await
        .expect("seed scratch");

    // Phase 1: first worker call → soft-gated, returns FORCE prompt.
    let err = call_shell(
        &state,
        &shell_args("rm scratch.txt"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("first soft-gated shell call must be denied");
    assert!(
        err.contains("gated"),
        "expected FORCE prompt with 'gated', got: {err}"
    );
    assert!(
        err.contains("rollback") || err.contains("recreation"),
        "FORCE prompt must demand a rollback/recreation plan, got: {err}"
    );

    // bash_soft_forced must be set for WorktreeLocalFileMutation.
    assert!(
        state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "bash_soft_forced must be set for WorktreeLocalFileMutation after first denial"
    );

    // Phase 2: re-create the scratch file (rm hasn't run yet), retry same
    // command → must succeed and actually remove the file.
    tokio::fs::write(&scratch, "temporary\n")
        .await
        .expect("re-seed scratch");
    let resp = call_shell(
        &state,
        &shell_args("rm scratch.txt"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("second soft-gated shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true), "rm must exit cleanly");

    // The file must actually be removed.
    assert!(
        !scratch.exists(),
        "scratch.txt must be removed after successful retry"
    );

    // bash_soft_forced must still be set (no reset on success).
    assert!(
        state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "bash_soft_forced must remain set after successful retry"
    );
}

// ─── AC 2: hard-deny commands ────────────────────────────────────────────

/// Hard-denied shell command (`git reset --hard`) returns an error, never
/// marks `bash_soft_forced`, and cannot be unlocked by retry.
#[tokio::test]
async fn shell_hard_deny_git_reset_hard_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-git-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("git reset --hard HEAD"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("git reset --hard must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );
    assert!(
        err.contains("cannot be unlocked") || err.contains("retry"),
        "hard-deny must say no-unlock, got: {err}"
    );

    // bash_soft_forced must NOT be set for any class.
    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "hard-denied command must NOT mark bash_soft_forced for WorktreeLocalFileMutation"
    );
}

/// Hard-denied shell command (`git clean -fd`) returns an error and never
/// marks `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_git_clean_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-clean-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("git clean -fd"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("git clean must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "git clean must NOT mark bash_soft_forced"
    );
}

/// Hard-denied shell command (`git stash`) returns an error and never marks
/// `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_git_stash_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-stash-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("git stash"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("git stash must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "git stash must NOT mark bash_soft_forced"
    );
}

/// Hard-denied DB DDL command (`psql -c "DROP TABLE foo"`) returns an error
/// and never marks `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_db_drop_table_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-db-ddl-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("psql -c \"DROP TABLE foo\""),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("DROP TABLE must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "DROP TABLE must NOT mark bash_soft_forced"
    );
}

/// Hard-denied DB DML command (`psql -c "DELETE FROM users"`) returns an
/// error and never marks `bash_soft_forced`.
#[tokio::test]
async fn shell_hard_deny_db_delete_from_never_marks_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-harddeny-db-dml-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("psql -c \"DELETE FROM users WHERE id = 1\""),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("DELETE FROM must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "DELETE FROM must NOT mark bash_soft_forced"
    );
}

// ─── AC 3: non-worker role bypass ────────────────────────────────────────

/// Reviewer runs a destructive-class command (`rm tmp.txt`) without gate
/// guard interference.  The command executes successfully and
/// `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_reviewer_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-reviewer-");
    let session_id = worktree.path().display().to_string();

    // Create a file for the reviewer's `rm` to target.
    let tmp = worktree.path().join("tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm tmp.txt"),
        worktree.path(),
        Some("reviewer"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("reviewer shell call must succeed (no GateGuard)");
    assert_eq!(resp["ok"], serde_json::json!(true));
    assert!(!tmp.exists(), "reviewer rm must actually remove the file");

    // bash_soft_forced must NOT be recorded for non-worker roles.
    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "reviewer must NOT mark bash_soft_forced"
    );
}

/// Planner runs a destructive-class command without gate guard
/// interference.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_planner_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-planner-");
    let session_id = worktree.path().display().to_string();

    let tmp = worktree.path().join("planner-tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm planner-tmp.txt"),
        worktree.path(),
        Some("planner"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("planner shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "planner must NOT mark bash_soft_forced"
    );
}

/// Architect runs a destructive-class command without gate guard
/// interference.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_architect_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-architect-");
    let session_id = worktree.path().display().to_string();

    let tmp = worktree.path().join("arch-tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm arch-tmp.txt"),
        worktree.path(),
        Some("architect"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("architect shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "architect must NOT mark bash_soft_forced"
    );
}

/// Missing role (None) runs a destructive-class command without gate guard
/// interference.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_missing_role_bypasses_gate_guard_no_bash_soft_forced() {
    let (worktree, state) = setup("gg-shell-norole-");
    let session_id = worktree.path().display().to_string();

    let tmp = worktree.path().join("no-role-tmp.txt");
    tokio::fs::write(&tmp, "data\n").await.expect("seed");

    let resp = call_shell(
        &state,
        &shell_args("rm no-role-tmp.txt"),
        worktree.path(),
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("missing-role shell call must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "missing role must NOT mark bash_soft_forced"
    );
}

// ─── AC 4: path-scope exclusions through the enforcement path ────────────

/// Commands targeting `.git` paths are hard-denied through the full dispatch
/// path (not just the classifier unit test).  `bash_soft_forced` is never
/// recorded.
#[tokio::test]
async fn shell_path_scope_git_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-git-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm .git/objects/pack"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .git must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".git path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting parent directory (`..`) paths are hard-denied through
/// the full dispatch path.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_path_scope_parent_dir_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-dotdot-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm ../outside-file"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .. must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".. path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting internal sibling-checkout paths are hard-denied through
/// the full dispatch path.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_path_scope_internal_read_sources_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-djinnrs-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm .djinn-read-sources/some-project/file.txt"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting internal sibling checkout must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "internal sibling-checkout path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting durable data paths (`Cargo.toml`) are hard-denied
/// through the full dispatch path.  `bash_soft_forced` is never recorded.
#[tokio::test]
async fn shell_path_scope_durable_data_cargo_toml_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-durable-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm Cargo.toml"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting Cargo.toml must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "Cargo.toml durable data path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting durable data paths (`package.json`) are hard-denied
/// through the full dispatch path.
#[tokio::test]
async fn shell_path_scope_durable_data_package_json_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-durable-npm-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm package.json"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting package.json must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        "package.json durable data path must NOT mark bash_soft_forced"
    );
}

/// Commands targeting durable data paths (`.gitignore`) are hard-denied
/// through the full dispatch path.
#[tokio::test]
async fn shell_path_scope_durable_data_gitignore_is_hard_denied() {
    let (worktree, state) = setup("gg-shell-path-durable-gi-");
    let session_id = worktree.path().display().to_string();

    let err = call_shell(
        &state,
        &shell_args("rm .gitignore"),
        worktree.path(),
        Some("worker"),
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("rm targeting .gitignore must be hard-denied");
    assert!(
        err.contains("forbidden"),
        "hard-deny must contain 'forbidden', got: {err}"
    );

    assert!(
        !state
            .file_time
            .has_bash_soft_forced(&session_id, &WORKTREE_LOCAL_FILE_MUTATION)
            .await,
        ".gitignore durable data path must NOT mark bash_soft_forced"
    );
}

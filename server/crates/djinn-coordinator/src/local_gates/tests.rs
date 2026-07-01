use super::*;
use std::sync::Mutex;

use futures::future::FutureExt;

// ── Test doubles ──────────────────────────────────────────────────────────────

/// A `CommandRunner` that returns a pre-configured `ExecOutput` for every call.
struct StubRunner {
    output: ExecOutput,
}

impl CommandRunner for StubRunner {
    fn run(
        &self,
        _repo_root: &Path,
        _command: &[&str],
        _cwd: &str,
        _timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecOutput> + Send>> {
        let out = self.output.clone();
        Box::pin(async move { out })
    }
}

fn passed_output() -> ExecOutput {
    ExecOutput {
        exit_code: Some(0),
        stdout: "ok".to_string(),
        stderr: String::new(),
        duration: Duration::from_millis(10),
        unavailable: false,
    }
}

fn failed_output() -> ExecOutput {
    ExecOutput {
        exit_code: Some(1),
        stdout: String::new(),
        stderr: "error: something failed".to_string(),
        duration: Duration::from_millis(20),
        unavailable: false,
    }
}

fn unavailable_output() -> ExecOutput {
    ExecOutput {
        exit_code: None,
        stdout: String::new(),
        stderr: "command not found: cargo".to_string(),
        duration: Duration::from_millis(1),
        unavailable: true,
    }
}

/// A `CommandRunner` that records all invocations and returns outputs keyed by
/// the command string.
struct RecordingRunner {
    /// Map from command-joined-by-space to the output to return.
    outputs: std::collections::HashMap<String, ExecOutput>,
    /// Captured calls (command, cwd, timeout) in invocation order.
    calls: Mutex<Vec<(String, String, Duration)>>,
}

impl RecordingRunner {
    fn new() -> Self {
        Self {
            outputs: std::collections::HashMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_output(mut self, command: &str, output: ExecOutput) -> Self {
        self.outputs.insert(command.to_string(), output);
        self
    }

    fn calls_snapshot(&self) -> Vec<(String, String, Duration)> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(
        &self,
        _repo_root: &Path,
        command: &[&str],
        cwd: &str,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecOutput> + Send>> {
        let key = command.join(" ");
        let out = self
            .outputs
            .get(&key)
            .cloned()
            .unwrap_or_else(|| ExecOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                duration: Duration::from_millis(1),
                unavailable: false,
            });
        self.calls
            .lock()
            .unwrap()
            .push((key, cwd.to_string(), timeout));
        Box::pin(async move { out })
    }
}

// ── Registry contents ─────────────────────────────────────────────────────────

#[test]
fn registry_contains_minimum_required_gates() {
    let reg = builtin_registry();
    let ids: Vec<&str> = reg.iter().map(|s| s.id).collect();

    // Proposal-specified mappings must exist.
    assert!(
        ids.contains(&"server-size-guard"),
        "missing size guard gate"
    );
    assert!(ids.contains(&"rustfmt"), "missing rustfmt gate");
    assert!(ids.contains(&"clippy"), "missing clippy gate");
    assert!(ids.contains(&"rust-tests"), "missing rust-tests gate");
}

#[test]
fn size_guard_maps_to_check_file_size_script() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "server-size-guard")
        .expect("size guard gate exists");

    assert_eq!(spec.command[0], "./scripts/check-file-size.sh");
    // Repo root = empty cwd.
    assert_eq!(spec.cwd, "");
    // Short timeout.
    assert!(spec.timeout <= Duration::from_secs(60));
    // Required.
    assert_eq!(spec.applicability, GateApplicability::Required);
    // Aliases cover both the job id and display name.
    assert!(spec.check_aliases.contains(&"server-size-guard"));
    assert!(spec.check_aliases.contains(&"Server Size Guard"));
}

#[test]
fn rustfmt_maps_to_cargo_fmt_check() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "rustfmt")
        .expect("rustfmt gate exists");

    let cmd = spec.command.join(" ");
    assert!(
        cmd.contains("cargo fmt") && cmd.contains("--check"),
        "rustfmt command should be `cargo fmt --check`-like, got: {cmd}"
    );
    assert_eq!(spec.cwd, "server");
    assert!(spec.timeout <= Duration::from_secs(180));
    assert_eq!(spec.applicability, GateApplicability::Required);
}

#[test]
fn clippy_maps_to_cargo_clippy() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "clippy")
        .expect("clippy gate exists");

    let cmd = spec.command.join(" ");
    assert!(
        cmd.contains("cargo clippy"),
        "clippy command should invoke cargo clippy, got: {cmd}"
    );
    assert_eq!(spec.cwd, "server");
    assert!(spec.timeout.as_secs() > 0);
}

#[test]
fn rust_tests_maps_to_cargo_test() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "rust-tests")
        .expect("rust-tests gate exists");

    let cmd = spec.command.join(" ");
    assert!(
        cmd.starts_with("cargo test"),
        "rust-tests command should start with `cargo test`, got: {cmd}"
    );
    assert_eq!(spec.cwd, "server");
}

// ── Registry matching (structured CI metadata) ────────────────────────────────

#[test]
fn build_plan_matches_on_check_name() {
    let input = CiGateInput::from_task_fields(r#"["server-size-guard"]"#, None, None, Vec::new());
    let plan = build_plan(&input);
    let ids = plan.ids();
    assert!(
        ids.contains(&"server-size-guard"),
        "size guard should match on check name, got: {ids:?}"
    );
}

#[test]
fn build_plan_matches_on_check_name_case_insensitive() {
    let input = CiGateInput::from_task_fields(r#"["SERVER-SIZE-GUARD"]"#, None, None, Vec::new());
    let plan = build_plan(&input);
    assert!(plan.ids().contains(&"server-size-guard"));
}

#[test]
fn build_plan_matches_on_fingerprint_substring() {
    let input = CiGateInput::from_task_fields(
        r#"["Server Clippy"]"#,
        Some("sha:abc123|checks:clippy,size".to_string()),
        None,
        Vec::new(),
    );
    let plan = build_plan(&input);
    let ids = plan.ids();
    // Clippy matches via both check name and fingerprint substring.
    assert!(ids.contains(&"clippy"), "clippy should match, got: {ids:?}");
}

#[test]
fn build_plan_matches_on_job_name_substring() {
    let input = CiGateInput::from_task_fields(
        r#"["Quality Gate"]"#,
        None,
        None,
        vec!["Server Clippy".to_string(), "Server Test".to_string()],
    );
    let plan = build_plan(&input);
    let ids = plan.ids();
    assert!(
        ids.contains(&"clippy"),
        "clippy should match on job name, got: {ids:?}"
    );
    assert!(
        ids.contains(&"rust-tests"),
        "rust-tests should match on job name, got: {ids:?}"
    );
}

#[test]
fn build_plan_matches_multiple_gates_for_multiple_checks() {
    let input = CiGateInput::from_task_fields(
        r#"["server-size-guard","Server Clippy","Server Test"]"#,
        Some("sha:abc|checks:clippy,test,size".to_string()),
        None,
        Vec::new(),
    );
    let plan = build_plan(&input);
    let ids = plan.ids();
    assert!(ids.contains(&"server-size-guard"));
    assert!(ids.contains(&"clippy"));
    assert!(ids.contains(&"rust-tests"));
}

#[test]
fn build_plan_returns_empty_when_no_checks_match() {
    let input = CiGateInput::from_task_fields(r#"["Vercel Preview"]"#, None, None, Vec::new());
    let plan = build_plan(&input);
    assert!(
        plan.gates.is_empty(),
        "no gates should match advisory checks"
    );
    assert!(!plan.has_required());
}

#[test]
fn build_plan_ignores_empty_check_names() {
    let input = CiGateInput::from_task_fields("", None, None, Vec::new());
    let plan = build_plan(&input);
    assert!(plan.gates.is_empty());
}

#[test]
fn build_plan_tolerates_malformed_check_names_json() {
    let input = CiGateInput::from_task_fields("not-valid-json", None, None, Vec::new());
    let plan = build_plan(&input);
    assert!(plan.gates.is_empty());
}

#[test]
fn ci_gate_input_from_task_fields_preserves_fingerprint_and_baseline() {
    let input = CiGateInput::from_task_fields(
        r#"["Server Clippy"]"#,
        Some("fp-abc".to_string()),
        Some("sha-base".to_string()),
        vec!["job1".to_string()],
    );
    assert_eq!(input.blocking_check_names, vec!["Server Clippy"]);
    assert_eq!(input.failure_fingerprint.as_deref(), Some("fp-abc"));
    assert_eq!(input.last_remediation_base_sha.as_deref(), Some("sha-base"));
    assert_eq!(input.implicated_job_names, vec!["job1"]);
}

// ── Non-applicable / advisory behavior ────────────────────────────────────────

#[test]
fn non_matching_gate_is_excluded_from_plan() {
    // Only size-guard implicated; clippy/tests should not be in the plan.
    let input = CiGateInput::from_task_fields(r#"["server-size-guard"]"#, None, None, Vec::new());
    let plan = build_plan(&input);
    let ids = plan.ids();
    assert!(ids.contains(&"server-size-guard"));
    assert!(!ids.contains(&"clippy"), "clippy should not match");
    assert!(!ids.contains(&"rust-tests"), "rust-tests should not match");
}

#[test]
fn advisory_gate_produces_skipped_result() {
    // Construct a plan with an advisory gate directly.
    let advisory_spec = GateSpec {
        id: "advisory-test",
        check_aliases: &[],
        fingerprint_substrings: &[],
        job_name_substrings: &[],
        command: &["true"],
        cwd: "",
        timeout: Duration::from_secs(1),
        applicability: GateApplicability::Advisory,
    };
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: advisory_spec,
        }],
    };

    let runner = StubRunner {
        output: failed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("/nonexistent"), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].outcome, GateOutcome::Skipped);
    assert!(!result.results[0].blocking);
    assert!(!result.has_blocking_failure());
}

#[test]
fn non_applicable_gate_produces_skipped_result() {
    let na_spec = GateSpec {
        id: "na-test",
        check_aliases: &[],
        fingerprint_substrings: &[],
        job_name_substrings: &[],
        command: &["true"],
        cwd: "",
        timeout: Duration::from_secs(1),
        applicability: GateApplicability::NonApplicable,
    };
    let plan = GatePlan {
        gates: vec![PlannedGate { spec: na_spec }],
    };

    let runner = StubRunner {
        output: failed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("/nonexistent"), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].outcome, GateOutcome::Skipped);
    assert!(!result.has_blocking_failure());
}

// ── Execution: passed / failed / unavailable ──────────────────────────────────

#[test]
fn required_gate_passing_does_not_block() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "server-size-guard")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = StubRunner {
        output: passed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].outcome, GateOutcome::Passed);
    assert!(!result.results[0].is_blocking_failure());
    assert!(result.all_clean());
}

#[test]
fn required_gate_failing_blocks() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "server-size-guard")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = StubRunner {
        output: failed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert_eq!(result.results[0].outcome, GateOutcome::Failed);
    assert!(result.results[0].is_blocking_failure());
    assert!(result.has_blocking_failure());
    assert_eq!(result.blocking_gate_ids(), vec!["server-size-guard"]);
}

#[test]
fn unavailable_required_gate_blocks_and_is_not_a_pass() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "rustfmt")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = StubRunner {
        output: unavailable_output(),
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert_eq!(result.results[0].outcome, GateOutcome::Unavailable);
    assert!(result.results[0].is_blocking_failure());
    assert!(result.has_blocking_failure());
    // The critical property: unavailable is NOT a pass.
    assert_ne!(result.results[0].outcome, GateOutcome::Passed);
}

#[test]
fn unavailable_and_failed_both_block_but_differ() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "clippy")
        .unwrap();

    // Unavailable
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = StubRunner {
        output: unavailable_output(),
    };
    let unavailable_result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");
    assert_eq!(
        unavailable_result.results[0].outcome,
        GateOutcome::Unavailable
    );
    assert!(unavailable_result.has_blocking_failure());

    // Failed
    let runner = StubRunner {
        output: failed_output(),
    };
    let failed_result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");
    assert_eq!(failed_result.results[0].outcome, GateOutcome::Failed);
    assert!(failed_result.has_blocking_failure());
}

// ── Output truncation / artifact summary shape ────────────────────────────────

#[test]
fn stdout_summary_is_truncated_when_very_long() {
    // Build an output with a very long stdout.
    let long_stdout = "HEAD\n".to_string() + &"x".repeat(OUTPUT_SUMMARY_MAX_BYTES * 4) + "\nTAIL\n";
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "server-size-guard")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = StubRunner {
        output: ExecOutput {
            exit_code: Some(1),
            stdout: long_stdout,
            stderr: String::new(),
            duration: Duration::from_millis(5),
            unavailable: false,
        },
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    let summary = &result.results[0].stdout_summary;
    // Summary should be within the budget (plus a small separator allowance).
    assert!(
        summary.len() <= OUTPUT_SUMMARY_MAX_BYTES + 200,
        "summary should be truncated to ~{} bytes, got {}",
        OUTPUT_SUMMARY_MAX_BYTES,
        summary.len()
    );
    // Smart truncation preserves head and tail.
    assert!(summary.contains("HEAD"), "head should be preserved");
    assert!(summary.contains("TAIL"), "tail should be preserved");
    assert!(
        summary.contains("omitted"),
        "omission marker should be present"
    );
}

#[test]
fn result_contains_command_cwd_timeout_and_exit_code() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "rustfmt")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = StubRunner {
        output: failed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    let r = &result.results[0];
    assert!(!r.command.is_empty());
    assert_eq!(r.cwd, "server");
    assert_eq!(r.timeout, RUSTFMT_TIMEOUT);
    assert_eq!(r.exit_code, Some(1));
    assert!(r.duration.is_some());
}

#[test]
fn empty_output_produces_empty_summary() {
    assert_eq!(summarize_output(""), "");
    assert_eq!(summarize_output("   "), "   "); // not empty, just whitespace
}

// ── Execution passes correct cwd/command/timeout to runner ────────────────────

#[test]
fn execute_gate_passes_correct_command_and_cwd() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "rustfmt")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner = RecordingRunner::new().with_output("cargo fmt --all -- --check", passed_output());

    let _ = super::execute_plan(&plan, Path::new("/repo"), &runner)
        .now_or_never()
        .expect("recording runner is immediately ready");

    let calls = runner.calls_snapshot();
    assert_eq!(calls.len(), 1);
    let (cmd, cwd, timeout) = &calls[0];
    assert_eq!(cmd, "cargo fmt --all -- --check");
    assert_eq!(cwd, "server");
    assert_eq!(*timeout, RUSTFMT_TIMEOUT);
}

#[test]
fn execute_gate_passes_empty_cwd_for_repo_root_gates() {
    let spec = builtin_registry()
        .iter()
        .find(|s| s.id == "server-size-guard")
        .unwrap();
    let plan = GatePlan {
        gates: vec![PlannedGate {
            spec: (*spec).clone(),
        }],
    };
    let runner =
        RecordingRunner::new().with_output("./scripts/check-file-size.sh --all", passed_output());

    let _ = super::execute_plan(&plan, Path::new("/repo"), &runner)
        .now_or_never()
        .expect("recording runner is immediately ready");

    let calls = runner.calls_snapshot();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, ""); // cwd is empty = repo root
}

// ── Plan-level aggregation ────────────────────────────────────────────────────

#[test]
fn plan_result_all_clean_when_all_pass() {
    let input = CiGateInput::from_task_fields(r#"["server-size-guard"]"#, None, None, Vec::new());
    let plan = build_plan(&input);
    let runner = StubRunner {
        output: passed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert!(result.all_clean());
    assert!(!result.has_blocking_failure());
}

#[test]
fn plan_result_not_clean_when_one_fails() {
    let input = CiGateInput::from_task_fields(
        r#"["server-size-guard","Server Clippy"]"#,
        None,
        None,
        Vec::new(),
    );
    let plan = build_plan(&input);

    // Both gates get the same failed output.
    let runner = StubRunner {
        output: failed_output(),
    };
    let result = super::execute_plan(&plan, Path::new("."), &runner)
        .now_or_never()
        .expect("stub is immediately ready");

    assert!(!result.all_clean());
    assert!(result.has_blocking_failure());
    let blockers = result.blocking_gate_ids();
    assert!(blockers.contains(&"server-size-guard"));
    assert!(blockers.contains(&"clippy"));
}

// ── ProcessRunner: real unavailable-command classification ────────────────────

#[tokio::test]
async fn process_runner_returns_unavailable_when_cwd_missing() {
    let runner = ProcessRunner;
    let output = runner
        .run(
            Path::new("/nonexistent-repo-root-xyz"),
            &["true"],
            "",
            Duration::from_secs(5),
        )
        .await;

    assert!(output.unavailable, "missing cwd should be unavailable");
    assert!(output.exit_code.is_none());
}

#[tokio::test]
async fn process_runner_returns_unavailable_when_binary_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runner = ProcessRunner;
    let output = runner
        .run(
            tmp.path(),
            &["this-binary-definitely-does-not-exist-xyz123"],
            "",
            Duration::from_secs(5),
        )
        .await;

    assert!(
        output.unavailable,
        "missing binary should be unavailable, got: {output:?}"
    );
    assert!(output.exit_code.is_none());
}

#[tokio::test]
async fn process_runner_returns_exit_code_for_successful_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runner = ProcessRunner;
    let output = runner
        .run(tmp.path(), &["true"], "", Duration::from_secs(5))
        .await;

    assert!(!output.unavailable);
    assert_eq!(output.exit_code, Some(0));
}

#[tokio::test]
async fn process_runner_returns_nonzero_for_failing_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runner = ProcessRunner;
    let output = runner
        .run(tmp.path(), &["false"], "", Duration::from_secs(5))
        .await;

    assert!(!output.unavailable);
    assert_eq!(output.exit_code, Some(1));
}

#[tokio::test]
async fn process_runner_detects_timeout_as_unavailable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runner = ProcessRunner;
    // `sleep 10` with a 1-second timeout should time out.
    let output = runner
        .run(tmp.path(), &["sleep", "10"], "", Duration::from_secs(1))
        .await;

    assert!(
        output.unavailable,
        "timeout should be classified as unavailable, got: {output:?}"
    );
    assert!(output.exit_code.is_none());
}

#[tokio::test]
async fn process_runner_resolves_relative_cwd_under_repo_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Create a subdirectory to use as cwd.
    let sub = tmp.path().join("subdir");
    std::fs::create_dir_all(&sub).expect("create subdir");

    let runner = ProcessRunner;
    let output = runner
        .run(tmp.path(), &["pwd"], "subdir", Duration::from_secs(5))
        .await;

    assert!(!output.unavailable);
    assert_eq!(output.exit_code, Some(0));
    // pwd should output the resolved subdirectory path.
    assert!(
        output.stdout.contains("subdir"),
        "pwd should resolve relative cwd, got: {}",
        output.stdout
    );
}

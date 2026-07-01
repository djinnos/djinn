//! Coordinator-side CI reproduction preflight executor.
//!
//! Consumes a [`CiFailureContextBundle`] produced by the provider crate
//! (`djinn-provider::github_api`) and produces structured
//! [`CiPreflightResult`] outcomes: `passed`, `reproduced_failure`, or
//! `unreproducible`.
//!
//! The executor has **no** hardcoded registry of repo- or language-specific
//! commands. It runs exactly the commands present in each bundle.
//!
//! ## Reproducibility classification
//!
//! | Outcome | Meaning |
//! |---------|---------|
//! | [`CiPreflightOutcome::Passed`] | Failing step command exited 0 (check now passes locally) |
//! | [`CiPreflightOutcome::ReproducedFailure`] | Failing step command exited non-zero (same failure reproduced) |
//! | [`CiPreflightOutcome::Unreproducible`] | Bundle cannot be safely reproduced locally |
//!
//! A bundle is considered safely reproducible only when:
//! - The `step_script` is a non-empty shell command
//! - All preceding `setup_steps` are ordinary local shell steps (have a
//!   `command` field and no `uses:` marketplace action reference)
//!
//! Unreproducible conditions are first-class results and are **never**
//! reported as passing.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use djinn_provider::github_api::CiFailureContextBundle;

/// Default timeout for a single reproduction command invocation (5 minutes).
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum log tail captured from reproduction output (8 KiB).
const MAX_LOG_TAIL_BYTES: usize = 8 * 1024;

// ─── Public result types ────────────────────────────────────────────────────

/// Structured result of a CI reproduction preflight attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiPreflightResult {
    /// The required check name from the bundle.
    pub check_name: String,
    /// The outcome classification.
    pub outcome: CiPreflightOutcome,
    /// The head SHA observed when the bundle was built.
    pub observed_head_sha: String,
}

/// The three-way classification of a CI reproduction preflight.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CiPreflightOutcome {
    /// The check's repo-derived command exited 0 — it passes locally.
    Passed,
    /// The check's repo-derived command exited non-zero — the failure is
    /// reproduced locally with captured details.
    ReproducedFailure {
        /// The shell command that was executed.
        command: String,
        /// Process exit code.
        exit_code: i32,
        /// Bounded first relevant failure output (combined stdout + stderr).
        output: String,
    },
    /// The check could not be reproduced locally. This is a blocking result,
    /// not a pass.
    Unreproducible {
        /// Why the check is unreproducible.
        reason: CiPreflightUnreproducibleReason,
        /// Optional human-readable detail.
        #[serde(default)]
        details: Option<String>,
    },
}

/// Typed reason why a required check cannot be locally reproduced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CiPreflightUnreproducibleReason {
    /// The `step_script` field is empty or whitespace-only.
    EmptyCommand,
    /// A setup step uses a marketplace action (`uses:`) which cannot be
    /// executed locally without the full Actions runtime.
    MarketplaceActionInSetup,
    /// A preceding setup step exited non-zero before the main command ran.
    SetupStepFailed,
    /// A command (setup step or main) could not be spawned — e.g. binary
    /// not available in the execution environment.
    CommandSpawnFailed,
    /// The command exceeded the configured timeout.
    Timeout,
}

// ─── Runner trait ───────────────────────────────────────────────────────────

/// Outcome of a single shell invocation by the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Abstraction over command execution for testability. The default production
/// implementation uses `sh -c` in the project worktree; tests supply a
/// [`FakeRunner`].
#[async_trait]
pub trait CiReproductionRunner: Send + Sync {
    /// Run a shell command in `workdir` with the given timeout. Returns
    /// `Ok(output)` on process completion, `Err` when the process cannot be
    /// spawned or times out.
    async fn run(
        &self,
        command: &str,
        workdir: &Path,
        timeout: Duration,
    ) -> Result<RunnerOutput, std::io::Error>;
}

/// Default production runner — spawns `sh -c` with piped stdout/stderr.
pub struct ShellRunner;

#[async_trait]
impl CiReproductionRunner for ShellRunner {
    async fn run(
        &self,
        command: &str,
        workdir: &Path,
        timeout: Duration,
    ) -> Result<RunnerOutput, std::io::Error> {
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out")
            })??;

        Ok(RunnerOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Attempt to locally reproduce a CI check from its context bundle.
///
/// 1. If the bundle's `step_script` is empty/whitespace, returns
///    `Unreproducible(EmptyCommand)`.
/// 2. If any setup step uses a marketplace action (`uses:` field), returns
///    `Unreproducible(MarketplaceActionInSetup)`.
/// 3. Runs setup steps sequentially — stopping at the first non-zero exit.
/// 4. Runs the main `step_script` and classifies the exit code.
///
/// Only repo-derived commands from the bundle are executed. There is no
/// registry of language- or repo-specific gates.
pub async fn preflight_reproduce(
    bundle: &CiFailureContextBundle,
    workdir: &Path,
    runner: &dyn CiReproductionRunner,
) -> CiPreflightResult {
    preflight_reproduce_with_timeout(bundle, workdir, runner, DEFAULT_COMMAND_TIMEOUT).await
}

/// Internal variant that accepts an explicit timeout (for testing).
async fn preflight_reproduce_with_timeout(
    bundle: &CiFailureContextBundle,
    workdir: &Path,
    runner: &dyn CiReproductionRunner,
    timeout: Duration,
) -> CiPreflightResult {
    let check_name = &bundle.required_check_name;
    let observed_head_sha = &bundle.observed_head_sha;

    // ── Validate the main command ───────────────────────────────────────
    if bundle.step_script.trim().is_empty() {
        return CiPreflightResult {
            check_name: check_name.clone(),
            observed_head_sha: observed_head_sha.clone(),
            outcome: CiPreflightOutcome::Unreproducible {
                reason: CiPreflightUnreproducibleReason::EmptyCommand,
                details: None,
            },
        };
    }

    // ── Validate setup steps: reject marketplace actions ────────────────
    for step in &bundle.setup_steps {
        if step.uses.is_some() {
            return CiPreflightResult {
                check_name: check_name.clone(),
                observed_head_sha: observed_head_sha.clone(),
                outcome: CiPreflightOutcome::Unreproducible {
                    reason: CiPreflightUnreproducibleReason::MarketplaceActionInSetup,
                    details: Some(format!(
                        "setup step '{}' uses marketplace action '{}'",
                        step.name,
                        step.uses.as_deref().unwrap_or(""),
                    )),
                },
            };
        }
    }

    // ── Execute setup steps ─────────────────────────────────────────────
    for step in &bundle.setup_steps {
        let cmd = match &step.command {
            Some(c) if !c.trim().is_empty() => c,
            _ => continue, // skip empty/no-command steps
        };
        match runner.run(cmd, workdir, timeout).await {
            Ok(output) if output.exit_code == 0 => { /* continue */ }
            Ok(output) => {
                let tail = tail_bytes(&output.stderr, MAX_LOG_TAIL_BYTES);
                return CiPreflightResult {
                    check_name: check_name.clone(),
                    observed_head_sha: observed_head_sha.clone(),
                    outcome: CiPreflightOutcome::Unreproducible {
                        reason: CiPreflightUnreproducibleReason::SetupStepFailed,
                        details: Some(format!(
                            "setup step '{}' exited with code {}: {}",
                            step.name, output.exit_code, tail,
                        )),
                    },
                };
            }
            Err(e) => {
                let reason = if e.kind() == std::io::ErrorKind::TimedOut {
                    CiPreflightUnreproducibleReason::Timeout
                } else {
                    CiPreflightUnreproducibleReason::CommandSpawnFailed
                };
                return CiPreflightResult {
                    check_name: check_name.clone(),
                    observed_head_sha: observed_head_sha.clone(),
                    outcome: CiPreflightOutcome::Unreproducible {
                        reason,
                        details: Some(format!("setup step '{}' could not run: {}", step.name, e,)),
                    },
                };
            }
        }
    }

    // ── Execute the main failing-step command ───────────────────────────
    match runner.run(&bundle.step_script, workdir, timeout).await {
        Ok(output) if output.exit_code == 0 => CiPreflightResult {
            check_name: check_name.clone(),
            observed_head_sha: observed_head_sha.clone(),
            outcome: CiPreflightOutcome::Passed,
        },
        Ok(output) => {
            // Combine stdout and stderr for failure output.
            let mut combined = output.stdout;
            if !output.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&output.stderr);
            }
            let output_tail = tail_bytes(&combined, MAX_LOG_TAIL_BYTES);
            CiPreflightResult {
                check_name: check_name.clone(),
                observed_head_sha: observed_head_sha.clone(),
                outcome: CiPreflightOutcome::ReproducedFailure {
                    command: bundle.step_script.clone(),
                    exit_code: output.exit_code,
                    output: output_tail,
                },
            }
        }
        Err(e) => {
            let reason = if e.kind() == std::io::ErrorKind::TimedOut {
                CiPreflightUnreproducibleReason::Timeout
            } else {
                CiPreflightUnreproducibleReason::CommandSpawnFailed
            };
            CiPreflightResult {
                check_name: check_name.clone(),
                observed_head_sha: observed_head_sha.clone(),
                outcome: CiPreflightOutcome::Unreproducible {
                    reason,
                    details: Some(format!("command could not run: {}", e)),
                },
            }
        }
    }
}

// ─── Utility ────────────────────────────────────────────────────────────────

/// Take the last `max_bytes` of a string, aligned to UTF-8 char boundaries.
fn tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let start = s.len() - max_bytes;
    let start = if let Some((i, _)) = s[start..].char_indices().next() {
        start + i
    } else {
        return String::new();
    };
    s[start..].to_owned()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use djinn_provider::github_api::CiSetupStep;

    // ── Fake runner for deterministic tests ─────────────────────────────

    /// A fake runner that returns canned responses keyed by exact command
    /// string. Falls back to a default exit code.
    struct FakeRunner {
        /// Maps exact command → (exit_code, stdout, stderr).
        responses: std::collections::HashMap<String, (i32, String, String)>,
        /// Default response for commands not in the map.
        default_exit: i32,
        default_stdout: String,
        default_stderr: String,
    }

    impl FakeRunner {
        fn new(default_exit: i32) -> Self {
            Self {
                responses: std::collections::HashMap::new(),
                default_exit,
                default_stdout: String::new(),
                default_stderr: String::new(),
            }
        }

        fn with_response(
            mut self,
            command: &str,
            exit_code: i32,
            stdout: &str,
            stderr: &str,
        ) -> Self {
            self.responses.insert(
                command.to_string(),
                (exit_code, stdout.to_string(), stderr.to_string()),
            );
            self
        }

        /// Returns a runner where all commands succeed (exit 0).
        fn all_pass() -> Self {
            Self::new(0)
        }

        /// Returns a runner where all commands fail (exit 1).
        #[allow(dead_code)]
        fn all_fail() -> Self {
            Self::new(1)
        }
    }

    #[async_trait]
    impl CiReproductionRunner for FakeRunner {
        async fn run(
            &self,
            command: &str,
            _workdir: &Path,
            _timeout: Duration,
        ) -> Result<RunnerOutput, std::io::Error> {
            match self.responses.get(command) {
                Some(&(exit_code, ref stdout, ref stderr)) => Ok(RunnerOutput {
                    exit_code,
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                }),
                None => Ok(RunnerOutput {
                    exit_code: self.default_exit,
                    stdout: self.default_stdout.clone(),
                    stderr: self.default_stderr.clone(),
                }),
            }
        }
    }

    /// A runner that always returns a spawn error.
    struct FailingSpawnRunner;

    #[async_trait]
    impl CiReproductionRunner for FailingSpawnRunner {
        async fn run(
            &self,
            _command: &str,
            _workdir: &Path,
            _timeout: Duration,
        ) -> Result<RunnerOutput, std::io::Error> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "binary not found",
            ))
        }
    }

    /// A runner that always returns a timeout error.
    struct TimeoutRunner;

    #[async_trait]
    impl CiReproductionRunner for TimeoutRunner {
        async fn run(
            &self,
            _command: &str,
            _workdir: &Path,
            _timeout: Duration,
        ) -> Result<RunnerOutput, std::io::Error> {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "command timed out",
            ))
        }
    }

    // ── Test helpers ────────────────────────────────────────────────────

    fn test_workdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ci_repro_test_")
            .tempdir()
            .expect("test tempdir")
    }

    fn make_bundle(step_script: &str) -> CiFailureContextBundle {
        CiFailureContextBundle {
            owner: "test-org".into(),
            repo: "test-repo".into(),
            required_check_name: "Quality Gate".into(),
            workflow_run_id: 1000,
            workflow_id: Some(100),
            workflow_name: Some("CI".into()),
            workflow_path: Some(".github/workflows/ci.yml".into()),
            job_id: 2000,
            job_name: "lint".into(),
            failing_step_name: "Run checks".into(),
            failing_step_number: 5,
            step_script: step_script.into(),
            setup_steps: vec![],
            log_tail: "original CI log tail".into(),
            observed_head_sha: "abc123def".into(),
        }
    }

    fn make_bundle_with_setup(
        step_script: &str,
        setup_steps: Vec<CiSetupStep>,
    ) -> CiFailureContextBundle {
        let mut bundle = make_bundle(step_script);
        bundle.setup_steps = setup_steps;
        bundle
    }

    // ── Passed ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn passed_when_command_exits_zero() {
        let dir = test_workdir();
        let bundle = make_bundle("cargo test");
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        assert_eq!(result.check_name, "Quality Gate");
        assert_eq!(result.observed_head_sha, "abc123def");
        assert_eq!(result.outcome, CiPreflightOutcome::Passed);
    }

    #[tokio::test]
    async fn passed_with_successful_setup_steps() {
        let dir = test_workdir();
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![
                CiSetupStep {
                    name: "Install deps".into(),
                    command: Some("cargo fetch".into()),
                    uses: None,
                },
                CiSetupStep {
                    name: "Generate code".into(),
                    command: Some("cargo build --build-plan".into()),
                    uses: None,
                },
            ],
        );
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;
        assert_eq!(result.outcome, CiPreflightOutcome::Passed);
    }

    // ── Reproduced failure ──────────────────────────────────────────────

    #[tokio::test]
    async fn reproduced_failure_with_exit_code_and_output() {
        let dir = test_workdir();
        let bundle = make_bundle("cargo clippy -- -D warnings");
        let runner = FakeRunner::new(1).with_response(
            "cargo clippy -- -D warnings",
            1,
            "warning: unused variable `x`",
            "error: aborting due to previous error",
        );

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::ReproducedFailure {
                command,
                exit_code,
                output,
            } => {
                assert_eq!(command, "cargo clippy -- -D warnings");
                assert_eq!(*exit_code, 1);
                assert!(output.contains("unused variable"));
                assert!(output.contains("--- stderr ---"));
                assert!(output.contains("aborting due to previous error"));
            }
            other => panic!("expected ReproducedFailure, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn reproduced_failure_captures_stdout_and_stderr() {
        let dir = test_workdir();
        let bundle = make_bundle("run-check");
        let runner = FakeRunner::new(42).with_response(
            "run-check",
            42,
            "step 1 ok\nstep 2 ok\nstep 3 FAILED",
            "assertion failed at line 99",
        );

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::ReproducedFailure {
                exit_code, output, ..
            } => {
                assert_eq!(*exit_code, 42);
                assert!(output.contains("step 3 FAILED"));
                assert!(output.contains("assertion failed at line 99"));
                assert!(output.contains("--- stderr ---"));
            }
            other => panic!("expected ReproducedFailure, got {:?}", other),
        }
    }

    // ── Unreproducible: empty command ───────────────────────────────────

    #[tokio::test]
    async fn unreproducible_empty_command() {
        let dir = test_workdir();
        let bundle = make_bundle("");
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, .. } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::EmptyCommand);
            }
            other => panic!("expected Unreproducible(EmptyCommand), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn unreproducible_whitespace_only_command() {
        let dir = test_workdir();
        let bundle = make_bundle("   \t\n  ");
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, .. } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::EmptyCommand);
            }
            other => panic!("expected Unreproducible(EmptyCommand), got {:?}", other),
        }
    }

    // ── Unreproducible: marketplace action in setup ─────────────────────

    #[tokio::test]
    async fn unreproducible_marketplace_action_in_setup() {
        let dir = test_workdir();
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![CiSetupStep {
                name: "Setup Rust".into(),
                command: None,
                uses: Some("actions-rs/toolchain@v1".into()),
            }],
        );
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, details } => {
                assert_eq!(
                    *reason,
                    CiPreflightUnreproducibleReason::MarketplaceActionInSetup
                );
                let d = details.as_ref().expect("should have details");
                assert!(d.contains("actions-rs/toolchain@v1"));
                assert!(d.contains("Setup Rust"));
            }
            other => {
                panic!(
                    "expected Unreproducible(MarketplaceActionInSetup), got {:?}",
                    other
                )
            }
        }
    }

    #[tokio::test]
    async fn marketplace_action_detected_even_with_command_present() {
        let dir = test_workdir();
        // If both `command` and `uses` are set, `uses` makes it unreproducible.
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![CiSetupStep {
                name: "Hybrid step".into(),
                command: Some("echo setup".into()),
                uses: Some("actions/checkout@v4".into()),
            }],
        );
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, .. } => {
                assert_eq!(
                    *reason,
                    CiPreflightUnreproducibleReason::MarketplaceActionInSetup
                );
            }
            other => panic!(
                "expected Unreproducible(MarketplaceActionInSetup), got {:?}",
                other
            ),
        }
    }

    // ── Unreproducible: setup step failure ──────────────────────────────

    #[tokio::test]
    async fn unreproducible_setup_step_fails() {
        let dir = test_workdir();
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![CiSetupStep {
                name: "Install deps".into(),
                command: Some("cargo fetch".into()),
                uses: None,
            }],
        );
        let runner = FakeRunner::new(0).with_response(
            "cargo fetch",
            1,
            "",
            "error: failed to download crate",
        );

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, details } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::SetupStepFailed);
                let d = details.as_ref().expect("should have details");
                assert!(d.contains("Install deps"));
                assert!(d.contains("exited with code 1"));
                assert!(d.contains("failed to download crate"));
            }
            other => panic!("expected Unreproducible(SetupStepFailed), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn setup_step_stops_at_first_failure() {
        let dir = test_workdir();
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![
                CiSetupStep {
                    name: "Step A".into(),
                    command: Some("step-a".into()),
                    uses: None,
                },
                CiSetupStep {
                    name: "Step B (never runs)".into(),
                    command: Some("step-b".into()),
                    uses: None,
                },
            ],
        );
        // Step A fails, Step B never runs.
        let runner = FakeRunner::new(0)
            .with_response("step-a", 1, "", "step-a failed")
            .with_response("step-b", 0, "ok", "");

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, details } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::SetupStepFailed);
                let d = details.as_ref().expect("should have details");
                assert!(d.contains("Step A"));
            }
            other => panic!("expected Unreproducible(SetupStepFailed), got {:?}", other),
        }
    }

    // ── Unreproducible: spawn failure ───────────────────────────────────

    #[tokio::test]
    async fn unreproducible_command_spawn_failure() {
        let dir = test_workdir();
        let bundle = make_bundle("some-command");
        let runner = FailingSpawnRunner;

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, details } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::CommandSpawnFailed);
                assert!(details.as_ref().unwrap().contains("binary not found"));
            }
            other => panic!(
                "expected Unreproducible(CommandSpawnFailed), got {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn unreproducible_setup_step_spawn_failure() {
        let dir = test_workdir();
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![CiSetupStep {
                name: "Setup".into(),
                command: Some("missing-tool".into()),
                uses: None,
            }],
        );
        let runner = FailingSpawnRunner;

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, details } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::CommandSpawnFailed);
                let d = details.as_ref().unwrap();
                assert!(d.contains("Setup"));
            }
            other => panic!(
                "expected Unreproducible(CommandSpawnFailed), got {:?}",
                other
            ),
        }
    }

    // ── Unreproducible: timeout ─────────────────────────────────────────

    #[tokio::test]
    async fn unreproducible_command_timeout() {
        let dir = test_workdir();
        let bundle = make_bundle("slow-command");
        let runner = TimeoutRunner;

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::Unreproducible { reason, details } => {
                assert_eq!(*reason, CiPreflightUnreproducibleReason::Timeout);
                assert!(details.as_ref().unwrap().contains("timed out"));
            }
            other => panic!("expected Unreproducible(Timeout), got {:?}", other),
        }
    }

    // ── Empty/no-command setup steps are skipped ────────────────────────

    #[tokio::test]
    async fn empty_setup_steps_are_skipped() {
        let dir = test_workdir();
        let bundle = make_bundle_with_setup(
            "cargo test",
            vec![
                CiSetupStep {
                    name: "Empty step".into(),
                    command: None,
                    uses: None,
                },
                CiSetupStep {
                    name: "Whitespace step".into(),
                    command: Some("   ".into()),
                    uses: None,
                },
                CiSetupStep {
                    name: "Real step".into(),
                    command: Some("echo setup".into()),
                    uses: None,
                },
            ],
        );
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;
        assert_eq!(result.outcome, CiPreflightOutcome::Passed);
    }

    // ── Missing binary is a reproduced failure (not unreproducible) ─────

    #[tokio::test]
    async fn missing_binary_via_shell_is_reproduced_failure() {
        let dir = test_workdir();
        // The FakeRunner with exit 127 simulates a "not found" from sh -c.
        let bundle = make_bundle("nonexistent_tool --version");
        let runner = FakeRunner::new(127).with_response(
            "nonexistent_tool --version",
            127,
            "",
            "sh: nonexistent_tool: not found",
        );

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;

        match &result.outcome {
            CiPreflightOutcome::ReproducedFailure {
                exit_code, output, ..
            } => {
                assert_eq!(*exit_code, 127);
                assert!(output.contains("not found"));
            }
            other => panic!("expected ReproducedFailure(127), got {:?}", other),
        }
    }

    // ── Serialization round-trip ────────────────────────────────────────

    #[tokio::test]
    async fn result_serialization_round_trip() {
        let dir = test_workdir();
        let bundle = make_bundle("exit 7");
        let runner = FakeRunner::new(7).with_response("exit 7", 7, "failed", "");

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;
        let json = serde_json::to_string(&result).expect("serialize");
        let deserialized: CiPreflightResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[tokio::test]
    async fn unreproducible_serialization_round_trip() {
        let dir = test_workdir();
        let bundle = make_bundle("");
        let runner = FakeRunner::all_pass();

        let result = preflight_reproduce(&bundle, dir.path(), &runner).await;
        let json = serde_json::to_string(&result).expect("serialize");
        let deserialized: CiPreflightResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    // ── Unreproducible is never reported as passing ─────────────────────

    #[tokio::test]
    async fn unreproducible_is_never_passed() {
        let dir = test_workdir();

        // Test all unreproducible reason paths and verify none are `Passed`.
        let cases: Vec<(CiFailureContextBundle, &str)> = vec![
            (make_bundle(""), "empty command"),
            (
                make_bundle_with_setup(
                    "test",
                    vec![CiSetupStep {
                        name: "a".into(),
                        command: None,
                        uses: Some("actions/checkout@v4".into()),
                    }],
                ),
                "marketplace action",
            ),
            (
                make_bundle_with_setup(
                    "test",
                    vec![CiSetupStep {
                        name: "a".into(),
                        command: Some("fail-cmd".into()),
                        uses: None,
                    }],
                ),
                "setup failure",
            ),
        ];

        // Test setup failure case with a runner that fails setup commands.
        let setup_fail_runner = FakeRunner::new(0).with_response("fail-cmd", 1, "", "failed");

        for (bundle, label) in &cases[..2] {
            let runner = FakeRunner::all_pass();
            let result = preflight_reproduce(bundle, dir.path(), &runner).await;
            assert!(
                !matches!(result.outcome, CiPreflightOutcome::Passed),
                "case '{}': unreproducible must not be Passed",
                label,
            );
            assert!(
                matches!(result.outcome, CiPreflightOutcome::Unreproducible { .. }),
                "case '{}': should be Unreproducible",
                label,
            );
        }

        // Setup failure case needs the failing runner.
        let result = preflight_reproduce(&cases[2].0, dir.path(), &setup_fail_runner).await;
        assert!(
            !matches!(result.outcome, CiPreflightOutcome::Passed),
            "case 'setup failure': unreproducible must not be Passed",
        );
        assert!(
            matches!(result.outcome, CiPreflightOutcome::Unreproducible { .. }),
            "case 'setup failure': should be Unreproducible",
        );
    }

    // ── No hardcoded repo/language commands ─────────────────────────────

    #[test]
    fn no_hardcoded_language_or_repo_commands() {
        // Verify the production source (before `#[cfg(test)]`) does not
        // contain hardcoded repo- or language-specific gate commands.
        // The executor must only run commands from the bundle.
        let full_source = include_str!("ci_reproduction.rs");
        // Strip the test module to avoid false positives from test fixtures.
        let production_source = full_source
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(full_source);

        let forbidden = [
            "scripts/check-file-size.sh",
            "cargo fmt",
            "cargo clippy",
            "npm run lint",
            "npm test",
            "pytest",
            "make test",
            "go test",
            "go vet",
            "golangci-lint",
        ];
        for term in &forbidden {
            assert!(
                !production_source.contains(term),
                "production source contains hardcoded command '{}'; the executor must only run bundle-derived commands",
                term,
            );
        }
    }
}

//! Coordinator-side local-gate primitives for repo-derived CI reproduction.
//!
//! Each primitive consumes a [`RequiredCheckReproduction`] bundle produced by
//! the provider crate (`djinn-provider::github_api`) and executes only the
//! bundle's own command + setup steps in the project's working directory.
//! The module has **no** registry of repo- or language-specific commands; it
//! runs exactly the commands present in each bundle.
//!
//! ## Result classification
//!
//! | Outcome | Meaning |
//! |---------|---------|
//! | [`LocalGateResult::ReproducedPass`] | Command exited 0 |
//! | [`LocalGateResult::ReproducedFailure`] | Command exited non-zero |
//! | [`LocalGateResult::Unreproducible`] | Bundle was empty, setup failed, spawn failed, timed out, or the provider bundle was itself unreproducible |
//!
//! A `ReproducedFailure` **blocks** submit/approval.
//! An `Unreproducible` is routed to lead/human intervention and is never
//! reported as passing.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use djinn_provider::github_api::{RequiredCheckReproduction, RequiredCheckReproductionContext};

/// Default timeout for a single reproduction command invocation (5 minutes).
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum log tail captured from reproduction output (8 KiB).
const MAX_LOG_TAIL_BYTES: usize = 8 * 1024;

// ─── Public result types ────────────────────────────────────────────────────

/// Structured result of attempting to locally reproduce a CI check from a
/// provider bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum LocalGateResult {
    /// The command exited successfully (exit code 0).
    ReproducedPass(LocalGateOutcome),
    /// The command exited with a non-zero exit code.
    ReproducedFailure(LocalGateOutcome),
    /// The check could not be reproduced locally.
    Unreproducible(LocalGateUnreproducible),
}

/// Detail for a reproduced check (shared by pass and failure outcomes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalGateOutcome {
    /// The required check name from the provider bundle.
    pub required_check_name: String,
    /// The shell command that was executed.
    pub command: String,
    /// Process exit code (0 for pass, non-zero for failure).
    pub exit_code: i32,
    /// Relevant log tail — stdout for pass, combined stdout+stderr for failure.
    pub log_tail: String,
    /// The head SHA observed by the provider when building the bundle.
    pub observed_head_sha: String,
}

/// A check that could not be reproduced locally, with a typed reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalGateUnreproducible {
    pub required_check_name: String,
    pub observed_head_sha: String,
    pub reason: LocalGateUnreproducibleReason,
    /// Human-readable detail about why reproduction was not possible.
    #[serde(default)]
    pub details: Option<String>,
}

/// Typed reasons for an unreproducible local-gate result.
///
/// The first variant captures provider-side reasons (the bundle was itself
/// unreproducible). The remaining variants cover coordinator-side failures
/// encountered while executing the bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalGateUnreproducibleReason {
    /// The provider bundle was itself an [`RequiredCheckReproduction::Unreproducible`].
    ProviderUnreproducible,
    /// The `command` field in the bundle was empty or whitespace-only.
    EmptyCommand,
    /// A setup step exited with a non-zero code before the main command ran.
    SetupStepFailed,
    /// The command (or a setup step) process could not be spawned — e.g. the
    /// binary is not installed in the container image.
    CommandSpawnFailed,
    /// The command exceeded the configured timeout.
    Timeout,
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Attempt to locally reproduce one or more CI checks from their provider
/// bundles. Each bundle is executed independently in `workdir`.
///
/// For each bundle the function:
/// 1. Maps provider-level `Unreproducible` bundles to
///    [`LocalGateResult::Unreproducible`] without execution.
/// 2. Rejects empty/whitespace-only commands as `Unreproducible(EmptyCommand)`.
/// 3. Runs the bundle's `setup_steps` sequentially — stopping at the first
///    non-zero exit — then runs the `command`.
/// 4. Classifies the command exit code as pass (0) or failure (non-zero).
///
/// The primitive has **no** registry of repo- or language-specific commands;
/// it executes exactly the commands present in each bundle.
pub async fn reproduce_ci_checks(
    bundles: &[RequiredCheckReproduction],
    workdir: &Path,
) -> Vec<LocalGateResult> {
    let mut results = Vec::with_capacity(bundles.len());
    for bundle in bundles {
        results.push(reproduce_single(bundle, workdir).await);
    }
    results
}

/// Reproduce a single CI check from its provider bundle.
pub async fn reproduce_single(
    bundle: &RequiredCheckReproduction,
    workdir: &Path,
) -> LocalGateResult {
    let ctx = match bundle {
        RequiredCheckReproduction::Reproducible(ctx) => ctx,
        RequiredCheckReproduction::Unreproducible(u) => {
            return LocalGateResult::Unreproducible(LocalGateUnreproducible {
                required_check_name: u.required_check_name.clone(),
                observed_head_sha: u.observed_head_sha.clone(),
                reason: LocalGateUnreproducibleReason::ProviderUnreproducible,
                details: Some(format!("{:?}", u.reason)),
            });
        }
    };
    reproduce_from_context(ctx, workdir).await
}

// ─── Internal implementation ────────────────────────────────────────────────

async fn reproduce_from_context(
    ctx: &RequiredCheckReproductionContext,
    workdir: &Path,
) -> LocalGateResult {
    let required_check_name = &ctx.required_check_name;
    let observed_head_sha = &ctx.observed_head_sha;

    // Reject empty commands.
    if ctx.command.trim().is_empty() {
        return LocalGateResult::Unreproducible(LocalGateUnreproducible {
            required_check_name: required_check_name.clone(),
            observed_head_sha: observed_head_sha.clone(),
            reason: LocalGateUnreproducibleReason::EmptyCommand,
            details: None,
        });
    }

    // Run setup steps sequentially; stop at first failure.
    for step in &ctx.setup_steps {
        if step.command.trim().is_empty() {
            continue;
        }
        match run_shell_command(&step.command, workdir, DEFAULT_COMMAND_TIMEOUT).await {
            Ok(output) if output.exit_code == 0 => { /* continue to next step */ }
            Ok(output) => {
                let tail = tail_bytes(&output.stderr, MAX_LOG_TAIL_BYTES);
                return LocalGateResult::Unreproducible(LocalGateUnreproducible {
                    required_check_name: required_check_name.clone(),
                    observed_head_sha: observed_head_sha.clone(),
                    reason: LocalGateUnreproducibleReason::SetupStepFailed,
                    details: Some(format!(
                        "setup step '{}' (step {}) exited with code {}: {}",
                        step.name, step.number, output.exit_code, tail,
                    )),
                });
            }
            Err(e) => {
                return LocalGateResult::Unreproducible(LocalGateUnreproducible {
                    required_check_name: required_check_name.clone(),
                    observed_head_sha: observed_head_sha.clone(),
                    reason: LocalGateUnreproducibleReason::CommandSpawnFailed,
                    details: Some(format!(
                        "setup step '{}' (step {}) could not be spawned: {}",
                        step.name, step.number, e,
                    )),
                });
            }
        }
    }

    // Run the main failing command.
    match run_shell_command(&ctx.command, workdir, DEFAULT_COMMAND_TIMEOUT).await {
        Ok(output) if output.exit_code == 0 => {
            let log_tail = tail_bytes(&output.stdout, MAX_LOG_TAIL_BYTES);
            LocalGateResult::ReproducedPass(LocalGateOutcome {
                required_check_name: required_check_name.clone(),
                command: ctx.command.clone(),
                exit_code: output.exit_code,
                log_tail,
                observed_head_sha: observed_head_sha.clone(),
            })
        }
        Ok(output) => {
            // Combine stdout and stderr for the failure log tail.
            let mut combined = output.stdout;
            if !output.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&output.stderr);
            }
            let log_tail = tail_bytes(&combined, MAX_LOG_TAIL_BYTES);
            LocalGateResult::ReproducedFailure(LocalGateOutcome {
                required_check_name: required_check_name.clone(),
                command: ctx.command.clone(),
                exit_code: output.exit_code,
                log_tail,
                observed_head_sha: observed_head_sha.clone(),
            })
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::TimedOut {
                LocalGateResult::Unreproducible(LocalGateUnreproducible {
                    required_check_name: required_check_name.clone(),
                    observed_head_sha: observed_head_sha.clone(),
                    reason: LocalGateUnreproducibleReason::Timeout,
                    details: Some(format!("command '{}' timed out: {}", ctx.command, e)),
                })
            } else {
                LocalGateResult::Unreproducible(LocalGateUnreproducible {
                    required_check_name: required_check_name.clone(),
                    observed_head_sha: observed_head_sha.clone(),
                    reason: LocalGateUnreproducibleReason::CommandSpawnFailed,
                    details: Some(format!("command could not be spawned: {}", e)),
                })
            }
        }
    }
}

// ─── Shell execution helper ─────────────────────────────────────────────────

/// Minimal output from a shell invocation.
struct ShellOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Run a shell command in `workdir` with a timeout. Uses `sh -c` so that
/// pipelines, redirections, and compound commands from the CI bundle work
/// without modification.
async fn run_shell_command(
    command: &str,
    workdir: &Path,
    timeout: Duration,
) -> Result<ShellOutput, std::io::Error> {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"))??;

    Ok(ShellOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Take the last `max_bytes` of a string, aligned to UTF-8 char boundaries.
fn tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let start = s.len() - max_bytes;
    // Walk forward to the next valid UTF-8 char boundary.
    let start = if let Some((i, _)) = s[start..].char_indices().next() {
        start + i
    } else {
        // `start` is past the end — defensive; return empty.
        return String::new();
    };
    s[start..].to_owned()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use djinn_provider::github_api::{
        ReproductionJob, ReproductionSetupStep, ReproductionStep, RequiredCheckReproductionContext,
        RequiredCheckUnreproducible, RequiredCheckUnreproducibleReason,
    };

    fn test_workdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("local_gates_test_")
            .tempdir()
            .expect("test tempdir")
    }

    fn sample_context(command: &str) -> RequiredCheckReproductionContext {
        RequiredCheckReproductionContext {
            required_check_name: "ci/test".into(),
            observed_head_sha: "abc123".into(),
            check_run_id: 1,
            workflow_run_id: 42,
            workflow_name: Some("quality-gate.yml".into()),
            job: ReproductionJob {
                id: 100,
                name: "test".into(),
                html_url: "https://example.com/run/42/job/100".into(),
            },
            failing_step: ReproductionStep {
                number: 3,
                name: "Run tests".into(),
            },
            command: command.into(),
            setup_steps: vec![],
            log_tail: "original CI log tail".into(),
        }
    }

    fn sample_context_with_setup(
        command: &str,
        setup_steps: Vec<ReproductionSetupStep>,
    ) -> RequiredCheckReproductionContext {
        let mut ctx = sample_context(command);
        ctx.setup_steps = setup_steps;
        ctx
    }

    // ── Reproduced pass ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reproduced_pass_exiting_zero() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Reproducible(sample_context("true"));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;
        assert_eq!(results.len(), 1);

        match &results[0] {
            LocalGateResult::ReproducedPass(outcome) => {
                assert_eq!(outcome.required_check_name, "ci/test");
                assert_eq!(outcome.command, "true");
                assert_eq!(outcome.exit_code, 0);
                assert_eq!(outcome.observed_head_sha, "abc123");
            }
            other => panic!("expected ReproducedPass, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn reproduced_pass_echo_command() {
        let dir = test_workdir();
        let bundle =
            RequiredCheckReproduction::Reproducible(sample_context("echo 'all checks passed'"));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::ReproducedPass(outcome) => {
                assert!(outcome.log_tail.contains("all checks passed"));
                assert_eq!(outcome.exit_code, 0);
            }
            other => panic!("expected ReproducedPass, got {:?}", other),
        }
    }

    // ── Reproduced failure ──────────────────────────────────────────────

    #[tokio::test]
    async fn reproduced_failure_exiting_nonzero() {
        let dir = test_workdir();
        let bundle =
            RequiredCheckReproduction::Reproducible(sample_context("echo 'FAIL' >&2; exit 1"));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;
        assert_eq!(results.len(), 1);

        match &results[0] {
            LocalGateResult::ReproducedFailure(outcome) => {
                assert_eq!(outcome.required_check_name, "ci/test");
                assert_eq!(outcome.exit_code, 1);
                assert!(outcome.log_tail.contains("FAIL"));
                assert_eq!(outcome.observed_head_sha, "abc123");
            }
            other => panic!("expected ReproducedFailure, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn reproduced_failure_captures_stdout_and_stderr() {
        let dir = test_workdir();
        // Command writes to both stdout and stderr, then exits non-zero.
        let bundle = RequiredCheckReproduction::Reproducible(sample_context(
            "echo 'line1'; echo 'errline' >&2; exit 42",
        ));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::ReproducedFailure(outcome) => {
                assert_eq!(outcome.exit_code, 42);
                assert!(outcome.log_tail.contains("line1"));
                assert!(outcome.log_tail.contains("errline"));
                assert!(outcome.log_tail.contains("--- stderr ---"));
            }
            other => panic!("expected ReproducedFailure, got {:?}", other),
        }
    }

    // ── Unreproducible: provider-side ───────────────────────────────────

    #[tokio::test]
    async fn unreproducible_provider_bundle() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Unreproducible(RequiredCheckUnreproducible {
            required_check_name: "ci/missing".into(),
            observed_head_sha: "def456".into(),
            reason: RequiredCheckUnreproducibleReason::WorkflowRunNotFound,
            details: None,
        });
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::Unreproducible(u) => {
                assert_eq!(u.required_check_name, "ci/missing");
                assert_eq!(u.observed_head_sha, "def456");
                assert_eq!(
                    u.reason,
                    LocalGateUnreproducibleReason::ProviderUnreproducible
                );
                assert!(u.details.is_some());
            }
            other => panic!("expected Unreproducible, got {:?}", other),
        }
    }

    // ── Unreproducible: empty command ───────────────────────────────────

    #[tokio::test]
    async fn unreproducible_empty_command() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Reproducible(sample_context(""));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::Unreproducible(u) => {
                assert_eq!(u.reason, LocalGateUnreproducibleReason::EmptyCommand);
            }
            other => panic!("expected Unreproducible(EmptyCommand), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn unreproducible_whitespace_only_command() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Reproducible(sample_context("   "));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::Unreproducible(u) => {
                assert_eq!(u.reason, LocalGateUnreproducibleReason::EmptyCommand);
            }
            other => panic!("expected Unreproducible(EmptyCommand), got {:?}", other),
        }
    }

    // ── Unreproducible: setup step failure ──────────────────────────────

    #[tokio::test]
    async fn unreproducible_setup_step_fails() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Reproducible(sample_context_with_setup(
            "echo 'would run'",
            vec![ReproductionSetupStep {
                number: 1,
                name: "Install deps".into(),
                command: "echo 'install failed' >&2; exit 1".into(),
            }],
        ));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::Unreproducible(u) => {
                assert_eq!(u.reason, LocalGateUnreproducibleReason::SetupStepFailed);
                let d = u.details.as_ref().expect("details should be present");
                assert!(d.contains("Install deps"));
                assert!(d.contains("exited with code 1"));
            }
            other => panic!("expected Unreproducible(SetupStepFailed), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn empty_setup_steps_are_skipped() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Reproducible(sample_context_with_setup(
            "true",
            vec![
                ReproductionSetupStep {
                    number: 1,
                    name: "noop".into(),
                    command: "".into(), // empty — should be skipped
                },
                ReproductionSetupStep {
                    number: 2,
                    name: "real setup".into(),
                    command: "echo ok".into(),
                },
            ],
        ));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::ReproducedPass(outcome) => {
                assert_eq!(outcome.exit_code, 0);
            }
            other => panic!("expected ReproducedPass, got {:?}", other),
        }
    }

    // ── Missing binary produces reproduced failure, not unreproducible ─

    #[tokio::test]
    async fn missing_binary_is_reproduced_failure_127() {
        let dir = test_workdir();
        // A command that references a non-existent binary: `sh -c` spawns
        // successfully, the inner command gets exit code 127 from the shell.
        // This is a reproduced failure, NOT an unreproducible result — the
        // command was executable, it just failed.
        let bundle = RequiredCheckReproduction::Reproducible(sample_context(
            "nonexistent_binary_xyz_12345 --version",
        ));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::ReproducedFailure(outcome) => {
                assert_eq!(outcome.exit_code, 127);
                assert!(outcome.log_tail.contains("not found"));
            }
            other => panic!("expected ReproducedFailure(127), got {:?}", other),
        }
    }

    // ── Multi-bundle execution ──────────────────────────────────────────

    #[tokio::test]
    async fn multiple_bundles_classified_independently() {
        let dir = test_workdir();
        let bundles = vec![
            RequiredCheckReproduction::Reproducible(sample_context("true")),
            RequiredCheckReproduction::Reproducible(sample_context("exit 1")),
            RequiredCheckReproduction::Unreproducible(RequiredCheckUnreproducible {
                required_check_name: "ci/other".into(),
                observed_head_sha: "abc123".into(),
                reason: RequiredCheckUnreproducibleReason::CommandNotFound,
                details: Some("no command extracted".into()),
            }),
        ];
        let results = reproduce_ci_checks(&bundles, dir.path()).await;
        assert_eq!(results.len(), 3);

        assert!(
            matches!(results[0], LocalGateResult::ReproducedPass(_)),
            "first bundle should pass"
        );
        assert!(
            matches!(results[1], LocalGateResult::ReproducedFailure(_)),
            "second bundle should fail"
        );
        assert!(
            matches!(results[2], LocalGateResult::Unreproducible(ref u) if u.reason == LocalGateUnreproducibleReason::ProviderUnreproducible),
            "third bundle should be unreproducible"
        );
    }

    // ── Setup steps run before the main command ─────────────────────────

    #[tokio::test]
    async fn setup_steps_run_before_main_command() {
        let dir = test_workdir();
        // Create a file in a setup step, verify it exists in the main command.
        let file_path = dir.path().join("marker.txt");
        let setup_cmd = format!("touch {}", file_path.display());
        let verify_cmd = format!("test -f {}", file_path.display());

        let bundle = RequiredCheckReproduction::Reproducible(sample_context_with_setup(
            &verify_cmd,
            vec![ReproductionSetupStep {
                number: 1,
                name: "create marker".into(),
                command: setup_cmd,
            }],
        ));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        match &results[0] {
            LocalGateResult::ReproducedPass(outcome) => {
                assert_eq!(outcome.exit_code, 0);
            }
            other => panic!("expected ReproducedPass, got {:?}", other),
        }
    }

    // ── Serialization round-trip ────────────────────────────────────────

    #[tokio::test]
    async fn result_serialization_round_trip() {
        let dir = test_workdir();
        let bundle = RequiredCheckReproduction::Reproducible(sample_context("exit 7"));
        let results = reproduce_ci_checks(&[bundle], dir.path()).await;

        let json = serde_json::to_string(&results[0]).expect("serialize");
        let deserialized: LocalGateResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(results[0], deserialized);
    }
}

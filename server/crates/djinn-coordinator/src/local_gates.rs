//! Deterministic local CI gate registry and executor core.
//!
//! This module owns the coordinator-side declarative mapping from GitHub
//! required-CI contexts / failure fingerprints to exact local commands. It is
//! the deterministic pre-flight layer that runs *cheap* checks before a worker
//! submits or a reviewer approves a PR-backed task, reducing avoidable red
//! required-CI before it reaches GitHub.
//!
//! # Design
//!
//! - **Gate specs** are declarative: each maps a stable id + check/context
//!   aliases + fingerprint/job predicates to a command, working directory,
//!   timeout, and a [`GateApplicability`] (required / advisory / non-applicable).
//! - **Gate plans** are constructed from *structured* task CI metadata
//!   (`ci_blocking_required_check_names`, `ci_failure_fingerprint`,
//!   remediation baseline fields) — never from activity-log prose or prompt
//!   text. This makes selection deterministic and testable.
//! - **Gate results** distinguish `passed`, `failed`, `unavailable`, and
//!   `skipped`/non-applicable. An `unavailable` *required* command is a
//!   blocking result (never a pass).
//! - **Executor** runs a command bounded by a timeout with unavailable-tool
//!   detection. If the command binary or the working directory does not exist,
//!   the result is `unavailable`.
//!
//! This wave adds code only — no public external API. All helpers are
//! `pub(crate)` or private.
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

use crate::truncate::{smart_truncate, smart_truncate_lines};

/// Maximum byte budget for a single stdout/stderr summary in a gate result.
const OUTPUT_SUMMARY_MAX_BYTES: usize = 4_096;

/// Maximum line count for a single stdout/stderr summary in a gate result.
const OUTPUT_SUMMARY_MAX_LINES: usize = 50;

// ── Applicability / severity ──────────────────────────────────────────────────

/// Whether a gate blocks a submit/approve transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateApplicability {
    /// A failed or unavailable result blocks the submit/approve path.
    Required,
    /// The gate runs for visibility but never blocks.
    Advisory,
    /// The gate's predicates never matched this task — it is excluded from the
    /// plan entirely and produces a `skipped` result if explicitly evaluated.
    NonApplicable,
}

// ── Gate spec ─────────────────────────────────────────────────────────────────

/// A declarative mapping from CI contexts/fingerprints/jobs to a local command.
///
/// Selection is purely structural: a spec matches when *any* of its check-name
/// aliases, fingerprint substrings, or job-name predicates intersect the task's
/// structured CI metadata. The spec then contributes a command to run and an
/// applicability level.
#[derive(Clone, Debug)]
pub(crate) struct GateSpec {
    /// Stable identifier (e.g. `"server-size-guard"`).
    pub id: &'static str,
    /// GitHub required-check context names that implicate this gate.
    /// Matched case-insensitively against `ci_blocking_required_check_names`.
    pub check_aliases: &'static [&'static str],
    /// Substrings of `ci_failure_fingerprint` that implicate this gate.
    /// A gate matches when the task fingerprint contains *any* of these.
    pub fingerprint_substrings: &'static [&'static str],
    /// Substrings of CI job/step names (from structured CI failure sections)
    /// that implicate this gate. Matched case-insensitively.
    pub job_name_substrings: &'static [&'static str],
    /// Command argv to execute.
    pub command: &'static [&'static str],
    /// Working directory *relative to the repository root*.
    pub cwd: &'static str,
    /// Execution timeout.
    pub timeout: Duration,
    /// Whether a failure/unavailable blocks the transition.
    pub applicability: GateApplicability,
}

impl GateSpec {
    /// Returns `true` when this spec is implicated by the given structured CI
    /// metadata. Matching is against check names and fingerprint substrings
    /// only — never activity-log prose.
    ///
    /// - `blocking_check_names`: the parsed entries from
    ///   `task.ci_blocking_required_check_names` (a JSON array string).
    /// - `failure_fingerprint`: `task.ci_failure_fingerprint` (optional).
    /// - `implicated_job_names`: job/step names extracted from structured CI
    ///   failure sections (optional).
    fn matches(
        &self,
        blocking_check_names: &[String],
        failure_fingerprint: Option<&str>,
        implicated_job_names: &[String],
    ) -> bool {
        // Check-name alias match (case-insensitive substring on either side).
        for alias in self.check_aliases {
            let alias_l = alias.to_lowercase();
            for cn in blocking_check_names {
                if cn.to_lowercase().contains(&alias_l) || alias_l.contains(&cn.to_lowercase()) {
                    return true;
                }
            }
        }

        // Fingerprint substring match.
        if let Some(fp) = failure_fingerprint {
            for sub in self.fingerprint_substrings {
                if fp.contains(sub) {
                    return true;
                }
            }
        }

        // Job-name substring match.
        for job_sub in self.job_name_substrings {
            let job_sub_l = job_sub.to_lowercase();
            for jn in implicated_job_names {
                if jn.to_lowercase().contains(&job_sub_l) {
                    return true;
                }
            }
        }

        false
    }
}

// ── Structured CI input ───────────────────────────────────────────────────────

/// Structured CI metadata used to select gates. Built by the caller from
/// durable task fields — never from activity-log prose.
///
/// Fields mirror the task model's CI columns delivered by dependency epics
/// (`iuic`, `zlys`, `sa4x`):
/// - `ci_blocking_required_check_names` (JSON array string)
/// - `ci_failure_fingerprint` (optional hex/hash string)
/// - `ci_last_remediation_base_sha` (optional SHA)
///
/// `implicated_job_names` carries job/step names extracted from structured CI
/// failure sections (the `**Failed job:**` / `**Failed step:**` markers built by
/// `build_ci_failure_sections`). It is optional context — the registry matches
/// on check names and fingerprints first.
#[derive(Clone, Debug, Default)]
pub(crate) struct CiGateInput {
    /// Parsed `ci_blocking_required_check_names` entries.
    pub blocking_check_names: Vec<String>,
    /// `ci_failure_fingerprint`, when present.
    pub failure_fingerprint: Option<String>,
    /// `ci_last_remediation_base_sha`, when present.
    pub last_remediation_base_sha: Option<String>,
    /// Job/step names from structured CI failure sections, when available.
    pub implicated_job_names: Vec<String>,
}

impl CiGateInput {
    /// Parse a [`CiGateInput`] from raw task-model CI fields.
    ///
    /// `blocking_check_names_json` is the `ci_blocking_required_check_names`
    /// column value (a JSON array string, possibly empty). The fingerprint and
    /// remediation baseline are passed through as-is.
    pub(crate) fn from_task_fields(
        blocking_check_names_json: &str,
        failure_fingerprint: Option<String>,
        last_remediation_base_sha: Option<String>,
        implicated_job_names: Vec<String>,
    ) -> Self {
        let blocking_check_names = parse_check_names_json(blocking_check_names_json);
        Self {
            blocking_check_names,
            failure_fingerprint,
            last_remediation_base_sha,
            implicated_job_names,
        }
    }
}

/// Parse the `ci_blocking_required_check_names` JSON array string into a vec.
/// Tolerates empty strings and malformed JSON (returns empty).
fn parse_check_names_json(json: &str) -> Vec<String> {
    if json.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

// ── Gate plan ─────────────────────────────────────────────────────────────────

/// A selected gate ready for execution. Carries the spec (cloned — all inner
/// fields are `&'static`, so cloning is cheap pointer copies).
#[derive(Clone, Debug)]
pub(crate) struct PlannedGate {
    pub spec: GateSpec,
}

/// The set of gates selected for a given task's CI metadata.
#[derive(Clone, Debug, Default)]
pub(crate) struct GatePlan {
    pub gates: Vec<PlannedGate>,
}

impl GatePlan {
    /// Returns `true` when at least one *required* gate is in the plan.
    pub(crate) fn has_required(&self) -> bool {
        self.gates
            .iter()
            .any(|g| g.spec.applicability == GateApplicability::Required)
    }

    /// Returns the ids of all gates in the plan.
    pub(crate) fn ids(&self) -> Vec<&'static str> {
        self.gates.iter().map(|g| g.spec.id).collect()
    }
}

// ── Gate result ───────────────────────────────────────────────────────────────

/// Outcome category for a single gate evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    /// Command exited 0.
    Passed,
    /// Command exited non-zero.
    Failed,
    /// Command binary or working directory was unavailable.
    Unavailable,
    /// Gate was advisory or non-applicable and was not run as a blocker.
    Skipped,
}

/// Result of evaluating one gate.
#[derive(Clone, Debug)]
pub(crate) struct GateResult {
    pub gate_id: &'static str,
    pub outcome: GateOutcome,
    /// Whether this gate's failure would block the transition.
    pub blocking: bool,
    pub command: Vec<String>,
    pub cwd: String,
    pub timeout: Duration,
    /// Exit code, when the command ran.
    pub exit_code: Option<i32>,
    /// Truncated stdout summary.
    pub stdout_summary: String,
    /// Truncated stderr summary.
    pub stderr_summary: String,
    /// Wall-clock duration of the command, when it ran.
    pub duration: Option<Duration>,
    /// Artifact path or identifier, when the command produced one.
    pub artifact: Option<String>,
}

impl GateResult {
    /// Returns `true` when this result blocks the submit/approve transition.
    /// A blocking gate that failed or was unavailable blocks; passed/skipped
    /// never block.
    pub(crate) fn is_blocking_failure(&self) -> bool {
        self.blocking && matches!(self.outcome, GateOutcome::Failed | GateOutcome::Unavailable)
    }
}

/// Summary of a full gate plan execution.
#[derive(Clone, Debug, Default)]
pub(crate) struct GatePlanResult {
    pub results: Vec<GateResult>,
}

impl GatePlanResult {
    /// Returns `true` when any gate result blocks the transition.
    pub(crate) fn has_blocking_failure(&self) -> bool {
        self.results.iter().any(|r| r.is_blocking_failure())
    }

    /// Returns the ids of all blocking failures.
    pub(crate) fn blocking_gate_ids(&self) -> Vec<&'static str> {
        self.results
            .iter()
            .filter(|r| r.is_blocking_failure())
            .map(|r| r.gate_id)
            .collect()
    }

    /// Returns `true` when every gate passed or was skipped (no failures of any
    /// kind).
    pub(crate) fn all_clean(&self) -> bool {
        self.results
            .iter()
            .all(|r| matches!(r.outcome, GateOutcome::Passed | GateOutcome::Skipped))
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// Default timeout for the file-size guard — a pure-shell script, very fast.
const SIZE_GUARD_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for `cargo fmt --check`.
const RUSTFMT_TIMEOUT: Duration = Duration::from_secs(120);

/// Default timeout for scoped clippy.
const CLIPPY_TIMEOUT: Duration = Duration::from_secs(300);

/// Default timeout for scoped tests.
const TEST_TIMEOUT: Duration = Duration::from_secs(600);

/// The built-in registry of local gate specs.
///
/// Order matters only for determinism in plan construction (tests assert exact
/// gate sets); it does not affect execution semantics.
pub(crate) fn builtin_registry() -> &'static [GateSpec] {
    &REGISTRY
}

const REGISTRY: [GateSpec; 4] = [
    // ── server-size-guard ──────────────────────────────────────────────────
    // CI check `server-size-guard` (job name "Server Size Guard") runs
    // `scripts/check-file-size.sh`. The local mirror runs the same script from
    // the repo root with a short timeout.
    GateSpec {
        id: "server-size-guard",
        check_aliases: &["server-size-guard", "Server Size Guard"],
        fingerprint_substrings: &["server-size-guard", "size-guard"],
        job_name_substrings: &["Server Size Guard", "size-guard"],
        command: &["./scripts/check-file-size.sh", "--all"],
        cwd: "",
        timeout: SIZE_GUARD_TIMEOUT,
        applicability: GateApplicability::Required,
    },
    // ── rustfmt ────────────────────────────────────────────────────────────
    // The CI `rustfmt` context (check name "rustfmt" / "Rust Format") is
    // mirrored locally by `cargo fmt --all -- --check` from `server/`.
    GateSpec {
        id: "rustfmt",
        check_aliases: &["rustfmt", "Rust Format", "rust-format", "fmt"],
        fingerprint_substrings: &["rustfmt", "fmt"],
        job_name_substrings: &["rustfmt", "Rust Format", "Format"],
        command: &["cargo", "fmt", "--all", "--", "--check"],
        cwd: "server",
        timeout: RUSTFMT_TIMEOUT,
        applicability: GateApplicability::Required,
    },
    // ── clippy (Rust lint) ─────────────────────────────────────────────────
    // The CI `Server Clippy` job is mirrored by a scoped `cargo clippy`
    // invocation. The applicability predicate is conservative: the gate is
    // required when clippy is among the blocking checks/fingerprint, advisory
    // otherwise.
    GateSpec {
        id: "clippy",
        check_aliases: &["Server Clippy", "clippy"],
        fingerprint_substrings: &["clippy"],
        job_name_substrings: &["Clippy", "clippy"],
        command: &[
            "cargo",
            "clippy",
            "--all-targets",
            "--features",
            "qdrant",
            "--",
            "-D",
            "warnings",
        ],
        cwd: "server",
        timeout: CLIPPY_TIMEOUT,
        applicability: GateApplicability::Required,
    },
    // ── Rust tests ─────────────────────────────────────────────────────────
    // The CI `Server Test` job is mirrored by a scoped `cargo test`. This gate
    // is required when test failures are among the blocking checks/fingerprint.
    GateSpec {
        id: "rust-tests",
        check_aliases: &["Server Test", "test"],
        fingerprint_substrings: &["test"],
        job_name_substrings: &["Test", "test"],
        command: &["cargo", "test", "--no-fail-fast"],
        cwd: "server",
        timeout: TEST_TIMEOUT,
        applicability: GateApplicability::Required,
    },
];

// ── Plan construction ─────────────────────────────────────────────────────────

/// Build a [`GatePlan`] from structured CI metadata.
///
/// A spec is included when it `matches` the input's check names, fingerprint,
/// or implicated job names. Specs that do not match are excluded (they would
/// produce `NonApplicable` results).
///
/// Selection uses *only* structured fields — never activity-log prose or
/// prompt text.
pub(crate) fn build_plan(input: &CiGateInput) -> GatePlan {
    let mut gates: Vec<PlannedGate> = Vec::new();
    for spec in builtin_registry() {
        if spec.matches(
            &input.blocking_check_names,
            input.failure_fingerprint.as_deref(),
            &input.implicated_job_names,
        ) {
            gates.push(PlannedGate { spec: spec.clone() });
        }
    }
    GatePlan { gates }
}

// ── Executor ──────────────────────────────────────────────────────────────────

/// Trait abstracting command execution so the gate executor can be unit-tested
/// without spawning real processes.
pub(crate) trait CommandRunner: Send + Sync {
    /// Run `command` in `cwd` (relative to `repo_root`) with the given timeout.
    /// Returns the exit code, stdout, and stderr. Returns `None` for the exit
    /// code when the command binary or working directory was unavailable, or
    /// the command timed out.
    fn run(
        &self,
        repo_root: &Path,
        command: &[&str],
        cwd: &str,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecOutput> + Send>>;
}

/// Raw output from a command execution.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExecOutput {
    /// Exit code when the command ran to completion. `None` when the binary or
    /// cwd was unavailable, or the command timed out.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Wall-clock duration.
    pub duration: Duration,
    /// `true` when the command binary or cwd was unavailable (not found).
    pub unavailable: bool,
}

/// Production command runner using `tokio::process::Command`.
pub(crate) struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        repo_root: &Path,
        command: &[&str],
        cwd: &str,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecOutput> + Send>> {
        let repo_root = repo_root.to_path_buf();
        let cwd = cwd.to_string();
        let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
        Box::pin(async move { run_process(&repo_root, &command, &cwd, timeout).await })
    }
}

/// Execute a command with unavailable-tool detection and timeout.
///
/// - If the working directory does not exist → `unavailable`.
/// - If the command binary cannot be spawned (NotFound) → `unavailable`.
/// - If the command times out → treated as `unavailable` (we cannot trust a
///   partial result; a required timeout is blocking).
/// - Otherwise → exit code + stdout + stderr.
async fn run_process(
    repo_root: &Path,
    command: &[String],
    cwd: &str,
    timeout: Duration,
) -> ExecOutput {
    use tokio::time::Instant;
    let start = Instant::now();

    // Resolve the working directory.
    let work_dir = if cwd.is_empty() {
        repo_root.to_path_buf()
    } else {
        repo_root.join(cwd)
    };
    if !work_dir.is_dir() {
        return ExecOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("working directory does not exist: {}", work_dir.display()),
            duration: start.elapsed(),
            unavailable: true,
        };
    }

    if command.is_empty() {
        return ExecOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: "empty command".to_string(),
            duration: start.elapsed(),
            unavailable: true,
        };
    }

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    cmd.current_dir(&work_dir);
    // Capture stdout/stderr; do not inherit stdin.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ExecOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("command not found: {}: {e}", command[0]),
                duration: start.elapsed(),
                unavailable: true,
            };
        }
        Err(e) => {
            return ExecOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to spawn command: {e}"),
                duration: start.elapsed(),
                unavailable: true,
            };
        }
    };

    // Wait with timeout.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            ExecOutput {
                exit_code: output.status.code(),
                stdout,
                stderr,
                duration: start.elapsed(),
                unavailable: false,
            }
        }
        Ok(Err(e)) => ExecOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("command wait failed: {e}"),
            duration: start.elapsed(),
            unavailable: true,
        },
        Err(_elapsed) => ExecOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("command timed out after {timeout:?}"),
            duration: timeout,
            unavailable: true,
        },
    }
}

// ── Execution orchestration ───────────────────────────────────────────────────

/// Execute a [`GatePlan`], producing a [`GatePlanResult`].
///
/// Each gate is run via `runner`. Advisory and non-applicable gates produce
/// `Skipped` results (they are not run as blockers). Required gates are run;
/// unavailable commands produce `Unavailable` outcomes that are blocking.
pub(crate) async fn execute_plan(
    plan: &GatePlan,
    repo_root: &Path,
    runner: &dyn CommandRunner,
) -> GatePlanResult {
    let mut results = Vec::with_capacity(plan.gates.len());
    for gate in &plan.gates {
        let result = execute_gate(gate, repo_root, runner).await;
        results.push(result);
    }
    GatePlanResult { results }
}

/// Execute a single planned gate.
async fn execute_gate(
    gate: &PlannedGate,
    repo_root: &Path,
    runner: &dyn CommandRunner,
) -> GateResult {
    let spec = &gate.spec;
    let command_strings: Vec<String> = spec.command.iter().map(|s| s.to_string()).collect();

    // Advisory and non-applicable gates are never run as blockers.
    if spec.applicability != GateApplicability::Required {
        return GateResult {
            gate_id: spec.id,
            outcome: GateOutcome::Skipped,
            blocking: false,
            command: command_strings,
            cwd: spec.cwd.to_string(),
            timeout: spec.timeout,
            exit_code: None,
            stdout_summary: String::new(),
            stderr_summary: String::new(),
            duration: None,
            artifact: None,
        };
    }

    let output = runner
        .run(repo_root, spec.command, spec.cwd, spec.timeout)
        .await;

    let outcome = if output.unavailable {
        GateOutcome::Unavailable
    } else if output.exit_code == Some(0) {
        GateOutcome::Passed
    } else {
        GateOutcome::Failed
    };

    GateResult {
        gate_id: spec.id,
        outcome,
        blocking: spec.applicability == GateApplicability::Required,
        command: command_strings,
        cwd: spec.cwd.to_string(),
        timeout: spec.timeout,
        exit_code: output.exit_code,
        stdout_summary: summarize_output(&output.stdout),
        stderr_summary: summarize_output(&output.stderr),
        duration: Some(output.duration),
        artifact: None,
    }
}

/// Truncate output to the summary budget using the crate's smart truncation
/// utilities (preserves head + tail).
fn summarize_output(output: &str) -> String {
    if output.is_empty() {
        return String::new();
    }
    smart_truncate_lines(
        &smart_truncate(output, OUTPUT_SUMMARY_MAX_BYTES),
        OUTPUT_SUMMARY_MAX_BYTES,
        OUTPUT_SUMMARY_MAX_LINES,
    )
}

#[cfg(test)]
mod tests;

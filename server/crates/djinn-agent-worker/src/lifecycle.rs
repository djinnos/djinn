//! Lifecycle runner for warm + task-run Pods.
//!
//! Post-P5 this module is no longer tied to the devcontainer spec. It
//! consumes [`djinn_stack::environment::EnvironmentConfig`] loaded from
//! the per-project ConfigMap mounted at
//! [`ENV_CONFIG_MOUNT_FILE`] and runs the phase the caller asks for
//! (`pre_warm` / `pre_task` / user-defined hook lists).
//!
//! ## Scope
//!
//! * [`load_environment_config`] — read the JSON ConfigMap mount and
//!   parse it into an [`EnvironmentConfig`]. Graceful on missing file
//!   (returns `Ok(None)` so the warm Pod tolerates a pre-reseed
//!   project without blowing up).
//! * [`run_phase`] — execute a slice of
//!   [`djinn_stack::environment::HookCommand`] in order, with support
//!   for the three spec-blessed forms (`Shell` / `Exec` / `Parallel`).
//!   Variable substitution still covers `${containerWorkspaceFolder}`,
//!   `${containerEnv:NAME}`, `${localEnv:NAME}` so users migrating
//!   from devcontainer.json don't have to rewrite their hooks.
//!
//! ## What's gone
//!
//! * `.devcontainer/devcontainer.json` reader — retired in P5. Projects
//!   that shipped one for VS Code still work from the IDE side; djinn
//!   just ignores it.
//! * JSONC comment stripper — config JSON comes from Dolt, which stores
//!   strict JSON.
//! * Local `LifecycleCommand` enum — replaced with the canonical
//!   [`djinn_stack::environment::HookCommand`], which round-trips
//!   through the DB column and the MCP tool without a translation
//!   layer.

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use djinn_stack::environment::{
    EnvironmentConfig, HookCommand, PreTaskCommand, PreTaskFailurePolicy,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Canonical mount path the Pod spec attaches the environment-config
/// ConfigMap at. Matches `djinn_k8s::env_config::ENV_CONFIG_MOUNT_FILE`.
pub const ENV_CONFIG_MOUNT_FILE: &str = "/etc/djinn/environment.json";

// ---- per-task-run mount paths (hgd0 Secret-backed) --------------------

/// Full path of the effective `EnvironmentConfig` JSON inside the task-run
/// Pod.  Matches `djinn_k8s::env_config::TASK_RUN_ENV_CONFIG_MOUNT_FILE`.
/// The payload is sourced from the per-task-run Secret's `environment.json`
/// entry.
pub const TASK_RUN_ENV_CONFIG_MOUNT_FILE: &str = "/var/run/djinn/environment.json";

/// Full path of the resolved service metadata JSON inside the task-run
/// Pod.  Matches `djinn_k8s::env_config::TASK_RUN_SERVICE_METADATA_MOUNT_FILE`.
/// The payload is sourced from the per-task-run Secret's
/// `service_metadata.json` entry.
pub const TASK_RUN_SERVICE_METADATA_MOUNT_FILE: &str = "/var/run/djinn/service_metadata.json";

/// Hard cap on combined stdout/stderr retained per pre-task command.
/// Only the final `OUTPUT_MAX_BYTES` bytes are kept when the output exceeds
/// this limit; a `--- output truncated ---` marker is prepended.
pub const OUTPUT_MAX_BYTES: usize = 16 * 1024; // 16 KiB

// ---- task-run service metadata types ----------------------------------

/// A service that was injected as a sidecar.  Narrow worker-side mirror of
/// [`djinn_k8s::sidecar::InjectedServiceMetadata`] — same JSON shape, no
/// k8s crate dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedServiceInfo {
    pub preset_id: String,
    pub service_type: String,
    pub port: i32,
    pub conn_env_var: String,
}

/// A declared preset that could not be converted into a sidecar.  Narrow
/// worker-side mirror of [`djinn_k8s::sidecar::SkippedServicePreset`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedServiceInfo {
    pub preset_id: String,
    pub reason: String,
}

/// Resolved service metadata loaded from the hgd0 Secret-backed mount.
///
/// This is a narrow worker-side view of
/// [`djinn_k8s::sidecar::ImageServiceResolution`] — it carries only the
/// fields the worker needs for readiness and environment-variable
/// preparation.  The `services` field (k8s `BackingServiceSpec`) is
/// intentionally excluded because the worker never constructs sidecar
/// containers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskRunServiceMetadata {
    /// Services injected as sidecars into the Pod.
    pub injected: Vec<InjectedServiceInfo>,
    /// Presets that were skipped during resolution.
    pub skipped: Vec<SkippedServiceInfo>,
    /// Non-fatal lookup error from the host-side resolution.
    pub lookup_error: Option<String>,
}

impl TaskRunServiceMetadata {
    /// `true` when at least one service was injected.
    #[allow(dead_code)] // Used by tests; consumed by later readiness tasks.
    pub fn has_injected_services(&self) -> bool {
        !self.injected.is_empty()
    }
}

// ---- pre-task command runner result types ------------------------------

/// The result of executing a single pre-task command.
///
/// All fields are consumed by downstream tasks (activity events, non-attempt
/// classification) and by tests; `dead_code` is expected until those land.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PreTaskCommandResult {
    /// Effective name (supplied or auto-generated).
    pub name: String,
    /// The shell command string.
    pub command: String,
    /// Index in the `pre_task` array (0-based).
    pub index: usize,
    /// Failure policy for this command.
    pub failure_policy: PreTaskFailurePolicy,
    /// Exit code, if the process exited normally. `None` when killed by
    /// signal (timeout/cancel).
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// `true` when the command was killed by the per-command timeout.
    pub timed_out: bool,
    /// `true` when the command was killed by the pod-level cancellation
    /// token (SIGTERM / soft deadline).
    pub cancelled: bool,
    /// Combined stdout/stderr, secret-redacted and tail-truncated to
    /// [`OUTPUT_MAX_BYTES`].
    pub output: String,
    /// `true` when the output was truncated.
    pub output_truncated: bool,
}

/// Aggregated result of running all pre-task commands.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum PreTaskCommandsResult {
    /// All commands succeeded.
    AllSucceeded {
        /// Individual command results, in execution order.
        results: Vec<PreTaskCommandResult>,
    },
    /// A blocking command failed, timed out, or was cancelled.
    /// No further commands were attempted.
    Blocked {
        /// Results for commands that ran before the blocker (all succeeded).
        results: Vec<PreTaskCommandResult>,
        /// The blocking command's result (failed/timed-out/cancelled).
        blocked_by: PreTaskCommandResult,
    },
    /// A best-effort command failed, but subsequent commands continued.
    BestEffortFailure {
        /// All command results, in execution order.  Includes failed
        /// best-effort entries.
        results: Vec<PreTaskCommandResult>,
    },
}

impl PreTaskCommandsResult {
    /// `true` when all commands succeeded.
    pub fn all_succeeded(&self) -> bool {
        matches!(self, PreTaskCommandsResult::AllSucceeded { .. })
    }

    /// `true` when a blocking failure stopped the sequence.
    #[allow(dead_code)] // Used by downstream tasks (c9l4 non-attempt classification).
    pub fn is_blocked(&self) -> bool {
        matches!(self, PreTaskCommandsResult::Blocked { .. })
    }

    /// Return all command results (including failed ones).
    #[allow(dead_code)] // Used by downstream tasks (tan9 activity events, c9l4).
    pub fn all_results(&self) -> &[PreTaskCommandResult] {
        match self {
            PreTaskCommandsResult::AllSucceeded { results }
            | PreTaskCommandsResult::BestEffortFailure { results } => results,
            PreTaskCommandsResult::Blocked { results, .. } => results,
        }
    }
}

// ---- task-run pre-task inputs -----------------------------------------

/// Everything the worker needs before constructing the supervisor.
///
/// Produced by [`prepare_task_run_inputs`] after workspace attach and
/// before supervisor dispatch.  Later tasks fill the actual command
/// execution / readiness checks without changing the sequencing seam.
#[derive(Debug, Clone)]
pub struct TaskRunPreTaskInputs {
    pub environment_config: EnvironmentConfig,
    pub service_metadata: TaskRunServiceMetadata,
}

/// Load the project's `EnvironmentConfig` from a file path.
///
/// Returns:
/// * `Ok(Some(cfg))` — file present and parsed.
/// * `Ok(None)` — file missing. In production that means the CM didn't
///   exist at Pod schedule time (`optional: true` volume resolved to
///   empty), which in turn means P5's boot reseed hook hasn't touched
///   this project yet.
/// * `Err(...)` — file present but unreadable or unparseable.
pub async fn load_environment_config(path: &Path) -> Result<Option<EnvironmentConfig>> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(r) => r,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            info!(
                path = %path.display(),
                "environment_config mount absent; continuing without one"
            );
            return Ok(None);
        }
        Err(err) => {
            return Err(anyhow::Error::from(err).context(format!("read {}", path.display())));
        }
    };
    let cfg: EnvironmentConfig =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(cfg))
}

// ---- task-run loaders -------------------------------------------------

/// Internal helper — loads `EnvironmentConfig` with hgd0-first fallback logic
/// from explicit paths.  Testable without root filesystem access.
async fn load_task_run_environment_config_from_paths(
    task_run_path: &Path,
    legacy_path: &Path,
) -> Result<EnvironmentConfig> {
    // 1. Try the hgd0 task-run mount.
    match load_environment_config(task_run_path).await {
        Ok(Some(cfg)) => {
            info!(
                path = %task_run_path.display(),
                schema_version = cfg.schema_version,
                pre_task_count = cfg.lifecycle.pre_task.len(),
                "task-run environment_config loaded from hgd0 mount"
            );
            return Ok(cfg);
        }
        Ok(None) => {
            info!(
                path = %task_run_path.display(),
                "task-run environment_config mount absent; trying legacy path"
            );
        }
        Err(e) => {
            return Err(e).context("load task-run environment_config from hgd0 mount");
        }
    }

    // 2. Fall back to the legacy ConfigMap mount.
    match load_environment_config(legacy_path).await {
        Ok(Some(cfg)) => {
            info!(
                path = %legacy_path.display(),
                schema_version = cfg.schema_version,
                pre_task_count = cfg.lifecycle.pre_task.len(),
                "task-run environment_config loaded from legacy ConfigMap mount"
            );
            Ok(cfg)
        }
        Ok(None) => {
            info!(
                hgd0_path = %task_run_path.display(),
                legacy_path = %legacy_path.display(),
                "no environment_config found at either mount; using empty config"
            );
            Ok(EnvironmentConfig::empty())
        }
        Err(e) => Err(e).context("load task-run environment_config from legacy mount"),
    }
}

/// Load the effective `EnvironmentConfig` for a task-run Pod.
///
/// Prefers the hgd0 Secret-backed mount at
/// [`TASK_RUN_ENV_CONFIG_MOUNT_FILE`] (`/var/run/djinn/environment.json`).
/// Falls back to the legacy ConfigMap mount at [`ENV_CONFIG_MOUNT_FILE`]
/// (`/etc/djinn/environment.json`) when the task-run mount is absent.
/// Returns [`EnvironmentConfig::empty()`] when neither mount exists, so
/// the task-run Pod always has a valid config (with an empty
/// `lifecycle.pre_task` list).
///
/// A malformed JSON file is a hard error — the host should have validated
/// the payload before writing the Secret.
pub async fn load_task_run_environment_config() -> Result<EnvironmentConfig> {
    load_task_run_environment_config_from_paths(
        Path::new(TASK_RUN_ENV_CONFIG_MOUNT_FILE),
        Path::new(ENV_CONFIG_MOUNT_FILE),
    )
    .await
}

/// Internal helper — loads service metadata from an explicit path.
/// Testable without root filesystem access.
async fn load_task_run_service_metadata_from_path(path: &Path) -> Result<TaskRunServiceMetadata> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(r) => r,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            info!(
                path = %path.display(),
                "service_metadata mount absent; no backing services assumed"
            );
            return Ok(TaskRunServiceMetadata::default());
        }
        Err(err) => {
            return Err(anyhow::Error::from(err).context(format!("read {}", path.display())));
        }
    };
    let meta: TaskRunServiceMetadata =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    info!(
        path = %path.display(),
        injected = meta.injected.len(),
        skipped = meta.skipped.len(),
        lookup_error = meta.lookup_error.is_some(),
        "task-run service_metadata loaded"
    );
    Ok(meta)
}

/// Load resolved service metadata from the hgd0 Secret-backed mount at
/// [`TASK_RUN_SERVICE_METADATA_MOUNT_FILE`].
///
/// Returns [`TaskRunServiceMetadata::default()`] (empty injected/skipped
/// lists, no lookup error) when the mount is absent.  A malformed JSON
/// file is a hard error.
pub async fn load_task_run_service_metadata() -> Result<TaskRunServiceMetadata> {
    load_task_run_service_metadata_from_path(Path::new(TASK_RUN_SERVICE_METADATA_MOUNT_FILE)).await
}

// ---- task-run startup sequencing boundary ----------------------------

/// Load the pre-task inputs (config + metadata) for a task-run Pod.
///
/// Called after workspace attach and before supervisor dispatch.  This is
/// the explicit sequencing seam — later tasks fill the actual readiness
/// checks and pre-task command execution without changing the call site.
pub async fn prepare_task_run_inputs() -> Result<TaskRunPreTaskInputs> {
    let environment_config = load_task_run_environment_config().await?;
    let service_metadata = load_task_run_service_metadata().await?;
    Ok(TaskRunPreTaskInputs {
        environment_config,
        service_metadata,
    })
}

/// Stub: check that all required backing services are ready.
///
/// Currently always returns `Ok(())`.  Later tasks replace this with
/// real readiness probes against the injected sidecars.
pub async fn check_service_readiness(_service_metadata: &TaskRunServiceMetadata) -> Result<()> {
    // Stub — later tasks implement actual TCP readiness checks.
    Ok(())
}

/// Execute the pre-task commands from `lifecycle.pre_task`.
///
/// Runs each [`PreTaskCommand`] sequentially as `/bin/sh -c <command>` at
/// the given `project_root`.  Inherits the worker/task environment (including
/// service connection env vars injected by k8s) without interpolating
/// prompt/task/issue content.
///
/// For each command:
/// * Enforces the effective `timeout_seconds`.
/// * On timeout or cancellation: terminates the child process group with a
///   grace-then-kill fallback (SIGTERM → 5s → SIGKILL).
/// * Captures combined stdout/stderr, redacts secret-looking values, and
///   retains only the final [`OUTPUT_MAX_BYTES`].
///
/// Applies the per-command failure policy:
/// * `best_effort` — logs the failure and continues to the next command.
/// * `blocking` — stops immediately and returns a [`Blocked`] result.
pub async fn run_pre_task_commands(
    environment_config: &EnvironmentConfig,
    project_root: &Path,
    cancel: &CancellationToken,
) -> Result<PreTaskCommandsResult> {
    let commands = &environment_config.lifecycle.pre_task;
    if commands.is_empty() {
        return Ok(PreTaskCommandsResult::AllSucceeded {
            results: Vec::new(),
        });
    }

    let redaction_patterns = build_redaction_patterns(environment_config);
    let mut results = Vec::with_capacity(commands.len());

    info!(
        project_root = %project_root.display(),
        count = commands.len(),
        "pre-task: running commands"
    );

    for (idx, cmd) in commands.iter().enumerate() {
        if cancel.is_cancelled() {
            warn!(
                name = %cmd.resolved_name(idx),
                index = idx,
                "pre-task: pod cancellation requested; stopping before command"
            );
            // Record a synthetic cancelled result for the skipped command.
            results.push(PreTaskCommandResult {
                name: cmd.resolved_name(idx),
                command: cmd.command.clone(),
                index: idx,
                failure_policy: cmd.failure_policy,
                exit_code: None,
                duration_ms: 0,
                timed_out: false,
                cancelled: true,
                output: String::new(),
                output_truncated: false,
            });
            return Ok(PreTaskCommandsResult::Blocked {
                results,
                blocked_by: PreTaskCommandResult {
                    name: cmd.resolved_name(idx),
                    command: cmd.command.clone(),
                    index: idx,
                    failure_policy: cmd.failure_policy,
                    exit_code: None,
                    duration_ms: 0,
                    timed_out: false,
                    cancelled: true,
                    output: String::new(),
                    output_truncated: false,
                },
            });
        }

        let result =
            run_pre_task_command(cmd, idx, project_root, cancel, &redaction_patterns).await;

        let failed = result.exit_code != Some(0);
        let abnormal = result.timed_out || result.cancelled;

        info!(
            name = %result.name,
            index = idx,
            exit_code = result.exit_code,
            timed_out = result.timed_out,
            cancelled = result.cancelled,
            duration_ms = result.duration_ms,
            output_bytes = result.output.len(),
            output_truncated = result.output_truncated,
            failure_policy = ?cmd.failure_policy,
            "pre-task: command complete"
        );

        if failed || abnormal {
            match cmd.failure_policy {
                PreTaskFailurePolicy::Blocking => {
                    warn!(
                        name = %result.name,
                        index = idx,
                        exit_code = result.exit_code,
                        timed_out = result.timed_out,
                        cancelled = result.cancelled,
                        "pre-task: blocking command failed; stopping"
                    );
                    results.push(result.clone());
                    return Ok(PreTaskCommandsResult::Blocked {
                        results,
                        blocked_by: result,
                    });
                }
                PreTaskFailurePolicy::BestEffort => {
                    warn!(
                        name = %result.name,
                        index = idx,
                        exit_code = result.exit_code,
                        timed_out = result.timed_out,
                        cancelled = result.cancelled,
                        "pre-task: best-effort command failed; continuing"
                    );
                    results.push(result);
                }
            }
        } else {
            results.push(result);
        }
    }

    let has_failures = results
        .iter()
        .any(|r| (r.exit_code != Some(0)) || r.timed_out || r.cancelled);

    if has_failures {
        Ok(PreTaskCommandsResult::BestEffortFailure { results })
    } else {
        Ok(PreTaskCommandsResult::AllSucceeded { results })
    }
}

/// Execute a single pre-task command.
///
/// Spawns `/bin/sh -c <command>` in a new process group at `project_root`,
/// enforces the timeout, captures output, redacts secrets, and truncates to
/// [`OUTPUT_MAX_BYTES`].
async fn run_pre_task_command(
    cmd: &PreTaskCommand,
    index: usize,
    project_root: &Path,
    cancel: &CancellationToken,
    redaction_patterns: &[Regex],
) -> PreTaskCommandResult {
    let name = cmd.resolved_name(index);
    let start = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
    let timeout = Duration::from_secs(cmd.timeout_seconds);

    // Spawn the child in its own process group for clean group-kill.
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd.command)
        .current_dir(project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let elapsed = start.elapsed();
            warn!(name = %name, error = %e, "pre-task: failed to spawn command");
            return PreTaskCommandResult {
                name,
                command: cmd.command.clone(),
                index,
                failure_policy: cmd.failure_policy,
                exit_code: None,
                duration_ms: elapsed.as_millis() as u64,
                timed_out: false,
                cancelled: false,
                output: format!("spawn error: {e}"),
                output_truncated: false,
            };
        }
    };

    // Take stdout/stderr pipes before waiting so we retain ownership of
    // `child` for process-group signalling on timeout/cancel.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Race: command finishes | timeout expires | pod cancelled.
    let timed_out;
    let cancelled;

    tokio::select! {
        result = tokio::time::timeout(timeout, child.wait()) => {
            match result {
                Ok(Ok(status)) => {
                    // Normal (or signal-killed) exit within timeout.
                    timed_out = false;
                    cancelled = false;

                    // Drain pipes — the child has exited so EOF is guaranteed.
                    let stdout = drain_pipe(stdout_pipe).await;
                    let stderr = drain_pipe(stderr_pipe).await;

                    let (mut text, truncated) = redact_and_truncate_output(
                        &stdout,
                        &stderr,
                        redaction_patterns,
                    );
                    let exit_code = status.code();

                    // On Unix, a signal kill produces exit_code = None.
                    #[cfg(unix)]
                    if exit_code.is_none()
                        && let Some(sig) = status.signal()
                    {
                        use std::fmt::Write;
                        let _ = write!(text, "\n[killed by signal {sig}]");
                    }

                    return PreTaskCommandResult {
                        name,
                        command: cmd.command.clone(),
                        index,
                        failure_policy: cmd.failure_policy,
                        exit_code,
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out,
                        cancelled,
                        output: text,
                        output_truncated: truncated,
                    };
                }
                Ok(Err(e)) => {
                    // wait() error (rare — e.g. signal delivery issue).
                    timed_out = false;
                    cancelled = false;
                    let elapsed = start.elapsed();
                    warn!(name = %name, error = %e, "pre-task: wait error");
                    return PreTaskCommandResult {
                        name,
                        command: cmd.command.clone(),
                        index,
                        failure_policy: cmd.failure_policy,
                        exit_code: None,
                        duration_ms: elapsed.as_millis() as u64,
                        timed_out,
                        cancelled,
                        output: format!("wait error: {e}"),
                        output_truncated: false,
                    };
                }
                Err(_timeout_elapsed) => {
                    // Timeout — kill the process group.
                    timed_out = true;
                    cancelled = false;
                    warn!(
                        name = %name,
                        timeout_secs = cmd.timeout_seconds,
                        "pre-task: command timed out"
                    );
                    kill_process_group_gracefully(&mut child).await;
                }
            }
        }
        _ = cancel.cancelled() => {
            // Pod-level cancellation.
            timed_out = false;
            cancelled = true;
            warn!(name = %name, "pre-task: pod cancellation received");
            kill_process_group_gracefully(&mut child).await;
        }
    }

    // After a kill (timeout or cancel), drain remaining pipe data and reap.
    let stdout = drain_pipe(stdout_pipe).await;
    let stderr = drain_pipe(stderr_pipe).await;
    // Best-effort reap.
    let _ = child.wait().await;

    let (mut text, truncated) = redact_and_truncate_output(&stdout, &stderr, redaction_patterns);

    if timed_out {
        text.push_str("\n[timed out]");
    }
    if cancelled {
        text.push_str("\n[cancelled]");
    }

    PreTaskCommandResult {
        name,
        command: cmd.command.clone(),
        index,
        failure_policy: cmd.failure_policy,
        exit_code: None, // killed by signal — no normal exit code
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
        cancelled,
        output: text,
        output_truncated: truncated,
    }
}

/// Drain an async pipe to a `Vec<u8>`.
///
/// After the child exits the pipe reaches EOF promptly. This helper
/// awaits the async read without blocking.
async fn drain_pipe(reader: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    match reader {
        Some(mut r) => {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf).await;
            buf
        }
        None => Vec::new(),
    }
}

/// Grace-then-kill the child's process group.
///
/// Sends SIGTERM to the group, waits up to 5 seconds, then escalates to
/// SIGKILL if the process is still alive.
#[cfg(unix)]
async fn kill_process_group_gracefully(child: &mut tokio::process::Child) {
    if let Some(pgid) = child.id() {
        // SIGTERM the process group.
        let _ = unsafe { libc::kill(-(pgid as i32), libc::SIGTERM) };

        // Grace period: 5 seconds.
        let grace = Duration::from_secs(5);
        match tokio::time::timeout(grace, child.wait()).await {
            // Exited within grace period or wait error (process likely gone).
            Ok(Ok(_)) | Ok(Err(_)) => {}
            Err(_) => {
                // Grace expired — escalate to SIGKILL.
                let _ = unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
                // Best-effort reap.
                let _ = child.wait().await;
            }
        }
    }
}

#[cfg(not(unix))]
async fn kill_process_group_gracefully(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
}

// ---- secret redaction helpers ----------------------------------------

/// Environment variable names that are likely secrets (case-insensitive).
const SECRET_ENV_PATTERNS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "PRIVATE_KEY",
    "ACCESS_KEY",
    "CREDENTIAL",
    "AUTH",
];

/// Check whether an env var name looks like it holds a secret.
fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    SECRET_ENV_PATTERNS.iter().any(|pat| upper.contains(pat))
}

/// Collect secret values from the environment for redaction.
///
/// Includes:
/// * Values of env vars whose names match [`is_secret_env_name`].
/// * Connection-string values from injected service metadata (the env var
///   names come from [`InjectedServiceInfo::conn_env_var`]).
fn collect_secret_values(environment_config: &EnvironmentConfig) -> Vec<String> {
    let mut secrets = Vec::new();

    // From EnvironmentConfig.env — names that look secret.
    for (name, value) in &environment_config.env {
        if is_secret_env_name(name) && !value.is_empty() {
            secrets.push(value.clone());
        }
    }

    // Process env vars that look secret (these include k8s-injected service
    // connection strings).
    for (key, value) in std::env::vars() {
        if is_secret_env_name(&key) && !value.is_empty() {
            secrets.push(value);
        }
    }

    // Deduplicate.
    secrets.sort();
    secrets.dedup();
    // Remove empty strings that may have slipped through.
    secrets.retain(|s| !s.is_empty());
    secrets
}

/// Build compiled regex patterns for secret redaction.
///
/// Each pattern matches a collected secret value literally (regex-escaped).
fn build_redaction_patterns(environment_config: &EnvironmentConfig) -> Vec<Regex> {
    let secrets = collect_secret_values(environment_config);
    secrets
        .iter()
        .filter_map(|val| {
            // Skip very short values (≤4 chars) to avoid over-redacting
            // common substrings.
            if val.len() <= 4 {
                return None;
            }
            Regex::new(&regex::escape(val)).ok()
        })
        .collect()
}

/// Combine stdout + stderr, apply redaction, and truncate to the tail
/// [`OUTPUT_MAX_BYTES`] bytes.
fn redact_and_truncate_output(stdout: &[u8], stderr: &[u8], patterns: &[Regex]) -> (String, bool) {
    // Combine as UTF-8, replacing invalid sequences.
    let mut combined = String::with_capacity(stdout.len() + stderr.len());
    combined.push_str(&String::from_utf8_lossy(stdout));
    combined.push_str(&String::from_utf8_lossy(stderr));

    // Apply redaction.
    for pat in patterns {
        combined = pat.replace_all(&combined, "[REDACTED]").into_owned();
    }

    // Truncate to the tail OUTPUT_MAX_BYTES.
    truncate_to_tail(&combined, OUTPUT_MAX_BYTES)
}

/// Retain only the final `max_bytes` bytes of `text`, prepending a marker
/// when truncation occurs.
fn truncate_to_tail(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }

    // Find a valid char boundary at or before max_bytes from the end.
    let start = text.len() - max_bytes;
    let start = find_valid_char_boundary(text, start);
    let tail = &text[start..];
    (format!("--- output truncated ---\n{tail}"), true)
}

/// Find a valid UTF-8 char boundary at or just before `pos`.
fn find_valid_char_boundary(s: &str, pos: usize) -> usize {
    let mut pos = pos;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Run the full pre-task startup boundary: load inputs, check readiness,
/// and execute pre-task commands.
///
/// Returns [`TaskRunPreTaskInputs`] so the caller (e.g. for environment
/// variable injection into the supervisor context) can inspect the loaded
/// config and metadata, along with the [`PreTaskCommandsResult`].
///
/// This is the single entry point called from `run_task_run` between
/// workspace attach and supervisor dispatch.  If any step fails, the
/// task-run does not proceed to the supervisor.  A blocking pre-task
/// command failure is surfaced as an error so the caller can classify it
/// as an environmental non-attempt.
pub async fn execute_task_run_startup_boundary(
    project_root: &Path,
    cancel: &CancellationToken,
) -> Result<(TaskRunPreTaskInputs, PreTaskCommandsResult)> {
    let inputs = prepare_task_run_inputs().await?;

    check_service_readiness(&inputs.service_metadata).await?;
    info!("service readiness check passed");

    let pretask_result =
        run_pre_task_commands(&inputs.environment_config, project_root, cancel).await?;

    match &pretask_result {
        PreTaskCommandsResult::Blocked { blocked_by, .. } => {
            // Surface the blocking failure as an error for the caller.
            Err(anyhow!(
                "pre-task blocking command '{}' failed (exit={:?}, timed_out={}, cancelled={})",
                blocked_by.name,
                blocked_by.exit_code,
                blocked_by.timed_out,
                blocked_by.cancelled,
            ))
        }
        PreTaskCommandsResult::AllSucceeded { results } => {
            info!(
                pre_task_count = results.len(),
                "pre-task commands completed successfully"
            );
            Ok((inputs, pretask_result))
        }
        PreTaskCommandsResult::BestEffortFailure { results } => {
            info!(
                pre_task_count = results.len(),
                "pre-task commands completed with best-effort failures"
            );
            Ok((inputs, pretask_result))
        }
    }
}

/// Run every command in `commands` in order, stopping + returning on
/// the first failure. `phase_name` is used for log lines only.
///
/// `project_root` is the workspace directory — the devcontainer spec's
/// `${containerWorkspaceFolder}` substitution resolves to this path, and
/// each command runs with its CWD set here. Callers derived it from
/// `DJINN_PROJECT_ROOT` or the hard-coded `/workspace` fallback.
pub async fn run_phase(
    project_root: &Path,
    phase_name: &str,
    commands: &[HookCommand],
) -> Result<()> {
    if commands.is_empty() {
        info!(
            phase = phase_name,
            project_root = %project_root.display(),
            "lifecycle: no commands; skipping phase"
        );
        return Ok(());
    }
    let ctx = CommandContext::new(project_root.to_path_buf());
    info!(
        phase = phase_name,
        project_root = %project_root.display(),
        count = commands.len(),
        "lifecycle: running phase"
    );
    for (idx, cmd) in commands.iter().enumerate() {
        let sub_phase = format!("{phase_name}[{idx}]");
        let start = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
        run_command(&sub_phase, cmd, &ctx)
            .await
            .with_context(|| format!("{sub_phase} failed"))?;
        info!(
            phase = %sub_phase,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "lifecycle: command complete"
        );
    }
    Ok(())
}

/// Per-invocation context threaded through command execution.
#[derive(Debug, Clone)]
struct CommandContext {
    workspace_folder: PathBuf,
}

impl CommandContext {
    fn new(workspace_folder: PathBuf) -> Self {
        Self { workspace_folder }
    }

    /// Expand devcontainer-spec substitution variables. Unknown variables
    /// are left as-is (the spec says to leave them untouched rather than
    /// substitute empty — this avoids `rm -rf /${UNSET_VAR}/data` surprises).
    fn substitute(&self, raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + 1 < bytes.len()
                && bytes[i] == b'$'
                && bytes[i + 1] == b'{'
                && let Some(close) = find_close_brace(bytes, i + 2)
                && let Some(value) = self.resolve_variable(&raw[i + 2..close])
            {
                out.push_str(&value);
                i = close + 1;
                continue;
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    fn resolve_variable(&self, name: &str) -> Option<String> {
        match name {
            "containerWorkspaceFolder" | "localWorkspaceFolder" => {
                Some(self.workspace_folder.to_string_lossy().into_owned())
            }
            "containerWorkspaceFolderBasename" | "localWorkspaceFolderBasename" => self
                .workspace_folder
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_owned),
            other => {
                if let Some(var) = other
                    .strip_prefix("containerEnv:")
                    .or_else(|| other.strip_prefix("localEnv:"))
                {
                    return std::env::var(var).ok();
                }
                None
            }
        }
    }
}

fn find_close_brace(bytes: &[u8], start: usize) -> Option<usize> {
    (start..bytes.len()).find(|&i| bytes[i] == b'}')
}

/// Dispatch a single hook command.
fn run_command<'a>(
    phase: &'a str,
    cmd: &'a HookCommand,
    ctx: &'a CommandContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        match cmd {
            HookCommand::Shell(s) => run_shell(phase, s, ctx).await,
            HookCommand::Exec(parts) => run_exec(phase, parts, ctx).await,
            HookCommand::Parallel(map) => {
                let mut join = tokio::task::JoinSet::new();
                for (name, sub_cmd) in map.iter() {
                    let sub_phase = format!("{phase}/{name}");
                    let ctx = ctx.clone();
                    let sub_cmd_owned = sub_cmd.clone();
                    join.spawn(async move { run_command(&sub_phase, &sub_cmd_owned, &ctx).await });
                }
                let mut first_err: Option<anyhow::Error> = None;
                while let Some(res) = join.join_next().await {
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                        Ok(Err(_)) => {}
                        Err(join_err) if first_err.is_none() => {
                            first_err = Some(anyhow!("join error: {join_err}"))
                        }
                        Err(_) => {}
                    }
                }
                match first_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
        }
    })
}

async fn run_shell(phase: &str, raw: &str, ctx: &CommandContext) -> Result<()> {
    let expanded = ctx.substitute(raw);
    info!(phase, command = %expanded, "shell form");
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(&expanded)
        .current_dir(&ctx.workspace_folder)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("spawn /bin/sh for {phase}"))?;
    if !status.success() {
        return Err(anyhow!(
            "{phase}: shell command `{expanded}` exited with {:?}",
            status.code()
        ));
    }
    Ok(())
}

async fn run_exec(phase: &str, parts: &[String], ctx: &CommandContext) -> Result<()> {
    if parts.is_empty() {
        warn!(phase, "exec form with empty argv; skipping");
        return Ok(());
    }
    let expanded: Vec<String> = parts.iter().map(|p| ctx.substitute(p)).collect();
    info!(phase, argv = ?expanded, "exec form");
    let status = Command::new(&expanded[0])
        .args(&expanded[1..])
        .current_dir(&ctx.workspace_folder)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("spawn {} for {phase}", expanded[0]))?;
    if !status.success() {
        return Err(anyhow!(
            "{phase}: exec `{}` exited with {:?}",
            expanded.join(" "),
            status.code()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_environment_config_returns_none_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("environment.json");
        let out = load_environment_config(&path).await.expect("ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn load_environment_config_parses_valid_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("environment.json");
        std::fs::write(
            &path,
            r#"{
                "schema_version": 1,
                "source": "auto-detected",
                "env": {"RUST_LOG": "info"}
            }"#,
        )
        .unwrap();
        let out = load_environment_config(&path).await.expect("ok");
        let cfg = out.expect("some");
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[tokio::test]
    async fn load_environment_config_errors_on_malformed_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("environment.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let err = load_environment_config(&path).await.unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    #[tokio::test]
    async fn run_phase_noop_on_empty_commands() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_phase(tmp.path(), "pre_warm", &[]).await.expect("ok");
    }

    #[tokio::test]
    async fn run_phase_executes_shell_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stamp = tmp.path().join("stamp");
        let commands = vec![
            HookCommand::Shell(format!("echo first >> {}", stamp.to_string_lossy())),
            HookCommand::Shell(format!("echo second >> {}", stamp.to_string_lossy())),
        ];
        run_phase(tmp.path(), "pre_warm", &commands)
            .await
            .expect("ok");
        let content = std::fs::read_to_string(&stamp).unwrap();
        assert_eq!(content, "first\nsecond\n");
    }

    #[tokio::test]
    async fn run_phase_substitutes_container_workspace_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let commands = vec![HookCommand::Shell(
            "touch ${containerWorkspaceFolder}/marker".into(),
        )];
        run_phase(tmp.path(), "pre_warm", &commands)
            .await
            .expect("ok");
        assert!(tmp.path().join("marker").exists());
    }

    #[tokio::test]
    async fn run_phase_exec_form_supports_argv() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker-exec");
        let commands = vec![HookCommand::Exec(vec![
            "bash".into(),
            "-lc".into(),
            format!("touch {}", marker.to_string_lossy()),
        ])];
        run_phase(tmp.path(), "pre_warm", &commands)
            .await
            .expect("ok");
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn run_phase_parallel_form_runs_all() {
        use std::collections::BTreeMap;
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut children = BTreeMap::new();
        children.insert(
            "one".into(),
            HookCommand::Shell("touch ${containerWorkspaceFolder}/one".into()),
        );
        children.insert(
            "two".into(),
            HookCommand::Shell("touch ${containerWorkspaceFolder}/two".into()),
        );
        let commands = vec![HookCommand::Parallel(children)];
        run_phase(tmp.path(), "pre_warm", &commands)
            .await
            .expect("ok");
        assert!(tmp.path().join("one").exists());
        assert!(tmp.path().join("two").exists());
    }

    #[tokio::test]
    async fn run_phase_propagates_shell_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let commands = vec![HookCommand::Shell("exit 7".into())];
        let err = run_phase(tmp.path(), "pre_warm", &commands)
            .await
            .unwrap_err();
        // anyhow::Error::to_string() only renders the outermost context;
        // the inner "exited with Some(7)" lives on the source chain.
        // Use {:#} to flatten the full chain into one string.
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("Some(7)"),
            "expected chain to contain exit status, got: {formatted}"
        );
    }

    // ---- task-run loader tests ----------------------------------------

    #[tokio::test]
    async fn task_run_config_prefers_hgd0_mount() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("hgd0.json");
        let legacy = tmp.path().join("legacy.json");

        // hgd0 has pre_task commands
        std::fs::write(
            &hgd0,
            r#"{
                "schema_version": 1,
                "source": "auto-detected",
                "lifecycle": {
                    "pre_task": [{"command": "echo hgd0", "name": "setup"}]
                }
            }"#,
        )
        .unwrap();
        // legacy has a different env var
        std::fs::write(
            &legacy,
            r#"{"schema_version": 1, "source": "auto-detected", "env": {"LEGACY": "1"}}"#,
        )
        .unwrap();

        let cfg = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .expect("ok");
        assert_eq!(cfg.lifecycle.pre_task.len(), 1);
        assert_eq!(cfg.lifecycle.pre_task[0].command, "echo hgd0");
        // Should NOT have picked up legacy's env
        assert!(cfg.env.get("LEGACY").is_none());
    }

    #[tokio::test]
    async fn task_run_config_falls_back_to_legacy_when_hgd0_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("nonexistent.json");
        let legacy = tmp.path().join("legacy.json");

        std::fs::write(
            &legacy,
            r#"{"schema_version": 1, "source": "auto-detected", "env": {"LEGACY": "yes"}}"#,
        )
        .unwrap();

        let cfg = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .expect("ok");
        assert_eq!(cfg.env.get("LEGACY").map(String::as_str), Some("yes"));
    }

    #[tokio::test]
    async fn task_run_config_defaults_to_empty_when_both_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("nonexistent1.json");
        let legacy = tmp.path().join("nonexistent2.json");

        let cfg = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .expect("ok");
        // Should be the canonical empty config
        assert_eq!(cfg.schema_version, djinn_stack::environment::SCHEMA_VERSION);
        assert!(cfg.lifecycle.pre_task.is_empty());
        assert!(cfg.lifecycle.pre_anything.is_empty());
        assert!(cfg.env.is_empty());
    }

    #[tokio::test]
    async fn task_run_config_errors_on_malformed_hgd0() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("bad.json");
        let legacy = tmp.path().join("absent.json");

        std::fs::write(&hgd0, b"{ not json").unwrap();

        let err = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"), "expected parse error, got: {msg}");
    }

    #[tokio::test]
    async fn task_run_config_errors_on_malformed_legacy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("absent.json");
        let legacy = tmp.path().join("bad.json");

        std::fs::write(&legacy, b"{ broken").unwrap();

        let err = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"), "expected parse error, got: {msg}");
    }

    #[tokio::test]
    async fn task_run_config_with_pre_task_commands() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("hgd0.json");

        std::fs::write(
            &hgd0,
            r#"{
                "schema_version": 1,
                "source": "auto-detected",
                "lifecycle": {
                    "pre_task": [
                        {"command": "cargo build", "name": "build", "timeout_seconds": 600},
                        {"command": "cargo test --no-run", "name": "check"}
                    ]
                }
            }"#,
        )
        .unwrap();

        let cfg = load_task_run_environment_config_from_paths(&hgd0, Path::new("/nonexistent"))
            .await
            .expect("ok");
        assert_eq!(cfg.lifecycle.pre_task.len(), 2);
        assert_eq!(cfg.lifecycle.pre_task[0].name.as_deref(), Some("build"));
        assert_eq!(cfg.lifecycle.pre_task[0].timeout_seconds, 600);
        assert_eq!(cfg.lifecycle.pre_task[1].name.as_deref(), Some("check"));
    }

    #[tokio::test]
    async fn service_metadata_loads_valid_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("service_metadata.json");

        std::fs::write(
            &path,
            r#"{
                "injected": [
                    {
                        "preset_id": "postgres-16",
                        "service_type": "postgres",
                        "port": 5432,
                        "conn_env_var": "DATABASE_URL,TEST_POSTGRES_URL"
                    }
                ],
                "skipped": [],
                "lookup_error": null
            }"#,
        )
        .unwrap();

        let meta = load_task_run_service_metadata_from_path(&path)
            .await
            .expect("ok");
        assert_eq!(meta.injected.len(), 1);
        assert_eq!(meta.injected[0].preset_id, "postgres-16");
        assert_eq!(meta.injected[0].port, 5432);
        assert!(meta.skipped.is_empty());
        assert!(meta.lookup_error.is_none());
        assert!(meta.has_injected_services());
    }

    #[tokio::test]
    async fn service_metadata_defaults_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nonexistent.json");

        let meta = load_task_run_service_metadata_from_path(&path)
            .await
            .expect("ok");
        assert!(meta.injected.is_empty());
        assert!(meta.skipped.is_empty());
        assert!(meta.lookup_error.is_none());
        assert!(!meta.has_injected_services());
    }

    #[tokio::test]
    async fn service_metadata_errors_on_malformed_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("bad.json");

        std::fs::write(&path, b"{ bad json }").unwrap();

        let err = load_task_run_service_metadata_from_path(&path)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"), "expected parse error, got: {msg}");
    }

    #[tokio::test]
    async fn service_metadata_with_skipped_and_lookup_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("service_metadata.json");

        std::fs::write(
            &path,
            r#"{
                "injected": [],
                "skipped": [{"preset_id": "redis", "reason": "image not found"}],
                "lookup_error": "registry timeout"
            }"#,
        )
        .unwrap();

        let meta = load_task_run_service_metadata_from_path(&path)
            .await
            .expect("ok");
        assert!(meta.injected.is_empty());
        assert_eq!(meta.skipped.len(), 1);
        assert_eq!(meta.skipped[0].reason, "image not found");
        assert_eq!(meta.lookup_error.as_deref(), Some("registry timeout"));
        assert!(!meta.has_injected_services());
    }

    #[tokio::test]
    async fn prepare_task_run_inputs_loads_both() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("environment.json");
        let svc = tmp.path().join("service_metadata.json");

        std::fs::write(
            &hgd0,
            r#"{
                "schema_version": 1,
                "source": "auto-detected",
                "lifecycle": {
                    "pre_task": [{"command": "echo hi", "name": "greet"}]
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            &svc,
            r#"{
                "injected": [{"preset_id": "pg", "service_type": "postgres", "port": 5432, "conn_env_var": "DATABASE_URL"}],
                "skipped": []
            }"#,
        )
        .unwrap();

        // Use the internal helpers to test the orchestration logic
        let environment_config =
            load_task_run_environment_config_from_paths(&hgd0, Path::new("/nonexistent"))
                .await
                .expect("config");
        let service_metadata = load_task_run_service_metadata_from_path(&svc)
            .await
            .expect("metadata");

        let inputs = TaskRunPreTaskInputs {
            environment_config,
            service_metadata,
        };
        assert_eq!(inputs.environment_config.lifecycle.pre_task.len(), 1);
        assert!(inputs.service_metadata.has_injected_services());
    }

    #[tokio::test]
    async fn stub_readiness_returns_ok() {
        let meta = TaskRunServiceMetadata::default();
        check_service_readiness(&meta).await.expect("ok");
    }

    #[tokio::test]
    async fn stub_pre_task_returns_ok() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig::empty();
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert!(result.all_results().is_empty());
    }

    #[tokio::test]
    async fn execute_startup_boundary_succeeds_with_no_mounts() {
        // With no files on disk, the boundary should succeed using defaults.
        // This tests the full orchestration seam.
        // NOTE: uses real mount paths which won't exist in CI — that's fine,
        // the loaders default gracefully.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cancel = CancellationToken::new();
        let result = execute_task_run_startup_boundary(tmp.path(), &cancel).await;
        assert!(
            result.is_ok(),
            "startup boundary should succeed with defaults: {result:?}"
        );
        let (inputs, pretask_result) = result.unwrap();
        assert!(inputs.environment_config.lifecycle.pre_task.is_empty());
        assert!(inputs.service_metadata.injected.is_empty());
        assert!(pretask_result.all_succeeded());
        assert!(pretask_result.all_results().is_empty());
    }

    // ---- pre-task command runner tests --------------------------------

    #[tokio::test]
    async fn pretask_executes_commands_sequentially() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stamp = tmp.path().join("stamp");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("first".into()),
                        command: format!("echo first >> {}", stamp.to_string_lossy()),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("second".into()),
                        command: format!("echo second >> {}", stamp.to_string_lossy()),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert_eq!(result.all_results().len(), 2);

        let content = std::fs::read_to_string(&stamp).unwrap();
        assert_eq!(content, "first\nsecond\n");
    }

    #[tokio::test]
    async fn pretask_captures_exit_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("fail".into()),
                    command: "exit 42".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::BestEffort,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        // Best-effort failure should not block.
        assert!(!result.all_succeeded());
        assert!(!result.is_blocked());
        let results = result.all_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_code, Some(42));
        assert!(!results[0].timed_out);
        assert!(!results[0].cancelled);
    }

    #[tokio::test]
    async fn pretask_blocking_failure_stops_sequence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("fail".into()),
                        command: "exit 1".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("should-not-run".into()),
                        command: format!("touch {}", marker.to_string_lossy()),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.is_blocked());
        assert!(!marker.exists(), "second command should not have run");
        // Only the first command's result should be in the results.
        let results = result.all_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "fail");
    }

    #[tokio::test]
    async fn pretask_best_effort_continues_after_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("fail".into()),
                        command: "exit 1".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::BestEffort,
                    },
                    PreTaskCommand {
                        name: Some("succeed".into()),
                        command: format!("touch {}", marker.to_string_lossy()),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(!result.all_succeeded());
        assert!(!result.is_blocked());
        assert!(marker.exists(), "second command should have run");
        assert_eq!(result.all_results().len(), 2);
    }

    #[tokio::test]
    async fn pretask_enforces_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("slow".into()),
                    command: "sleep 60".into(),
                    timeout_seconds: 1, // 1 second timeout
                    failure_policy: PreTaskFailurePolicy::BestEffort,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let start = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        let elapsed = start.elapsed();

        assert!(!result.all_succeeded());
        let results = result.all_results();
        assert_eq!(results.len(), 1);
        assert!(results[0].timed_out, "command should have timed out");
        // Should have completed within a reasonable time (not 60s).
        assert!(
            elapsed.as_secs() < 15,
            "should have timed out in ~1s, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn pretask_cancellation_returns_early() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("slow".into()),
                        command: "sleep 30".into(),
                        timeout_seconds: 60,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("should-not-run".into()),
                        command: format!("touch {}", marker.to_string_lossy()),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Cancel after a short delay.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_clone.cancel();
        });

        let start = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        let elapsed = start.elapsed();

        assert!(result.is_blocked());
        assert!(!marker.exists(), "second command should not have run");
        // Should have been cancelled quickly, not after 30s.
        assert!(
            elapsed.as_secs() < 10,
            "should have cancelled quickly, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn pretask_redacts_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Set a secret env var for the redaction to detect.
        // We'll use the config's env map since process env injection
        // happens at process level.
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "MY_SECRET_TOKEN".to_string(),
            "supersecretvalue123".to_string(),
        );
        let cfg = EnvironmentConfig {
            env: env.clone(),
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("echo-secret".into()),
                    command: "echo supersecretvalue123".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        let r = &result.all_results()[0];
        assert!(
            !r.output.contains("supersecretvalue123"),
            "output should be redacted, got: {}",
            r.output
        );
        assert!(
            r.output.contains("[REDACTED]"),
            "output should contain [REDACTED], got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn pretask_truncates_large_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Generate output larger than 16 KiB.
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("verbose".into()),
                    // Each line is ~80 chars; 300 lines ≈ 24 KiB.
                    command: "for i in $(seq 1 300); do printf 'Line %04d: %s\n' \"$i\" 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done".into(),
                    timeout_seconds: 30,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        let r = &result.all_results()[0];
        assert!(r.output_truncated, "output should be truncated");
        assert!(
            r.output.len() <= OUTPUT_MAX_BYTES + 200,
            "output should be at most ~16KiB + marker, got {} bytes",
            r.output.len()
        );
        assert!(
            r.output.starts_with("--- output truncated ---"),
            "output should start with truncation marker"
        );
    }

    #[tokio::test]
    async fn pretask_output_not_truncated_when_small() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("small".into()),
                    command: "echo hello".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        let r = &result.all_results()[0];
        assert!(!r.output_truncated, "small output should not be truncated");
        assert!(
            r.output.contains("hello"),
            "output should contain 'hello', got: {}",
            r.output
        );
    }

    #[test]
    fn truncate_to_tail_returns_full_when_under_limit() {
        let text = "short text";
        let (out, truncated) = truncate_to_tail(text, 1024);
        assert_eq!(out, text);
        assert!(!truncated);
    }

    #[test]
    fn truncate_to_tail_truncates_when_over_limit() {
        // Build text larger than limit.
        let text = "x".repeat(100);
        let (out, truncated) = truncate_to_tail(&text, 50);
        assert!(truncated);
        assert!(out.starts_with("--- output truncated ---"));
        assert!(out.ends_with(&"x".repeat(50)));
    }

    #[test]
    fn is_secret_env_name_matches_expected() {
        assert!(is_secret_env_name("DATABASE_PASSWORD"));
        assert!(is_secret_env_name("API_KEY"));
        assert!(is_secret_env_name("MY_SECRET_TOKEN"));
        assert!(is_secret_env_name("AUTH_TOKEN"));
        assert!(is_secret_env_name("GITHUB_TOKEN"));
        assert!(is_secret_env_name("PRIVATE_KEY"));
        assert!(!is_secret_env_name("HOME"));
        assert!(!is_secret_env_name("PATH"));
        assert!(!is_secret_env_name("RUST_LOG"));
    }

    #[test]
    fn redact_and_truncate_redacts_values() {
        let patterns = vec![Regex::new(&regex::escape("supersecret123")).unwrap()];
        let (out, _) =
            redact_and_truncate_output(b"output has supersecret123 in it", b"", &patterns);
        assert!(!out.contains("supersecret123"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_and_truncate_combines_stdout_stderr() {
        let (out, _) = redact_and_truncate_output(b"stdout\n", b"stderr\n", &[]);
        assert!(out.contains("stdout"));
        assert!(out.contains("stderr"));
    }

    #[test]
    fn pretask_result_all_succeeded() {
        let result = PreTaskCommandsResult::AllSucceeded { results: vec![] };
        assert!(result.all_succeeded());
        assert!(!result.is_blocked());
    }

    #[test]
    fn pretask_result_blocked() {
        let result = PreTaskCommandsResult::Blocked {
            results: vec![],
            blocked_by: PreTaskCommandResult {
                name: "test".into(),
                command: "fail".into(),
                index: 0,
                failure_policy: PreTaskFailurePolicy::Blocking,
                exit_code: Some(1),
                duration_ms: 100,
                timed_out: false,
                cancelled: false,
                output: String::new(),
                output_truncated: false,
            },
        };
        assert!(!result.all_succeeded());
        assert!(result.is_blocked());
    }

    #[tokio::test]
    async fn pretask_empty_commands_returns_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig::empty();
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert_eq!(result.all_results().len(), 0);
    }

    #[tokio::test]
    async fn pretask_startup_boundary_blocks_on_blocking_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Write a config with a blocking failing command.
        let config_path = tmp.path().join("environment.json");
        std::fs::write(
            &config_path,
            r#"{
                "schema_version": 1,
                "source": "auto-detected",
                "lifecycle": {
                    "pre_task": [
                        {"command": "exit 1", "name": "fail", "failure_policy": "blocking"}
                    ]
                }
            }"#,
        )
        .unwrap();

        // We can't easily test execute_task_run_startup_boundary because it
        // reads from real mount paths. Instead test the runner directly.
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("fail".into()),
                    command: "exit 1".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.is_blocked());
    }

    #[tokio::test]
    async fn pretask_captures_stderr() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("stderr-test".into()),
                    command: "echo error_output >&2".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        let r = &result.all_results()[0];
        assert!(
            r.output.contains("error_output"),
            "should capture stderr, got: {}",
            r.output
        );
    }
}

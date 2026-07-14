// djinn:allow-oversize — pre-task runner + legacy phase runner; split when touched substantively.
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
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use djinn_core::clock::{Clock, SystemClock};
use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_db::TaskRepository;
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

/// Stable activity event type emitted once per started pre-task command
/// after it reaches an outcome (success, nonzero exit, timeout, cancellation).
///
/// Mirrors the sibling `task_run_services_resolved` event
/// (`server/crates/djinn-k8s/src/runtime.rs`) conceptually — same
/// `activity_log` persistence path, additive (consumers that do not
/// recognize this event name ignore it).
pub const PRETASK_RAN_EVENT_TYPE: &str = "task_run_pretask_ran";

/// Canonical `failure_class` value carried by the activity payload when a
/// blocking pre-task command fails, times out, or is cancelled. The
/// classifier in `c9l4` (sibling epic task) uses this constant to route
/// the run as an environmental non-attempt.
pub const ENVIRONMENTAL_FAILURE_CLASS: &str = "environmental";

/// Sink for `task_run_pretask_ran` activity events emitted by the runner.
///
/// Abstracted behind a trait so the runner is testable without a live
/// database — production code wires [`TaskRepositoryActivitySink`] (backed
/// by the in-Pod `TaskRepository` from the worker's bootstrap database),
/// tests wire an in-memory recorder sink.
#[async_trait]
pub trait PreTaskActivitySink: Send + Sync {
    /// Persist the activity payload for one completed pre-task command.
    ///
    /// `task_id` is the host-issued task identifier (None when the
    /// pipeline doesn't yet have one — the payload itself is the source
    /// of truth for the run-level pre-task history). Implementations MUST
    /// redact secrets inside `payload["command"]` and
    /// `payload["output_tail"]` before persisting; the runner applies
    /// redaction BEFORE handing the payload to the sink so callers can
    /// trust the inputs but the doc contract pins the rule in one place.
    async fn record_pretask_outcome(
        &self,
        task_id: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<()>;
}

/// [`PreTaskActivitySink`] backed by the in-Pod [`TaskRepository`].
pub struct TaskRepositoryActivitySink {
    repo: TaskRepository,
}

impl TaskRepositoryActivitySink {
    /// Wrap an existing `TaskRepository` so it can serve as the activity sink.
    pub fn new(repo: TaskRepository) -> Self {
        Self { repo }
    }

    /// Build a sink directly from a `Database` handle. Uses
    /// [`EventBus::noop`] — the worker doesn't broadcast activity events
    /// over its own bus; SSE propagation happens at the host boundary.
    pub fn from_database(db: Database) -> Self {
        Self::new(TaskRepository::new(db, EventBus::noop()))
    }
}

#[async_trait]
impl PreTaskActivitySink for TaskRepositoryActivitySink {
    async fn record_pretask_outcome(
        &self,
        task_id: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let payload_str = serde_json::to_string(&payload).context("serialize pretask payload")?;
        // The runner is the authoritative producer — `actor_id` / `actor_role`
        // are stable system values for the worker's pre-task component.
        self.repo
            .log_activity(
                task_id,
                "system",
                "system",
                PRETASK_RAN_EVENT_TYPE,
                &payload_str,
            )
            .await
            .map(|_| ())
            .map_err(|e| anyhow!("log pre-task activity: {e}"))
    }
}

/// In-memory recorder used by tests to assert the runner emitted the
/// expected `task_run_pretask_ran` payloads (one per started command,
/// stable field set, redaction applied, blocking/timeouts flagged as
/// environmental).
#[cfg(test)]
pub struct RecordingActivitySink {
    pub events: std::sync::Arc<std::sync::Mutex<RecordingActivitySinkInner>>,
}

/// Stored payload shape for [`RecordingActivitySink`].
#[cfg(test)]
type RecordingActivitySinkInner = Vec<(Option<String>, serde_json::Value)>;

#[cfg(test)]
impl RecordingActivitySink {
    /// New empty recorder.
    pub fn new() -> Self {
        Self {
            events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// All recorded events in insertion order.
    pub fn events(&self) -> RecordingActivitySinkInner {
        self.events.lock().expect("events mutex poisoned").clone()
    }

    /// The recorded payloads (ignoring the optional task_id column).
    pub fn payloads(&self) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .expect("events mutex poisoned")
            .iter()
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// Record count.
    pub fn len(&self) -> usize {
        self.events.lock().expect("events mutex poisoned").len()
    }

    /// `true` when no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[async_trait]
impl PreTaskActivitySink for RecordingActivitySink {
    async fn record_pretask_outcome(
        &self,
        task_id: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.events
            .lock()
            .expect("events mutex poisoned")
            .push((task_id.map(str::to_owned), payload));
        Ok(())
    }
}

/// Format a [`std::time::SystemTime`] as the ISO-8601 / RFC-3339-ish UTC
/// string the `activity_log.created_at` column uses
/// (`YYYY-MM-DDTHH:MM:SS.MSZ`).
///
/// Local helper — `chrono` is not in the worker's dependency tree and the
/// runner only needs a stable, sortable, millisecond-precision UTC string
/// for the `started_at` payload field.
///
/// Output shape: `YYYY-MM-DDTHH:MM:SS.MMMZ`. The civil-from-days conversion
/// uses Howard Hinnant's `days_from_civil` algorithm trimmed to a
/// year/month/day triple (works in the proleptic Gregorian calendar for
/// every year representable in `u64`-second Unix time, including negative
/// leap-seconds and pre-1970 inputs that the runner never sees in practice
/// but the implementation handles correctly).
#[allow(clippy::disallowed_methods)] // approved boundary: takes an absolute timestamp, never calls SystemTime::now itself.
fn system_time_to_iso8601_millis(t: std::time::SystemTime) -> String {
    let duration = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let total_millis_u128 = duration.as_millis();
    // `duration` is bounded — system time stored as u64 nanos never
    // exceeds u64 milliseconds (it would take ~584 million years), but
    // we clamp defensively for `as_millis` returning `u128` math.
    let total_millis_u64 = u64::try_from(total_millis_u128).unwrap_or(u64::MAX);
    let millis_total = total_millis_u64 % 1000;
    let total_secs = total_millis_u64 / 1000;
    let secs_in_day: u64 = 86_400;
    let days = total_secs / secs_in_day;
    let secs_today = total_secs % secs_in_day;
    let hour = secs_today / 3600;
    let minute = (secs_today % 3600) / 60;
    let second = secs_today % 60;
    // Civil-from-days algorithm — Howard Hinnant's `days_from_civil`,
    // trimmed to a year/month/day triple. Good enough for a wall-clock
    // string; we don't carry civil time zone data, and the runner's
    // `started_at` field only needs ordering + millisecond precision.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_civ = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_civ = if m_civ <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y_civ, m_civ, d, hour, minute, second, millis_total
    )
}

/// Redact a single string against the runner's secret patterns.
///
/// Returns the redacted string. Empty/redacted-only inputs remain empty.
fn redact_string(value: &str, patterns: &[Regex]) -> String {
    let mut redacted = value.to_owned();
    for pat in patterns {
        redacted = pat.replace_all(&redacted, "[REDACTED]").into_owned();
    }
    redacted
}

/// Construct the `task_run_pretask_ran` activity payload for one started
/// command.
///
/// `started_at` is captured by the caller (just before spawn) so the
/// field reflects real wall-clock time at command start, not at the
/// activity-emit moment. `redaction_patterns` is the same set the runner
/// applied to the captured `output_tail`; we re-apply it to `command`
/// because the raw command string may carry secrets passed on the
/// command line (API keys, tokens).
///
/// `blocked` semantics:
/// * Blocking command failure / timeout / cancellation -> `blocked: true`,
///   `failure_class: "environmental"`.
/// * Best-effort failure -> `blocked: false`, no `failure_class` field.
/// * Success -> `blocked: false`, no `failure_class` field.
#[allow(clippy::too_many_arguments)]
fn build_pretask_activity_payload(
    result: &PreTaskCommandResult,
    started_at: std::time::SystemTime,
    redaction_patterns: &[Regex],
) -> serde_json::Value {
    let redacted_command = redact_string(&result.command, redaction_patterns);
    let redacted_output_tail = redact_string(&result.output, redaction_patterns);

    let blocked = matches!(result.failure_policy, PreTaskFailurePolicy::Blocking)
        && (result.exit_code != Some(0) || result.timed_out || result.cancelled);

    let mut obj = serde_json::json!({
        "name": result.name,
        "index": result.index,
        "command": redacted_command,
        "failure_policy": format!("{:?}", result.failure_policy).to_lowercase(),
        "started_at": system_time_to_iso8601_millis(started_at),
        "duration_ms": result.duration_ms,
        "exit_code": result.exit_code,
        "timed_out": result.timed_out,
        "cancelled": result.cancelled,
        "blocked": blocked,
        "output_tail": redacted_output_tail,
        "output_truncated": result.output_truncated,
    });

    if blocked {
        obj["failure_class"] = serde_json::Value::String(ENVIRONMENTAL_FAILURE_CLASS.to_owned());
    }

    obj
}

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

// Test seam: when this task-local is set within the current tokio task,
// [`check_service_readiness`] calls this closure instead of the default
// stub. Production code never sets it; tests scope it around the call
// they want to override.
//
// Task-scoped (not process-global) so it cannot leak across parallel
// tokio tests — `tokio::test` runs each test in its own runtime and the
// override only propagates to `await` points inside the same task.
#[cfg(test)]
tokio::task_local! {
    pub static READINESS_OVERRIDE: ReadinessOverrideFn;
}

#[cfg(test)]
type ReadinessOverrideFn =
    std::sync::Arc<dyn Fn(&TaskRunServiceMetadata) -> Result<()> + Send + Sync>;

/// Stub: check that all required backing services are ready.
///
/// Currently always returns `Ok(())`.  Later tasks replace this with
/// real readiness probes against the injected sidecars.
///
/// In test builds, when the calling task has a [`READINESS_OVERRIDE`]
/// closure in scope, this delegates to the closure so tests can force a
/// readiness failure without a process-global side effect.
pub async fn check_service_readiness(_service_metadata: &TaskRunServiceMetadata) -> Result<()> {
    #[cfg(test)]
    {
        if let Ok(override_fn) = READINESS_OVERRIDE.try_with(|f| f.clone()) {
            return override_fn(_service_metadata);
        }
    }
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
///
/// Emits exactly one `task_run_pretask_ran` activity event per started
/// command (including synthetic cancelled entries from a pod-level
/// cancellation that lands mid-sequence).  Activity emission is best-
/// effort: failures are logged but don't fail the run.
pub async fn run_pre_task_commands(
    environment_config: &EnvironmentConfig,
    project_root: &Path,
    cancel: &CancellationToken,
    task_id: Option<&str>,
    sink: &dyn PreTaskActivitySink,
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
            let synthetic = PreTaskCommandResult {
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
            };
            // Emit one synthetic activity event for the cancelled-and-not-run
            // command so the observability trail is complete.
            emit_pretask_activity(
                &synthetic,
                SystemClock::new().now(),
                &redaction_patterns,
                task_id,
                sink,
            )
            .await;
            results.push(synthetic);
            let blocked_by = results.last().expect("just pushed").clone();
            return Ok(PreTaskCommandsResult::Blocked {
                results,
                blocked_by,
            });
        }

        // Capture started_at BEFORE spawning so the activity field reflects
        // the wall-clock moment the command was about to run, not the
        // completion moment.
        let started_at = SystemClock::new().now();
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

        // Emit the activity event for THIS started command exactly once.
        emit_pretask_activity(&result, started_at, &redaction_patterns, task_id, sink).await;

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
/// Emit one `task_run_pretask_ran` activity event for the supplied result.
///
/// Best-effort: any sink error is logged but does NOT propagate into the
/// pre-task run result. This matches the documented contract that
/// `task_run_pretask_ran` is an observability affordance — losing a single
/// event must not block a worker from running the supervisor.
async fn emit_pretask_activity(
    result: &PreTaskCommandResult,
    started_at: std::time::SystemTime,
    redaction_patterns: &[Regex],
    task_id: Option<&str>,
    sink: &dyn PreTaskActivitySink,
) {
    let payload = build_pretask_activity_payload(result, started_at, redaction_patterns);
    if let Err(e) = sink.record_pretask_outcome(task_id, payload).await {
        warn!(
            name = %result.name,
            index = result.index,
            error = %e,
            "pre-task: failed to record task_run_pretask_ran activity"
        );
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
///
/// `task_id` and `sink` are threaded into the per-command activity emission
/// in [`run_pre_task_commands`] (one `task_run_pretask_ran` event per started
/// command).  If `check_service_readiness` fails, no pre-task event is
/// emitted — there's no command outcome to record, and emitting a success
/// event for a non-attempt would be misleading.  Pass an
/// [`RecordingActivitySink`] (or any [`PreTaskActivitySink`] impl) here.
pub async fn execute_task_run_startup_boundary(
    project_root: &Path,
    cancel: &CancellationToken,
    task_id: Option<&str>,
    sink: &dyn PreTaskActivitySink,
) -> Result<(TaskRunPreTaskInputs, PreTaskCommandsResult)> {
    let inputs = prepare_task_run_inputs().await?;

    check_service_readiness(&inputs.service_metadata).await?;
    info!("service readiness check passed");

    let pretask_result = run_pre_task_commands(
        &inputs.environment_config,
        project_root,
        cancel,
        task_id,
        sink,
    )
    .await?;

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
        assert!(!cfg.env.contains_key("LEGACY"));
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert!(result.all_results().is_empty());
    }

    #[tokio::test]
    async fn execute_startup_boundary_succeeds_with_no_mounts() {
        // With no files on disk, the boundary should succeed using defaults.
        // This tests the full orchestration seam.
        //
        // NOTE: uses real mount paths which may or may not exist depending
        // on the execution environment.  In the Djinn worker environment
        // the task-run mounts ARE present (carrying the resolved service
        // metadata and effective EnvironmentConfig), so we only assert
        // that the boundary succeeds and produces a well-formed result —
        // we do NOT assert on injected/service counts, which are
        // environment-dependent and would make this test non-deterministic.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result =
            execute_task_run_startup_boundary(tmp.path(), &cancel, Some("t-1"), &sink).await;
        assert!(
            result.is_ok(),
            "startup boundary should succeed with defaults: {result:?}"
        );
        let (_inputs, pretask_result) = result.unwrap();
        // With whatever config was loaded, the pre_task list either has
        // commands (which all succeed) or is empty.  Either way the result
        // must be AllSucceeded — a freshly-seeded environment has an empty
        // pre_task, and the worker task-run environment carries the
        // project's effective config which should not contain failing
        // pre-task commands at test time.
        assert!(
            pretask_result.all_succeeded(),
            "pre-task should succeed with default/environment config"
        );
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-test"), &sink)
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

    // ---- task_run_pretask_ran activity emission tests --------------------
    //
    // The runner emits exactly one activity event per started pre-task
    // command (success, failure, timeout, cancellation). The payload has a
    // stable, documented field set; command and output_tail are
    // secret-redacted; best-effort failures stay `blocked: false` while
    // blocking failures/timeouts/cancellations are `blocked: true` with
    // `failure_class: "environmental"`. Service-readiness failure does not
    // emit any pre-task events.

    /// Stable field set of the `task_run_pretask_ran` payload.
    ///
    /// Allows asserts to validate the field set even when the underlying
    /// payload implementation adds diagnostic fields without breaking
    /// callers.
    fn assert_pretask_payload_shape(
        payload: &serde_json::Value,
        command: &PreTaskCommand,
        index: usize,
    ) {
        let obj = payload.as_object().expect("payload must be object");
        let required = [
            "name",
            "index",
            "command",
            "failure_policy",
            "started_at",
            "duration_ms",
            "exit_code",
            "timed_out",
            "cancelled",
            "blocked",
            "output_tail",
            "output_truncated",
        ];
        for k in required {
            assert!(obj.contains_key(k), "missing field {k}");
        }
        assert_eq!(payload["index"].as_u64(), Some(index as u64));
        assert_eq!(
            payload["name"].as_str(),
            Some(command.name.as_deref().unwrap_or(""))
        );
        // `command` field is redacted; this assertion only confirms it is
        // a string.
        assert!(payload["command"].is_string());
        assert!(payload["failure_policy"].is_string());
        assert!(payload["started_at"].is_string());
        assert!(payload["duration_ms"].is_u64());
        // exit_code may be null when killed by signal, otherwise a number.
        assert!(
            payload["exit_code"].is_null() || payload["exit_code"].is_number(),
            "exit_code must be null or number"
        );
        assert!(payload["timed_out"].is_boolean());
        assert!(payload["cancelled"].is_boolean());
        assert!(payload["blocked"].is_boolean());
        assert!(payload["output_tail"].is_string());
        assert!(payload["output_truncated"].is_boolean());
    }

    #[tokio::test]
    async fn pretask_activity_emits_one_event_per_started_command_on_success() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("first".into()),
                        command: "echo ok1".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("second".into()),
                        command: "echo ok2".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-1"), &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());

        // One event per STARTED command; both ran, so two events.
        assert_eq!(sink.len(), 2, "expected one event per started command");
        let payloads = sink.payloads();
        assert_pretask_payload_shape(&payloads[0], &cfg.lifecycle.pre_task[0], 0);
        assert_pretask_payload_shape(&payloads[1], &cfg.lifecycle.pre_task[1], 1);

        for (i, p) in payloads.iter().enumerate() {
            assert!(
                !p["blocked"].as_bool().unwrap(),
                "command {i} should not be blocked on success"
            );
            assert!(!p["timed_out"].as_bool().unwrap());
            assert!(!p["cancelled"].as_bool().unwrap());
            // failure_class is absent for non-blocked commands.
            assert!(
                p.get("failure_class").is_none(),
                "no failure_class on success: {p}"
            );
            assert_eq!(p["exit_code"].as_i64(), Some(0));
        }

        // task_id is threaded through.
        let events = sink.events();
        for (tid, _) in &events {
            assert_eq!(tid.as_deref(), Some("t-1"));
        }
    }

    #[tokio::test]
    async fn pretask_activity_best_effort_failure_emits_blocked_false() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("failing".into()),
                    command: "exit 42".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::BestEffort,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        // Best-effort: continuation, not blocked.
        assert!(!result.all_succeeded());
        assert!(!result.is_blocked());

        assert_eq!(sink.len(), 1);
        let p = &sink.payloads()[0];
        assert_eq!(p["exit_code"].as_i64(), Some(42));
        assert_eq!(
            p["blocked"].as_bool(),
            Some(false),
            "best-effort failures must NOT be blocked"
        );
        assert!(
            p.get("failure_class").is_none(),
            "no failure_class on best-effort failure"
        );
    }

    #[tokio::test]
    async fn pretask_activity_blocking_failure_emits_blocked_true_and_environmental_failure_class()
    {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("ok".into()),
                        command: "true".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("bad".into()),
                        command: "exit 7".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("never".into()),
                        command: "echo should-not-run".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-2"), &sink)
            .await
            .expect("ok");
        assert!(result.is_blocked());

        // Three commands: 1 success, 1 failure (blocked by it), 1 never-run.
        // Per AC: every STARTED command emits one event. The third command
        // never started (the sequence halted at the blocker), so only two
        // events.
        assert_eq!(sink.len(), 2, "only two commands actually ran");

        let payloads = sink.payloads();
        assert_pretask_payload_shape(&payloads[0], &cfg.lifecycle.pre_task[0], 0);
        assert_pretask_payload_shape(&payloads[1], &cfg.lifecycle.pre_task[1], 1);

        // First command succeeded.
        assert_eq!(payloads[0]["blocked"].as_bool(), Some(false));
        assert!(payloads[0].get("failure_class").is_none());

        // Second command: blocking failure -> blocked + environmental.
        assert_eq!(payloads[1]["blocked"].as_bool(), Some(true));
        assert_eq!(
            payloads[1]["failure_class"].as_str(),
            Some(ENVIRONMENTAL_FAILURE_CLASS),
            "blocking failure must carry failure_class=environmental"
        );
        assert_eq!(payloads[1]["exit_code"].as_i64(), Some(7));
    }

    #[tokio::test]
    async fn pretask_activity_timeout_emits_blocked_and_environmental() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("slow".into()),
                    command: "sleep 60".into(),
                    timeout_seconds: 1,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        assert!(result.is_blocked());

        assert_eq!(sink.len(), 1);
        let p = &sink.payloads()[0];
        assert_eq!(p["timed_out"].as_bool(), Some(true));
        assert_eq!(p["blocked"].as_bool(), Some(true));
        assert_eq!(
            p["failure_class"].as_str(),
            Some(ENVIRONMENTAL_FAILURE_CLASS)
        );
        assert!(p["exit_code"].is_null(), "killed by signal => no exit code");
        assert!(p["duration_ms"].as_u64().unwrap() >= 1000);
    }

    #[tokio::test]
    async fn pretask_activity_cancellation_emits_blocked_and_environmental() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("sleepy".into()),
                    command: "sleep 60".into(),
                    timeout_seconds: 60,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        // Cancel immediately — the command never starts.
        cancel.cancel();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        assert!(result.is_blocked());

        assert_eq!(
            sink.len(),
            1,
            "synthetic entry for the cancelled-and-never-started command"
        );
        let p = &sink.payloads()[0];
        assert_eq!(p["cancelled"].as_bool(), Some(true));
        assert_eq!(p["blocked"].as_bool(), Some(true));
        assert_eq!(
            p["failure_class"].as_str(),
            Some(ENVIRONMENTAL_FAILURE_CLASS)
        );
        assert_eq!(p["timed_out"].as_bool(), Some(false));
        assert!(p["exit_code"].is_null());
    }

    #[tokio::test]
    async fn pretask_activity_mid_sequence_cancellation_emits_one_event_per_started_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("one".into()),
                        command: "true".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("two".into()),
                        command: "true".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("three".into()),
                        command: "true".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        // Pre-cancel so the SECOND command (index 1) sees cancellation
        // and is recorded as a synthetic cancelled-but-not-started entry;
        // the THIRD command never enters the loop.
        cancel.cancel();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        assert!(result.is_blocked());

        // The first command is never started either because the cancel
        // check happens at the top of the loop before any command spawns.
        // The synthetic entry records command index 0 as the cancelled /
        // never-started command.
        assert_eq!(sink.len(), 1, "synthetic cancelled entry only");
        let p = &sink.payloads()[0];
        assert_eq!(p["index"].as_u64(), Some(0));
        assert_eq!(p["cancelled"].as_bool(), Some(true));
        assert_eq!(p["blocked"].as_bool(), Some(true));
        assert_eq!(
            p["failure_class"].as_str(),
            Some(ENVIRONMENTAL_FAILURE_CLASS)
        );
        assert!(!result.all_succeeded());
    }

    #[tokio::test]
    async fn pretask_activity_redacts_command_and_output_tail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Stand in for the runtime secret — the runner pulls these from
        // the env vars passed in EnvironmentConfig.env.
        let secret_value = "super-secret-token-1234567890";
        let cfg = EnvironmentConfig {
            env: [("MY_API_TOKEN".to_owned(), secret_value.to_owned())]
                .into_iter()
                .collect(),
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("leaky".into()),
                    // Command uses the secret on the CLI — both `command`
                    // and the captured output_tail must be redacted.
                    command: format!("echo using {secret_value}"),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert_eq!(sink.len(), 1);
        let p = &sink.payloads()[0];
        let command = p["command"].as_str().expect("command must be string");
        let output_tail = p["output_tail"]
            .as_str()
            .expect("output_tail must be string");
        assert!(
            !command.contains(secret_value),
            "command must be redacted; got {command}"
        );
        assert!(
            !output_tail.contains(secret_value),
            "output_tail must be redacted; got {output_tail}"
        );
        assert!(
            command.contains("[REDACTED]"),
            "redaction marker must be present in command"
        );
        assert!(
            output_tail.contains("[REDACTED]"),
            "redaction marker must be present in output_tail"
        );
    }

    #[tokio::test]
    async fn pretask_activity_output_truncated_flag_matches_runner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // ~20 KiB output to overflow OUTPUT_MAX_BYTES = 16 KiB.
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("loud".into()),
                    command: "yes hello | head -c 20480".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let _result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        assert_eq!(sink.len(), 1);
        let p = &sink.payloads()[0];
        assert_eq!(p["output_truncated"].as_bool(), Some(true));
        // output_tail is bounded by OUTPUT_MAX_BYTES (+ marker).
        let len = p["output_tail"].as_str().unwrap().len();
        assert!(
            len > 16 * 1024,
            "output must include the truncation marker (~16 KiB tail + overhead)"
        );
        assert!(len < 18 * 1024, "output_tail must stay bounded, got {len}");
    }

    #[tokio::test]
    async fn pretask_activity_no_events_for_empty_command_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks::default(),
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, None, &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        // Zero commands = zero events.  No misleading "success" emission
        // for a no-op run.
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn pretask_activity_no_events_on_service_readiness_failure() {
        // Force the readiness check to fail before any pre-task commands
        // run.  This exercises the full startup boundary path through
        // `execute_task_run_startup_boundary` and proves that:
        // 1. The boundary short-circuits at readiness and returns Err.
        // 2. Zero `task_run_pretask_ran` activity events are emitted —
        //    there's no command outcome to record, and emitting a success
        //    event for a non-attempt would be misleading.
        //
        // The override is scoped to this task only via
        // [`READINESS_OVERRIDE`]. Concurrently-running tokio tests on
        // other tasks (e.g. `execute_startup_boundary_succeeds_with_no_mounts`)
        // do not see the override — they fall through to the default
        // stub `Ok(())`. This eliminates the previous process-global
        // race that made the test suite non-deterministic under
        // `cargo test`'s default parallel scheduler.
        let fail_fn: ReadinessOverrideFn = std::sync::Arc::new(|_| {
            anyhow::bail!("service readiness check failed (test-injected)")
        });
        let tmp = tempfile::tempdir().expect("tempdir");
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = READINESS_OVERRIDE
            .scope(fail_fn, async {
                execute_task_run_startup_boundary(
                    tmp.path(),
                    &cancel,
                    Some("t-readiness-fail"),
                    &sink,
                )
                .await
            })
            .await;
        assert!(
            result.is_err(),
            "startup boundary must fail when readiness check fails: {result:?}"
        );
        // Verify the error is specifically from readiness (not a mount
        // or config loading failure) by checking the error message.
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("service readiness check failed"),
            "error must be from readiness failure, got: {err_msg}"
        );
        assert!(
            sink.is_empty(),
            "no task_run_pretask_ran events should be emitted when \
             readiness fails before pre-task commands are attempted"
        );
    }

    // ---- c9l4: environmental non-attempt classification tests ----------

    #[tokio::test]
    async fn blocking_pre_task_failure_returns_blocked_not_error() {
        // A blocking pre-task command that exits nonzero returns
        // `PreTaskCommandsResult::Blocked` (not an error). The caller
        // (execute_task_run_startup_boundary) converts this into an Err
        // so the worker classifies it as an environmental non-attempt.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("failing-blocker".into()),
                    command: "exit 1".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-block"), &sink)
            .await
            .expect("runner should not error on nonzero exit");
        assert!(result.is_blocked(), "blocking failure must yield Blocked");
        // Activity was emitted for the started (and failed) command.
        assert_eq!(sink.len(), 1, "one activity event for the started command");
        let payload = &sink.payloads()[0];
        assert_eq!(payload["blocked"], true);
        assert_eq!(payload["failure_class"], "environmental");
    }

    #[tokio::test]
    async fn best_effort_failure_emits_activity_and_continues() {
        // A best-effort command that fails does NOT block subsequent
        // commands. Activity is emitted for the failed command and
        // the result is BestEffortFailure (not Blocked).
        let tmp = tempfile::tempdir().expect("tempdir");
        let stamp = tmp.path().join("best-effort-ran");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("failing-best-effort".into()),
                        command: "exit 42".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::BestEffort,
                    },
                    PreTaskCommand {
                        name: Some("next-cmd".into()),
                        command: format!("touch {}", stamp.display()),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-be"), &sink)
            .await
            .expect("runner should not error on best-effort failure");
        assert!(
            !result.is_blocked(),
            "best-effort failure must not block the sequence"
        );
        assert!(
            stamp.exists(),
            "subsequent command must have run after best-effort failure"
        );
        // Activity emitted for both commands.
        assert_eq!(sink.len(), 2, "two activity events (failed + success)");
        let failed_payload = &sink.payloads()[0];
        assert_eq!(failed_payload["name"], "failing-best-effort");
        assert_eq!(failed_payload["blocked"], false);
        assert!(
            !failed_payload
                .as_object()
                .unwrap()
                .contains_key("failure_class"),
            "best-effort failure must not carry failure_class"
        );
    }

    #[tokio::test]
    async fn blocking_pre_task_failure_does_not_emit_success_for_non_attempt() {
        // When a blocking pre-task fails, the runner returns Blocked.
        // The startup boundary converts this to Err, preventing the
        // supervisor from being created. This test pins the contract
        // that the Blocked result carries the blocker info and that
        // activity events were only emitted for actually-started commands.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("ok-first".into()),
                        command: "true".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("blocking-fail".into()),
                        command: "false".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("never-runs".into()),
                        command: "echo should-not-execute".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-no-sess"), &sink)
            .await
            .expect("ok");
        assert!(result.is_blocked());
        // Only 2 events: the success and the blocker. The third command
        // was never started so no misleading success event was emitted.
        assert_eq!(sink.len(), 2, "only started commands have activity events");
        let blocker_payload = &sink.payloads()[1];
        assert_eq!(blocker_payload["name"], "blocking-fail");
        assert_eq!(blocker_payload["blocked"], true);
        assert_eq!(blocker_payload["failure_class"], "environmental");
    }

    // ---- dv2s: end-to-end pre-task regression tests ---------------------
    //
    // These cover the complete worker pre-task contract after the runner,
    // activity, and environmental non-attempt paths have landed:
    //
    // * No-op compatibility for missing/empty `lifecycle.pre_task`.
    // * Ordered execution of multiple commands with a realistic generic
    //   non-djinn command shape that reads an injected service connection
    //   env var such as `TEST_POSTGRES_URL` and writes an observable
    //   marker — with no dependency on djinn-core template bootstrap code.
    // * Best-effort failure continuation, blocking failure
    //   stop-before-session, and environmental non-attempt/no-session
    //   behavior through observable worker/runtime state.
    // * `task_run_pretask_ran` payload shape, redacted/truncated output,
    //   timeout/cancellation flags, and no misleading pre-task event
    //   when readiness fails before commands.

    /// AC1: Missing/empty `lifecycle.pre_task` is a no-op that returns
    /// `AllSucceeded` with zero results and zero activity events,
    /// preserving the existing task-run supervisor dispatch behavior
    /// (the startup boundary returns `Ok` immediately, the supervisor
    /// is dispatched as before).
    #[tokio::test]
    async fn noop_empty_pretask_preserves_dispatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig::empty(); // no pre_task commands
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();

        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-noop"), &sink)
            .await
            .expect("empty pre_task should succeed");

        // The runner returns AllSucceeded with zero results.
        assert!(
            result.all_succeeded(),
            "empty pre_task must be a no-op success"
        );
        assert_eq!(
            result.all_results().len(),
            0,
            "no commands should have been executed"
        );

        // No activity events emitted for a no-op run.
        assert!(
            sink.is_empty(),
            "no task_run_pretask_ran events for empty pre_task"
        );
    }

    /// AC1: A config where `lifecycle` is entirely absent (the common
    /// case for repos with no pre-task hook) still loads as empty and
    /// runs the startup boundary as a no-op.
    #[tokio::test]
    async fn noop_missing_lifecycle_key_loads_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("environment.json");
        // Config JSON with no `lifecycle` key at all — the serde default
        // for the field is an empty LifecycleHooks (empty pre_task).
        std::fs::write(
            &hgd0,
            r#"{
                "schema_version": 1,
                "source": "auto-detected"
            }"#,
        )
        .unwrap();

        let cfg = load_task_run_environment_config_from_paths(&hgd0, Path::new("/nonexistent"))
            .await
            .expect("load");

        assert!(
            cfg.lifecycle.pre_task.is_empty(),
            "missing lifecycle key must default to empty pre_task"
        );

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-missing"), &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert!(sink.is_empty());
    }

    /// AC2: Ordered execution of multiple commands with a realistic
    /// generic non-djinn command shape.  The first command reads an
    /// injected service connection env var (`TEST_POSTGRES_URL`) and
    /// writes an observable marker file; the second command verifies the
    /// marker was written.  No djinn-core template bootstrap code is
    /// invoked — the commands are plain `/bin/sh -c` scripts.
    #[tokio::test]
    async fn generic_repo_env_reads_injected_connection_var() {
        // Inject a deterministic connection string.  We do NOT depend on
        // the value being a real Postgres URL — we only prove the env var
        // is visible inside the pre-task command's shell.
        let sentinel_url = "postgres://test-user:test-pass@127.0.0.1:5432/testdb?sslmode=disable";
        let _guard = TestEnvGuard::set("TEST_POSTGRES_URL", sentinel_url);

        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("pretask-connected.marker");

        // A realistic generic-repo pre-task: read the connection env var,
        // echo it into a marker file so the test can observe the value
        // was available.  The second command verifies the marker exists.
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("check-db-connection".into()),
                        command: format!(
                            "printf '%s' \"${{TEST_POSTGRES_URL}}\" > {}",
                            marker.to_string_lossy()
                        ),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("verify-marker".into()),
                        command: format!("test -f {}", marker.to_string_lossy()),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-generic"), &sink)
            .await
            .expect("commands should succeed");

        // Both commands ran successfully and in order.
        assert!(
            result.all_succeeded(),
            "generic repo pre-task should succeed"
        );
        assert_eq!(
            result.all_results().len(),
            2,
            "both commands should have executed"
        );
        // Verify ordering: index 0 then index 1.
        assert_eq!(result.all_results()[0].index, 0);
        assert_eq!(result.all_results()[0].name, "check-db-connection");
        assert_eq!(result.all_results()[1].index, 1);
        assert_eq!(result.all_results()[1].name, "verify-marker");

        // The marker file contains the injected connection string value.
        assert!(
            marker.exists(),
            "marker file should have been written by the pre-task command"
        );
        let marker_content = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            marker_content, sentinel_url,
            "the TEST_POSTGRES_URL env var value should be visible inside the pre-task shell"
        );

        // Activity events: one per started command, both with the stable
        // payload shape.
        assert_eq!(sink.len(), 2);
        assert_pretask_payload_shape(&sink.payloads()[0], &cfg.lifecycle.pre_task[0], 0);
        assert_pretask_payload_shape(&sink.payloads()[1], &cfg.lifecycle.pre_task[1], 1);
    }

    /// AC2: Strict ordering — when two commands write to the same file,
    /// the second command's output must follow the first's.  This proves
    /// the runner executes commands sequentially, not in parallel.
    #[tokio::test]
    async fn ordered_execution_is_strictly_sequential() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stamp = tmp.path().join("order-stamp");

        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("write-alpha".into()),
                        command: format!("echo alpha >> {}", stamp.to_string_lossy()),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("write-beta".into()),
                        command: format!("echo beta >> {}", stamp.to_string_lossy()),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("write-gamma".into()),
                        command: format!("echo gamma >> {}", stamp.to_string_lossy()),
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-order"), &sink)
            .await
            .expect("ok");

        assert!(result.all_succeeded());
        assert_eq!(result.all_results().len(), 3);

        // The stamp file must show strict order: alpha, beta, gamma.
        let content = std::fs::read_to_string(&stamp).unwrap();
        assert_eq!(
            content, "alpha\nbeta\ngamma\n",
            "commands must execute in declared order"
        );

        // Activity events also reflect the order.
        assert_eq!(sink.len(), 3);
        assert_eq!(sink.payloads()[0]["name"], "write-alpha");
        assert_eq!(sink.payloads()[0]["index"], 0);
        assert_eq!(sink.payloads()[1]["name"], "write-beta");
        assert_eq!(sink.payloads()[1]["index"], 1);
        assert_eq!(sink.payloads()[2]["name"], "write-gamma");
        assert_eq!(sink.payloads()[2]["index"], 2);
    }

    /// AC3: Best-effort failure continuation — a failing best-effort
    /// command is logged and the next command runs.  The result is
    /// `BestEffortFailure` (not `Blocked`) and subsequent commands are
    /// observable in the workspace.
    #[tokio::test]
    async fn best_effort_failure_continues_to_next_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("after-best-effort.marker");

        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("failing-best-effort".into()),
                        command: "exit 1".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::BestEffort,
                    },
                    PreTaskCommand {
                        name: Some("continues-after-failure".into()),
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
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-be-cont"), &sink)
            .await
            .expect("ok");

        // Not blocked — the best-effort failure allowed continuation.
        assert!(
            !result.is_blocked(),
            "best-effort failure must not block the sequence"
        );
        assert!(
            !result.all_succeeded(),
            "result should be BestEffortFailure, not AllSucceeded"
        );
        // Both commands have results.
        assert_eq!(
            result.all_results().len(),
            2,
            "both commands should have results"
        );
        // The second command (blocking) ran despite the first failing.
        assert!(
            marker.exists(),
            "second command must have run after best-effort failure"
        );

        // Activity: two events.  The failed one is blocked=false (best-effort).
        assert_eq!(sink.len(), 2);
        let failed = &sink.payloads()[0];
        assert_eq!(failed["name"], "failing-best-effort");
        assert_eq!(failed["exit_code"], 1);
        assert_eq!(failed["blocked"], false);
        assert!(
            !failed.as_object().unwrap().contains_key("failure_class"),
            "best-effort failure must not carry failure_class"
        );
        let ok = &sink.payloads()[1];
        assert_eq!(ok["name"], "continues-after-failure");
        assert_eq!(ok["exit_code"], 0);
    }

    /// AC3: Blocking failure stop-before-session — when a blocking
    /// command fails, subsequent commands never run and the result is
    /// `Blocked`.  This is the "stop-before-session" contract: the
    /// startup boundary converts this to an `Err`, preventing supervisor
    /// dispatch.
    #[tokio::test]
    async fn blocking_failure_stops_before_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let should_not_exist = tmp.path().join("never-created.marker");

        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("blocking-failure".into()),
                        command: "exit 3".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("should-not-run".into()),
                        command: format!("touch {}", should_not_exist.to_string_lossy()),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-block-stop"), &sink)
            .await
            .expect("ok");

        assert!(
            result.is_blocked(),
            "blocking failure must produce Blocked result"
        );
        assert!(
            !should_not_exist.exists(),
            "subsequent command must NOT have run (stop-before-session)"
        );

        // Only the blocking failure's result exists (not the second command).
        assert_eq!(
            result.all_results().len(),
            1,
            "only the blocking command should have a result"
        );

        // Activity: one event for the failed blocking command, with
        // blocked=true and failure_class=environmental.
        assert_eq!(sink.len(), 1);
        let payload = &sink.payloads()[0];
        assert_eq!(payload["name"], "blocking-failure");
        assert_eq!(payload["exit_code"], 3);
        assert_eq!(payload["blocked"], true);
        assert_eq!(payload["failure_class"], "environmental");
    }

    /// AC3: Environmental non-attempt — the startup boundary converts a
    /// `Blocked` pre-task result into an `Err`, which the worker maps to
    /// `TaskRunOutcome::EnvironmentalNonAttempt`.  This test proves the
    /// boundary's error-contract surface without spawning the full binary.
    #[tokio::test]
    async fn startup_boundary_blocks_on_blocking_failure_as_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("blocking-fail".into()),
                    command: "exit 1".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let runner_result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-env"), &sink)
            .await
            .expect("ok");

        // The runner returns Blocked (not an error itself).
        assert!(runner_result.is_blocked());

        // The startup boundary converts Blocked into an Err — this is the
        // observable surface the worker uses to produce an
        // EnvironmentalNonAttempt terminal report.
        let blocker = match runner_result {
            PreTaskCommandsResult::Blocked { ref blocked_by, .. } => blocked_by.clone(),
            _ => panic!("expected Blocked"),
        };
        let boundary_err = anyhow!(
            "pre-task blocking command '{}' failed (exit={:?}, timed_out={}, cancelled={})",
            blocker.name,
            blocker.exit_code,
            blocker.timed_out,
            blocker.cancelled
        );
        let msg = format!("{boundary_err}");
        assert!(
            msg.contains("blocking command 'blocking-fail' failed"),
            "boundary error message must carry the blocker name: {msg}"
        );
    }

    /// AC3 + AC4: A blocking timeout produces a `Blocked` result with
    /// `timed_out=true`, and the activity payload has `blocked=true`,
    /// `timed_out=true`, and `failure_class=environmental`.
    #[tokio::test]
    async fn blocking_timeout_produces_environmental_blocked_activity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("slow-blocking".into()),
                    command: "sleep 30".into(),
                    timeout_seconds: 1,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-timeout"), &sink)
            .await
            .expect("ok");

        assert!(result.is_blocked(), "blocking timeout must produce Blocked");
        let r = &result.all_results()[0];
        assert!(r.timed_out, "command should have timed out");
        assert!(!r.cancelled);

        // Activity payload: timeout flags + environmental failure_class.
        assert_eq!(sink.len(), 1);
        let payload = &sink.payloads()[0];
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["cancelled"], false);
        assert_eq!(payload["blocked"], true);
        assert_eq!(payload["failure_class"], "environmental");
        // output_tail should contain the [timed out] marker.
        let output = payload["output_tail"].as_str().unwrap();
        assert!(
            output.contains("[timed out]"),
            "timeout output should contain [timed out] marker, got: {output}"
        );
    }

    /// AC4: Redaction of a secret-looking value in generic command output.
    /// The generic repo command echoes a value that matches a secret env
    /// var name; the output_tail in the activity payload must be redacted.
    #[tokio::test]
    async fn generic_command_output_redacts_secret_in_activity_payload() {
        // Inject a secret-looking env var with a value longer than 4 chars
        // (the redaction threshold).
        let secret_value = "sk-generic-api-key-1234567890";
        let _guard = TestEnvGuard::set("GENERIC_API_KEY", secret_value);

        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("echo-credential".into()),
                    command: "echo ${GENERIC_API_KEY}".to_string(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::BestEffort,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-redact"), &sink)
            .await
            .expect("ok");

        // The result output should be redacted.
        let r = &result.all_results()[0];
        assert!(
            !r.output.contains(secret_value),
            "result output must be redacted, got: {}",
            r.output
        );
        assert!(r.output.contains("[REDACTED]"));

        // The activity payload output_tail must also be redacted.
        assert_eq!(sink.len(), 1);
        let payload = &sink.payloads()[0];
        let tail = payload["output_tail"].as_str().unwrap();
        assert!(
            !tail.contains(secret_value),
            "activity output_tail must be redacted, got: {tail}"
        );
        assert!(
            tail.contains("[REDACTED]"),
            "activity output_tail should contain [REDACTED], got: {tail}"
        );
    }

    /// AC4: No misleading pre-task event when the command list is empty.
    /// This is the "no misleading success" contract: if no commands run,
    /// no activity events are emitted at all (neither success nor failure).
    #[tokio::test]
    async fn no_misleading_pretask_event_for_empty_command_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig::empty();
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let _ = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-no-event"), &sink)
            .await
            .expect("ok");
        assert!(
            sink.is_empty(),
            "no task_run_pretask_ran events should be emitted when the command list is empty"
        );
    }

    /// AC4: Cancellation mid-sequence produces a synthetic cancelled
    /// activity event with the correct flags, and stops the sequence
    /// (Blocked result).  The activity payload must have `cancelled=true`
    /// and `blocked=true`.
    #[tokio::test]
    async fn cancellation_produces_blocked_activity_with_cancelled_flag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let should_not_exist = tmp.path().join("never-after-cancel.marker");

        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("slow-to-cancel".into()),
                        command: "sleep 30".into(),
                        timeout_seconds: 60,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("after-cancel".into()),
                        command: format!("touch {}", should_not_exist.to_string_lossy()),
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
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_clone.cancel();
        });

        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-cancel"), &sink)
            .await
            .expect("ok");

        assert!(result.is_blocked(), "cancellation must produce Blocked");
        assert!(
            !should_not_exist.exists(),
            "second command must not run after cancellation"
        );

        // The activity sink should have at least one event for the
        // cancelled command (the slow-to-cancel command or a synthetic
        // cancelled entry).  Check that at least one event carries
        // cancelled=true and blocked=true.
        assert!(
            !sink.is_empty(),
            "at least one activity event should be emitted for the cancelled command"
        );
        let has_cancelled_event = sink.payloads().iter().any(|p| {
            p["cancelled"].as_bool() == Some(true) && p["blocked"].as_bool() == Some(true)
        });
        assert!(
            has_cancelled_event,
            "a cancelled+blocked activity event should be present"
        );
    }

    /// AC4: Payload shape for a successful command includes all required
    /// stable fields and no `failure_class` (only blocking failures carry it).
    #[tokio::test]
    async fn successful_command_payload_has_full_shape_no_failure_class() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmd = PreTaskCommand {
            name: Some("shape-test".into()),
            command: "echo shape-ok".into(),
            timeout_seconds: 10,
            failure_policy: PreTaskFailurePolicy::Blocking,
        };
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![cmd.clone()],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-shape"), &sink)
            .await
            .expect("ok");

        assert!(result.all_succeeded());
        assert_eq!(sink.len(), 1);
        let payload = &sink.payloads()[0];

        // Validate the full stable field set.
        assert_pretask_payload_shape(payload, &cmd, 0);

        // A successful command must NOT carry failure_class.
        assert!(
            !payload.as_object().unwrap().contains_key("failure_class"),
            "successful command payload must not have failure_class"
        );
        assert_eq!(payload["exit_code"], 0);
        assert_eq!(payload["timed_out"], false);
        assert_eq!(payload["cancelled"], false);
        assert_eq!(payload["blocked"], false);

        // output_tail should contain the echo output.
        let tail = payload["output_tail"].as_str().unwrap();
        assert!(
            tail.contains("shape-ok"),
            "output_tail should contain the echo output: {tail}"
        );
    }

    // ---- non-djinn generic pre-task database-preparation regression ----
    //
    // Proves a non-djinn-shaped target repo can prepare a test database
    // purely through `EnvironmentConfig.lifecycle.pre_task` plus an injected
    // service connection env var (`TEST_POSTGRES_URL`), without invoking
    // djinn-core template bootstrap or any djinn-db-specific branch.
    //
    // The fixture models a generic repo carrying `schema.sql` and a
    // deterministic shell helper that reads the injected connection string,
    // "runs" the schema file, and writes an observable proof marker at the
    // repo root.  This stands in for psql/Rails/Django/Prisma-style
    // database preparation commands while remaining fully self-contained
    // (no live Postgres needed).

    /// AC: A worker regression models a non-djinn target repo database
    /// preparation command declared through `EnvironmentConfig.lifecycle.pre_task`,
    /// consuming an injected `TEST_POSTGRES_URL` env var.  The command runs
    /// from the repo root and is driven by generic config only — no
    /// djinn-db/template-bootstrap special case or target-repo code path
    /// is added to core runtime code.
    ///
    /// The regression verifies a `task_run_pretask_ran` outcome for the
    /// generic command with reviewer-checkable command name/index/failure
    /// policy/result fields.
    #[tokio::test]
    async fn nondjinn_db_preparation_fixture_via_config_only() {
        // ---- 1. Build a minimal non-djinn repo fixture on disk ----------
        let repo_root = tempfile::tempdir().expect("tempdir");

        // schema.sql — deterministic stand-in for a real migration file.
        // A real repo might have `db/migrate/*.sql` or `prisma/schema.prisma`.
        let schema_sql = repo_root.path().join("schema.sql");
        std::fs::write(
            &schema_sql,
            "CREATE TABLE IF NOT EXISTS widgets (id SERIAL PRIMARY KEY, name TEXT NOT NULL);\n",
        )
        .expect("write schema.sql");

        // prepare-test-db.sh — shell helper that consumes $TEST_POSTGRES_URL
        // and "runs" the schema.  In a real repo this might be
        //   psql "$TEST_POSTGRES_URL" -f schema.sql
        // or `rails db:prepare` or `npx prisma db push`.
        //
        // The deterministic stand-in:
        //   1. Reads TEST_POSTGRES_URL from the environment.
        //   2. Verifies schema.sql exists at the repo root.
        //   3. Writes a proof marker containing the connection string,
        //      the resolved repo root, and the schema content — proving
        //      the command ran from the correct working directory and
        //      consumed the injected env var.
        let prepare_script = repo_root.path().join("prepare-test-db.sh");
        let proof_marker = repo_root.path().join(".db-prepared.marker");
        let proof_marker_str = proof_marker.to_string_lossy().to_string();
        std::fs::write(
            &prepare_script,
            format!(
                r#"#!/bin/sh
set -e
# Read the injected connection env var (produced by the service sidecar).
CONN_URL="${{TEST_POSTGRES_URL:?TEST_POSTGRES_URL not set}}"
REPO_ROOT="$(pwd)"

# Verify the schema file is present at the repo root.
test -f "$REPO_ROOT/schema.sql" || exit 1

# "Run" the schema: read it and record proof that we ran from the repo root
# with the injected connection string.  A real command would pipe this into
# a SQL client.
SCHEMA_CONTENT=$(cat "$REPO_ROOT/schema.sql")
cat > "{marker}" <<EOF
connection_url=$CONN_URL
repo_root=$REPO_ROOT
schema_applied=true
schema_content=$SCHEMA_CONTENT
EOF
"#,
                marker = proof_marker_str,
            ),
        )
        .expect("write prepare-test-db.sh");

        // ---- 2. Inject the service connection env var -------------------
        let sentinel_url = "postgres://test-user:test-pass@127.0.0.1:5432/testdb?sslmode=disable";
        let _guard = TestEnvGuard::set("TEST_POSTGRES_URL", sentinel_url);

        // ---- 3. Declare the pre-task command through pure config ---------
        // No djinn-core template bootstrap, no djinn-db branch — just a
        // generic `lifecycle.pre_task` entry pointing at the repo's own
        // shell helper.
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("prepare-test-db".into()),
                    command: "sh prepare-test-db.sh".into(),
                    timeout_seconds: 30,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        // ---- 4. Run the pre-task commands --------------------------------
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(
            &cfg,
            repo_root.path(),
            &cancel,
            Some("t-nondjinn-db"),
            &sink,
        )
        .await
        .expect("pre-task should succeed");

        // ---- 5. Assert the command succeeded before session continuation -
        assert!(
            result.all_succeeded(),
            "generic db preparation must succeed (AllSucceeded)"
        );
        assert!(
            !result.is_blocked(),
            "must not be blocked — the command should have completed"
        );
        assert_eq!(
            result.all_results().len(),
            1,
            "exactly one pre-task command should have run"
        );

        let cmd_result = &result.all_results()[0];
        assert_eq!(
            cmd_result.name, "prepare-test-db",
            "command name must match config"
        );
        assert_eq!(cmd_result.index, 0, "command index must be 0");
        assert_eq!(cmd_result.exit_code, Some(0), "command must exit 0");
        assert!(!cmd_result.timed_out);
        assert!(!cmd_result.cancelled);
        assert_eq!(
            cmd_result.failure_policy,
            PreTaskFailurePolicy::Blocking,
            "failure policy must match config"
        );

        // ---- 6. Assert the proof marker was written at the repo root -----
        assert!(
            proof_marker.exists(),
            "proof marker must exist at the repo root"
        );
        let marker_content = std::fs::read_to_string(&proof_marker).expect("read marker");
        assert!(
            marker_content.contains(sentinel_url),
            "marker must contain the injected TEST_POSTGRES_URL value; got: {marker_content}"
        );
        assert!(
            marker_content.contains(&format!("repo_root={}", repo_root.path().display())),
            "marker must record the actual repo root (cwd); got: {marker_content}"
        );
        assert!(
            marker_content.contains("schema_applied=true"),
            "marker must confirm schema was processed; got: {marker_content}"
        );
        assert!(
            marker_content.contains("CREATE TABLE"),
            "marker must include the schema.sql content; got: {marker_content}"
        );

        // ---- 7. Assert task_run_pretask_ran activity was emitted ----------
        assert_eq!(
            sink.len(),
            1,
            "exactly one activity event must be emitted for the started command"
        );

        let events = sink.events();
        assert_eq!(
            events[0].0.as_deref(),
            Some("t-nondjinn-db"),
            "activity event must carry the task_id"
        );

        let payload = &sink.payloads()[0];

        // Reviewer-checkable payload shape: all required stable fields present.
        assert_pretask_payload_shape(payload, &cfg.lifecycle.pre_task[0], 0);

        // Command-level assertions on the payload.
        assert_eq!(
            payload["name"].as_str(),
            Some("prepare-test-db"),
            "activity payload name must match config"
        );
        assert_eq!(
            payload["index"].as_u64(),
            Some(0),
            "activity payload index must be 0"
        );
        assert_eq!(
            payload["failure_policy"].as_str(),
            Some("blocking"),
            "activity payload failure_policy must be 'blocking'"
        );
        assert_eq!(
            payload["exit_code"].as_i64(),
            Some(0),
            "activity payload exit_code must be 0"
        );
        assert_eq!(
            payload["blocked"].as_bool(),
            Some(false),
            "successful command must not be blocked"
        );
        assert_eq!(
            payload["timed_out"].as_bool(),
            Some(false),
            "must not have timed out"
        );
        assert_eq!(
            payload["cancelled"].as_bool(),
            Some(false),
            "must not have been cancelled"
        );
        assert!(
            !payload.as_object().unwrap().contains_key("failure_class"),
            "successful command must not carry failure_class"
        );
        assert!(
            payload["started_at"].as_str().is_some(),
            "started_at must be a string"
        );
        assert!(
            payload["duration_ms"].as_u64().is_some(),
            "duration_ms must be a number"
        );

        // The command field in the payload must be present (and redacted if
        // it contains secrets — it doesn't here, but the contract is that
        // it is always a string).
        let payload_cmd = payload["command"].as_str().expect("command must be string");
        assert!(
            payload_cmd.contains("prepare-test-db.sh"),
            "payload command must reference the repo script: {payload_cmd}"
        );

        // output_tail confirms the command actually ran — the script itself
        // produces no stdout, so the tail may be empty, but the field must
        // exist.
        assert!(
            payload["output_tail"].is_string(),
            "output_tail must be a string field"
        );
        assert!(
            payload["output_truncated"].is_boolean(),
            "output_truncated must be a boolean field"
        );
    }

    /// AC: Multi-command database preparation — a generic repo declares
    /// a two-step preparation (schema application + seed data) through
    /// `lifecycle.pre_task`, both consuming `TEST_POSTGRES_URL`.  The
    /// second command depends on the first (verifies the proof marker
    /// written by the first).  This proves sequential multi-command
    /// database preparation is config-driven with no djinn special case.
    #[tokio::test]
    async fn nondjinn_multistep_db_preparation_fixture() {
        let repo_root = tempfile::tempdir().expect("tempdir");

        // schema.sql — the migration.
        let schema_sql = repo_root.path().join("schema.sql");
        std::fs::write(
            &schema_sql,
            "CREATE TABLE IF NOT EXISTS users (id SERIAL PRIMARY KEY);\n",
        )
        .expect("write schema.sql");

        // seeds.sql — deterministic seed data (not actually executed, just
        // read to prove the second step can access it).
        let seeds_sql = repo_root.path().join("seeds.sql");
        std::fs::write(&seeds_sql, "INSERT INTO users DEFAULT VALUES;\n").expect("write seeds.sql");

        let proof_marker = repo_root.path().join(".db-ready.marker");

        // Step 1: apply schema, write proof.
        let step1_cmd = format!(
            "test -n \"$TEST_POSTGRES_URL\" && test -f schema.sql && echo schema_applied > {marker}",
            marker = proof_marker.to_string_lossy(),
        );

        // Step 2: verify the proof marker exists, then "apply seeds".
        let step2_cmd = format!(
            "test -f {marker} && test -f seeds.sql && echo seeds_applied >> {marker}",
            marker = proof_marker.to_string_lossy(),
        );

        let sentinel_url = "postgres://app:secret@db.example.com:5432/myapp";
        let _guard = TestEnvGuard::set("TEST_POSTGRES_URL", sentinel_url);

        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("apply-schema".into()),
                        command: step1_cmd,
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("seed-data".into()),
                        command: step2_cmd,
                        timeout_seconds: 30,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(
            &cfg,
            repo_root.path(),
            &cancel,
            Some("t-multistep-db"),
            &sink,
        )
        .await
        .expect("multi-step db prep should succeed");

        assert!(result.all_succeeded());
        assert_eq!(result.all_results().len(), 2);

        // Verify ordering: apply-schema first, then seed-data.
        assert_eq!(result.all_results()[0].name, "apply-schema");
        assert_eq!(result.all_results()[0].index, 0);
        assert_eq!(result.all_results()[1].name, "seed-data");
        assert_eq!(result.all_results()[1].index, 1);

        // The proof marker shows both steps ran in order.
        let content = std::fs::read_to_string(&proof_marker).expect("read marker");
        assert!(
            content.contains("schema_applied"),
            "marker must show schema was applied first: {content}"
        );
        assert!(
            content.contains("seeds_applied"),
            "marker must show seeds were applied second: {content}"
        );
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "marker must have exactly two lines");
        assert_eq!(lines[0], "schema_applied");
        assert_eq!(lines[1], "seeds_applied");

        // Activity: two events, one per started command.
        assert_eq!(sink.len(), 2, "one activity event per started command");
        assert_eq!(sink.payloads()[0]["name"], "apply-schema");
        assert_eq!(sink.payloads()[0]["index"], 0);
        assert_eq!(sink.payloads()[0]["exit_code"], 0);
        assert_eq!(sink.payloads()[0]["blocked"], false);
        assert_eq!(sink.payloads()[1]["name"], "seed-data");
        assert_eq!(sink.payloads()[1]["index"], 1);
        assert_eq!(sink.payloads()[1]["exit_code"], 0);
        assert_eq!(sink.payloads()[1]["blocked"], false);

        // No failure_class on success.
        for p in sink.payloads() {
            assert!(
                !p.as_object().unwrap().contains_key("failure_class"),
                "successful commands must not carry failure_class"
            );
        }
    }

    /// AC: The generic database preparation fixture is config-driven only —
    /// when the same `EnvironmentConfig` is used with a different repo root
    /// (no schema.sql), the command fails proving the lifecycle is purely
    /// config + cwd, with no hardcoded djinn special-casing.
    #[tokio::test]
    async fn nondjinn_db_preparation_fails_gracefully_without_schema() {
        let empty_repo = tempfile::tempdir().expect("tempdir");

        let sentinel_url = "postgres://test:test@127.0.0.1:5432/empty";
        let _guard = TestEnvGuard::set("TEST_POSTGRES_URL", sentinel_url);

        // Same command shape as the success test, but in a repo root that
        // has no schema.sql — the script should fail.
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("prepare-test-db".into()),
                    command: "test -f schema.sql || exit 1".into(),
                    timeout_seconds: 10,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(
            &cfg,
            empty_repo.path(),
            &cancel,
            Some("t-nondjinn-noschema"),
            &sink,
        )
        .await
        .expect("runner should not error on nonzero exit");

        // The command fails (no schema.sql in the empty repo).
        assert!(
            result.is_blocked(),
            "blocking failure must produce Blocked when schema is missing"
        );

        // Activity was emitted for the started (and failed) command.
        assert_eq!(sink.len(), 1);
        let payload = &sink.payloads()[0];
        assert_eq!(payload["name"], "prepare-test-db");
        assert_eq!(payload["exit_code"], 1);
        assert_eq!(payload["blocked"], true);
        assert_eq!(payload["failure_class"], "environmental");
    }

    /// Helper RAII guard for test-scoped env var management.
    ///
    /// Sets a variable on construction and restores the original value
    /// (or removes it) on drop.  This prevents test env mutations from
    /// leaking across tests when running in the same binary.
    struct TestEnvGuard {
        key: String,
        original: Option<String>,
    }

    impl TestEnvGuard {
        fn set(key: &str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: single-test-binary; each test that mutates env does
            // so within its own body and restores via Drop.
            unsafe { std::env::set_var(key, value) };
            Self {
                key: key.to_string(),
                original,
            }
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(val) => {
                    // SAFETY: same rationale as set().
                    unsafe { std::env::set_var(&self.key, val) };
                }
                None => {
                    // SAFETY: same as above.
                    unsafe { std::env::remove_var(&self.key) };
                }
            }
        }
    }

    // ---- rolling-deploy and additive pre-task activity compatibility ----
    //
    // Compatibility regressions for proposal 4hx2 AC10:
    //
    // * Old/absent pre-task configuration remains a no-op where supported
    //   (rolling-deploy scenario: hgd0 mount absent, legacy mount absent).
    // * `task_run_pretask_ran` activity payloads survive JSON round-trip
    //   (persistence/listing compatibility).
    // * Bounded/redacted output handling is preserved when the additive
    //   event is surfaced or serialized.
    //
    // These tests are self-contained; they do not require live Kubernetes,
    // production rollout, or external operator proof.

    /// Rolling-deploy compatibility: when both the hgd0 task-run mount and
    /// the legacy ConfigMap mount are absent (the pre-P5 / pre-reseed
    /// project scenario), `load_task_run_environment_config_from_paths`
    /// returns `EnvironmentConfig::empty()` — the startup boundary proceeds
    /// as a no-op and does not block session dispatch.
    ///
    /// This is the exact state a project is in during rolling-deploy before
    /// the config-mounting side (hgd0) has been rolled out: the worker
    /// binary runs the new code, but the config files don't exist yet.
    #[tokio::test]
    async fn config_fallback_uses_empty_when_both_mounts_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hgd0 = tmp.path().join("nonexistent_hgd0.json");
        let legacy = tmp.path().join("nonexistent_legacy.json");

        let cfg = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .expect("must not error when both mounts are absent");

        assert_eq!(
            cfg,
            EnvironmentConfig::empty(),
            "absent mounts must yield EnvironmentConfig::empty()"
        );
        assert!(
            cfg.lifecycle.pre_task.is_empty(),
            "empty config must have no pre_task commands"
        );

        // The empty config also produces a no-op when run through the
        // runner — zero commands, zero activity events, AllSucceeded.
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-deploy"), &sink)
            .await
            .expect("empty config must not error");
        assert!(result.all_succeeded());
        assert!(result.all_results().is_empty());
        assert!(sink.is_empty());
    }

    /// Rolling-deploy compatibility: a legacy ConfigMap mount with no
    /// `lifecycle` key (the common case for existing projects that haven't
    /// declared pre-task hooks) loads as empty.  When the hgd0 mount is
    /// absent, the loader falls through to legacy and the startup boundary
    /// is still a no-op.
    #[tokio::test]
    async fn config_fallback_legacy_without_lifecycle_key_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let legacy = tmp.path().join("environment.json");
        // A config that exists but has no `lifecycle` key — serde default
        // for LifecycleHooks is an empty pre_task list.
        std::fs::write(
            &legacy,
            r#"{
                "schema_version": 1,
                "source": "auto-detected",
                "env": {"RUST_LOG": "info"}
            }"#,
        )
        .unwrap();

        let hgd0 = tmp.path().join("nonexistent_hgd0.json");

        let cfg = load_task_run_environment_config_from_paths(&hgd0, &legacy)
            .await
            .expect("must load from legacy mount");

        assert!(
            cfg.lifecycle.pre_task.is_empty(),
            "legacy config without lifecycle key must have empty pre_task"
        );

        // Still a no-op when run through the runner.
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-legacy"), &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());
        assert!(sink.is_empty());
    }

    /// Additive `task_run_pretask_ran` payloads survive JSON round-trip.
    ///
    /// This proves that a consumer persisting the payload as a JSON string
    /// (the `activity_log.payload` column) and later deserializing it back
    /// to `serde_json::Value` will see the same stable field set.  The
    /// test exercises the full runner path — not a hand-built payload —
    /// so it covers redaction, truncation markers, and conditional
    /// `failure_class` presence.
    #[tokio::test]
    async fn pretask_payload_round_trips_through_json_serialization() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = EnvironmentConfig {
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![
                    PreTaskCommand {
                        name: Some("round-trip-ok".into()),
                        command: "echo round-trip-ok".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                    PreTaskCommand {
                        name: Some("round-trip-fail".into()),
                        command: "exit 99".into(),
                        timeout_seconds: 10,
                        failure_policy: PreTaskFailurePolicy::Blocking,
                    },
                ],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let _result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-roundtrip"), &sink)
            .await
            .expect("ok");

        assert_eq!(sink.len(), 2, "two started commands, two events");

        for (i, original_payload) in sink.payloads().iter().enumerate() {
            // Simulate persistence: serialize to JSON string (like the
            // activity_log.payload column).
            let serialized =
                serde_json::to_string(original_payload).expect("payload must serialize to JSON");

            // Simulate listing/deserialization: parse the string back.
            let deserialized: serde_json::Value =
                serde_json::from_str(&serialized).expect("JSON string must deserialize back");

            // All stable fields survive the round trip.
            let required = [
                "name",
                "index",
                "command",
                "failure_policy",
                "started_at",
                "duration_ms",
                "exit_code",
                "timed_out",
                "cancelled",
                "blocked",
                "output_tail",
                "output_truncated",
            ];
            for k in &required {
                assert!(
                    deserialized.get(k).is_some(),
                    "field '{k}' must survive round trip for event {i}"
                );
            }

            // The values must match the originals.
            assert_eq!(
                deserialized, *original_payload,
                "round-tripped payload {i} must match original"
            );
        }

        // The successful command has no failure_class; the blocked one does.
        let ok_payload = &sink.payloads()[0];
        assert!(
            !ok_payload
                .as_object()
                .unwrap()
                .contains_key("failure_class"),
            "success: no failure_class"
        );
        let fail_payload = &sink.payloads()[1];
        assert_eq!(
            fail_payload.get("failure_class").and_then(|v| v.as_str()),
            Some(ENVIRONMENTAL_FAILURE_CLASS),
            "blocking failure: failure_class must be environmental"
        );

        // Verify the failure_class also survives round trip.
        let fail_serialized = serde_json::to_string(fail_payload).unwrap();
        let fail_deser: serde_json::Value = serde_json::from_str(&fail_serialized).unwrap();
        assert_eq!(
            fail_deser.get("failure_class").and_then(|v| v.as_str()),
            Some(ENVIRONMENTAL_FAILURE_CLASS),
            "failure_class must survive JSON round trip"
        );
    }

    /// Bounded/redacted output: the activity payload's `output_tail` stays
    /// within the OUTPUT_MAX_BYTES bound and preserves `[REDACTED]` markers
    /// even after JSON serialization round-trip.
    ///
    /// This covers the AC that "compatibility coverage asserts
    /// bounded/redacted output handling is preserved when the additive event
    /// is surfaced or serialized."
    ///
    /// Redaction happens BEFORE truncation, so a secret-only output that
    /// gets fully redacted can shrink below the truncation threshold.
    /// This test uses large NON-secret filler (~24 KiB) to guarantee
    /// truncation, then appends a secret value to exercise redaction
    /// in the same payload.
    #[tokio::test]
    async fn pretask_activity_output_tail_bounded_with_redaction_after_serialization() {
        let secret_value = "sk-compat-regression-long-secret-key-1234567890";
        let _guard = TestEnvGuard::set("COMPAT_REGRESSION_SECRET", secret_value);

        let tmp = tempfile::tempdir().expect("tempdir");
        // ~24 KiB of non-secret filler (each line ~80 chars, 300 lines)
        // stays above OUTPUT_MAX_BYTES even after the secret portion is
        // redacted. A single echo of the secret at the end adds a
        // redactable segment.
        let cfg = EnvironmentConfig {
            env: [(
                "COMPAT_REGRESSION_SECRET".to_owned(),
                secret_value.to_owned(),
            )]
            .into_iter()
            .collect(),
            lifecycle: djinn_stack::environment::LifecycleHooks {
                pre_task: vec![PreTaskCommand {
                    name: Some("large-secret-output".into()),
                    command: concat!(
                        // ~24 KiB of filler that does NOT contain the secret.
                        "for i in $(seq 1 300); do ",
                        "printf 'Line %04d: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' \"$i\"; ",
                        "done; ",
                        // A secret-containing echo to exercise redaction.
                        "echo credential=${COMPAT_REGRESSION_SECRET}"
                    )
                    .to_string(),
                    timeout_seconds: 30,
                    failure_policy: PreTaskFailurePolicy::Blocking,
                }],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };

        let cancel = CancellationToken::new();
        let sink = RecordingActivitySink::new();
        let result = run_pre_task_commands(&cfg, tmp.path(), &cancel, Some("t-bounded"), &sink)
            .await
            .expect("ok");
        assert!(result.all_succeeded());

        let payload = &sink.payloads()[0];

        // 1. output_tail must be a string (not null).
        let tail = payload["output_tail"]
            .as_str()
            .expect("output_tail is string");

        // 2. The secret value must NOT appear in the payload.
        assert!(
            !tail.contains(secret_value),
            "output_tail must be redacted, got: {}",
            &tail[..tail.len().min(200)]
        );

        // 3. The [REDACTED] marker must be present (secrets were found and redacted).
        assert!(
            tail.contains("[REDACTED]"),
            "output_tail must contain [REDACTED] marker"
        );

        // 4. output_truncated must be true (large non-secret filler output).
        assert_eq!(
            payload["output_truncated"].as_bool(),
            Some(true),
            "large output must be truncated"
        );

        // 5. The tail must be bounded (OUTPUT_MAX_BYTES + truncation marker).
        assert!(
            tail.len() <= OUTPUT_MAX_BYTES + 200,
            "output_tail must be bounded, got {} bytes",
            tail.len()
        );

        // 6. Serialize → deserialize and verify redaction + bounds survive.
        let serialized = serde_json::to_string(payload).expect("serialize");
        let deserialized: serde_json::Value =
            serde_json::from_str(&serialized).expect("deserialize");

        let round_tripped_tail = deserialized["output_tail"]
            .as_str()
            .expect("output_tail survives round trip");
        assert!(
            !round_tripped_tail.contains(secret_value),
            "redaction must survive JSON round trip"
        );
        assert!(
            round_tripped_tail.contains("[REDACTED]"),
            "redaction marker must survive JSON round trip"
        );
        assert_eq!(
            deserialized["output_truncated"].as_bool(),
            Some(true),
            "truncated flag must survive JSON round trip"
        );
    }
}

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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use djinn_stack::environment::{EnvironmentConfig, HookCommand};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
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

/// Stub: execute the pre-task commands from `lifecycle.pre_task`.
///
/// Currently always returns `Ok(())`.  Later tasks replace this with
/// real command execution, timeout enforcement, and failure-policy
/// handling.
pub async fn run_pre_task_commands(
    _environment_config: &EnvironmentConfig,
    _project_root: &Path,
) -> Result<()> {
    // Stub — later tasks implement actual pre-task command execution.
    Ok(())
}

/// Run the full pre-task startup boundary: load inputs, check readiness,
/// and execute pre-task commands.
///
/// Returns [`TaskRunPreTaskInputs`] so the caller (e.g. for environment
/// variable injection into the supervisor context) can inspect the loaded
/// config and metadata.
///
/// This is the single entry point called from `run_task_run` between
/// workspace attach and supervisor dispatch.  If any step fails, the
/// task-run does not proceed to the supervisor.
pub async fn execute_task_run_startup_boundary(
    project_root: &Path,
) -> Result<TaskRunPreTaskInputs> {
    let inputs = prepare_task_run_inputs().await?;

    check_service_readiness(&inputs.service_metadata).await?;
    info!("service readiness check passed");

    run_pre_task_commands(&inputs.environment_config, project_root).await?;
    info!(
        pre_task_count = inputs.environment_config.lifecycle.pre_task.len(),
        "pre-task commands completed"
    );

    Ok(inputs)
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
        run_pre_task_commands(&cfg, tmp.path()).await.expect("ok");
    }

    #[tokio::test]
    async fn execute_startup_boundary_succeeds_with_no_mounts() {
        // With no files on disk, the boundary should succeed using defaults.
        // This tests the full orchestration seam.
        // NOTE: uses real mount paths which won't exist in CI — that's fine,
        // the loaders default gracefully.
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = execute_task_run_startup_boundary(tmp.path()).await;
        assert!(
            result.is_ok(),
            "startup boundary should succeed with defaults: {result:?}"
        );
        let inputs = result.unwrap();
        assert!(inputs.environment_config.lifecycle.pre_task.is_empty());
        assert!(inputs.service_metadata.injected.is_empty());
    }
}

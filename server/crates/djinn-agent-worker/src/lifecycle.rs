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
use tokio::process::Command;
use tracing::{info, warn};

/// Canonical mount path the Pod spec attaches the environment-config
/// ConfigMap at. Matches `djinn_k8s::env_config::ENV_CONFIG_MOUNT_FILE`.
pub const ENV_CONFIG_MOUNT_FILE: &str = "/etc/djinn/environment.json";

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
            return Err(anyhow::Error::from(err)
                .context(format!("read {}", path.display())));
        }
    };
    let cfg: EnvironmentConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(cfg))
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
        let start = std::time::Instant::now();
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
            HookCommand::Shell(format!(
                "echo first >> {}",
                stamp.to_string_lossy()
            )),
            HookCommand::Shell(format!(
                "echo second >> {}",
                stamp.to_string_lossy()
            )),
        ];
        run_phase(tmp.path(), "pre_warm", &commands).await.expect("ok");
        let content = std::fs::read_to_string(&stamp).unwrap();
        assert_eq!(content, "first\nsecond\n");
    }

    #[tokio::test]
    async fn run_phase_substitutes_container_workspace_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let commands = vec![HookCommand::Shell(
            "touch ${containerWorkspaceFolder}/marker".into(),
        )];
        run_phase(tmp.path(), "pre_warm", &commands).await.expect("ok");
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
        run_phase(tmp.path(), "pre_warm", &commands).await.expect("ok");
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
        run_phase(tmp.path(), "pre_warm", &commands).await.expect("ok");
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
        assert!(err.to_string().contains("Some(7)"), "got: {err}");
    }
}

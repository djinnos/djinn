//! Minimal devcontainer lifecycle runner for the K8s warm Pod.
//!
//! The warm Pod bypasses the `devcontainer up` CLI flow (Node-dependent,
//! docker-socket-hungry, expects persistent containers) — we schedule Pods
//! via the Kubernetes API and just `exec` our own entrypoint. The cost: none
//! of the user's `onCreateCommand` / `postCreateCommand` /
//! `updateContentCommand` hooks fire, so per-project setup steps (like
//! `rustup component add rust-analyzer` for a pinned toolchain) silently
//! don't happen — and the canonical-graph indexers fail at runtime because
//! the tools they need aren't where they're supposed to be.
//!
//! This module implements just enough of the devcontainer Remote-Containers
//! Development Specification to run the four hooks that fire after container
//! creation. Scope:
//!
//! * Read `.devcontainer/devcontainer.json` or `.devcontainer.json` at the
//!   cloned workspace root.
//! * Strip JSONC comments (`//` and `/* */`) and trailing commas before
//!   feeding to `serde_json` — VS Code's devcontainer.json is JSONC, not
//!   strict JSON.
//! * Run hooks in spec order (`onCreateCommand` → `updateContentCommand` →
//!   `postCreateCommand` → `postStartCommand`). Each hook supports the three
//!   spec forms:
//!     * **String** — passed to `/bin/sh -c`, so shell metacharacters work.
//!     * **Array** — exec'd directly with no shell interpretation (argv form).
//!     * **Object** — map of named commands run **in parallel**.
//! * Variable substitution: `${containerWorkspaceFolder}`,
//!   `${containerWorkspaceFolderBasename}`, `${containerEnv:NAME}`,
//!   `${localEnv:NAME}`. In the warm Pod there's no separate host/container,
//!   so `localEnv` == `containerEnv` (both resolve against this process's env).
//!
//! Explicitly out of scope for this version:
//!
//! * `initializeCommand` — spec'd to run on the host before the container
//!   starts; we have no host/container split, nothing to run.
//! * `postAttachCommand` — only meaningful in interactive attach flows,
//!   which warm Pods don't do.
//! * `features.<id>` resolution — image-time Feature installation already
//!   handles this in djinn's build flow.
//! * "Has onCreateCommand ever run" persistence — warm Pods are ephemeral
//!   (one Pod per warm invocation, TTL-cleaned), so every run is effectively
//!   the first. Cheap and correct; revisit if we start sharing containers.
//! * Forwarding stdout/stderr to a structured sink — for now we just inherit
//!   stdio so hook output lands in the Pod's kubectl-logs stream.
//!
//! Failures are returned as `anyhow::Error`. The warm-graph driver in
//! `main.rs` calls this best-effort (log + continue) so a flaky hook doesn't
//! block the indexer path — the status quo without this runner was zero
//! hook execution, so any hook progress is strictly additive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{info, warn};

/// Load the project's `EnvironmentConfig` from the ConfigMap mount the
/// warm / task-run Pod specs attach at [`ENV_CONFIG_MOUNT_FILE`].
///
/// Returns:
/// * `Ok(Some(cfg))` — file present and parsed.
/// * `Ok(None)` — file missing (pre-P6 state, or the CM didn't exist at
///   Pod schedule time so Kubelet resolved the `optional: true` volume
///   to empty).
/// * `Err(...)` — file present but unreadable or unparseable. Caller
///   decides whether to hard-fail or log + continue; today's `run_lifecycle`
///   coexists with this path and still reads devcontainer.json, so we
///   don't collapse into a single fatal.
///
/// The caller supplies the path explicitly so tests can point at a tmp
/// dir; production callers pass [`ENV_CONFIG_MOUNT_FILE`] verbatim.
pub async fn load_environment_config(
    path: &Path,
) -> Result<Option<djinn_stack::environment::EnvironmentConfig>> {
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
    let cfg: djinn_stack::environment::EnvironmentConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(cfg))
}

/// Canonical mount path the Pod spec attaches the environment-config
/// ConfigMap at. Matches `djinn_k8s::env_config::ENV_CONFIG_MOUNT_FILE`.
pub const ENV_CONFIG_MOUNT_FILE: &str = "/etc/djinn/environment.json";

/// Public entrypoint. Resolves `project_root/.devcontainer/devcontainer.json`
/// (falling back to `.devcontainer.json`), parses it, and runs every
/// lifecycle hook the spec lists for container-creation time in the order
/// the spec mandates.
///
/// Returns `Ok(())` when no devcontainer.json is present — a project that
/// doesn't ship one is not an error.
pub async fn run_lifecycle(project_root: &Path) -> Result<()> {
    let Some(spec_path) = find_devcontainer_json(project_root) else {
        info!(
            project_root = %project_root.display(),
            "devcontainer lifecycle: no devcontainer.json found; skipping"
        );
        return Ok(());
    };

    info!(
        spec = %spec_path.display(),
        "devcontainer lifecycle: parsing spec"
    );

    let raw = tokio::fs::read_to_string(&spec_path)
        .await
        .with_context(|| format!("read {}", spec_path.display()))?;
    let stripped = strip_jsonc_comments(&raw);
    let spec: DevcontainerSpec = serde_json::from_str(&stripped)
        .with_context(|| format!("parse {}", spec_path.display()))?;

    let ctx = CommandContext::new(project_root.to_path_buf());

    let phases: [(&str, Option<&LifecycleCommand>); 4] = [
        ("onCreateCommand", spec.on_create_command.as_ref()),
        ("updateContentCommand", spec.update_content_command.as_ref()),
        ("postCreateCommand", spec.post_create_command.as_ref()),
        ("postStartCommand", spec.post_start_command.as_ref()),
    ];

    for (phase_name, maybe_cmd) in phases {
        let Some(cmd) = maybe_cmd else { continue };
        info!(phase = phase_name, "devcontainer lifecycle: running phase");
        let start = std::time::Instant::now();
        run_command(phase_name, cmd, &ctx)
            .await
            .with_context(|| format!("{phase_name} failed"))?;
        info!(
            phase = phase_name,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "devcontainer lifecycle: phase complete"
        );
    }

    Ok(())
}

/// Fields of devcontainer.json we actually read. Everything else is ignored
/// so the deserializer doesn't fail on unknown keys (the real spec has
/// dozens of fields we don't care about).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevcontainerSpec {
    #[serde(default)]
    on_create_command: Option<LifecycleCommand>,
    #[serde(default)]
    post_create_command: Option<LifecycleCommand>,
    #[serde(default)]
    update_content_command: Option<LifecycleCommand>,
    #[serde(default)]
    post_start_command: Option<LifecycleCommand>,
}

/// A lifecycle command in any of the three spec-blessed forms.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LifecycleCommand {
    /// Single string, passed to `/bin/sh -c`.
    Shell(String),
    /// Argv array, exec'd directly (no shell).
    Exec(Vec<String>),
    /// Map of named commands, run in parallel. Recursive: each value is
    /// itself a `LifecycleCommand` (so nested objects are technically
    /// allowed; we flatten them at run time).
    Parallel(BTreeMap<String, LifecycleCommand>),
}

/// Locations the spec searches, in order of precedence.
fn find_devcontainer_json(project_root: &Path) -> Option<PathBuf> {
    let primary = project_root.join(".devcontainer").join("devcontainer.json");
    if primary.is_file() {
        return Some(primary);
    }
    let fallback = project_root.join(".devcontainer.json");
    if fallback.is_file() {
        return Some(fallback);
    }
    None
}

/// Strip `//` line comments and `/* ... */` block comments from JSONC,
/// producing strict JSON. Honours strings + backslash escapes so `//`
/// inside a string literal survives.
fn strip_jsonc_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if escape {
            out.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if in_string {
            match c {
                '\\' => {
                    out.push(c);
                    escape = true;
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                _ => out.push(c),
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() {
            match bytes[i + 1] as char {
                '/' => {
                    // Line comment: skip to end of line (keep the newline
                    // so line numbers stay stable in parser errors).
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                '*' => {
                    // Block comment: skip until */. Preserve any newlines
                    // inside by emitting them into `out` so line numbers
                    // stay stable (whitespace is irrelevant to the parser
                    // but errors reference lines).
                    i += 2;
                    while i + 1 < bytes.len()
                        && !(bytes[i] == b'*' && bytes[i + 1] == b'/')
                    {
                        if bytes[i] == b'\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                    i += 2; // consume the closing */
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Per-invocation context threaded through command execution — cheap to
/// clone (just a PathBuf + a &self reference for env lookups).
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
                // ${containerEnv:NAME} and ${localEnv:NAME} both resolve
                // against this process's env — the warm Pod has no separate
                // host.
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

/// Dispatch a single lifecycle command in whatever form the user wrote it.
fn run_command<'a>(
    phase: &'a str,
    cmd: &'a LifecycleCommand,
    ctx: &'a CommandContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    // Returns a boxed future so recursive Parallel nesting typechecks
    // without help from async-recursion.
    Box::pin(async move {
        match cmd {
            LifecycleCommand::Shell(s) => run_shell(phase, s, ctx).await,
            LifecycleCommand::Exec(parts) => run_exec(phase, parts, ctx).await,
            LifecycleCommand::Parallel(map) => {
                let mut join = tokio::task::JoinSet::new();
                for (name, sub_cmd) in map.iter() {
                    let sub_phase = format!("{phase}/{name}");
                    let ctx = ctx.clone();
                    // Serialize the nested command by converting to owned.
                    // Recursion path is rare enough that the clone isn't
                    // hot.
                    let sub_cmd_owned = clone_command(sub_cmd);
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

/// Deep-clone a `LifecycleCommand`. Needed because `JoinSet::spawn` wants an
/// owned future; passing `&LifecycleCommand` via reference would require
/// lifetime gymnastics with Pin<Box<dyn Future + 'a>> that fight `spawn`'s
/// `'static` bound. The values are tiny (a few strings), so clone is cheap.
fn clone_command(cmd: &LifecycleCommand) -> LifecycleCommand {
    match cmd {
        LifecycleCommand::Shell(s) => LifecycleCommand::Shell(s.clone()),
        LifecycleCommand::Exec(parts) => LifecycleCommand::Exec(parts.clone()),
        LifecycleCommand::Parallel(map) => LifecycleCommand::Parallel(
            map.iter()
                .map(|(k, v)| (k.clone(), clone_command(v)))
                .collect(),
        ),
    }
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

    #[test]
    fn strip_jsonc_preserves_strings() {
        let input = r#"{
            // this is a line comment
            "name": "has // inside",
            /* block
               comment */
            "url": "https://example.com/foo" // trailing
        }"#;
        let out = strip_jsonc_comments(input);
        // Comments gone, string content intact.
        assert!(!out.contains("line comment"));
        assert!(!out.contains("block"));
        assert!(out.contains("has // inside"));
        assert!(out.contains("https://example.com/foo"));
        // Strict JSON parse must succeed.
        let v: serde_json::Value = serde_json::from_str(&out).expect("parse");
        assert_eq!(v["name"], "has // inside");
        assert_eq!(v["url"], "https://example.com/foo");
    }

    #[test]
    fn substitute_known_variables() {
        let ctx = CommandContext::new(PathBuf::from("/workspace/abc"));
        assert_eq!(
            ctx.substitute("cd ${containerWorkspaceFolder} && pwd"),
            "cd /workspace/abc && pwd"
        );
        assert_eq!(
            ctx.substitute("name=${containerWorkspaceFolderBasename}"),
            "name=abc"
        );
        // Unknown variables are preserved verbatim so accidental
        // misspellings don't silently collapse to the empty string.
        assert_eq!(
            ctx.substitute("hi ${nonsenseVar}"),
            "hi ${nonsenseVar}"
        );
    }

    #[test]
    fn substitute_env_variables() {
        // SAFETY: test is single-threaded in cargo nextest's isolation; we
        // set + remove the var immediately.
        unsafe { std::env::set_var("DJINN_LIFECYCLE_TEST_X", "hello"); }
        let ctx = CommandContext::new(PathBuf::from("/tmp"));
        assert_eq!(
            ctx.substitute("x=${containerEnv:DJINN_LIFECYCLE_TEST_X}"),
            "x=hello"
        );
        assert_eq!(
            ctx.substitute("x=${localEnv:DJINN_LIFECYCLE_TEST_X}"),
            "x=hello"
        );
        unsafe { std::env::remove_var("DJINN_LIFECYCLE_TEST_X"); }
    }

    #[test]
    fn parse_all_three_command_forms() {
        let input = r#"{
            "onCreateCommand": "echo hi",
            "postCreateCommand": ["echo", "array"],
            "updateContentCommand": {
                "a": "echo a",
                "b": ["echo", "b"]
            }
        }"#;
        let spec: DevcontainerSpec =
            serde_json::from_str(&strip_jsonc_comments(input)).expect("parse");
        assert!(matches!(spec.on_create_command, Some(LifecycleCommand::Shell(_))));
        assert!(matches!(spec.post_create_command, Some(LifecycleCommand::Exec(_))));
        assert!(matches!(
            spec.update_content_command,
            Some(LifecycleCommand::Parallel(_))
        ));
    }

    #[test]
    fn find_devcontainer_prefers_dot_devcontainer_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let primary = root.join(".devcontainer");
        std::fs::create_dir(&primary).unwrap();
        std::fs::write(primary.join("devcontainer.json"), "{}").unwrap();
        std::fs::write(root.join(".devcontainer.json"), "{}").unwrap();
        let found = find_devcontainer_json(root).expect("found");
        assert_eq!(found, primary.join("devcontainer.json"));
    }

    #[test]
    fn find_devcontainer_falls_back_to_root_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join(".devcontainer.json"), "{}").unwrap();
        let found = find_devcontainer_json(root).expect("found");
        assert_eq!(found, root.join(".devcontainer.json"));
    }

    #[test]
    fn find_devcontainer_returns_none_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(find_devcontainer_json(tmp.path()).is_none());
    }

    #[tokio::test]
    async fn run_lifecycle_is_noop_without_devcontainer_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_lifecycle(tmp.path()).await.expect("ok");
    }

    #[tokio::test]
    async fn run_lifecycle_executes_shell_hook() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker");
        let json = format!(
            r#"{{
                "postCreateCommand": "touch {}"
            }}"#,
            marker.display()
        );
        std::fs::write(tmp.path().join(".devcontainer.json"), json).unwrap();
        run_lifecycle(tmp.path()).await.expect("ok");
        assert!(marker.exists(), "postCreateCommand should have created marker");
    }

    #[tokio::test]
    async fn run_lifecycle_executes_hooks_in_spec_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("order.log");
        let json = format!(
            r#"{{
                "onCreateCommand": "echo on >> {log}",
                "updateContentCommand": "echo update >> {log}",
                "postCreateCommand": "echo post >> {log}",
                "postStartCommand": "echo start >> {log}"
            }}"#,
            log = log.display()
        );
        std::fs::write(tmp.path().join(".devcontainer.json"), json).unwrap();
        run_lifecycle(tmp.path()).await.expect("ok");
        let content = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines, vec!["on", "update", "post", "start"]);
    }

    #[tokio::test]
    async fn run_lifecycle_parallel_object_runs_all_children() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let json = format!(
            r#"{{
                "postCreateCommand": {{
                    "alpha": "touch {}",
                    "beta":  ["touch", "{}"]
                }}
            }}"#,
            a.display(),
            b.display()
        );
        std::fs::write(tmp.path().join(".devcontainer.json"), json).unwrap();
        run_lifecycle(tmp.path()).await.expect("ok");
        assert!(a.exists());
        assert!(b.exists());
    }

    #[tokio::test]
    async fn run_lifecycle_propagates_shell_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let json = r#"{ "postCreateCommand": "exit 7" }"#;
        std::fs::write(tmp.path().join(".devcontainer.json"), json).unwrap();
        let err = run_lifecycle(tmp.path()).await.expect_err("should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("postCreateCommand"), "err: {msg}");
    }

    #[tokio::test]
    async fn run_lifecycle_substitutes_container_workspace_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker");
        // Use ${containerWorkspaceFolder} in the command.
        let json = r#"{
            "postCreateCommand": "touch ${containerWorkspaceFolder}/marker"
        }"#;
        std::fs::write(tmp.path().join(".devcontainer.json"), json).unwrap();
        run_lifecycle(tmp.path()).await.expect("ok");
        assert!(marker.exists());
    }

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
}

use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use djinn_core::commands::{CommandResult, CommandSpec};

const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Compute an isolated `CARGO_TARGET_DIR` path for a given working directory.
///
/// Each worktree gets its own cargo target dir so parallel verification runs
/// don't contend on the workspace-level build cache or each other's `target/`
/// lock files.  The path shape matches the legacy `.djinn/settings.json` setup
/// script it replaces so existing cached artifacts stay valid:
/// `/tmp/djinn-targets/<basename(working_dir)>-server`.
///
/// Returns `None` when the working directory has no filename component
/// (e.g. `/`), in which case the caller should leave `CARGO_TARGET_DIR`
/// untouched and fall back to cargo's default.
fn isolated_cargo_target_dir(working_dir: &Path) -> Option<std::path::PathBuf> {
    let basename = working_dir.file_name()?.to_str()?;
    Some(std::path::PathBuf::from("/tmp/djinn-targets").join(format!("{basename}-server")))
}

/// Run a pre-configured `std::process::Command` with process-group isolation
/// and timeout kill-tree behavior.
async fn spawn_command(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::process::isolate_process_group(&mut cmd);
    crate::process::output_with_kill(cmd, timeout).await
}

pub async fn run_commands(
    commands: &[CommandSpec],
    working_dir: &Path,
) -> anyhow::Result<Vec<CommandResult>> {
    let mut results = Vec::with_capacity(commands.len());

    for spec in commands {
        let timeout_secs = spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let duration = Duration::from_secs(timeout_secs);
        let start = Instant::now();

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&spec.command).current_dir(working_dir);

        // Inject a per-worktree CARGO_TARGET_DIR so parallel verification
        // pipelines don't share a single workspace `target/` lock or stomp
        // on each other's incremental compile state.  Respect an explicit
        // override from the parent process env, so operators can still
        // pin the cache location globally.
        if std::env::var_os("CARGO_TARGET_DIR").is_none()
            && let Some(target_dir) = isolated_cargo_target_dir(working_dir)
        {
            cmd.env("CARGO_TARGET_DIR", &target_dir);
        }

        let output = spawn_command(cmd, duration)
            .await
            .map_err(|e| anyhow::anyhow!("failed to run '{}': {}", spec.name, e))?;

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        results.push(CommandResult {
            name: spec.name.clone(),
            command: spec.command.clone(),
            exit_code,
            stdout,
            stderr,
            duration_ms,
        });

        if exit_code != 0 {
            break;
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_echo_command() {
        let commands = vec![CommandSpec {
            name: "greet".into(),
            command: "echo hello".into(),
            timeout_secs: None,
        }];
        let results = run_commands(&commands, &tmp_dir()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_code, 0);
        assert!(results[0].stdout.contains("hello"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_fail_fast_on_nonzero_exit() {
        let commands = vec![
            CommandSpec {
                name: "fail".into(),
                command: "false".into(),
                timeout_secs: None,
            },
            CommandSpec {
                name: "unreachable".into(),
                command: "echo should_not_run".into(),
                timeout_secs: None,
            },
        ];
        let results = run_commands(&commands, &tmp_dir()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_ne!(results[0].exit_code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_timeout_enforced() {
        let commands = vec![CommandSpec {
            name: "slow".into(),
            command: "sleep 10".into(),
            timeout_secs: Some(1),
        }];
        let results = run_commands(&commands, &tmp_dir()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_ne!(results[0].exit_code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stderr_captured() {
        let commands = vec![CommandSpec {
            name: "err".into(),
            command: "echo oops >&2; exit 1".into(),
            timeout_secs: None,
        }];
        let results = run_commands(&commands, &tmp_dir()).await.unwrap();
        assert!(results[0].stderr.contains("oops"));
        assert_ne!(results[0].exit_code, 0);
    }

    #[test]
    fn isolated_cargo_target_dir_uses_basename() {
        let path = Path::new("/home/dev/.djinn/worktrees/0bhg");
        assert_eq!(
            isolated_cargo_target_dir(path),
            Some(PathBuf::from("/tmp/djinn-targets/0bhg-server"))
        );
    }

    #[test]
    fn isolated_cargo_target_dir_handles_trailing_slash() {
        let path = Path::new("/home/dev/project/");
        assert_eq!(
            isolated_cargo_target_dir(path),
            Some(PathBuf::from("/tmp/djinn-targets/project-server"))
        );
    }

    #[test]
    fn isolated_cargo_target_dir_returns_none_for_root() {
        assert_eq!(isolated_cargo_target_dir(Path::new("/")), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_commands_sequential() {
        let commands = vec![
            CommandSpec {
                name: "first".into(),
                command: "echo one".into(),
                timeout_secs: None,
            },
            CommandSpec {
                name: "second".into(),
                command: "echo two".into(),
                timeout_secs: None,
            },
        ];
        let results = run_commands(&commands, &tmp_dir()).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].stdout.contains("one"));
        assert!(results[1].stdout.contains("two"));
    }
}

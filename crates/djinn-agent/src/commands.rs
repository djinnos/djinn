use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tokio::time::timeout;

use djinn_core::commands::{CommandResult, CommandSpec};

const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Run a pre-configured `std::process::Command` on a blocking thread.
///
/// Uses `spawn_blocking` rather than `tokio::process::Command` to avoid
/// reactor fd issues when the server runs as a daemon with null stdio.
async fn spawn_command(mut cmd: Command) -> io::Result<Output> {
    crate::process::isolate_process_group(&mut cmd);
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
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
        let output = timeout(duration, spawn_command(cmd))
            .await
            .map_err(|_| {
                anyhow::anyhow!("command '{}' timed out after {}s", spec.name, timeout_secs)
            })?
            .map_err(|e| anyhow::anyhow!("failed to run '{}': {}", spec.name, e))?;

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        results.push(CommandResult {
            name: spec.name.clone(),
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
        let err = run_commands(&commands, &tmp_dir()).await.unwrap_err();
        assert!(err.to_string().contains("timed out"));
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

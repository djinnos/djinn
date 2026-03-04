use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::{Error, Result};

const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub name: String,
    pub command: String,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub name: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

pub async fn run_commands(commands: &[CommandSpec], working_dir: &Path) -> Result<Vec<CommandResult>> {
    let mut results = Vec::with_capacity(commands.len());

    for spec in commands {
        let timeout_secs = spec.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let duration = Duration::from_secs(timeout_secs);
        let start = Instant::now();

        let output = timeout(
            duration,
            Command::new("sh")
                .arg("-c")
                .arg(&spec.command)
                .current_dir(working_dir)
                .output(),
        )
        .await
        .map_err(|_| Error::Internal(format!(
            "command '{}' timed out after {}s",
            spec.name, timeout_secs
        )))?
        .map_err(|e| Error::Internal(format!("failed to run '{}': {}", spec.name, e)))?;

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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
    async fn test_timeout_enforced() {
        let commands = vec![CommandSpec {
            name: "slow".into(),
            command: "sleep 10".into(),
            timeout_secs: Some(1),
        }];
        let err = run_commands(&commands, &tmp_dir()).await.unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
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

    #[tokio::test]
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

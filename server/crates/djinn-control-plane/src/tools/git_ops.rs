use tokio::time::{Duration, timeout};

/// Maximum time we will wait for a best-effort git shell-out before treating it
/// as a failure.  This is intentionally conservative: these calls are used for
/// control-plane MCP tools where returning an empty result is better than hanging
/// the server.
const GIT_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `git log --format="%H|||%s|||%an|||%ai" -n <limit> -- <path>` and parse
/// degrades gracefully.
pub async fn git_log_for_file(file_path: &str, limit: i64) -> Vec<djinn_memory::GitLogEntry> {
    let args = vec![
        "log".to_string(),
        "--format=%H|||%s|||%an|||%ai".to_string(),
        format!("-n{limit}"),
        "--".to_string(),
        file_path.to_string(),
    ];

    let output = timeout(
        GIT_CMD_TIMEOUT,
        djinn_git::run_git_command_in(std::path::Path::new(file_path), args),
    )
    .await;

    let out = match output {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(path = %file_path, error = %e, "git_log_for_file failed");
            return vec![];
        }
        Err(_) => {
            tracing::warn!(path = %file_path, "git_log_for_file timed out");
            return vec![];
        }
    };

    out.stdout
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, "|||").collect();
            if parts.len() == 4 {
                Some(djinn_memory::GitLogEntry {
                    sha: parts[0].to_string(),
                    message: parts[1].to_string(),
                    author: parts[2].to_string(),
                    date: parts[3].to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Resolve the base commit SHA via `git rev-parse HEAD` in `project_path`.
/// On any failure returns `"unknown"` so the response remains well-formed.
pub async fn resolve_base_commit(project_path: &str) -> String {
    let path = std::path::Path::new(project_path);
    let fut =
        djinn_git::run_git_command_in(path, vec!["rev-parse".to_string(), "HEAD".to_string()]);

    match timeout(GIT_CMD_TIMEOUT, fut).await {
        Ok(Ok(out)) => {
            let sha = out.stdout.trim().to_string();
            if sha.is_empty() {
                tracing::warn!(
                    project = %project_path,
                    "git rev-parse HEAD returned empty output; reporting 'unknown'"
                );
                "unknown".to_string()
            } else {
                sha
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(
                project = %project_path,
                error = %e,
                "git rev-parse HEAD failed; reporting 'unknown'"
            );
            "unknown".to_string()
        }
        Err(_) => {
            tracing::warn!(
                project = %project_path,
                "git rev-parse HEAD timed out; reporting 'unknown'"
            );
            "unknown".to_string()
        }
    }
}

/// Run `git fetch --all --prune` inside `path`. Best-effort refresh for an
/// existing server-managed clone.
pub async fn git_fetch_in(path: &str) -> Result<(), String> {
    let fut = djinn_git::run_git_command_in(
        std::path::Path::new(path),
        vec![
            "fetch".to_string(),
            "--all".to_string(),
            "--prune".to_string(),
        ],
    );
    let output = timeout(GIT_CMD_TIMEOUT, fut)
        .await
        .map_err(|_| "git fetch timed out".to_string())?
        .map_err(|e| format!("git fetch failed: {e}"))?;

    if output.code != 0 {
        return Err(format!("git fetch failed: {}", output.stderr.trim()));
    }
    Ok(())
}

/// Clone `remote_url` into `clone_path` with a blob filter to keep history
/// light.  Returns the raw command output on success or an error string on
/// failure.
pub async fn git_clone_blob_none(remote_url: &str, clone_path: &str) -> Result<String, String> {
    let fut = djinn_git::run_git_command_in(
        std::path::Path::new("."),
        vec![
            "clone".to_string(),
            "--filter=blob:none".to_string(),
            remote_url.to_string(),
            clone_path.to_string(),
        ],
    );
    let output = timeout(GIT_CMD_TIMEOUT, fut)
        .await
        .map_err(|_| "git clone timed out".to_string())?
        .map_err(|e| format!("git clone failed: {e}"))?;

    if output.code != 0 {
        return Err(format!("git clone failed: {}", output.stderr.trim()));
    }
    Ok(output.stdout)
}

/// Set a git config value in `clone_path` for `key` to `value`.  Errors are
/// logged and returned as strings so callers can decide whether to fail or
/// continue best-effort.
pub async fn git_config_set(clone_path: &str, key: &str, value: &str) -> Result<(), String> {
    let fut = djinn_git::run_git_command_in(
        std::path::Path::new(clone_path),
        vec!["config".to_string(), key.to_string(), value.to_string()],
    );
    let output = timeout(GIT_CMD_TIMEOUT, fut)
        .await
        .map_err(|_| format!("git config {key} timed out"))?
        .map_err(|e| format!("git config {key} failed: {e}"))?;

    if output.code != 0 {
        return Err(format!("git config {key} failed: {}", output.stderr.trim()));
    }
    Ok(())
}

/// Resolve the current branch via `git rev-parse --abbrev-ref HEAD` in `path`.
/// Returns `Ok(None)` when the repo is in detached HEAD state or the command
/// fails, matching the previous behavior that treated `HEAD` as no current
/// branch.
pub async fn git_current_branch(path: &str) -> Result<Option<String>, String> {
    let fut = djinn_git::run_git_command_in(
        std::path::Path::new(path),
        vec![
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "HEAD".to_string(),
        ],
    );
    let output = timeout(GIT_CMD_TIMEOUT, fut)
        .await
        .map_err(|_| "git rev-parse timed out".to_string())?
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if output.code != 0 {
        return Err(format!("git rev-parse failed: {}", output.stderr.trim()));
    }
    let raw = output.stdout.trim().to_string();
    if raw.is_empty() || raw == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(raw))
    }
}

/// List local branches via `git branch --list --format=%(refname:short)` in
/// `path`.  Returns the raw stdout on success; errors are returned as strings.
pub async fn git_local_branches(path: &str) -> Result<String, String> {
    let fut = djinn_git::run_git_command_in(
        std::path::Path::new(path),
        vec![
            "branch".to_string(),
            "--list".to_string(),
            "--format=%(refname:short)".to_string(),
        ],
    );
    let output = timeout(GIT_CMD_TIMEOUT, fut)
        .await
        .map_err(|_| "git branch timed out".to_string())?
        .map_err(|e| format!("git branch failed: {e}"))?;

    if output.code != 0 {
        return Err(format!("git branch failed: {}", output.stderr.trim()));
    }
    Ok(output.stdout)
}
